//! An in-memory backend.
//!
//! It exists so the test suite can exercise the whole request path without a
//! database, and so `portcullis demo` works on a laptop with nothing installed. It
//! evaluates plans directly rather than going through SQL, which also makes it
//! a second opinion: if the SQL builder and this evaluator ever disagree about
//! what a filter means, a test notices.
//!
//! It is not a database. There are no transactions, no concurrency control and
//! no durability, and it refuses to be used outside those two roles.

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use portcullis_core::spec::OrderTerm;
use portcullis_core::{Column, Error, Result, Schema, Table, Value, WriteMode};
use portcullis_sql::{CmpOp, Expr, Term};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::Backend;
use crate::plan::{ExecCtx, ReadPlan, Rows, WriteOutcome, WritePlan};

type Row = BTreeMap<String, Value>;

/// Rows held in memory, with a schema to validate against.
#[derive(Debug)]
pub struct MemoryBackend {
    schema: Schema,
    data: RwLock<BTreeMap<String, Vec<Row>>>,
    origin: String,
}

/// The on-disk fixture format used by `portcullis demo` and the tests.
#[derive(Debug, Deserialize)]
pub struct Fixture {
    /// Tables, with their columns and starting rows.
    pub tables: Vec<FixtureTable>,
}

/// One table in a fixture.
#[derive(Debug, Deserialize)]
pub struct FixtureTable {
    /// Qualified table name.
    pub name: String,
    /// Column definitions.
    pub columns: Vec<Column>,
    /// Primary key columns.
    #[serde(default)]
    pub primary_key: Vec<String>,
    /// Starting rows, as JSON objects keyed by column name.
    #[serde(default)]
    pub rows: Vec<serde_json::Map<String, serde_json::Value>>,
}

