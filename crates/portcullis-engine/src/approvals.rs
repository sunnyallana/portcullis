//! Parked calls waiting for a human.
//!
//! The store is an append-only event log: each state change is a line, and the
//! current state is the replay of those lines. That keeps it inspectable with
//! `tail`, survives a crash mid-write (a torn last line is dropped on load),
//! and never needs a migration.
//!
//! Arguments are kept exactly as they arrived, as JSON. They are re-validated
//! against the action when the call is released, so an approval cannot smuggle
//! a value that the action would have rejected.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use portcullis_core::{Caller, Error, Result};
use serde::{Deserialize, Serialize};

/// Where a parked call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting for a decision.
    Pending,
    /// Approved and executed.
    Executed,
    /// Refused by a human.
    Denied,
    /// Timed out before anyone decided.
    Expired,
}

/// One parked call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    /// Short identifier an approver quotes.
    pub id: String,
    /// Action that was called.
    pub action: String,
    /// Arguments exactly as received.
    pub args: serde_json::Value,
    /// Who called.
    pub caller: Caller,
    /// Why approval was needed.
    pub reason: String,
    /// When it was raised, RFC 3339.
    pub created: String,
    /// Current state.
    pub status: Status,
    /// Who decided, once decided.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub decided_by: Option<String>,
    /// When it was decided.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub decided: Option<String>,
}

impl Approval {
    /// Has this request outlived the configured window?
    pub fn is_expired(&self, ttl: Duration) -> bool {
        let Ok(created) = self.created.parse::<jiff::Timestamp>() else {
            return false;
        };
        let age = jiff::Timestamp::now().as_second() - created.as_second();
        age > i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX)
    }
}

/// A durable set of parked calls.
#[derive(Debug)]
pub struct ApprovalStore {
    path: PathBuf,
    ttl: Duration,
    state: Mutex<BTreeMap<String, Approval>>,
}

impl ApprovalStore {
    /// Open the store, replaying whatever is already on disk.
    pub fn open(path: impl AsRef<Path>, ttl: Duration) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut state = BTreeMap::new();
        if path.exists() {
            let file = std::fs::File::open(&path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                // A torn final line from a crash is skipped rather than fatal.
                if let Ok(a) = serde_json::from_str::<Approval>(&line) {
                    state.insert(a.id.clone(), a);
                }
            }
        }
        for a in state.values_mut() {
            if a.status == Status::Pending && a.is_expired(ttl) {
                a.status = Status::Expired;
            }
        }
        Ok(Self {
            path,
            ttl,
            state: Mutex::new(state),
        })
    }

    /// Park a call and return its identifier.
    pub fn create(
        &self,
        action: &str,
        args: serde_json::Value,
        caller: &Caller,
        reason: &str,
    ) -> Result<Approval> {
        let approval = Approval {
            id: short_id(),
            action: action.to_owned(),
            args,
            caller: caller.clone(),
            reason: reason.to_owned(),
            created: jiff::Timestamp::now().to_string(),
            status: Status::Pending,
            decided_by: None,
            decided: None,
        };
        self.persist(&approval)?;
        self.state
            .lock()
            .expect("approval store mutex was poisoned")
            .insert(approval.id.clone(), approval.clone());
        Ok(approval)
    }

    /// Fetch one request.
    pub fn get(&self, id: &str) -> Option<Approval> {
        self.state
            .lock()
            .expect("approval store mutex was poisoned")
            .get(id)
            .cloned()
    }

    /// Every request still waiting.
    pub fn pending(&self) -> Vec<Approval> {
        self.state
            .lock()
            .expect("approval store mutex was poisoned")
            .values()
            .filter(|a| a.status == Status::Pending && !a.is_expired(self.ttl))
            .cloned()
            .collect()
    }

    /// Every request, newest last.
    pub fn all(&self) -> Vec<Approval> {
        self.state
            .lock()
            .expect("approval store mutex was poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Claim a pending request so it can be executed exactly once.
    ///
    /// Returns an error if it is missing, already decided or expired, which is
    /// what stops a double release from writing twice.
    pub fn claim(&self, id: &str, decided_by: &str, status: Status) -> Result<Approval> {
        let mut state = self
            .state
            .lock()
            .expect("approval store mutex was poisoned");
        let approval = state
            .get_mut(id)
            .ok_or_else(|| Error::Approval(format!("no approval request `{id}`")))?;
        match approval.status {
            Status::Pending if approval.is_expired(self.ttl) => {
                approval.status = Status::Expired;
                let snapshot = approval.clone();
                drop(state);
                self.persist(&snapshot)?;
                Err(Error::Approval(format!(
                    "approval request `{id}` has expired"
                )))
            }
            Status::Pending => {
                approval.status = status;
                approval.decided_by = Some(decided_by.to_owned());
                approval.decided = Some(jiff::Timestamp::now().to_string());
                let snapshot = approval.clone();
                drop(state);
                self.persist(&snapshot)?;
                Ok(snapshot)
            }
            other => Err(Error::Approval(format!(
                "approval request `{id}` is already {}",
                match other {
                    Status::Executed => "executed",
                    Status::Denied => "denied",
                    Status::Expired => "expired",
                    Status::Pending => unreachable!(),
                }
            ))),
        }
    }

    fn persist(&self, approval: &Approval) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(approval)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
}

/// A short, unambiguous identifier a human can read aloud.
fn short_id() -> String {
    let u = uuid::Uuid::new_v4();
    let s = u.simple().to_string();
    format!("apr_{}", &s[..10])
}

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_core::Value;

    fn temp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "portcullis-approvals-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        p
    }

    fn caller() -> Caller {
        Caller::new("alice", "support").with("region", Value::Text("EU".into()))
    }

    #[test]
    fn a_request_survives_a_restart() {
        let path = temp();
        let id = {
            let store = ApprovalStore::open(&path, Duration::from_secs(60)).unwrap();
            store
                .create(
                    "refund_order",
                    serde_json::json!({"amount": "1200"}),
                    &caller(),
                    "over 500",
                )
                .unwrap()
                .id
        };
        let store = ApprovalStore::open(&path, Duration::from_secs(60)).unwrap();
        assert_eq!(store.pending().len(), 1);
        assert_eq!(store.get(&id).unwrap().action, "refund_order");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_request_can_only_be_claimed_once() {
        let path = temp();
        let store = ApprovalStore::open(&path, Duration::from_secs(60)).unwrap();
        let id = store
            .create("refund_order", serde_json::json!({}), &caller(), "always")
            .unwrap()
            .id;
        assert!(store.claim(&id, "manager", Status::Executed).is_ok());
        let second = store.claim(&id, "manager", Status::Executed).unwrap_err();
        assert!(format!("{second}").contains("already executed"), "{second}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_expired_request_cannot_be_released() {
        let path = temp();
        let store = ApprovalStore::open(&path, Duration::from_secs(0)).unwrap();
        let id = store
            .create("refund_order", serde_json::json!({}), &caller(), "always")
            .unwrap()
            .id;
        // A zero TTL means anything at least a second old is stale; force it.
        {
            let mut state = store.state.lock().unwrap();
            let a = state.get_mut(&id).unwrap();
            a.created = "2020-01-01T00:00:00Z".to_string();
        }
        let err = store.claim(&id, "manager", Status::Executed).unwrap_err();
        assert!(format!("{err}").contains("expired"), "{err}");
        let _ = std::fs::remove_file(&path);
    }
}
