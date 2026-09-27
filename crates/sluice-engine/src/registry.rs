//! Validation and publication.
//!
//! An action becomes callable only after every name in it has been matched
//! against the live schema, every expression has parsed, and every parameter
//! type has been checked against the column it will be compared to. A
//! deployment that would fail at 3am inside a tool call fails at startup
//! instead, naming the file, the action and the key.

use std::collections::BTreeSet;

use indexmap::IndexMap;
use serde_json::json;
use sluice_core::spec::OrderTerm;
use sluice_core::{
    ActionKind, ActionSpec, Column, DataType, Error, ParamSpec, Result, Schema, Table, Value,
    WriteMode, did_you_mean,
};
use sluice_sql::{Expr, Term, parse_term};

use crate::config::Config;

/// A validated, callable action.
#[derive(Debug, Clone)]
pub struct Action {
    /// What the operator wrote.
    pub spec: ActionSpec,
    /// Fully qualified table name resolved from the schema.
    pub table: String,
    /// Parsed operator filter.
    pub filter: Option<Expr>,
    /// Parsed caller-scoped filter.
    pub row_filter: Option<Expr>,
    /// Parsed write column terms, in declaration order.
    pub write_columns: IndexMap<String, Term>,
    /// JSON Schema published to MCP clients.
    pub input_schema: serde_json::Value,
}

impl Action {
    /// Effective row ceiling, after the deployment-wide cap.
    pub fn limit(&self, cap: u32) -> u32 {
        self.spec.max_rows.min(cap)
    }
}

/// Every published action.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    actions: IndexMap<String, Action>,
}

/// Something worth telling the operator that does not stop startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Action the warning is about.
    pub action: String,
    /// What to look at.
    pub message: String,
}

impl Registry {
    /// Validate every action against the live schema.
    pub fn build(config: &Config, schema: &Schema) -> Result<(Self, Vec<Warning>)> {
        let mut actions = IndexMap::new();
        let mut warnings = Vec::new();
        for (name, spec) in &config.actions {
            let (action, mut w) = validate(spec, schema, config)?;
            warnings.append(&mut w);
            actions.insert(name.clone(), action);
        }
        Ok((Self { actions }, warnings))
    }

    /// Look up a published action.
    pub fn get(&self, name: &str) -> Option<&Action> {
        self.actions.get(name)
    }

    /// Every published action, in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Action)> {
        self.actions.iter()
    }

    /// How many actions are published.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// True when nothing is published.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Action names, for suggestions.
    pub fn names(&self) -> Vec<&str> {
        self.actions.keys().map(String::as_str).collect()
    }
}

fn err(action: &str, problem: impl Into<String>) -> Error {
    Error::Validation {
        action: action.to_owned(),
        problem: problem.into(),
    }
}

