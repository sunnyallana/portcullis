//! The action specification: what an operator writes, and what an agent is
//! consequently allowed to do.
//!
//! An action is a named, typed operation with a fixed shape. The agent chooses
//! the action and supplies its declared parameters; it never supplies SQL, a
//! table name, a column list or a predicate. Everything structural is decided
//! here, at configuration time, and verified against the live schema before the
//! action is published.

use std::collections::BTreeMap;
use std::time::Duration;

use indexmap::IndexMap;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::mask::Mask;
use crate::value::DataType;

/// Whether an action reads or changes data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Returns rows, changes nothing.
    Read,
    /// Changes rows.
    Write,
}

impl ActionKind {
    /// The MCP `readOnlyHint` / `destructiveHint` pair for this kind.
    pub fn hints(self) -> (bool, bool) {
        match self {
            Self::Read => (true, false),
            Self::Write => (false, true),
        }
    }
}

/// One declared parameter of an action.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParamSpec {
    /// Declared type. Arguments that are not this type are rejected.
    pub ty: DataType,
    /// Whether the caller must supply it.
    pub required: bool,
    /// Shown to the model in the tool schema.
    pub description: Option<String>,
    /// Maximum length for text, in characters.
    pub max_len: Option<usize>,
    /// Closed set of acceptable values.
    pub one_of: Option<Vec<String>>,
}

impl ParamSpec {
    /// A required parameter of the given type.
    pub fn required(ty: DataType) -> Self {
        Self {
            ty,
            required: true,
            description: None,
            max_len: None,
            one_of: None,
        }
    }
}

/// How a write reaches the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Always insert a new row.
    Insert,
    /// Insert, or update the existing row when the key already exists.
    Upsert,
    /// Update the rows matched by the key; never insert.
    Update,
}

/// The write half of an action.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteSpec {
    /// Insert, upsert or update.
    pub mode: WriteMode,
    /// Target column to source term. Each term is `:param`, `$caller.field`,
    /// `now()`, `uuid()` or a literal.
    pub columns: IndexMap<String, String>,
    /// Key columns for upsert and update.
    pub keys: Vec<String>,
    /// Parameters whose combined value makes a call replayable exactly once.
    ///
    /// A retry with the same key returns the first result instead of writing
    /// again, which matters because agents retry on timeouts.
    pub idempotency: Vec<String>,
    /// Columns to read back and return to the caller.
    pub returning: Vec<String>,
}

/// When a call must wait for a human.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApprovalRule {
    /// Every call needs approval.
    pub always: bool,
    /// Calls need approval when this parameter exceeds the threshold.
    pub over: Option<Threshold>,
}

/// A numeric ceiling on one parameter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Threshold {
    /// Parameter to inspect.
    pub param: String,
    /// Value above which approval is required.
    pub amount: Decimal,
}

/// A column to sort by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderTerm {
    /// Column name.
    pub column: String,
    /// Descending when true.
    pub descending: bool,
}

/// A complete action definition.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActionSpec {
    /// Name the agent calls, taken from the config key.
    pub name: String,
    /// Read or write.
    pub kind: ActionKind,
    /// One line telling the model when to use this action.
    pub description: String,
    /// Table or view this action touches.
    pub table: String,
    /// Declared parameters, in declaration order.
    pub params: IndexMap<String, ParamSpec>,
    /// Columns returned by a read.
    pub returns: Vec<String>,
    /// Operator-written predicate over the table, referencing `:params`.
    pub filter: Option<String>,
    /// Predicate injected into every call, referencing `$caller.*`.
    ///
    /// The agent cannot see it, cannot pass it and cannot remove it.
    pub row_filter: Option<String>,
    /// Sort order for reads.
    pub order_by: Vec<OrderTerm>,
    /// Per-column masking applied to results.
    pub mask: BTreeMap<String, Mask>,
    /// Hard ceiling on rows returned.
    pub max_rows: u32,
    /// Statement timeout.
    pub timeout: Duration,
    /// Calls per minute allowed per caller. `None` means unlimited.
    pub rate_limit: Option<u32>,
    /// Present for writes.
    pub write: Option<WriteSpec>,
    /// Present when calls can be parked for approval.
    pub approval: Option<ApprovalRule>,
}

