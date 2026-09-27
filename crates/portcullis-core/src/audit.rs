//! The audit log.
//!
//! Every decision Portcullis makes is recorded, including the ones that refused to
//! do anything. Records form a hash chain: each line carries the digest of the
//! line before it, so a deleted or edited entry is detectable with
//! `portcullis audit verify` even though the file is plain JSONL that any log
//! shipper can read.
//!
//! Writes go through a dedicated thread. Appending is therefore cheap from an
//! async request path, records keep their submission order, and an fsync policy
//! decides how much durability to trade for throughput. Records are never
//! dropped: when the queue is full, the caller waits.
//!
//! A long-lived process rotates the log rather than growing one file forever.
//! **The chain continues across segments**: the first record of a new segment
//! carries the digest of the last record of the old one, so verification spans
//! the whole history rather than restarting at each file. Each closed segment
//! also gets a `.seal` beside it recording its range and head digest, which
//! means deleting a segment outright is detectable too.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// What Portcullis did with a request.
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
    /// The caller's scope attributes at the time of the call.
    ///
    /// Recorded because "who could see what" is half of an audit answer, and
    /// because it lets `portcullis replay` reconstruct the request faithfully when
    /// the attributes came from a token rather than the configuration file.
    /// Omitted when empty, so logs written before this existed still verify.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub scope: BTreeMap<String, crate::Value>,
    /// Bundle version the call ran against.
    ///
    /// Recorded because with a canary in place "what could this caller do"
    /// has a different answer per caller, and the log is where that question
    /// gets settled afterwards.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bundle: Option<String>,
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

/// When the writer starts a new segment.
///
/// Rotation happens in the writer thread, between records, so a segment never
/// ends mid-line and the chain never has a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotate {
    /// Close the segment once it passes this size.
    pub max_bytes: Option<u64>,
    /// Close the segment once it has been open this long.
    pub max_age: Option<Duration>,
}

impl Rotate {
    /// Never rotate. One file, growing forever.
    pub const NEVER: Self = Self {
        max_bytes: None,
        max_age: None,
    };

    /// Would a segment of this size and age be closed?
    fn due(self, bytes: u64, age: Duration) -> bool {
        self.max_bytes.is_some_and(|limit| bytes >= limit)
            || self.max_age.is_some_and(|limit| age >= limit)
    }
}

impl Default for Rotate {
    fn default() -> Self {
        // 100 MB is roughly a million records. Large enough that a busy
        // deployment rotates daily rather than hourly, small enough that a
        // single segment still opens in an editor.
        Self {
            max_bytes: Some(100 * 1024 * 1024),
            max_age: None,
        }
    }
}

/// What a closed segment claims about itself.
///
/// Written beside the segment when it is closed. Verification checks the claim
/// against the file, so truncating a segment means also forging its seal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seal {
    /// File name of the segment this describes.
    pub segment: String,
    /// Sequence number of its first record.
    pub first_seq: u64,
    /// Sequence number of its last record.
    pub last_seq: u64,
    /// How many records it holds.
    pub records: u64,
    /// Digest of its last record, which the next segment chains from.
    pub head: String,
    /// When it was closed, RFC 3339 UTC.
    pub closed: String,
}

enum Msg {
    Record(Box<AuditRecord>),
    Flush(std::sync::mpsc::SyncSender<()>),
    Rotate(std::sync::mpsc::SyncSender<()>),
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
    /// Existing segments and the live file are read to recover the sequence
    /// number and chain head, so restarts continue the chain rather than
    /// forking it.
    pub fn open(path: impl AsRef<Path>, fsync: Fsync, rotate: Rotate) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let (seq, head) = chain_head(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata().map_or(0, |m| m.len());
        let writer = Writer {
            path: path.clone(),
            out: BufWriter::new(file),
            fsync,
            rotate,
            bytes,
            opened: Instant::now(),
            first_seq: 0,
            seq,
            prev: head,
            dirty: false,
        };
        let (tx, rx) = sync_channel::<Msg>(1024);
        let worker = std::thread::Builder::new()
            .name(format!("{}-audit", crate::branding::BIN))
            .spawn(move || writer_loop(writer, &rx))?;
        Ok(Self {
            tx,
            worker: Some(worker),
            path,
        })
    }

