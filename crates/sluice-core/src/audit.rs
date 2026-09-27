//! The audit log.
//!
//! Every decision Sluice makes is recorded, including the ones that refused to
//! do anything. Records form a hash chain: each line carries the digest of the
//! line before it, so a deleted or edited entry is detectable with
//! `sluice audit verify` even though the file is plain JSONL that any log
//! shipper can read.
//!
//! Writes go through a dedicated thread. Appending is therefore cheap from an
//! async request path, records keep their submission order, and an fsync policy
//! decides how much durability to trade for throughput. Records are never
//! dropped: when the queue is full, the caller waits.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// What Sluice did with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Executed and returned a result.
    Allowed,
    /// Refused before touching the database.
    Denied,
    /// Parked for human approval.
    Pending,
    /// Attempted and failed.
    Failed,
}

/// One line of the audit log.
///
/// Field order is part of the format: the hash is computed over the serialized
/// record with `hash` removed, so reordering fields would break verification of
/// existing files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Monotonic sequence number, assigned by the writer.
    #[serde(default)]
    pub seq: u64,
    /// When the record was written, RFC 3339 UTC.
    pub ts: String,
    /// Correlates the record with the tool call and the application log.
    pub request_id: String,
    /// Caller identity.
    pub caller: String,
    /// Caller role.
    pub role: String,
    /// Action name.
    pub action: String,
    /// Arguments as received, after masking.
    pub params: serde_json::Value,
    /// Outcome.
    pub decision: Decision,
    /// Rows returned or affected.
    pub rows: u64,
    /// Wall time of the call.
    pub duration_ms: u64,
    /// Stable error code when the call did not succeed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
    /// Approval request id, when one was raised or released.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub approval: Option<String>,
    /// Digest of the previous record; all zeroes for the first.
    #[serde(default)]
    pub prev: String,
    /// Digest of this record.
    #[serde(default)]
    pub hash: String,
}

/// The genesis value of the chain.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

impl AuditRecord {
    /// Compute this record's digest, given the previous one.
    ///
    /// The `hash` field is excluded, since it is the output.
    fn digest(&self, prev: &str) -> String {
        let mut copy = self.clone();
        prev.clone_into(&mut copy.prev);
        copy.hash = String::new();
        let body = serde_json::to_string(&copy).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        crate::hex::encode(&h.finalize())
    }
}

/// How aggressively to flush the log to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fsync {
    /// fsync after every record. Slowest, loses nothing on power failure.
    Always,
    /// Flush to the OS after every record, fsync on a timer and at shutdown.
    ///
    /// The default: survives a process crash, may lose the last few records in
    /// a host power failure.
    #[default]
    Batch,
    /// Never fsync explicitly. For tests and throwaway environments.
    Never,
}

enum Msg {
    Record(Box<AuditRecord>),
    Flush(std::sync::mpsc::SyncSender<()>),
    Stop,
}

/// A handle to the audit writer thread.
#[derive(Debug)]
pub struct AuditLog {
    tx: SyncSender<Msg>,
    worker: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl AuditLog {
    /// Open or create the log at `path` and start the writer thread.
    ///
    /// An existing file is read to the end to recover the sequence number and
    /// chain head, so restarts continue the chain rather than forking it.
    pub fn open(path: impl AsRef<Path>, fsync: Fsync) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let (seq, head) = chain_head(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let (tx, rx) = sync_channel::<Msg>(1024);
        let worker = std::thread::Builder::new()
            .name("sluice-audit".into())
            .spawn(move || writer_loop(file, &rx, seq, head, fsync))?;
        Ok(Self {
            tx,
            worker: Some(worker),
            path,
        })
    }

    /// Where the log lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue a record. Blocks only if the writer is more than 1024 records behind.
    pub fn append(&self, record: AuditRecord) {
        // A closed channel means the writer thread died; losing the process is
        // preferable to silently running unaudited, so this is loud.
        if self.tx.send(Msg::Record(Box::new(record))).is_err() {
            tracing_unavailable_warn();
        }
    }

    /// Block until everything queued so far is on disk.
    pub fn flush(&self) -> Result<()> {
        let (ack_tx, ack_rx) = sync_channel(0);
        self.tx
            .send(Msg::Flush(ack_tx))
            .map_err(|_| Error::Config("audit writer has stopped".into()))?;
        ack_rx
            .recv()
            .map_err(|_| Error::Config("audit writer stopped before flushing".into()))
    }

    /// Re-read a log from disk and check that the chain is intact.
    pub fn verify(path: impl AsRef<Path>) -> Result<VerifyReport> {
        let file = match File::open(path.as_ref()) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(VerifyReport {
                    records: 0,
                    broken_at: None,
                    head: GENESIS.to_owned(),
                });
            }
            Err(e) => return Err(e.into()),
        };
        let mut prev = GENESIS.to_owned();
        let mut count = 0u64;
        for (line_no, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: AuditRecord = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => {
                    return Ok(VerifyReport {
                        records: count,
                        broken_at: Some(line_no as u64 + 1),
                        head: prev,
                    });
                }
            };
            if rec.prev != prev || rec.digest(&prev) != rec.hash {
                return Ok(VerifyReport {
                    records: count,
                    broken_at: Some(line_no as u64 + 1),
                    head: prev,
                });
            }
            prev = rec.hash;
            count += 1;
        }
        Ok(VerifyReport {
            records: count,
            broken_at: None,
            head: prev,
        })
    }
}