impl ActionSpec {
    /// Turn the raw configuration table into a specification.
    ///
    /// Errors here are operator-facing and name the exact key at fault.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear pass over the file's keys; splitting it would scatter the diagnostics"
    )]
    pub fn from_raw(name: &str, raw: RawAction) -> Result<Self, String> {
        let kind = match raw.kind.as_deref() {
            None => {
                if raw.write.is_some() {
                    ActionKind::Write
                } else {
                    ActionKind::Read
                }
            }
            Some("read") => ActionKind::Read,
            Some("write") => ActionKind::Write,
            Some(other) => {
                return Err(format!(
                    "`kind` must be \"read\" or \"write\", found \"{other}\""
                ));
            }
        };

        if raw.description.trim().is_empty() {
            return Err(
                "`description` is required; the model uses it to decide when to call this action"
                    .into(),
            );
        }

        let mut params = IndexMap::new();
        for (pname, raw_param) in raw.params {
            if !is_identifier(&pname) {
                return Err(format!(
                    "parameter `{pname}` is not a valid name (letters, digits and underscore, not starting with a digit)"
                ));
            }
            params.insert(pname.clone(), raw_param.into_spec(&pname)?);
        }

        let mut mask = BTreeMap::new();
        for (col, spelling) in raw.mask {
            let m = Mask::parse(&spelling).ok_or_else(|| {
                format!("mask `{spelling}` on column `{col}` is not one of: none, partial, last4, hash, redact")
            })?;
            mask.insert(col, m);
        }

        let order_by = raw
            .order_by
            .iter()
            .map(|term| {
                let (col, desc) = match term.rsplit_once(char::is_whitespace) {
                    Some((c, dir)) if dir.eq_ignore_ascii_case("desc") => (c.trim(), true),
                    Some((c, dir)) if dir.eq_ignore_ascii_case("asc") => (c.trim(), false),
                    _ => (term.trim(), false),
                };
                OrderTerm {
                    column: col.to_owned(),
                    descending: desc,
                }
            })
            .collect();

        let write = match raw.write {
            None => None,
            Some(w) => {
                if kind == ActionKind::Read {
                    return Err("`write` is set but `kind` is \"read\"".into());
                }
                if w.columns.is_empty() {
                    return Err("`write.columns` must list at least one column".into());
                }
                let mode = w.mode.unwrap_or(WriteMode::Insert);
                if matches!(mode, WriteMode::Upsert | WriteMode::Update) && w.keys.is_empty() {
                    return Err(format!(
                        "`write.keys` is required for mode \"{}\"; without it the write cannot find its row",
                        if mode == WriteMode::Upsert {
                            "upsert"
                        } else {
                            "update"
                        }
                    ));
                }
                Some(WriteSpec {
                    mode,
                    columns: w.columns,
                    keys: w.keys,
                    idempotency: w.idempotency,
                    returning: w.returning,
                })
            }
        };

        if kind == ActionKind::Write && write.is_none() {
            return Err("a write action needs a `[action.<name>.write]` block".into());
        }
        if kind == ActionKind::Read && raw.returns.is_empty() {
            return Err("`returns` must list the columns this action exposes; an empty list would expose nothing".into());
        }

        let approval = match raw.approval {
            None => None,
            Some(a) => {
                let over = match (a.over_param, a.over_amount) {
                    (Some(param), Some(amount)) => {
                        if !params.contains_key(&param) {
                            return Err(format!(
                                "`approval.over_param` names `{param}`, which is not a parameter of this action"
                            ));
                        }
                        let amount = Decimal::from_str_exact(&amount)
                            .map_err(|e| format!("`approval.over_amount` is not a decimal: {e}"))?;
                        Some(Threshold { param, amount })
                    }
                    (None, None) => None,
                    _ => {
                        return Err(
                            "`approval.over_param` and `approval.over_amount` must be set together"
                                .into(),
                        );
                    }
                };
                if !a.always && over.is_none() {
                    return Err("`approval` needs either `always = true` or an `over_param`/`over_amount` pair".into());
                }
                Some(ApprovalRule {
                    always: a.always,
                    over,
                })
            }
        };

        let timeout = match raw.timeout.as_deref() {
            None => Duration::from_secs(30),
            Some(s) => parse_duration(s)?,
        };

        Ok(Self {
            name: name.to_owned(),
            kind,
            description: raw.description,
            table: raw.table,
            params,
            returns: raw.returns,
            filter: raw.filter,
            row_filter: raw.row_filter,
            order_by,
            mask,
            max_rows: raw.max_rows.unwrap_or(100),
            timeout,
            rate_limit: raw.rate_limit,
            write,
            approval,
        })
    }
}

/// Accept `250ms`, `30s`, `2m`, or a bare number of seconds.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, 1u64)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, 1_000)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60_000)
    } else {
        (s, 1_000)
    };
    let n: u64 = num.trim().parse().map_err(|_| {
        format!("`{s}` is not a duration; use forms like \"250ms\", \"30s\" or \"2m\"")
    })?;
    if n == 0 {
        return Err("a timeout of zero would fail every call".into());
    }
    Ok(Duration::from_millis(n * mult))
}

/// A name that is safe to use unquoted in diagnostics and JSON Schema.
pub fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Raw configuration shapes. These mirror the TOML exactly; every friendly
// alias and defaulting rule lives in `from_raw` so that errors can be precise.
// ---------------------------------------------------------------------------

