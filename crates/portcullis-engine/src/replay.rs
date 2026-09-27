//! Replaying recorded calls to see whether behaviour changed.
//!
//! The question this answers is the one a change-advisory board asks: you
//! edited the configuration, so what do agents see now that they did not see
//! before? Take the read calls from the audit log, run them again, and diff.
//!
//! Three rules keep it safe to run against production:
//!
//! - **Writes are never replayed.** Not once, not behind a flag. Re-running a
//!   refund to see if it still works is not a test, it is a second refund.
//! - **Calls whose arguments were masked are skipped**, because the log holds
//!   the masked form and replaying it would compare against a different query.
//! - **The recorded scope is used**, so a call made by an EU caller is replayed
//!   as an EU caller even when the attributes came from a token.

use std::collections::BTreeMap;
use std::path::Path;

use portcullis_core::audit::{AuditRecord, Decision};
use portcullis_core::{ActionKind, Caller, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Engine;

/// One recorded call worth replaying.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// Audit sequence number it came from.
    pub seq: u64,
    /// Action called.
    pub action: String,
    /// Arguments as recorded.
    pub args: serde_json::Value,
    /// Caller identity.
    pub caller: String,
    /// Caller role.
    pub role: String,
    /// Caller attributes at the time.
    pub scope: BTreeMap<String, portcullis_core::Value>,
    /// Rows the call returned then.
    pub rows: u64,
}

/// Why a recorded call was not replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skipped {
    /// It changed data.
    Write,
    /// It did not succeed the first time.
    NotAllowed,
    /// The action no longer exists.
    Gone,
    /// Its arguments were masked in the log.
    Masked,
    /// The role no longer exists.
    NoRole,
}

impl Skipped {
    /// A short explanation.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Write => "writes are never replayed",
            Self::NotAllowed => "the original call did not succeed",
            Self::Gone => "the action no longer exists",
            Self::Masked => "its arguments are masked in the log",
            Self::NoRole => "the role no longer exists",
        }
    }
}

/// What one replayed call produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Outcome {
    /// Action called.
    pub action: String,
    /// Role it ran as.
    pub role: String,
    /// Arguments used, as recorded.
    pub args: String,
    /// Rows returned now.
    pub rows: u64,
    /// Digest of the rows, so content changes show up and content does not
    /// have to be stored.
    pub digest: String,
    /// Error code, when the replay failed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

impl Outcome {
    /// A stable key for matching a replay against a baseline.
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.action, self.role, self.args)
    }
}

/// A saved set of outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    /// When it was recorded.
    pub recorded: String,
    /// The outcomes.
    pub outcomes: Vec<Outcome>,
}

impl Baseline {
    /// Read a baseline from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            Error::Config(format!("cannot read `{}`: {e}", path.as_ref().display()))
        })?;
        serde_json::from_str(&text).map_err(|e| {
            Error::Config(format!(
                "`{}` is not a baseline: {e}",
                path.as_ref().display()
            ))
        })
    }

    /// Write a baseline to disk.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// How one call compares to its baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Same rows, same content.
    Same,
    /// Same row count, different content.
    ContentChanged,
    /// Different row count.
    RowsChanged {
        /// Rows before.
        before: u64,
        /// Rows now.
        after: u64,
    },
    /// It used to work and now fails, or the other way round.
    StatusChanged {
        /// Error before, if any.
        before: Option<String>,
        /// Error now, if any.
        after: Option<String>,
    },
    /// Not present in the baseline.
    New,
}

impl Verdict {
    /// True when this is a difference worth a human's attention.
    pub fn is_change(&self) -> bool {
        !matches!(self, Self::Same | Self::New)
    }
}

/// The result of a whole replay.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Outcomes produced now.
    pub outcomes: Vec<Outcome>,
    /// Verdicts, when a baseline was supplied.
    pub verdicts: Vec<(String, Verdict)>,
    /// Calls that were not replayed, and why.
    pub skipped: Vec<(String, Skipped)>,
}

impl Report {
    /// Differences worth reporting.
    pub fn changes(&self) -> Vec<&(String, Verdict)> {
        self.verdicts
            .iter()
            .filter(|(_, v)| v.is_change())
            .collect()
    }

    /// True when nothing changed.
    pub fn is_clean(&self) -> bool {
        self.changes().is_empty()
    }
}

/// Calls worth replaying, and the ones that were passed over.
pub type Replayable = (Vec<Recorded>, Vec<(String, Skipped)>);