impl MemoryBackend {
    /// Load a fixture from a JSON file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read fixture `{}`: {e}", path.display())))?;
        let fixture: Fixture = serde_json::from_str(&text).map_err(|e| {
            Error::Config(format!("fixture `{}` is not valid: {e}", path.display()))
        })?;
        Self::from_fixture(fixture, &path.display().to_string())
    }

    /// Build a backend from a parsed fixture.
    pub fn from_fixture(fixture: Fixture, origin: &str) -> Result<Self> {
        let mut tables = Vec::new();
        let mut data = BTreeMap::new();
        for ft in fixture.tables {
            let table = Table {
                name: ft.name.clone(),
                columns: ft.columns,
                primary_key: ft.primary_key,
            };
            let mut rows = Vec::new();
            for (i, raw) in ft.rows.into_iter().enumerate() {
                let mut row = Row::new();
                for (key, json) in raw {
                    let col = table.column(&key).ok_or_else(|| {
                        Error::Config(format!(
                            "fixture row {i} of `{}` sets unknown column `{key}`",
                            ft.name
                        ))
                    })?;
                    let value = Value::from_json(&json, col.ty).map_err(|e| {
                        Error::Config(format!(
                            "fixture row {i} of `{}`, column `{key}`: {e}",
                            ft.name
                        ))
                    })?;
                    row.insert(col.name.clone(), value);
                }
                for col in &table.columns {
                    row.entry(col.name.clone()).or_insert(Value::Null);
                }
                rows.push(row);
            }
            data.insert(table.name.clone(), rows);
            tables.push(table);
        }
        Ok(Self {
            schema: Schema::new(tables),
            data: RwLock::new(data),
            origin: origin.to_owned(),
        })
    }

    fn resolve_table(&self, name: &str) -> Result<String> {
        self.schema
            .table(name)
            .map(|t| t.name.clone())
            .ok_or_else(|| Error::Backend(format!("no table named `{name}`")))
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    fn describe(&self) -> String {
        format!("in-memory ({})", self.origin)
    }

    async fn schema(&self) -> Result<Schema> {
        Ok(self.schema.clone())
    }

    async fn read(&self, plan: &ReadPlan<'_>, ctx: &ExecCtx<'_>) -> Result<Rows> {
        let table = self.resolve_table(plan.table)?;
        let data = self.data.read().await;
        let all = data.get(&table).map(Vec::as_slice).unwrap_or_default();

        let filters = [plan.row_filter, plan.filter];
        let mut selected: Vec<&Row> = Vec::new();
        for row in all {
            let mut keep = true;
            for f in filters.into_iter().flatten() {
                if !matches(f, row, ctx)? {
                    keep = false;
                    break;
                }
            }
            if keep {
                selected.push(row);
            }
        }

        sort_rows(&mut selected, plan.order_by);
        selected.truncate(plan.limit as usize);

        Ok(Rows {
            columns: plan.columns.to_vec(),
            rows: selected
                .into_iter()
                .map(|r| {
                    plan.columns
                        .iter()
                        .map(|c| r.get(c).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect(),
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one branch per write mode, mirroring the SQL builder"
    )]
    async fn write(&self, plan: &WritePlan<'_>, ctx: &ExecCtx<'_>) -> Result<WriteOutcome> {
        let table_name = self.resolve_table(plan.table)?;
        let table = self
            .schema
            .table(&table_name)
            .ok_or_else(|| Error::Backend(format!("no table named `{}`", plan.table)))?
            .clone();

        let forced: BTreeMap<&str, &Term> = if plan.mode == WriteMode::Update {
            BTreeMap::new()
        } else {
            plan.row_filter
                .map(portcullis_sql::conjunct_equalities)
                .transpose()?
                .unwrap_or_default()
        };

        let mut incoming = Row::new();
        for (col, term) in plan.columns {
            let term = forced.get(col.as_str()).copied().unwrap_or(term);
            if let Some(v) = resolve(term, ctx)? {
                incoming.insert(col.clone(), v);
            }
        }
        for (col, term) in &forced {
            if !incoming.contains_key(*col) {
                if let Some(v) = resolve(term, ctx)? {
                    incoming.insert((*col).to_owned(), v);
                }
            }
        }
        for col in incoming.keys() {
            if table.column(col).is_none() {
                return Err(Error::Backend(format!(
                    "table `{table_name}` has no column `{col}`"
                )));
            }
        }

        let mut data = self.data.write().await;
        let rows = data.entry(table_name.clone()).or_default();

        let key_match = |row: &Row, incoming: &Row| {
            !plan.keys.is_empty()
                && plan
                    .keys
                    .iter()
                    .all(|k| incoming.get(k).is_some_and(|v| row.get(k) == Some(v)))
        };

        let mut affected: Vec<Row> = Vec::new();
        let mut count = 0u64;
        match plan.mode {
            WriteMode::Insert => {
                let mut row = incoming.clone();
                for col in &table.columns {
                    row.entry(col.name.clone()).or_insert(Value::Null);
                }
                affected.push(row.clone());
                rows.push(row);
                count = 1;
            }
            WriteMode::Upsert => {
                if let Some(existing) = rows.iter_mut().find(|r| key_match(r, &incoming)) {
                    for (k, v) in &incoming {
                        if !plan.keys.iter().any(|key| key == k) {
                            existing.insert(k.clone(), v.clone());
                        }
                    }
                    affected.push(existing.clone());
                } else {
                    let mut row = incoming.clone();
                    for col in &table.columns {
                        row.entry(col.name.clone()).or_insert(Value::Null);
                    }
                    affected.push(row.clone());
                    rows.push(row);
                }
                count = 1;
            }
            WriteMode::Update => {
                for row in rows.iter_mut() {
                    if !key_match(row, &incoming) {
                        continue;
                    }
                    if let Some(rf) = plan.row_filter {
                        if !matches(rf, row, ctx)? {
                            continue;
                        }
                    }
                    for (k, v) in &incoming {
                        if !plan.keys.iter().any(|key| key == k) {
                            row.insert(k.clone(), v.clone());
                        }
                    }
                    affected.push(row.clone());
                    count += 1;
                }
            }
        }

        let returned = if plan.returning.is_empty() {
            Rows::default()
        } else {
            Rows {
                columns: plan.returning.to_vec(),
                rows: affected
                    .iter()
                    .map(|r| {
                        plan.returning
                            .iter()
                            .map(|c| r.get(c).cloned().unwrap_or(Value::Null))
                            .collect()
                    })
                    .collect(),
            }
        };

        Ok(WriteOutcome {
            rows_affected: count,
            returned,
        })
    }

    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Three-valued logic, as SQL uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn of(b: bool) -> Self {
        if b { Self::True } else { Self::False }
    }
}

/// Does a row satisfy a predicate?
///
/// A predicate whose parameter was not supplied drops out, matching the SQL
/// builder. Unknown (null-touching) results are not matches.
fn matches(expr: &Expr, row: &Row, ctx: &ExecCtx<'_>) -> Result<bool> {
    Ok(eval(expr, row, ctx)?.unwrap_or(Tri::True) == Tri::True)
}

/// `Ok(None)` means the predicate dropped out because a parameter was absent.
fn eval(expr: &Expr, row: &Row, ctx: &ExecCtx<'_>) -> Result<Option<Tri>> {
    Ok(match expr {
        Expr::And(a, b) => match (eval(a, row, ctx)?, eval(b, row, ctx)?) {
            (Some(x), Some(y)) => Some(and(x, y)),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
        Expr::Or(a, b) => match (eval(a, row, ctx)?, eval(b, row, ctx)?) {
            (Some(x), Some(y)) => Some(or(x, y)),
            _ => {
                return Err(Error::Validation {
                    action: String::new(),
                    problem: "an optional parameter is used inside `or`".into(),
                });
            }
        },
        Expr::Not(a) => match eval(a, row, ctx)? {
            Some(Tri::True) => Some(Tri::False),
            Some(Tri::False) => Some(Tri::True),
            Some(Tri::Unknown) => Some(Tri::Unknown),
            None => {
                return Err(Error::Validation {
                    action: String::new(),
                    problem: "an optional parameter is used inside `not`".into(),
                });
            }
        },
        Expr::Cmp { column, op, term } => {
            let Some(right) = resolve(term, ctx)? else {
                return Ok(None);
            };
            let left = row.get(column).cloned().unwrap_or(Value::Null);
            if right.is_null() && matches!(op, CmpOp::Eq | CmpOp::Ne) {
                let is_null = left.is_null();
                Some(Tri::of(if *op == CmpOp::Eq { is_null } else { !is_null }))
            } else if left.is_null() || right.is_null() {
                Some(Tri::Unknown)
            } else {
                Some(Tri::of(compare(&left, *op, &right)))
            }
        }
        Expr::IsNull { column, negated } => {
            let is_null = row.get(column).is_none_or(Value::is_null);
            Some(Tri::of(is_null != *negated))
        }
        Expr::In {
            column,
            negated,
            terms,
        } => {
            let left = row.get(column).cloned().unwrap_or(Value::Null);
            if left.is_null() {
                return Ok(Some(Tri::Unknown));
            }
            let mut found = false;
            for t in terms {
                let Some(v) = resolve(t, ctx)? else {
                    return Ok(None);
                };
                if compare(&left, CmpOp::Eq, &v) {
                    found = true;
                }
            }
            Some(Tri::of(found != *negated))
        }
    })
}

fn and(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown,
    }
}

fn or(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::True, _) | (_, Tri::True) => Tri::True,
        (Tri::False, Tri::False) => Tri::False,
        _ => Tri::Unknown,
    }
}

fn resolve(term: &Term, ctx: &ExecCtx<'_>) -> Result<Option<Value>> {
    Ok(match term {
        Term::Param(p) => ctx.args.get(p).cloned(),
        Term::Caller(a) => {
            Some(
                ctx.caller
                    .lookup(a)
                    .ok_or_else(|| Error::MissingCallerAttribute {
                        attribute: a.clone(),
                    })?,
            )
        }
        Term::Lit(v) => Some(v.clone()),
        Term::Now => Some(Value::Timestamp(jiff::Timestamp::now())),
        Term::NewUuid => Some(Value::Uuid(uuid::Uuid::new_v4())),
    })
}

fn compare(left: &Value, op: CmpOp, right: &Value) -> bool {
    if op == CmpOp::Like {
        return match (left.as_text(), right.as_text()) {
            (Some(l), Some(r)) => like(l, r),
            _ => false,
        };
    }
    let Some(ord) = order_of(left, right) else {
        return false;
    };
    match op {
        CmpOp::Eq => ord == std::cmp::Ordering::Equal,
        CmpOp::Ne => ord != std::cmp::Ordering::Equal,
        CmpOp::Lt => ord == std::cmp::Ordering::Less,
        CmpOp::Le => ord != std::cmp::Ordering::Greater,
        CmpOp::Gt => ord == std::cmp::Ordering::Greater,
        CmpOp::Ge => ord != std::cmp::Ordering::Less,
        CmpOp::Like => unreachable!("handled above"),
    }
}

fn order_of(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use Value as V;
    match (a, b) {
        (V::Bool(x), V::Bool(y)) => Some(x.cmp(y)),
        (V::Text(x), V::Text(y)) => Some(x.cmp(y)),
        (V::Timestamp(x), V::Timestamp(y)) => Some(x.cmp(y)),
        (V::Uuid(x), V::Uuid(y)) => Some(x.cmp(y)),
        (V::Float(x), V::Float(y)) => x.partial_cmp(y),
        _ => match (a.as_decimal(), b.as_decimal()) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            _ => None,
        },
    }
}

/// SQL `LIKE`, supporting `%` and `_`.
fn like(text: &str, pattern: &str) -> bool {
    fn go(t: &[char], p: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('%') => (0..=t.len()).any(|i| go(&t[i..], &p[1..])),
            Some('_') => !t.is_empty() && go(&t[1..], &p[1..]),
            Some(c) => t.first() == Some(c) && go(&t[1..], &p[1..]),
        }
    }
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    go(&t, &p)
}