#[allow(clippy::too_many_lines)]
fn validate(spec: &ActionSpec, schema: &Schema, config: &Config) -> Result<(Action, Vec<Warning>)> {
    let name = spec.name.as_str();
    let mut warnings = Vec::new();

    if schema.is_ambiguous(&spec.table) {
        return Err(err(
            name,
            format!(
                "`{}` exists in more than one schema; qualify it, for example `public.{}`",
                spec.table, spec.table
            ),
        ));
    }
    let table = schema.table(&spec.table).ok_or_else(|| {
        let names = schema.table_names();
        let hint = did_you_mean(&spec.table, &names)
            .map_or_else(String::new, |s| format!("; did you mean `{s}`?"));
        err(name, format!("table `{}` does not exist{hint}", spec.table))
    })?;

    let column = |col: &str, context: &str| -> Result<Column> {
        table.column(col).cloned().ok_or_else(|| {
            let names = table.column_names();
            let hint = did_you_mean(col, &names)
                .map_or_else(String::new, |s| format!("; did you mean `{s}`?"));
            err(
                name,
                format!(
                    "{context} names column `{col}`, which `{}` does not have{hint}",
                    table.name
                ),
            )
        })
    };

    // Reads project columns; writes do not.
    if spec.kind == ActionKind::Read {
        let mut seen = BTreeSet::new();
        for col in &spec.returns {
            column(col, "`returns`")?;
            if !seen.insert(col.to_ascii_lowercase()) {
                return Err(err(name, format!("`returns` lists `{col}` twice")));
            }
        }
    } else if !spec.returns.is_empty() {
        return Err(err(
            name,
            "`returns` belongs to a read; a write uses `write.returning`",
        ));
    }

    for col in spec.mask.keys() {
        column(col, "`mask`")?;
        let visible = spec.returns.iter().any(|r| r.eq_ignore_ascii_case(col))
            || spec
                .write
                .as_ref()
                .is_some_and(|w| w.returning.iter().any(|r| r.eq_ignore_ascii_case(col)));
        if !visible {
            warnings.push(Warning {
                action: name.to_owned(),
                message: format!("`mask` covers `{col}`, which this action never returns"),
            });
        }
    }

    for term in &spec.order_by {
        column(&term.column, "`order_by`")?;
    }

    // Expressions.
    let filter = parse_expr(spec.filter.as_deref(), name, "filter")?;
    let row_filter = parse_expr(spec.row_filter.as_deref(), name, "row_filter")?;

    for expr in [filter.as_ref(), row_filter.as_ref()].into_iter().flatten() {
        for col in expr.columns() {
            column(col, "the filter")?;
        }
        for param in expr.params() {
            let declared = spec.params.get(param).ok_or_else(|| {
                let names = spec.params.keys().map(String::as_str).collect::<Vec<_>>();
                let hint = did_you_mean(param, &names)
                    .map_or_else(String::new, |s| format!("; did you mean `:{s}`?"));
                err(
                    name,
                    format!("the filter uses `:{param}`, which is not declared in `params`{hint}"),
                )
            })?;
            check_param_against_columns(name, expr, param, declared, table)?;
        }
    }

    // A row filter must not depend on caller-supplied arguments, or the agent
    // could steer its own scope.
    if let Some(rf) = &row_filter {
        if let Some(p) = rf.params().iter().next() {
            return Err(err(
                name,
                format!(
                    "`row_filter` uses `:{p}`; it may only reference `$caller.*` and literals, or the caller could widen their own scope"
                ),
            ));
        }
        // Every role that can call this action must supply the attributes the
        // filter needs, otherwise the call fails closed at runtime.
        for attr in rf.caller_attributes() {
            if sluice_core::Caller::BUILT_IN.contains(&attr) {
                continue;
            }
            for role in config.roles.values() {
                if role.allows(name) && !role.attributes.contains_key(attr) {
                    return Err(err(
                        name,
                        format!(
                            "`row_filter` needs `$caller.{attr}`, but role `{}` does not define it",
                            role.name
                        ),
                    ));
                }
            }
        }
    }

    // Writes.
    let mut write_columns = IndexMap::new();
    if let Some(w) = &spec.write {
        if table.primary_key.is_empty() && w.mode != WriteMode::Insert {
            warnings.push(Warning {
                action: name.to_owned(),
                message: format!(
                    "`{}` has no primary key; matching relies on `write.keys` alone",
                    table.name
                ),
            });
        }
        for (col, term_src) in &w.columns {
            let meta = column(col, "`write.columns`")?;
            let term = parse_term(term_src).map_err(|e| {
                err(
                    name,
                    format!("`write.columns.{col}` is not a valid value: {e}"),
                )
            })?;
            match &term {
                Term::Param(p) => {
                    let declared = spec.params.get(p).ok_or_else(|| {
                        err(name, format!("`write.columns.{col}` uses `:{p}`, which is not declared in `params`"))
                    })?;
                    if !assignable(declared.ty, meta.ty) {
                        return Err(err(
                            name,
                            format!(
                                "`write.columns.{col}` assigns {} parameter `:{p}` to a {} column",
                                declared.ty, meta.ty
                            ),
                        ));
                    }
                }
                Term::Lit(v) => {
                    if let Some(ty) = v.type_of() {
                        if !assignable(ty, meta.ty) {
                            return Err(err(
                                name,
                                format!(
                                    "`write.columns.{col}` assigns a {ty} literal to a {} column",
                                    meta.ty
                                ),
                            ));
                        }
                    }
                }
                Term::Now if meta.ty != DataType::Timestamp => {
                    return Err(err(
                        name,
                        format!("`write.columns.{col}` uses now() on a {} column", meta.ty),
                    ));
                }
                Term::NewUuid if meta.ty != DataType::Uuid => {
                    return Err(err(
                        name,
                        format!("`write.columns.{col}` uses uuid() on a {} column", meta.ty),
                    ));
                }
                _ => {}
            }
            if meta.generated {
                warnings.push(Warning {
                    action: name.to_owned(),
                    message: format!("`write.columns.{col}` sets a column the database generates"),
                });
            }
            write_columns.insert(col.clone(), term);
        }

        for key in &w.keys {
            column(key, "`write.keys`")?;
            if !w.columns.contains_key(key) {
                return Err(err(
                    name,
                    format!("`write.keys` names `{key}`, which `write.columns` does not set"),
                ));
            }
        }
        for col in &w.returning {
            column(col, "`write.returning`")?;
        }
        for p in &w.idempotency {
            if !spec.params.contains_key(p) {
                return Err(err(
                    name,
                    format!("`write.idempotency` names `{p}`, which is not a parameter"),
                ));
            }
        }
        if w.idempotency.is_empty() {
            warnings.push(Warning {
                action: name.to_owned(),
                message: "no `write.idempotency` key; a retried call will write twice".into(),
            });
        }

        // An insert cannot enforce a filter in a WHERE clause, so the filter
        // has to be expressible as column values.
        if let Some(rf) = &row_filter {
            if w.mode != WriteMode::Update {
                sluice_sql::conjunct_equalities(rf).map_err(|_| {
                    err(name, "a write action's `row_filter` must be a conjunction of `column = value` tests, because those values are written into the row")
                })?;
            }
        }
    }

    // Approvals.
    if let Some(rule) = &spec.approval {
        if let Some(t) = &rule.over {
            let p = spec.params.get(&t.param).ok_or_else(|| {
                err(
                    name,
                    format!(
                        "`approval.over_param` names `{}`, which is not a parameter",
                        t.param
                    ),
                )
            })?;
            if !matches!(p.ty, DataType::Decimal | DataType::Int) {
                return Err(err(
                    name,
                    format!(
                        "`approval.over_param` points at `{}`, which is {}; a threshold needs decimal or int",
                        t.param, p.ty
                    ),
                ));
            }
        }
        if spec.kind == ActionKind::Read {
            warnings.push(Warning {
                action: name.to_owned(),
                message: "a read action requires approval; that is unusual".into(),
            });
        }
    }

    if spec.max_rows == 0 {
        return Err(err(name, "`max_rows` of zero would return nothing"));
    }
    if spec.max_rows > config.limits.max_rows {
        warnings.push(Warning {
            action: name.to_owned(),
            message: format!(
                "`max_rows` of {} is capped to the deployment limit of {}",
                spec.max_rows, config.limits.max_rows
            ),
        });
    }
    if spec.rate_limit == Some(0) {
        return Err(err(name, "`rate_limit` of zero would refuse every call"));
    }

    let input_schema = input_schema(spec);

    Ok((
        Action {
            spec: spec.clone(),
            table: table.name.clone(),
            filter,
            row_filter,
            write_columns,
            input_schema,
        },
        warnings,
    ))
}