/// An `[action.<name>]` block as written in the file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAction {
    /// `read` or `write`; inferred from the presence of `write` when omitted.
    #[serde(default)]
    pub kind: Option<String>,
    /// Sentence describing when to use this action.
    pub description: String,
    /// Target table or view.
    pub table: String,
    /// Declared parameters.
    #[serde(default)]
    pub params: IndexMap<String, RawParam>,
    /// Columns a read returns.
    #[serde(default)]
    pub returns: Vec<String>,
    /// Operator predicate.
    #[serde(default)]
    pub filter: Option<String>,
    /// Injected caller-scoped predicate.
    #[serde(default)]
    pub row_filter: Option<String>,
    /// Sort terms such as `placed_at desc`.
    #[serde(default)]
    pub order_by: Vec<String>,
    /// Column to mask name.
    #[serde(default)]
    pub mask: BTreeMap<String, String>,
    /// Row ceiling.
    #[serde(default)]
    pub max_rows: Option<u32>,
    /// Statement timeout.
    #[serde(default)]
    pub timeout: Option<String>,
    /// Calls per minute per caller.
    #[serde(default)]
    pub rate_limit: Option<u32>,
    /// Write block.
    #[serde(default)]
    pub write: Option<RawWrite>,
    /// Approval block.
    #[serde(default)]
    pub approval: Option<RawApproval>,
}

/// A parameter written either as a bare type or as a table.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawParam {
    /// `order_no = "string"`
    Short(String),
    /// `order_no = { type = "string", required = true }`
    Full {
        /// Declared type.
        #[serde(rename = "type")]
        ty: String,
        /// Defaults to true.
        #[serde(default = "yes")]
        required: bool,
        /// Shown to the model.
        #[serde(default)]
        description: Option<String>,
        /// Maximum text length.
        #[serde(default)]
        max_len: Option<usize>,
        /// Closed value set.
        #[serde(default)]
        one_of: Option<Vec<String>>,
    },
}

fn yes() -> bool {
    true
}

impl RawParam {
    fn into_spec(self, name: &str) -> Result<ParamSpec, String> {
        let (ty, required, description, max_len, one_of) = match self {
            Self::Short(ty) => (ty, true, None, None, None),
            Self::Full {
                ty,
                required,
                description,
                max_len,
                one_of,
            } => (ty, required, description, max_len, one_of),
        };
        let parsed = DataType::parse(&ty).ok_or_else(|| {
            format!("parameter `{name}` has unknown type `{ty}`; expected one of bool, int, float, decimal, text, timestamp, uuid, json")
        })?;
        if max_len.is_some() && parsed != DataType::Text {
            return Err(format!(
                "parameter `{name}`: `max_len` only applies to text"
            ));
        }
        if one_of.as_ref().is_some_and(Vec::is_empty) {
            return Err(format!(
                "parameter `{name}`: `one_of` is empty, so no value could ever be accepted"
            ));
        }
        Ok(ParamSpec {
            ty: parsed,
            required,
            description,
            max_len,
            one_of,
        })
    }
}

/// A `[action.<name>.write]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawWrite {
    /// insert, upsert or update.
    #[serde(default)]
    pub mode: Option<WriteMode>,
    /// Column to source term.
    pub columns: IndexMap<String, String>,
    /// Key columns.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Parameters forming the idempotency key.
    #[serde(default)]
    pub idempotency: Vec<String>,
    /// Columns to return.
    #[serde(default)]
    pub returning: Vec<String>,
}

/// An `[action.<name>.approval]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawApproval {
    /// Require approval for every call.
    #[serde(default)]
    pub always: bool,
    /// Parameter to compare.
    #[serde(default)]
    pub over_param: Option<String>,
    /// Threshold, written as a string to stay exact.
    #[serde(default)]
    pub over_amount: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(toml_src: &str) -> RawAction {
        toml::from_str(toml_src).expect("test fixture should parse")
    }

    #[test]
    fn kind_is_inferred_from_the_write_block() {
        let a = ActionSpec::from_raw(
            "refund",
            raw(r#"
                description = "Refund an order"
                table = "refunds"
                params = { order_no = "string" }
                [write]
                columns = { order_no = ":order_no" }
            "#),
        )
        .unwrap();
        assert_eq!(a.kind, ActionKind::Write);
    }

    #[test]
    fn a_read_with_no_returns_is_rejected() {
        let err = ActionSpec::from_raw(
            "find",
            raw(r#"
                description = "Find an order"
                table = "orders"
            "#),
        )
        .unwrap_err();
        assert!(err.contains("`returns`"), "{err}");
    }

    #[test]
    fn upsert_without_keys_is_rejected() {
        let err = ActionSpec::from_raw(
            "u",
            raw(r#"
                description = "x"
                table = "t"
                [write]
                mode = "upsert"
                columns = { a = ":a" }
            "#),
        )
        .unwrap_err();
        assert!(err.contains("write.keys"), "{err}");
    }

    #[test]
    fn approval_threshold_must_name_a_real_parameter() {
        let err = ActionSpec::from_raw(
            "r",
            raw(r#"
                description = "x"
                table = "t"
                params = { order_no = "string" }
                [write]
                columns = { a = ":order_no" }
                [approval]
                over_param = "amount"
                over_amount = "500"
            "#),
        )
        .unwrap_err();
        assert!(err.contains("not a parameter"), "{err}");
    }

    #[test]
    fn unknown_keys_are_a_hard_error_rather_than_a_silent_no_op() {
        let err = toml::from_str::<RawAction>(
            r#"
                description = "x"
                table = "t"
                returns = ["a"]
                max_row = 10
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_row"), "{err}");
    }

    #[test]
    fn durations_accept_friendly_spellings() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("soon").is_err());
    }
}