/// Read the replayable calls out of an audit log.
pub fn recorded_calls(path: impl AsRef<Path>, engine: &Engine) -> Result<Replayable> {
    let text = std::fs::read_to_string(path.as_ref()).unwrap_or_default();
    let mut calls = Vec::new();
    let mut skipped = Vec::new();
    // A busy log holds the same call thousands of times. Replaying each one
    // would take hours and tell you nothing the first did not.
    let mut seen = std::collections::BTreeSet::new();

    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(record) = serde_json::from_str::<AuditRecord>(line) else {
            continue;
        };
        let label = format!("#{} {}", record.seq, record.action);

        if record.decision != Decision::Allowed {
            skipped.push((label, Skipped::NotAllowed));
            continue;
        }
        let Some(action) = engine.registry().get(&record.action) else {
            skipped.push((label, Skipped::Gone));
            continue;
        };
        if action.spec.kind == ActionKind::Write {
            skipped.push((label, Skipped::Write));
            continue;
        }
        // A masked parameter is recorded in its masked form, so the replay
        // would be asking a different question.
        let masked_arg = record.params.as_object().is_some_and(|o| {
            o.keys()
                .any(|k| action.spec.mask.keys().any(|m| m.eq_ignore_ascii_case(k)))
        });
        if masked_arg {
            skipped.push((label, Skipped::Masked));
            continue;
        }
        if engine.config().role(&record.role).is_none() {
            skipped.push((label, Skipped::NoRole));
            continue;
        }

        let fingerprint = format!(
            "{}|{}|{}",
            record.action,
            record.role,
            canonical(&record.params)
        );
        if !seen.insert(fingerprint) {
            continue;
        }

        calls.push(Recorded {
            seq: record.seq,
            action: record.action,
            args: record.params,
            caller: record.caller,
            role: record.role,
            scope: record.scope,
            rows: record.rows,
        });
    }

    Ok((calls, skipped))
}

/// Replay a set of calls, optionally comparing against a baseline.
pub async fn replay(
    engine: &Engine,
    calls: &[Recorded],
    baseline: Option<&Baseline>,
) -> Result<Report> {
    let mut report = Report::default();
    let previous: BTreeMap<String, &Outcome> = baseline
        .map(|b| b.outcomes.iter().map(|o| (o.key(), o)).collect())
        .unwrap_or_default();

    for call in calls {
        let mut caller = Caller::new(&call.caller, &call.role);
        // Prefer the recorded scope; fall back to what the role carries now.
        if call.scope.is_empty() {
            if let Ok(c) = engine.caller(&call.role, &call.caller) {
                caller = c;
            }
        } else {
            caller.attributes = call.scope.clone();
        }

        let outcome = match engine.call(&call.action, &call.args, &caller).await {
            Ok(result) => Outcome {
                action: call.action.clone(),
                role: call.role.clone(),
                args: canonical(&call.args),
                rows: result.rows.len() as u64,
                digest: digest_rows(&result.rows.to_json()),
                error: None,
            },
            Err(e) => Outcome {
                action: call.action.clone(),
                role: call.role.clone(),
                args: canonical(&call.args),
                rows: 0,
                digest: String::new(),
                error: Some(e.code().to_owned()),
            },
        };

        let verdict = match previous.get(&outcome.key()) {
            None => Verdict::New,
            Some(before) => compare(before, &outcome),
        };
        report.verdicts.push((outcome.key(), verdict));
        report.outcomes.push(outcome);
    }

    Ok(report)
}

fn compare(before: &Outcome, after: &Outcome) -> Verdict {
    if before.error != after.error {
        return Verdict::StatusChanged {
            before: before.error.clone(),
            after: after.error.clone(),
        };
    }
    if before.rows != after.rows {
        return Verdict::RowsChanged {
            before: before.rows,
            after: after.rows,
        };
    }
    if before.digest != after.digest {
        return Verdict::ContentChanged;
    }
    Verdict::Same
}

/// A stable rendering of the arguments, so the same call keys the same way.
fn canonical(args: &serde_json::Value) -> String {
    match args.as_object() {
        None => args.to_string(),
        Some(object) => {
            let sorted: BTreeMap<_, _> = object.iter().collect();
            serde_json::to_string(&sorted).unwrap_or_else(|_| args.to_string())
        }
    }
}

fn digest_rows(rows: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(rows.to_string().as_bytes());
    h.finalize()
        .iter()
        .take(12)
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(rows: u64, digest: &str, error: Option<&str>) -> Outcome {
        Outcome {
            action: "find_order".into(),
            role: "support".into(),
            args: "{\"order_no\":\"8812\"}".into(),
            rows,
            digest: digest.into(),
            error: error.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn identical_outcomes_compare_equal() {
        assert_eq!(
            compare(&outcome(1, "aa", None), &outcome(1, "aa", None)),
            Verdict::Same
        );
    }

    #[test]
    fn a_row_count_change_is_reported_before_a_content_change() {
        assert_eq!(
            compare(&outcome(1, "aa", None), &outcome(2, "bb", None)),
            Verdict::RowsChanged {
                before: 1,
                after: 2
            }
        );
    }

    #[test]
    fn same_count_different_content_is_caught() {
        assert_eq!(
            compare(&outcome(1, "aa", None), &outcome(1, "bb", None)),
            Verdict::ContentChanged
        );
    }

    #[test]
    fn a_call_that_started_failing_is_a_status_change() {
        let v = compare(&outcome(1, "aa", None), &outcome(0, "", Some("denied")));
        assert!(matches!(v, Verdict::StatusChanged { .. }));
        assert!(v.is_change());
    }

    #[test]
    fn a_new_call_is_not_counted_as_a_regression() {
        assert!(!Verdict::New.is_change());
        assert!(!Verdict::Same.is_change());
    }

    #[test]
    fn argument_order_does_not_change_the_key() {
        let a = canonical(&serde_json::json!({ "b": 2, "a": 1 }));
        let b = canonical(&serde_json::json!({ "a": 1, "b": 2 }));
        assert_eq!(a, b);
    }
}