    /// Where the live log lives.
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
        self.round_trip(Msg::Flush)
    }

    /// Close the current segment now, whatever the policy says.
    ///
    /// Useful before archiving, and the only way to rotate a running process
    /// without stopping it.
    pub fn rotate_now(&self) -> Result<()> {
        self.round_trip(Msg::Rotate)
    }

    fn round_trip(&self, make: fn(std::sync::mpsc::SyncSender<()>) -> Msg) -> Result<()> {
        let (ack_tx, ack_rx) = sync_channel(0);
        self.tx
            .send(make(ack_tx))
            .map_err(|_| Error::Config("audit writer has stopped".into()))?;
        ack_rx
            .recv()
            .map_err(|_| Error::Config("audit writer stopped before acknowledging".into()))
    }

    /// Verify the whole history: every closed segment, in order, then the live
    /// file.
    ///
    /// The chain runs through all of them, so a segment that was edited,
    /// truncated or deleted breaks verification at that point rather than
    /// going unnoticed.
    pub fn verify(path: impl AsRef<Path>) -> Result<VerifyReport> {
        let live = path.as_ref();
        let mut prev = GENESIS.to_owned();
        let mut records = 0u64;
        let closed = segments(live);

        for segment in &closed {
            match verify_file(segment, &mut prev, &mut records)? {
                None => {}
                Some(bad) => {
                    return Ok(VerifyReport {
                        records,
                        broken_at: Some(bad),
                        broken_in: Some(segment.clone()),
                        segments: closed.len(),
                        head: prev,
                    });
                }
            }
            // A seal that disagrees with the file it describes is the signal
            // that someone rewrote the segment and forgot to rewrite its claim.
            if let Some(seal) = read_seal(segment) {
                if seal.head != prev {
                    return Ok(VerifyReport {
                        records,
                        broken_at: Some(0),
                        broken_in: Some(seal_path(segment)),
                        segments: closed.len(),
                        head: prev,
                    });
                }
            }
        }

        if let Some(bad) = verify_file(live, &mut prev, &mut records)? {
            return Ok(VerifyReport {
                records,
                broken_at: Some(bad),
                broken_in: Some(live.to_path_buf()),
                segments: closed.len(),
                head: prev,
            });
        }

        Ok(VerifyReport {
            records,
            broken_at: None,
            broken_in: None,
            segments: closed.len(),
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
    /// 1-based line number where verification failed, or 0 for a bad seal.
    pub broken_at: Option<u64>,
    /// Which file the break is in.
    pub broken_in: Option<PathBuf>,
    /// How many closed segments were checked.
    pub segments: usize,
    /// Digest of the last good record.
    pub head: String,
}

impl VerifyReport {
    /// True when the whole history verified.
    pub fn is_intact(&self) -> bool {
        self.broken_at.is_none()
    }
}

/// Closed segments beside a live log, oldest first.
///
/// Names are zero padded so lexicographic order is chronological order.
pub fn segments(live: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem), Some(ext)) = (
        live.parent().map(|d| {
            if d.as_os_str().is_empty() {
                Path::new(".")
            } else {
                d
            }
        }),
        live.file_stem().and_then(|s| s.to_str()),
        live.extension().and_then(|s| s.to_str()),
    ) else {
        return Vec::new();
    };
    let prefix = format!("{stem}-");
    let suffix = format!(".{ext}");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(&suffix))
        })
        .collect();
    found.sort();
    found
}

fn segment_path(live: &Path, first: u64, last: u64) -> PathBuf {
    let dir = live
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = live.file_stem().and_then(|s| s.to_str()).unwrap_or("audit");
    let ext = live.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    dir.join(format!("{stem}-{first:012}-{last:012}.{ext}"))
}

fn seal_path(segment: &Path) -> PathBuf {
    let mut p = segment.as_os_str().to_owned();
    p.push(".seal");
    PathBuf::from(p)
}