fn parse_expr(src: Option<&str>, action: &str, field: &str) -> Result<Option<Expr>> {
    src.map(|s| {
        Expr::parse(s).map_err(|e| match e {
            Error::Parse { at, problem } => Error::Validation {
                action: action.to_owned(),
                problem: format!("`{field}` is invalid at character {at}: {problem}"),
            },
            other => other,
        })
    })
    .transpose()
}

/// Check a parameter's declared type against every column it is compared to.
fn check_param_against_columns(
    action: &str,
    expr: &Expr,
    param: &str,
    declared: &ParamSpec,
    table: &Table,
) -> Result<()> {
    let mut stack = vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::And(a, b) | Expr::Or(a, b) => {
                stack.push(a);
                stack.push(b);
            }
            Expr::Not(a) => stack.push(a),
            Expr::Cmp { column, term, .. } => {
                if matches!(term, Term::Param(p) if p == param) {
                    if let Some(col) = table.column(column) {
                        if !comparable(declared.ty, col.ty) {
                            return Err(Error::Validation {
                                action: action.to_owned(),
                                problem: format!(
                                    "`:{param}` is declared {} but is compared to `{column}`, which is {}",
                                    declared.ty, col.ty
                                ),
                            });
                        }
                    }
                }
            }
            Expr::In { column, terms, .. } => {
                if terms
                    .iter()
                    .any(|t| matches!(t, Term::Param(p) if p == param))
                {
                    if let Some(col) = table.column(column) {
                        if !comparable(declared.ty, col.ty) {
                            return Err(Error::Validation {
                                action: action.to_owned(),
                                problem: format!(
                                    "`:{param}` is declared {} but is compared to `{column}`, which is {}",
                                    declared.ty, col.ty
                                ),
                            });
                        }
                    }
                }
            }
            Expr::IsNull { .. } => {}
        }
    }
    Ok(())
}

