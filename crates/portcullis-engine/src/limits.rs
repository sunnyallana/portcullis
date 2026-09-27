//! Rate limiting and idempotency.
//!
//! Both exist because agents retry. A model that times out will call the same
//! tool again, and a loop that goes wrong will call it a hundred times. Neither
//! should turn into a hundred refunds or a hundred table scans.

use std::collections::{BTreeMap, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use portcullis_core::{Result, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A per-caller, per-action sliding window.
#[derive(Debug, Default)]
pub struct RateLimiter {
    windows: Mutex<BTreeMap<(String, String), VecDeque<Instant>>>,
}

impl RateLimiter {
    /// A limiter with nothing recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a call and report whether it is within the limit.
    ///
    /// `None` means the action has no limit.
    pub fn check(&self, caller: &str, action: &str, per_minute: Option<u32>) -> bool {
        let Some(limit) = per_minute else { return true };
        let now = Instant::now();
        let mut windows = self
            .windows
            .lock()
            .expect("rate limiter mutex was poisoned");
        let window = windows
            .entry((caller.to_owned(), action.to_owned()))
            .or_default();
        while window
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
        {
            window.pop_front();
        }
        if window.len() >= limit as usize {
            return false;
        }
        window.push_back(now);
        true
    }
}

/// A completed write, remembered so a retry does not repeat it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    key: String,
    action: String,
    response: serde_json::Value,
    at: i64,
}

/// Durable replay protection for writes.
///
/// The key is a digest of the action name, the caller and the parameters the
/// action nominates, so two different callers issuing the same refund are two
/// refunds, while one caller retrying is one.
#[derive(Debug)]
pub struct IdempotencyStore {
    path: PathBuf,
    ttl: Duration,
    seen: Mutex<BTreeMap<String, serde_json::Value>>,
}

impl IdempotencyStore {
    /// Open the store, dropping anything older than the TTL.
    pub fn open(path: impl AsRef<Path>, ttl: Duration) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let cutoff =
            jiff::Timestamp::now().as_second() - i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let mut seen = BTreeMap::new();
        if path.exists() {
            let file = std::fs::File::open(&path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(r) = serde_json::from_str::<Record>(&line) {
                    if r.at > cutoff {
                        seen.insert(r.key, r.response);
                    }
                }
            }
        }
        Ok(Self {
            path,
            ttl,
            seen: Mutex::new(seen),
        })
    }

    /// Build the key for one call.
    pub fn key(action: &str, caller: &str, params: &[(&str, &Value)]) -> String {
        let mut h = Sha256::new();
        h.update(action.as_bytes());
        h.update(b"\x1f");
        h.update(caller.as_bytes());
        for (name, value) in params {
            h.update(b"\x1f");
            h.update(name.as_bytes());
            h.update(b"\x1e");
            h.update(value.to_string().as_bytes());
        }
        let digest = h.finalize();
        digest.iter().take(16).fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// The response of a previous identical call, if there was one.
    pub fn get(&self, key: &str) -> Option<serde_json::Value> {
        self.seen
            .lock()
            .expect("idempotency store mutex was poisoned")
            .get(key)
            .cloned()
    }

    /// Remember a completed call.
    pub fn put(&self, key: &str, action: &str, response: &serde_json::Value) -> Result<()> {
        let record = Record {
            key: key.to_owned(),
            action: action.to_owned(),
            response: response.clone(),
            at: jiff::Timestamp::now().as_second(),
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(serde_json::to_string(&record)?.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        self.seen
            .lock()
            .expect("idempotency store mutex was poisoned")
            .insert(key.to_owned(), response.clone());
        Ok(())
    }

    /// How long entries survive.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_per_caller_and_action() {
        let l = RateLimiter::new();
        assert!(l.check("alice", "refund", Some(2)));
        assert!(l.check("alice", "refund", Some(2)));
        assert!(
            !l.check("alice", "refund", Some(2)),
            "third call is over the limit"
        );
        assert!(
            l.check("bob", "refund", Some(2)),
            "a different caller has its own window"
        );
        assert!(
            l.check("alice", "find", Some(2)),
            "a different action has its own window"
        );
    }

    #[test]
    fn no_limit_means_no_limit() {
        let l = RateLimiter::new();
        for _ in 0..1000 {
            assert!(l.check("alice", "find", None));
        }
    }

    #[test]
    fn keys_separate_callers_and_values() {
        let amount = Value::Decimal("1200.00".parse().unwrap());
        let other = Value::Decimal("1200.01".parse().unwrap());
        let a = IdempotencyStore::key("refund", "alice", &[("amount", &amount)]);
        let b = IdempotencyStore::key("refund", "bob", &[("amount", &amount)]);
        let c = IdempotencyStore::key("refund", "alice", &[("amount", &other)]);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            a,
            IdempotencyStore::key("refund", "alice", &[("amount", &amount)])
        );
    }

    #[test]
    fn a_remembered_response_survives_a_restart() {
        let mut path = std::env::temp_dir();
        path.push(format!("portcullis-idem-{}.jsonl", uuid::Uuid::new_v4()));
        let key = "abc123";
        {
            let s = IdempotencyStore::open(&path, Duration::from_secs(60)).unwrap();
            s.put(key, "refund", &serde_json::json!({"refund_id": 7}))
                .unwrap();
        }
        let s = IdempotencyStore::open(&path, Duration::from_secs(60)).unwrap();
        assert_eq!(s.get(key), Some(serde_json::json!({"refund_id": 7})));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn entries_older_than_the_ttl_are_dropped_on_load() {
        let mut path = std::env::temp_dir();
        path.push(format!("portcullis-idem-{}.jsonl", uuid::Uuid::new_v4()));
        let long_ago = jiff::Timestamp::now().as_second() - 86_400;
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::json!({ "key": "old", "action": "refund", "response": {}, "at": long_ago }),
                serde_json::json!({ "key": "new", "action": "refund", "response": {}, "at": jiff::Timestamp::now().as_second() }),
            ),
        )
        .unwrap();

        let s = IdempotencyStore::open(&path, Duration::from_secs(3600)).unwrap();
        assert_eq!(s.get("old"), None, "a day-old entry is past a one-hour ttl");
        assert!(s.get("new").is_some(), "a fresh entry survives");
        let _ = std::fs::remove_file(&path);
    }
}