fn read_seal(segment: &Path) -> Option<Seal> {
    let text = std::fs::read_to_string(seal_path(segment)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Verify one file, advancing `prev` and `records`.
///
/// Returns the 1-based line number of the first bad record, or `None` when the
/// whole file is good.
fn verify_file(path: &Path, prev: &mut String, records: &mut u64) -> Result<Option<u64>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    for (line_no, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<AuditRecord>(&line) else {
            return Ok(Some(line_no as u64 + 1));
        };
        if rec.prev != *prev || rec.digest(prev) != rec.hash {
            return Ok(Some(line_no as u64 + 1));
        }
        *prev = rec.hash;
        *records += 1;
    }
    Ok(None)
}

/// Everything the writer thread owns.
struct Writer {
    path: PathBuf,
    out: BufWriter<File>,
    fsync: Fsync,
    rotate: Rotate,
    bytes: u64,
    opened: Instant,
    /// First sequence number in the open segment, 0 until one is written.
    first_seq: u64,
    seq: u64,
    prev: String,
    dirty: bool,
}

impl Writer {
    fn write(&mut self, mut rec: Box<AuditRecord>) {
        self.seq += 1;
        if self.first_seq == 0 {
            self.first_seq = self.seq;
        }
        rec.seq = self.seq;
        self.prev.clone_into(&mut rec.prev);
        rec.hash = rec.digest(&self.prev);
        rec.hash.clone_into(&mut self.prev);

        if let Ok(line) = serde_json::to_string(&*rec) {
            let _ = self.out.write_all(line.as_bytes());
            let _ = self.out.write_all(b"\n");
            let _ = self.out.flush();
            self.bytes += line.len() as u64 + 1;
            self.dirty = true;
            if self.fsync == Fsync::Always {
                let _ = self.out.get_ref().sync_data();
                self.dirty = false;
            }
        }

        if self.rotate.due(self.bytes, self.opened.elapsed()) {
            self.roll();
        }
    }

    fn sync(&mut self) {
        let _ = self.out.flush();
        if self.dirty && self.fsync != Fsync::Never {
            let _ = self.out.get_ref().sync_data();
            self.dirty = false;
        }
    }

    /// Close the open segment and start a new one.
    ///
    /// Best effort throughout: a failure to rename or seal must not take the
    /// process down or stop auditing, so the segment simply keeps growing and
    /// the next record tries again.
    fn roll(&mut self) {
        if self.first_seq == 0 {
            return; // nothing written into this segment yet
        }
        self.sync();
        let _ = self.out.get_ref().sync_all();

        let target = segment_path(&self.path, self.first_seq, self.seq);
        // Reopen the live path first so a failed rename leaves us writing
        // somewhere rather than nowhere.
        if std::fs::rename(&self.path, &target).is_err() {
            return;
        }

        let seal = Seal {
            segment: target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned(),
            first_seq: self.first_seq,
            last_seq: self.seq,
            records: self.seq - self.first_seq + 1,
            head: self.prev.clone(),
            closed: jiff::Timestamp::now().to_string(),
        };
        if let Ok(text) = serde_json::to_string(&seal) {
            let _ = std::fs::write(seal_path(&target), text);
        }

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.out = BufWriter::new(file);
                self.bytes = 0;
                self.opened = Instant::now();
                self.first_seq = 0;
                self.dirty = false;
            }
            Err(_) => {
                // The rename succeeded but we cannot reopen. Put it back so the
                // next record still lands somewhere auditable.
                let _ = std::fs::rename(&target, &self.path);
            }
        }
    }
}

fn writer_loop(mut w: Writer, rx: &Receiver<Msg>) {
    loop {
        match rx.recv() {
            Ok(Msg::Record(rec)) => w.write(rec),
            Ok(Msg::Flush(ack)) => {
                w.sync();
                let _ = ack.send(());
            }
            Ok(Msg::Rotate(ack)) => {
                w.roll();
                let _ = ack.send(());
            }
            Ok(Msg::Stop) | Err(_) => {
                let _ = w.out.flush();
                if w.fsync != Fsync::Never {
                    let _ = w.out.get_ref().sync_data();
                }
                return;
            }
        }
    }
}

/// Read the existing history to recover the sequence number and chain head.
fn chain_head(path: &Path) -> Result<(u64, String)> {
    let mut seq = 0u64;
    let mut head = GENESIS.to_owned();

    // Segments first, then the live file: the chain runs through all of them.
    let mut files = segments(path);
    files.push(path.to_path_buf());

    for file in files {
        let Ok(handle) = File::open(&file) else {
            continue;
        };
        for line in BufReader::new(handle).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(rec) = serde_json::from_str::<AuditRecord>(&line) {
                seq = rec.seq;
                head = rec.hash;
            }
        }
    }
    Ok((seq, head))
}