fn sort_rows(rows: &mut [&Row], order_by: &[OrderTerm]) {
    if order_by.is_empty() {
        return;
    }
    rows.sort_by(|a, b| {
        for term in order_by {
            let left = a.get(&term.column).unwrap_or(&Value::Null);
            let right = b.get(&term.column).unwrap_or(&Value::Null);
            let ord = match (left.is_null(), right.is_null()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => order_of(left, right).unwrap_or(std::cmp::Ordering::Equal),
            };
            let ord = if term.descending { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_core::Caller;
    use std::time::Duration;

    fn backend() -> MemoryBackend {
        let fixture: Fixture = serde_json::from_str(
            r#"{
              "tables": [{
                "name": "public.orders",
                "primary_key": ["order_no"],
                "columns": [
                  { "name": "order_no", "type": "text", "nullable": false },
                  { "name": "region", "type": "text", "nullable": false },
                  { "name": "status", "type": "text", "nullable": false },
                  { "name": "total", "type": "decimal", "nullable": false },
                  { "name": "shipped_at", "type": "timestamp" }
                ],
                "rows": [
                  { "order_no": "8812", "region": "EU", "status": "open",    "total": "1200.00" },
                  { "order_no": "8813", "region": "US", "status": "open",    "total": "50.00" },
                  { "order_no": "8814", "region": "EU", "status": "shipped", "total": "80.00",
                    "shipped_at": "2026-09-01T10:00:00Z" }
                ]
              }]
            }"#,
        )
        .unwrap();
        MemoryBackend::from_fixture(fixture, "test").unwrap()
    }

    fn caller(region: &str) -> Caller {
        Caller::new("alice", "support").with("region", Value::Text(region.into()))
    }

    #[tokio::test]
    async fn row_filter_hides_other_regions() {
        let b = backend();
        let row_filter = Expr::parse("region = $caller.region").unwrap();
        let args = BTreeMap::new();
        let c = caller("EU");
        let cols = vec!["order_no".to_string()];
        let rows = b
            .read(
                &ReadPlan {
                    table: "orders",
                    columns: &cols,
                    filter: None,
                    row_filter: Some(&row_filter),
                    order_by: &[],
                    limit: 100,
                },
                &ExecCtx {
                    args: &args,
                    caller: &c,
                    timeout: Duration::from_secs(5),
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!format!("{:?}", rows.rows).contains("8813"));
    }

    #[tokio::test]
    async fn limit_is_enforced() {
        let b = backend();
        let args = BTreeMap::new();
        let c = caller("EU");
        let cols = vec!["order_no".to_string()];
        let rows = b
            .read(
                &ReadPlan {
                    table: "orders",
                    columns: &cols,
                    filter: None,
                    row_filter: None,
                    order_by: &[],
                    limit: 1,
                },
                &ExecCtx {
                    args: &args,
                    caller: &c,
                    timeout: Duration::from_secs(5),
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn null_comparisons_do_not_match() {
        let b = backend();
        let filter = Expr::parse("shipped_at >= :since").unwrap();
        let mut args = BTreeMap::new();
        args.insert(
            "since".to_string(),
            Value::Timestamp("2026-01-01T00:00:00Z".parse().unwrap()),
        );
        let c = caller("EU");
        let cols = vec!["order_no".to_string()];
        let rows = b
            .read(
                &ReadPlan {
                    table: "orders",
                    columns: &cols,
                    filter: Some(&filter),
                    row_filter: None,
                    order_by: &[],
                    limit: 100,
                },
                &ExecCtx {
                    args: &args,
                    caller: &c,
                    timeout: Duration::from_secs(5),
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "only the shipped order has a shipped_at");
    }

    #[test]
    fn like_handles_wildcards() {
        assert!(like("alice@example.com", "%@example.com"));
        assert!(like("8812", "88_2"));
        assert!(!like("8812", "88_"));
    }
}