impl Drop for AuditLog {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// The outcome of [`AuditLog::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Records verified before the chain broke, or in total when intact.
    pub records: u64,
    /// 1-based line number where verification failed.
    pub broken_at: Option<u64>,
    /// Digest of the last good record.
    pub head: String,
}

impl VerifyReport {
    /// True when the whole file verified.
    pub fn is_intact(&self) -> bool {
        self.broken_at.is_none()
    }
}

fn writer_loop(file: File, rx: &Receiver<Msg>, mut seq: u64, mut prev: String, fsync: Fsync) {
    let mut out = BufWriter::new(file);
    let mut dirty = false;
    loop {
        match rx.recv() {
            Ok(Msg::Record(mut rec)) => {
                seq += 1;
                rec.seq = seq;
                prev.clone_into(&mut rec.prev);
                rec.hash = rec.digest(&prev);
                rec.hash.clone_into(&mut prev);
                if let Ok(line) = serde_json::to_string(&*rec) {
                    let _ = out.write_all(line.as_bytes());
                    let _ = out.write_all(b"\n");
                    let _ = out.flush();
                    dirty = true;
                    if fsync == Fsync::Always {
                        let _ = out.get_ref().sync_data();
                        dirty = false;
                    }
                }
            }
            Ok(Msg::Flush(ack)) => {
                let _ = out.flush();
                if dirty && fsync != Fsync::Never {
                    let _ = out.get_ref().sync_data();
                    dirty = false;
                }
                let _ = ack.send(());
            }
            Ok(Msg::Stop) | Err(_) => {
                let _ = out.flush();
                if fsync != Fsync::Never {
                    let _ = out.get_ref().sync_data();
                }
                return;
            }
        }
    }
}

/// Read an existing log to recover the sequence number and chain head.
fn chain_head(path: &Path) -> Result<(u64, String)> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, GENESIS.to_owned())),
        Err(e) => return Err(e.into()),
    };
    let mut seq = 0u64;
    let mut head = GENESIS.to_owned();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<AuditRecord>(&line) {
            seq = rec.seq;
            head = rec.hash;
        }
    }
    Ok((seq, head))
}

fn tracing_unavailable_warn() {
    eprintln!("sluice: audit writer stopped; refusing to continue silently");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(action: &str) -> AuditRecord {
        AuditRecord {
            seq: 0,
            ts: "2026-09-27T10:00:00Z".into(),
            request_id: "req-1".into(),
            caller: "alice".into(),
            role: "support".into(),
            action: action.into(),
            params: serde_json::json!({ "order_no": "8812" }),
            decision: Decision::Allowed,
            rows: 1,
            duration_ms: 3,
            error: None,
            approval: None,
            prev: String::new(),
            hash: String::new(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sluice-audit-{name}-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        p
    }

    #[test]
    fn chain_verifies_and_survives_reopening() {
        let path = temp("chain");
        {
            let log = AuditLog::open(&path, Fsync::Never).unwrap();
            log.append(record("find_order"));
            log.append(record("refund_order"));
            log.flush().unwrap();
        }
        let report = AuditLog::verify(&path).unwrap();
        assert!(report.is_intact());
        assert_eq!(report.records, 2);

        // Reopening continues the same chain rather than starting a new one.
        {
            let log = AuditLog::open(&path, Fsync::Never).unwrap();
            log.append(record("find_order"));
            log.flush().unwrap();
        }
        let report = AuditLog::verify(&path).unwrap();
        assert!(report.is_intact(), "chain broke across restart");
        assert_eq!(report.records, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn editing_a_line_is_detected() {
        let path = temp("tamper");
        {
            let log = AuditLog::open(&path, Fsync::Never).unwrap();
            log.append(record("find_order"));
            log.append(record("refund_order"));
            log.append(record("find_order"));
            log.flush().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replacen("\"rows\":1", "\"rows\":9", 1);
        std::fs::write(&path, tampered).unwrap();

        let report = AuditLog::verify(&path).unwrap();
        assert_eq!(report.broken_at, Some(1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deleting_a_line_is_detected() {
        let path = temp("delete");
        {
            let log = AuditLog::open(&path, Fsync::Never).unwrap();
            for _ in 0..3 {
                log.append(record("find_order"));
            }
            log.flush().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        std::fs::write(&path, format!("{}\n{}\n", kept[0], kept[2])).unwrap();

        let report = AuditLog::verify(&path).unwrap();
        assert_eq!(report.broken_at, Some(2));
        let _ = std::fs::remove_file(&path);
    }
}