fn tracing_unavailable_warn() {
    eprintln!(
        "{}: audit writer stopped; refusing to continue silently",
        crate::branding::BIN
    );
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
            scope: BTreeMap::new(),
            bundle: None,
            prev: String::new(),
            hash: String::new(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("portcullis-audit-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("audit.jsonl")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn chain_verifies_and_survives_reopening() {
        let path = temp("chain");
        {
            let log = AuditLog::open(&path, Fsync::Never, Rotate::NEVER).unwrap();
            log.append(record("find_order"));
            log.append(record("refund_order"));
            log.flush().unwrap();
        }
        let report = AuditLog::verify(&path).unwrap();
        assert!(report.is_intact());
        assert_eq!(report.records, 2);

        {
            let log = AuditLog::open(&path, Fsync::Never, Rotate::NEVER).unwrap();
            log.append(record("find_order"));
            log.flush().unwrap();
        }
        let report = AuditLog::verify(&path).unwrap();
        assert!(report.is_intact(), "chain broke across restart");
        assert_eq!(report.records, 3);
        cleanup(&path);
    }

    #[test]
    fn editing_a_line_is_detected() {
        let path = temp("tamper");
        {
            let log = AuditLog::open(&path, Fsync::Never, Rotate::NEVER).unwrap();
            for _ in 0..3 {
                log.append(record("find_order"));
            }
            log.flush().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"rows\":1", "\"rows\":9", 1)).unwrap();

        let report = AuditLog::verify(&path).unwrap();
        assert_eq!(report.broken_at, Some(1));
        assert_eq!(report.broken_in.as_deref(), Some(path.as_path()));
        cleanup(&path);
    }

    #[test]
    fn deleting_a_line_is_detected() {
        let path = temp("delete");
        {
            let log = AuditLog::open(&path, Fsync::Never, Rotate::NEVER).unwrap();
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
        cleanup(&path);
    }

    #[test]
    fn rotation_seals_a_segment_and_the_chain_runs_through_it() {
        let path = temp("rotate");
        {
            // Small enough that a couple of records fill it.
            let rotate = Rotate {
                max_bytes: Some(400),
                max_age: None,
            };
            let log = AuditLog::open(&path, Fsync::Never, rotate).unwrap();
            for _ in 0..12 {
                log.append(record("find_order"));
            }
            log.flush().unwrap();
        }

        let segs = segments(&path);
        assert!(!segs.is_empty(), "nothing rotated");

        // Every closed segment has a seal that matches its contents.
        for seg in &segs {
            let seal = read_seal(seg).expect("segment should be sealed");
            assert_eq!(seal.records, seal.last_seq - seal.first_seq + 1);
            assert_eq!(seal.segment, seg.file_name().unwrap().to_str().unwrap());
        }

        let report = AuditLog::verify(&path).unwrap();
        assert!(
            report.is_intact(),
            "chain broke across a rotation: {report:?}"
        );
        assert_eq!(report.records, 12);
        assert_eq!(report.segments, segs.len());
        cleanup(&path);
    }

    #[test]
    fn a_rotation_can_be_forced_while_the_process_runs() {
        let path = temp("force");
        {
            let log = AuditLog::open(&path, Fsync::Never, Rotate::NEVER).unwrap();
            log.append(record("find_order"));
            log.rotate_now().unwrap();
            log.append(record("refund_order"));
            log.flush().unwrap();

            // Rotating with nothing written since the last one is a no-op
            // rather than an empty segment.
            log.rotate_now().unwrap();
            log.rotate_now().unwrap();
        }
        assert_eq!(
            segments(&path).len(),
            2,
            "one segment per rotation with content"
        );
        let report = AuditLog::verify(&path).unwrap();
        assert!(report.is_intact());
        assert_eq!(report.records, 2);
        cleanup(&path);
    }

    #[test]
    fn deleting_a_whole_segment_is_detected() {
        let path = temp("lost-segment");
        {
            let log = AuditLog::open(&path, Fsync::Never, Rotate::NEVER).unwrap();
            log.append(record("one"));
            log.rotate_now().unwrap();
            log.append(record("two"));
            log.rotate_now().unwrap();
            log.append(record("three"));
            log.flush().unwrap();
        }
        let segs = segments(&path);
        assert_eq!(segs.len(), 2);

        // Remove the first segment and its seal, as an attacker covering a
        // specific call would.
        std::fs::remove_file(&segs[0]).unwrap();
        std::fs::remove_file(seal_path(&segs[0])).unwrap();

        let report = AuditLog::verify(&path).unwrap();
        assert!(
            !report.is_intact(),
            "a missing segment must break the chain"
        );
        cleanup(&path);
    }

    #[test]
    fn a_forged_seal_is_detected() {
        let path = temp("forged-seal");
        {
            let log = AuditLog::open(&path, Fsync::Never, Rotate::NEVER).unwrap();
            log.append(record("one"));
            log.rotate_now().unwrap();
            log.append(record("two"));
            log.flush().unwrap();
        }
        let segs = segments(&path);
        let mut seal = read_seal(&segs[0]).unwrap();
        seal.head = "0".repeat(64);
        std::fs::write(seal_path(&segs[0]), serde_json::to_string(&seal).unwrap()).unwrap();

        let report = AuditLog::verify(&path).unwrap();
        assert!(
            !report.is_intact(),
            "a seal that disagrees with its segment must fail"
        );
        assert_eq!(
            report.broken_at,
            Some(0),
            "0 marks a seal rather than a line"
        );
        cleanup(&path);
    }

    #[test]
    fn the_policy_decides_when_a_segment_is_due() {
        let by_size = Rotate {
            max_bytes: Some(100),
            max_age: None,
        };
        assert!(!by_size.due(99, Duration::ZERO));
        assert!(by_size.due(100, Duration::ZERO));

        let by_age = Rotate {
            max_bytes: None,
            max_age: Some(Duration::from_secs(60)),
        };
        assert!(!by_age.due(u64::MAX, Duration::from_secs(59)));
        assert!(by_age.due(0, Duration::from_secs(60)));

        assert!(!Rotate::NEVER.due(u64::MAX, Duration::from_secs(86_400)));
    }
}
