//! Statement construction.
//!
//! Every statement Sluice sends is assembled here from parts that have already
//! been checked: table and column names come from the live schema, operators
//! come from a closed set, and every value becomes a bind parameter. No caller
//! input reaches the SQL text.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use sluice_core::spec::OrderTerm;
use sluice_core::{Caller, Error, Result, Value, WriteMode};

use crate::dialect::Dialect;
use crate::expr::{CmpOp, Expr, Term};

/// A statement and its bind values, ready for the driver.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    /// SQL text with dialect placeholders.
    pub sql: String,
    /// Values to bind, in placeholder order.
    pub binds: Vec<Value>,
}

/// Resolves terms into values and accumulates bind parameters.
#[derive(Debug)]
pub struct Binder<'a> {
    args: &'a BTreeMap<String, Value>,
    caller: &'a Caller,
    binds: Vec<Value>,
}

impl<'a> Binder<'a> {
    /// Start a binder for one statement.
    pub fn new(args: &'a BTreeMap<String, Value>, caller: &'a Caller) -> Self {
        Self {
            args,
            caller,
            binds: Vec::new(),
        }
    }

    /// Values bound so far, in placeholder order.
    pub fn into_binds(self) -> Vec<Value> {
        self.binds
    }

    /// Resolve a term.
    ///
    /// `Ok(None)` means an optional parameter was not supplied. A missing
    /// caller attribute is an error rather than a `None`, because a row filter
    /// that quietly disappears is exactly the failure this system exists to
    /// prevent.
    pub fn resolve(&self, term: &Term) -> Result<Option<Value>> {
        Ok(match term {
            Term::Param(p) => self.args.get(p).cloned(),
            Term::Caller(a) => {
                Some(
                    self.caller
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

    /// Bind a value and return its placeholder.
    ///
    /// Null is written into the statement as the `NULL` keyword rather than
    /// bound. A bind parameter carries a type, and there is no correct type to
    /// send for "nothing"; drivers guess, and the guess is wrong as soon as the
    /// target column is not text. `NULL` in the statement text is safe here
    /// because it encodes the absence of a value, never a caller's value.
    pub fn bind(&mut self, value: Value, dialect: &dyn Dialect) -> String {
        if value.is_null() {
            return "NULL".to_owned();
        }
        self.binds.push(value);
        dialect.placeholder(self.binds.len())
    }
}

/// Compile a predicate to SQL.
///
/// Returns `Ok(None)` when the whole predicate drops out because an optional
/// parameter was not supplied. Inside `OR` and `NOT` that would change the
/// meaning of the surrounding expression, so it is refused there instead.
pub fn compile_expr(
    expr: &Expr,
    dialect: &dyn Dialect,
    binder: &mut Binder,
) -> Result<Option<String>> {
    Ok(match expr {
        Expr::And(a, b) => {
            let left = compile_expr(a, dialect, binder)?;
            let right = compile_expr(b, dialect, binder)?;
            match (left, right) {
                (Some(l), Some(r)) => Some(format!("({l} AND {r})")),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
        Expr::Or(a, b) => {
            let left = compile_expr(a, dialect, binder)?;
            let right = compile_expr(b, dialect, binder)?;
            match (left, right) {
                (Some(l), Some(r)) => Some(format!("({l} OR {r})")),
                _ => {
                    return Err(Error::Validation {
                        action: String::new(),
                        problem: "an optional parameter is used inside `or`; move it to a top-level `and` so that omitting it cannot widen the result".into(),
                    });
                }
            }
        }
        Expr::Not(a) => match compile_expr(a, dialect, binder)? {
            Some(inner) => Some(format!("(NOT {inner})")),
            None => {
                return Err(Error::Validation {
                    action: String::new(),
                    problem: "an optional parameter is used inside `not`; omitting it would invert the filter".into(),
                });
            }
        },
        Expr::Cmp { column, op, term } => {
            let Some(value) = binder.resolve(term)? else {
                return Ok(None);
            };
            let col = dialect.quote(column)?;
            if value.is_null() && matches!(op, CmpOp::Eq | CmpOp::Ne) {
                // `= NULL` is never true in SQL; an operator who wrote it meant
                // a null test, so emit one rather than a silently empty result.
                let not = if *op == CmpOp::Ne { " NOT" } else { "" };
                Some(format!("{col} IS{not} NULL"))
            } else {
                let ph = binder.bind(value, dialect);
                Some(format!("{col} {} {ph}", op.sql()))
            }
        }
        Expr::IsNull { column, negated } => {
            let col = dialect.quote(column)?;
            let not = if *negated { " NOT" } else { "" };
            Some(format!("{col} IS{not} NULL"))
        }
        Expr::In {
            column,
            negated,
            terms,
        } => {
            let col = dialect.quote(column)?;
            let mut placeholders = Vec::new();
            for t in terms {
                let Some(v) = binder.resolve(t)? else {
                    return Ok(None);
                };
                placeholders.push(binder.bind(v, dialect));
            }
            let not = if *negated { " NOT" } else { "" };
            Some(format!("{col}{not} IN ({})", placeholders.join(", ")))
        }
    })
}

/// Everything needed to build a `SELECT`.
#[derive(Debug)]
pub struct ReadQuery<'a> {
    /// Target table.
    pub table: &'a str,
    /// Columns to project, in order.
    pub columns: &'a [String],
    /// Operator-written filter.
    pub filter: Option<&'a Expr>,
    /// Caller-scoped filter, always applied.
    pub row_filter: Option<&'a Expr>,
    /// Sort terms.
    pub order_by: &'a [OrderTerm],
    /// Row ceiling.
    pub limit: u32,
}

/// Build a `SELECT`.
pub fn select(q: &ReadQuery, dialect: &dyn Dialect, binder: &mut Binder) -> Result<Statement> {
    if q.columns.is_empty() {
        return Err(Error::Backend(
            "a read must project at least one column".into(),
        ));
    }
    let cols = q
        .columns
        .iter()
        .map(|c| dialect.quote(c))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let mut sql = format!("SELECT {cols} FROM {}", dialect.quote(q.table)?);

    // The row filter is compiled first so that it always contributes, and is
    // AND-ed with the operator filter rather than being merged into it.
    let mut wheres = Vec::new();
    if let Some(rf) = q.row_filter {
        if let Some(frag) = compile_expr(rf, dialect, binder)? {
            wheres.push(frag);
        }
    }
    if let Some(f) = q.filter {
        if let Some(frag) = compile_expr(f, dialect, binder)? {
            wheres.push(frag);
        }
    }
    if !wheres.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
    }

    if !q.order_by.is_empty() {
        let terms = q
            .order_by
            .iter()
            .map(|t| {
                let c = dialect.quote(&t.column)?;
                Ok(if t.descending { format!("{c} DESC") } else { c })
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        sql.push_str(" ORDER BY ");
        sql.push_str(&terms);
    }

    sql.push(' ');
    sql.push_str(&dialect.limit_clause(q.limit));

    let binds = std::mem::take(&mut binder.binds);
    Ok(Statement { sql, binds })
}

/// Everything needed to build an `INSERT`, `UPDATE` or upsert.
#[derive(Debug)]
pub struct WriteQuery<'a> {
    /// Target table.
    pub table: &'a str,
    /// Insert, upsert or update.
    pub mode: WriteMode,
    /// Column to source term, in declaration order.
    pub columns: &'a IndexMap<String, Term>,
    /// Key columns for upsert and update.
    pub keys: &'a [String],
    /// Columns to read back.
    pub returning: &'a [String],
    /// Caller-scoped filter. Enforced as a `WHERE` on updates and as forced
    /// column values on inserts.
    pub row_filter: Option<&'a Expr>,
}

/// Build a write statement.
#[allow(
    clippy::too_many_lines,
    reason = "one statement shape per write mode; the modes read better together"
)]
pub fn write(q: &WriteQuery, dialect: &dyn Dialect, binder: &mut Binder) -> Result<Statement> {
    let table = dialect.quote(q.table)?;

    // Values the row filter pins down. An insert has no WHERE clause to put
    // them in, so they are written into the row itself: a caller scoped to one
    // region cannot create a row belonging to another. An update does have a
    // WHERE clause, so the filter is enforced there instead and the row's
    // existing values are left alone.
    let forced: BTreeMap<&str, &Term> = if q.mode == WriteMode::Update {
        BTreeMap::new()
    } else {
        q.row_filter
            .map(conjunct_equalities)
            .transpose()?
            .unwrap_or_default()
    };

    let mut resolved: IndexMap<String, Value> = IndexMap::new();
    for (col, term) in q.columns {
        let term = forced.get(col.as_str()).copied().unwrap_or(term);
        if let Some(v) = binder.resolve(term)? {
            resolved.insert(col.clone(), v);
        }
    }
    for (col, term) in &forced {
        if !resolved.contains_key(*col) {
            if let Some(v) = binder.resolve(term)? {
                resolved.insert((*col).to_owned(), v);
            }
        }
    }
    if resolved.is_empty() {
        return Err(Error::Backend(
            "a write must set at least one column".into(),
        ));
    }

    let sql = match q.mode {
        WriteMode::Insert | WriteMode::Upsert => {
            let mut cols = Vec::new();
            let mut phs = Vec::new();
            for (col, value) in &resolved {
                cols.push(dialect.quote(col)?);
                phs.push(binder.bind(value.clone(), dialect));
            }
            let mut sql = format!(
                "INSERT INTO {table} ({}) VALUES ({})",
                cols.join(", "),
                phs.join(", ")
            );
            if q.mode == WriteMode::Upsert {
                let updates: Vec<String> = resolved
                    .keys()
                    .filter(|c| !q.keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
                    .cloned()
                    .collect();
                sql.push(' ');
                sql.push_str(&dialect.upsert_clause(q.keys, &updates)?);
            }
            sql
        }
        WriteMode::Update => {
            if q.keys.is_empty() {
                return Err(Error::Backend("an update needs key columns".into()));
            }
            let mut sets = Vec::new();
            for (col, value) in &resolved {
                if q.keys.iter().any(|k| k.eq_ignore_ascii_case(col)) {
                    continue;
                }
                let ph = binder.bind(value.clone(), dialect);
                sets.push(format!("{} = {ph}", dialect.quote(col)?));
            }
            if sets.is_empty() {
                return Err(Error::Backend(
                    "an update must change at least one non-key column".into(),
                ));
            }
            let mut wheres = Vec::new();
            for key in q.keys {
                let value = resolved.get(key).ok_or_else(|| {
                    Error::Backend(format!("key column `{key}` has no value in this write"))
                })?;
                let ph = binder.bind(value.clone(), dialect);
                wheres.push(format!("{} = {ph}", dialect.quote(key)?));
            }
            if let Some(rf) = q.row_filter {
                if let Some(frag) = compile_expr(rf, dialect, binder)? {
                    wheres.push(frag);
                }
            }
            format!(
                "UPDATE {table} SET {} WHERE {}",
                sets.join(", "),
                wheres.join(" AND ")
            )
        }
    };

    let mut sql = sql;
    if !q.returning.is_empty() && dialect.supports_returning() {
        let cols = q
            .returning
            .iter()
            .map(|c| dialect.quote(c))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        sql.push_str(" RETURNING ");
        sql.push_str(&cols);
    }

    Ok(Statement {
        sql,
        binds: std::mem::take(&mut binder.binds),
    })
}

/// Flatten a predicate into `column = term` pairs, if that is all it is.
///
/// Used to enforce a row filter on an insert. A filter that is not a plain
/// conjunction of equalities cannot be turned into column values, and write
/// actions using one are refused at validation time.
pub fn conjunct_equalities(expr: &Expr) -> Result<BTreeMap<&str, &Term>> {
    let mut out = BTreeMap::new();
    collect_equalities(expr, &mut out)?;
    Ok(out)
}

fn collect_equalities<'a>(expr: &'a Expr, out: &mut BTreeMap<&'a str, &'a Term>) -> Result<()> {
    match expr {
        Expr::And(a, b) => {
            collect_equalities(a, out)?;
            collect_equalities(b, out)
        }
        Expr::Cmp {
            column,
            op: CmpOp::Eq,
            term,
        } => {
            out.insert(column.as_str(), term);
            Ok(())
        }
        _ => Err(Error::Validation {
            action: String::new(),
            problem: "a write action's row_filter must be a conjunction of `column = value` tests, because those values are written into the row".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Postgres;

    fn caller() -> Caller {
        Caller::new("alice", "support").with("region", Value::Text("EU".into()))
    }

    fn args(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn caller_input_only_ever_becomes_a_bind() {
        let filter = Expr::parse("order_no = :order_no").unwrap();
        let a = args(&[(
            "order_no",
            Value::Text("8812'; DROP TABLE orders; --".into()),
        )]);
        let c = caller();
        let mut b = Binder::new(&a, &c);
        let cols = vec!["order_no".to_string()];
        let stmt = select(
            &ReadQuery {
                table: "public.orders",
                columns: &cols,
                filter: Some(&filter),
                row_filter: None,
                order_by: &[],
                limit: 50,
            },
            &Postgres,
            &mut b,
        )
        .unwrap();
        assert_eq!(
            stmt.sql,
            "SELECT \"order_no\" FROM \"public\".\"orders\" WHERE \"order_no\" = $1 LIMIT 50"
        );
        assert_eq!(
            stmt.binds,
            vec![Value::Text("8812'; DROP TABLE orders; --".into())]
        );
    }

    #[test]
    fn row_filter_is_always_present_and_comes_first() {
        let filter = Expr::parse("status = :status").unwrap();
        let row = Expr::parse("region = $caller.region").unwrap();
        let a = args(&[("status", Value::Text("open".into()))]);
        let c = caller();
        let mut b = Binder::new(&a, &c);
        let cols = vec!["order_no".to_string()];
        let stmt = select(
            &ReadQuery {
                table: "orders",
                columns: &cols,
                filter: Some(&filter),
                row_filter: Some(&row),
                order_by: &[],
                limit: 10,
            },
            &Postgres,
            &mut b,
        )
        .unwrap();
        assert!(
            stmt.sql
                .contains("WHERE \"region\" = $1 AND \"status\" = $2"),
            "{}",
            stmt.sql
        );
        assert_eq!(stmt.binds[0], Value::Text("EU".into()));
    }

    #[test]
    fn an_omitted_optional_parameter_drops_its_predicate() {
        let filter = Expr::parse("status = :status and total >= :min_total").unwrap();
        let a = args(&[("status", Value::Text("open".into()))]);
        let c = caller();
        let mut b = Binder::new(&a, &c);
        let cols = vec!["order_no".to_string()];
        let stmt = select(
            &ReadQuery {
                table: "orders",
                columns: &cols,
                filter: Some(&filter),
                row_filter: None,
                order_by: &[],
                limit: 10,
            },
            &Postgres,
            &mut b,
        )
        .unwrap();
        assert!(stmt.sql.contains("WHERE \"status\" = $1"), "{}", stmt.sql);
        assert!(!stmt.sql.contains("total"), "{}", stmt.sql);
    }

    #[test]
    fn an_optional_parameter_inside_or_is_refused() {
        let filter = Expr::parse("status = :status or total >= :min_total").unwrap();
        let a = args(&[("status", Value::Text("open".into()))]);
        let c = caller();
        let mut b = Binder::new(&a, &c);
        let err = compile_expr(&filter, &Postgres, &mut b).unwrap_err();
        assert!(format!("{err}").contains("or"), "{err}");
    }

    #[test]
    fn a_missing_caller_attribute_fails_closed() {
        let row = Expr::parse("region = $caller.region").unwrap();
        let a = args(&[]);
        let c = Caller::new("bob", "support");
        let mut b = Binder::new(&a, &c);
        let err = compile_expr(&row, &Postgres, &mut b).unwrap_err();
        assert!(format!("{err}").contains("region"), "{err}");
    }

    #[test]
    fn insert_writes_the_row_filter_into_the_row() {
        let row = Expr::parse("region = $caller.region").unwrap();
        let mut columns = IndexMap::new();
        columns.insert("order_no".to_string(), Term::Param("order_no".into()));
        columns.insert("region".to_string(), Term::Lit(Value::Text("US".into())));
        let a = args(&[("order_no", Value::Text("8812".into()))]);
        let c = caller();
        let mut b = Binder::new(&a, &c);
        let stmt = write(
            &WriteQuery {
                table: "refunds",
                mode: WriteMode::Insert,
                columns: &columns,
                keys: &[],
                returning: &["refund_id".to_string()],
                row_filter: Some(&row),
            },
            &Postgres,
            &mut b,
        )
        .unwrap();
        // The spec said US; the caller's scope says EU, and the scope wins.
        assert_eq!(stmt.binds[1], Value::Text("EU".into()));
        assert!(
            stmt.sql.ends_with("RETURNING \"refund_id\""),
            "{}",
            stmt.sql
        );
    }

    #[test]
    fn update_applies_the_row_filter_in_the_where_clause() {
        let row = Expr::parse("region = $caller.region").unwrap();
        let mut columns = IndexMap::new();
        columns.insert("order_no".to_string(), Term::Param("order_no".into()));
        columns.insert("status".to_string(), Term::Param("status".into()));
        let a = args(&[
            ("order_no", Value::Text("8812".into())),
            ("status", Value::Text("held".into())),
        ]);
        let c = caller();
        let mut b = Binder::new(&a, &c);
        let stmt = write(
            &WriteQuery {
                table: "orders",
                mode: WriteMode::Update,
                columns: &columns,
                keys: &["order_no".to_string()],
                returning: &[],
                row_filter: Some(&row),
            },
            &Postgres,
            &mut b,
        )
        .unwrap();
        assert_eq!(
            stmt.sql,
            "UPDATE \"orders\" SET \"status\" = $1 WHERE \"order_no\" = $2 AND \"region\" = $3"
        );
    }

    #[test]
    fn upsert_updates_only_non_key_columns() {
        let mut columns = IndexMap::new();
        columns.insert("id".to_string(), Term::Param("id".into()));
        columns.insert("note".to_string(), Term::Param("note".into()));
        let a = args(&[("id", Value::Int(1)), ("note", Value::Text("hi".into()))]);
        let c = caller();
        let mut b = Binder::new(&a, &c);
        let stmt = write(
            &WriteQuery {
                table: "notes",
                mode: WriteMode::Upsert,
                columns: &columns,
                keys: &["id".to_string()],
                returning: &[],
                row_filter: None,
            },
            &Postgres,
            &mut b,
        )
        .unwrap();
        assert!(
            stmt.sql
                .contains("ON CONFLICT (\"id\") DO UPDATE SET \"note\" = EXCLUDED.\"note\""),
            "{}",
            stmt.sql
        );
    }

    #[test]
    fn a_non_equality_row_filter_is_refused_for_writes() {
        let row = Expr::parse("region = $caller.region or region is null").unwrap();
        assert!(conjunct_equalities(&row).is_err());
    }
}
