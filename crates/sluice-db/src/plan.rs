//! What a backend is asked to do.
//!
//! The boundary is a plan, not SQL text. A SQL backend compiles the plan with
//! `sluice-sql`; a backend that is not SQL at all can evaluate it directly.
//! Keeping the boundary above SQL also means the engine cannot accidentally
//! hand a backend a string it assembled itself.

use std::collections::BTreeMap;
use std::time::Duration;

use indexmap::IndexMap;
use sluice_core::spec::OrderTerm;
use sluice_core::{Caller, Value, WriteMode};
use sluice_sql::{Expr, Term};

/// A read.
#[derive(Debug, Clone)]
pub struct ReadPlan<'a> {
    /// Table or view to read.
    pub table: &'a str,
    /// Columns to project, in order.
    pub columns: &'a [String],
    /// Operator-written filter.
    pub filter: Option<&'a Expr>,
    /// Caller-scoped filter. Always applied.
    pub row_filter: Option<&'a Expr>,
    /// Sort terms.
    pub order_by: &'a [OrderTerm],
    /// Hard row ceiling.
    pub limit: u32,
}

/// A write.
#[derive(Debug, Clone)]
pub struct WritePlan<'a> {
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
    /// Caller-scoped filter.
    pub row_filter: Option<&'a Expr>,
}

/// Per-request context: the values terms resolve against, and the deadline.
#[derive(Debug, Clone, Copy)]
pub struct ExecCtx<'a> {
    /// Arguments supplied by the caller, already type-checked.
    pub args: &'a BTreeMap<String, Value>,
    /// Who is asking.
    pub caller: &'a Caller,
    /// Wall-clock budget for the statement.
    pub timeout: Duration,
}

/// A result set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rows {
    /// Column names, matching the order of every row.
    pub columns: Vec<String>,
    /// Row values.
    pub rows: Vec<Vec<Value>>,
}

impl Rows {
    /// Number of rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no rows came back.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render as a list of JSON objects, which is what a tool result carries.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.rows
                .iter()
                .map(|row| {
                    let mut obj = serde_json::Map::new();
                    for (name, value) in self.columns.iter().zip(row) {
                        obj.insert(name.clone(), value.to_json());
                    }
                    serde_json::Value::Object(obj)
                })
                .collect(),
        )
    }
}

/// The effect of a write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WriteOutcome {
    /// Rows inserted or changed.
    pub rows_affected: u64,
    /// Values read back, when the action asked for any.
    pub returned: Rows,
}