/// May a value of type `from` be compared to a column of type `to`?
fn comparable(from: DataType, to: DataType) -> bool {
    from == to
        || matches!(
            (from, to),
            (DataType::Int, DataType::Decimal | DataType::Float)
        )
}

/// May a value of type `from` be written into a column of type `to`?
fn assignable(from: DataType, to: DataType) -> bool {
    comparable(from, to)
}

/// Build the JSON Schema an MCP client sees for this action.
fn input_schema(spec: &ActionSpec) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, p) in &spec.params {
        let mut prop = serde_json::Map::new();
        prop.insert("type".into(), json!(p.ty.json_schema_type()));
        if let Some(fmt) = p.ty.json_schema_format() {
            prop.insert("format".into(), json!(fmt));
        }
        if let Some(d) = &p.description {
            prop.insert("description".into(), json!(d));
        }
        if let Some(max) = p.max_len {
            prop.insert("maxLength".into(), json!(max));
        }
        if let Some(values) = &p.one_of {
            prop.insert("enum".into(), json!(values));
        }
        properties.insert(name.clone(), serde_json::Value::Object(prop));
        if p.required {
            required.push(name.clone());
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Type-check and convert one incoming argument.
pub fn coerce_argument(param: &str, spec: &ParamSpec, json: &serde_json::Value) -> Result<Value> {
    let value = Value::from_json(json, spec.ty).map_err(|problem| Error::BadArgument {
        param: param.to_owned(),
        problem,
    })?;
    if let Some(text) = value.as_text() {
        if let Some(max) = spec.max_len {
            if text.chars().count() > max {
                return Err(Error::BadArgument {
                    param: param.to_owned(),
                    problem: format!("is longer than the {max} characters this action accepts"),
                });
            }
        }
        if let Some(allowed) = &spec.one_of {
            if !allowed.iter().any(|a| a == text) {
                return Err(Error::BadArgument {
                    param: param.to_owned(),
                    problem: format!("must be one of: {}", allowed.join(", ")),
                });
            }
        }
    }
    Ok(value)
}

/// Sort terms are re-exported so the engine can hand them to a backend.
pub type Order = OrderTerm;
