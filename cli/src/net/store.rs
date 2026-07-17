//! Session event log — appends completed [FlowRecord]s to
//! `~/.shadowdroid/net/<serial>.jsonl`, one JSON object per line. Backs `net
//! log` (recall) and `net show` (detail by id). No daemon IPC needed for
//! recall: the log is a plain file any `net` invocation can read.

use crate::ids::Serial;
use anyhow::{Context, Result};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::events::Event;
use crate::net::flow::FlowRecord;
use crate::net::{Matcher, paths};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CheckpointRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub checkpoint: String,
    pub capture_session_id: String,
    pub created_at: f64,
    pub last_flow_id: Option<String>,
    pub last_flow_sequence: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClearRecord {
    #[serde(rename = "type")]
    pub kind: String,
    pub capture_session_id: String,
    pub cleared_at: f64,
    pub after_flow_id: Option<String>,
    pub after_flow_sequence: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub matcher: Matcher,
    pub limit: usize,
    pub capture_session_id: Option<String>,
    pub since_ts: Option<f64>,
    pub after_flow_sequence: Option<u64>,
    pub after_ts: Option<f64>,
    pub rule_id: Option<String>,
}

#[derive(Debug)]
pub struct TimelineResult {
    pub items: Vec<serde_json::Value>,
    pub older_events_excluded: bool,
    pub excluded_count: usize,
    pub logical_clear_applied: bool,
}

/// Cap each on-disk generation. One prior generation is retained, bounding a
/// capture session to roughly 128 MiB even under sustained traffic.
const SESSION_LOG_BYTES: u64 = 64 * 1024 * 1024;
static APPEND_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Append one completed flow to the session log (creating it if needed).
pub fn append(serial: &Serial, rec: &FlowRecord) -> Result<()> {
    append_line(serial, &serde_json::to_string(rec)?)
}

/// Append a non-flow [Event] (currently only `tls_error`) as its own JSON line.
/// Flow-only readers skip these; [`read_recent_timeline_query`] interleaves them with
/// compact HTTP events for `net log`.
pub fn append_event(serial: &Serial, ev: &Event) -> Result<()> {
    append_line(serial, &serde_json::to_string(ev)?)
}

pub fn append_checkpoint(serial: &Serial, checkpoint: &CheckpointRecord) -> Result<()> {
    append_line(serial, &serde_json::to_string(checkpoint)?)
}

pub fn append_clear(serial: &Serial, clear: &ClearRecord) -> Result<()> {
    append_line(serial, &serde_json::to_string(clear)?)
}

fn append_line(serial: &Serial, line: &str) -> Result<()> {
    let _guard = APPEND_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    paths::ensure_net_dir()?;
    let path = paths::session_log_path(serial)?;
    let _file_lock = acquire_log_lock(&path)?;
    secure_existing_log(&path)?;
    let line_bytes = u64::try_from(line.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if std::fs::metadata(&path)
        .map(|metadata| metadata.len().saturating_add(line_bytes) > SESSION_LOG_BYTES)
        .unwrap_or(false)
    {
        let rotated = path.with_added_extension("1");
        let _ = std::fs::remove_file(&rotated);
        std::fs::rename(&path, &rotated)
            .with_context(|| format!("rotate {} to {}", path.display(), rotated.display()))?;
    }
    // 0600 on creation: the log holds full captured headers + bodies (live auth
    // tokens, cookies), so — like the CA key — it must not be world-readable.
    // `net start` clears the log each session, so a fresh file always gets these
    // perms; `--redact` additionally strips sensitive headers before capture.
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Tighten legacy logs too; OpenOptions::mode only affects creation.
        let mut permissions = f.metadata()?.permissions();
        if permissions.mode() & 0o777 != 0o600 {
            permissions.set_mode(0o600);
            f.set_permissions(permissions)
                .with_context(|| format!("secure {}", path.display()))?;
        }
    }
    writeln!(f, "{line}").with_context(|| format!("append {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn secure_existing_log(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
    };
    let mut permissions = metadata.permissions();
    if permissions.mode() & 0o777 != 0o600 {
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("secure {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_existing_log(_path: &Path) -> Result<()> {
    Ok(())
}

fn acquire_log_lock(path: &Path) -> Result<File> {
    acquire_log_lock_mode(path, true)
}

fn acquire_log_read_lock(path: &Path) -> Result<File> {
    acquire_log_lock_mode(path, false)
}

fn acquire_log_lock_mode(path: &Path, exclusive: bool) -> Result<File> {
    let lock_path = path.with_added_extension("lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let result = if exclusive {
            lock.try_lock()
        } else {
            lock.try_lock_shared()
        };
        match result {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(std::io::Error::new(
                    ErrorKind::WouldBlock,
                    "timed out waiting for session-log lock",
                ))
                .with_context(|| format!("lock {}", lock_path.display()));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("lock {}", lock_path.display()));
            }
        }
    }
    Ok(lock)
}

enum StoredLine {
    Flow(serde_json::Value),
    TlsError(serde_json::Value),
    Checkpoint(CheckpointRecord),
    Clear(ClearRecord),
}

fn flow_is_after_floor(flow_sequence: u64, floor: u64) -> bool {
    floor == 0 || flow_sequence > floor
}

/// Visit valid records across the rotated generation and then the current one.
/// Only one line is resident at a time; malformed/partial lines and unrelated
/// event kinds are skipped so one interrupted append never poisons recall.
fn visit_records(path: &Path, mut visit: impl FnMut(StoredLine)) -> Result<()> {
    // Snapshot both generation handles while rotation is excluded, then drop
    // the shared lock before parsing potentially large logs. Open handles keep
    // referring to the same inodes even if a writer rotates paths afterward.
    let files = open_generation_snapshot(path)?;
    for (candidate, file, length) in files {
        let mut reader = BufReader::new(file.take(length));
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = reader
                .read_line(&mut line)
                .with_context(|| format!("read {}", candidate.display()))?;
            if bytes == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            match value.get("type").and_then(serde_json::Value::as_str) {
                Some("tls_error") => visit(StoredLine::TlsError(value)),
                Some("capture_checkpoint") => {
                    if let Ok(checkpoint) = serde_json::from_value(value) {
                        visit(StoredLine::Checkpoint(checkpoint));
                    }
                }
                Some("capture_clear") => {
                    if let Ok(clear) = serde_json::from_value(value) {
                        visit(StoredLine::Clear(clear));
                    }
                }
                _ => visit(StoredLine::Flow(value)),
            }
        }
    }
    Ok(())
}

fn open_generation_snapshot(path: &Path) -> Result<Vec<(std::path::PathBuf, File, u64)>> {
    if path.parent().is_some_and(|parent| !parent.exists()) {
        return Ok(Vec::new());
    }
    let _read_lock = acquire_log_read_lock(path)?;
    let mut files = Vec::new();
    for candidate in [path.with_added_extension("1"), path.to_path_buf()] {
        match File::open(&candidate) {
            Ok(file) => {
                let length = file
                    .metadata()
                    .with_context(|| format!("stat {}", candidate.display()))?
                    .len();
                files.push((candidate, file, length));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("open {}", candidate.display()));
            }
        }
    }
    Ok(files)
}

fn read_all_from(path: &Path) -> Result<Vec<FlowRecord>> {
    let mut flows = Vec::new();
    let mut floor = 0;
    visit_records(path, |record| match record {
        StoredLine::Flow(value) => {
            if let Ok(flow) = serde_json::from_value::<FlowRecord>(value)
                && flow_is_after_floor(flow.flow_sequence, floor)
            {
                flows.push(flow);
            }
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            flows.clear();
        }
        StoredLine::TlsError(_) | StoredLine::Checkpoint(_) => {}
    })?;
    Ok(flows)
}

/// All flows in the session log, oldest first. Records are decoded one line at
/// a time, avoiding a second allocation for the complete pair of generations.
pub fn read_all(serial: &Serial) -> Result<Vec<FlowRecord>> {
    read_all_from(&paths::session_log_path(serial)?)
}

/// The last `limit` flows matching `m`, oldest first. Retained for the bounded
/// diagnostic snapshot used by `why`; `net log` uses [`read_recent_timeline_query`]
/// so HTTP and TLS records share one combined limit and one disk pass.
pub fn read_filtered(serial: &Serial, m: &Matcher, limit: usize) -> Result<Vec<FlowRecord>> {
    let path = paths::session_log_path(serial)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut recent = VecDeque::new();
    let mut floor = 0;
    visit_records(&path, |record| match record {
        StoredLine::Flow(value) => {
            if let Ok(flow) = serde_json::from_value::<FlowRecord>(value)
                && flow_is_after_floor(flow.flow_sequence, floor)
                && flow.matches(m)
            {
                recent.push_back(flow);
                if recent.len() > limit {
                    recent.pop_front();
                }
            }
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            recent.clear();
        }
        StoredLine::TlsError(_) | StoredLine::Checkpoint(_) => {}
    })?;
    Ok(recent.into_iter().collect())
}

fn find_by_id_from(path: &Path, id: &str) -> Result<Option<FlowRecord>> {
    let mut found = None;
    let mut floor = 0;
    visit_records(path, |record| match record {
        StoredLine::Flow(value) => {
            if let Ok(flow) = serde_json::from_value::<FlowRecord>(value)
                && flow_is_after_floor(flow.flow_sequence, floor)
                && flow.id == id
            {
                found = Some(flow);
            }
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            found = None;
        }
        StoredLine::TlsError(_) | StoredLine::Checkpoint(_) => {}
    })?;
    Ok(found)
}

/// Most recent flow with this id (ids can repeat across daemon runs). The scan
/// keeps only the latest match instead of materializing every stored flow.
pub fn find_by_id(serial: &Serial, id: &str) -> Result<Option<FlowRecord>> {
    find_by_id_from(&paths::session_log_path(serial)?, id)
}

struct RankedEvent {
    ts: f64,
    /// Preserve the prior stable merge's tie order: HTTP events were inserted
    /// before TLS events, and append order broke ties within each kind.
    kind: u8,
    sequence: u64,
    value: serde_json::Value,
}

impl PartialEq for RankedEvent {
    fn eq(&self, other: &Self) -> bool {
        self.ts.total_cmp(&other.ts) == Ordering::Equal
            && self.kind == other.kind
            && self.sequence == other.sequence
    }
}

impl Eq for RankedEvent {}

impl PartialOrd for RankedEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ts
            .total_cmp(&other.ts)
            .then_with(|| self.kind.cmp(&other.kind))
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

fn tls_host_matches(value: &serde_json::Value, host: Option<&str>) -> bool {
    host.is_none_or(|needle| {
        value
            .get("host")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|candidate| candidate.to_lowercase().contains(&needle.to_lowercase()))
    })
}

#[cfg(test)]
fn read_recent_timeline_from(
    path: &Path,
    serial: &Serial,
    matcher: &Matcher,
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    Ok(read_recent_timeline_query_from(
        path,
        serial,
        &LogQuery {
            matcher: matcher.clone(),
            limit,
            ..Default::default()
        },
    )?
    .items)
}

fn read_recent_timeline_query_from(
    path: &Path,
    serial: &Serial,
    query: &LogQuery,
) -> Result<TimelineResult> {
    if query.limit == 0 {
        return Ok(TimelineResult {
            items: Vec::new(),
            older_events_excluded: false,
            excluded_count: 0,
            logical_clear_applied: false,
        });
    }

    let mut recent = BinaryHeap::<Reverse<RankedEvent>>::new();
    let mut sequence = 0_u64;
    let mut floor = 0_u64;
    let mut excluded_count = 0_usize;
    let mut older_events_excluded = false;
    let mut logical_clear_applied = false;
    visit_records(path, |record| {
        let entry =
            match record {
                StoredLine::Flow(value) => serde_json::from_value::<FlowRecord>(value)
                    .ok()
                    .and_then(|flow| {
                        if !flow.matches(&query.matcher) {
                            return None;
                        }
                        let before_boundary = (floor > 0 && flow.flow_sequence <= floor)
                            || query
                                .after_flow_sequence
                                .is_some_and(|after| flow.flow_sequence <= after)
                            || query.since_ts.is_some_and(|since| flow.ts < since)
                            || query.after_ts.is_some_and(|after| flow.ts <= after);
                        if before_boundary {
                            excluded_count = excluded_count.saturating_add(1);
                            older_events_excluded = true;
                            return None;
                        }
                        if query
                            .capture_session_id
                            .as_deref()
                            .is_some_and(|session| flow.capture_session_id.as_str() != session)
                            || query
                                .rule_id
                                .as_deref()
                                .is_some_and(|rule| !flow.rule_ids.iter().any(|id| id == rule))
                        {
                            return None;
                        }
                        let ts = flow.ts;
                        serde_json::to_value(flow.http_event(serial))
                            .ok()
                            .map(|value| RankedEvent {
                                ts,
                                kind: 0,
                                sequence,
                                value,
                            })
                    }),
                StoredLine::TlsError(value)
                    if tls_host_matches(&value, query.matcher.host.as_deref()) =>
                {
                    let ts = value
                        .get("ts")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    let before_boundary = (query.after_flow_sequence.is_some()
                        && query.after_ts.is_none())
                        || query.since_ts.is_some_and(|since| ts < since)
                        || query.after_ts.is_some_and(|after| ts <= after);
                    if before_boundary {
                        excluded_count = excluded_count.saturating_add(1);
                        older_events_excluded = true;
                        None
                    } else if query.rule_id.is_some()
                        || query.capture_session_id.as_deref().is_some_and(|session| {
                            value
                                .get("capture_session_id")
                                .and_then(serde_json::Value::as_str)
                                != Some(session)
                        })
                    {
                        None
                    } else {
                        Some(RankedEvent {
                            ts,
                            kind: 1,
                            sequence,
                            value,
                        })
                    }
                }
                StoredLine::Clear(clear) => {
                    floor = clear.after_flow_sequence;
                    excluded_count = excluded_count.saturating_add(recent.len());
                    older_events_excluded = true;
                    logical_clear_applied = true;
                    recent.clear();
                    None
                }
                StoredLine::TlsError(_) | StoredLine::Checkpoint(_) => None,
            };
        sequence = sequence.saturating_add(1);
        if let Some(entry) = entry {
            recent.push(Reverse(entry));
            if recent.len() > query.limit {
                recent.pop();
            }
        }
    })?;

    let mut recent = recent
        .into_iter()
        .map(|Reverse(entry)| entry)
        .collect::<Vec<_>>();
    recent.sort_unstable();
    Ok(TimelineResult {
        items: recent.into_iter().map(|entry| entry.value).collect(),
        older_events_excluded,
        excluded_count,
        logical_clear_applied,
    })
}

/// Most recent matching HTTP and TLS-error events, ordered chronologically.
/// Both generations are scanned once and only `query.limit` compact events are
/// kept; full captured bodies are dropped after conversion to public events.
pub fn read_recent_timeline_query(serial: &Serial, query: &LogQuery) -> Result<TimelineResult> {
    read_recent_timeline_query_from(&paths::session_log_path(serial)?, serial, query)
}

/// Drop the session log (called on `net start` so each session is fresh).
pub fn clear(serial: &Serial) -> Result<()> {
    paths::ensure_net_dir()?;
    let path = paths::session_log_path(serial)?;
    let _file_lock = acquire_log_lock(&path)?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    let rotated = path.with_added_extension("1");
    if rotated.exists() {
        std::fs::remove_file(&rotated).with_context(|| format!("remove {}", rotated.display()))?;
    }
    Ok(())
}

fn read_checkpoint_from(path: &Path, checkpoint_id: &str) -> Result<Option<CheckpointRecord>> {
    let mut found = None;
    let mut floor = 0_u64;
    let mut cleared_at = 0.0_f64;
    visit_records(path, |record| match record {
        StoredLine::Checkpoint(checkpoint)
            if checkpoint.checkpoint == checkpoint_id
                && checkpoint.last_flow_sequence >= floor
                && checkpoint.created_at >= cleared_at =>
        {
            found = Some(checkpoint);
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            cleared_at = clear.cleared_at;
            found = None;
        }
        StoredLine::Flow(_) | StoredLine::TlsError(_) | StoredLine::Checkpoint(_) => {}
    })?;
    Ok(found)
}

pub fn read_checkpoint(serial: &Serial, checkpoint_id: &str) -> Result<Option<CheckpointRecord>> {
    read_checkpoint_from(&paths::session_log_path(serial)?, checkpoint_id)
}

fn read_tls_errors_from(
    path: &Path,
    host: Option<&str>,
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut errors = VecDeque::new();
    visit_records(path, |record| match record {
        StoredLine::TlsError(value) if tls_host_matches(&value, host) => {
            errors.push_back(value);
            if errors.len() > limit {
                errors.pop_front();
            }
        }
        StoredLine::Clear(_) => errors.clear(),
        StoredLine::Flow(_) | StoredLine::TlsError(_) | StoredLine::Checkpoint(_) => {}
    })?;
    Ok(errors.into_iter().collect())
}

/// The last `limit` TLS-error events in log order, optionally restricted to
/// hosts containing `host` (case-insensitive substring). Returned as raw JSON
/// values since [Event] is serialize-only.
pub fn read_tls_errors(
    serial: &Serial,
    host: Option<&str>,
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    read_tls_errors_from(&paths::session_log_path(serial)?, host, limit)
}

#[cfg(test)]
mod tests {
    use super::{
        CheckpointRecord, ClearRecord, LogQuery, acquire_log_lock, acquire_log_read_lock,
        find_by_id_from, read_all_from, read_checkpoint_from, read_recent_timeline_from,
        read_recent_timeline_query_from, read_tls_errors_from, secure_existing_log,
    };
    use crate::ids::Serial;
    use crate::net::Matcher;
    use crate::net::flow::FlowRecord;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn log_lock_serializes_separate_file_handles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let first = acquire_log_read_lock(&path).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let contender_path = path.clone();
        let contender = std::thread::spawn(move || {
            let lock = acquire_log_lock(&contender_path).unwrap();
            sender.send(()).unwrap();
            drop(lock);
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "second file handle must wait for the OS lock"
        );
        drop(first);
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        contender.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn legacy_log_permissions_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        std::fs::write(&path, "secret\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        secure_existing_log(&path).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn flow(id: &str, ts: f64, host: &str, path: &str) -> FlowRecord {
        FlowRecord {
            id: id.into(),
            ts,
            method: "GET".into(),
            scheme: "https".into(),
            host: host.into(),
            path: path.into(),
            status: Some(500),
            req_body: Some("large private request body".into()),
            resp_body: Some("large private response body".into()),
            ..Default::default()
        }
    }

    fn line(value: impl serde::Serialize) -> String {
        serde_json::to_string(&value).unwrap()
    }

    fn write_lines(path: &Path, lines: &[String], trailing_newline: bool) {
        let mut text = lines.join("\n");
        if trailing_newline {
            text.push('\n');
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn generations_stream_oldest_first_without_joining_boundary_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let rotated = path.with_added_extension("1");
        // A valid final line without `\n` must not be concatenated with the
        // first current-generation line (the old whole-file reader did that).
        write_lines(
            &rotated,
            &[line(flow("old", 1.0, "old.example", "/"))],
            false,
        );
        write_lines(
            &path,
            &[
                "malformed partial json".into(),
                line(json!({"type":"tls_error","ts":2.0,"host":"api.example"})),
                line(flow("new", 3.0, "new.example", "/")),
                String::new(),
            ],
            true,
        );

        let flows = read_all_from(&path).unwrap();
        assert_eq!(
            flows
                .iter()
                .map(|flow| flow.id.as_str())
                .collect::<Vec<_>>(),
            ["old", "new"]
        );
    }

    #[test]
    fn missing_log_parent_is_an_empty_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-created/device.jsonl");
        assert!(read_all_from(&path).unwrap().is_empty());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn recent_timeline_is_combined_bounded_matched_and_compact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let mut excluded = flow("excluded", 100.0, "api.example.com", "/drop");
        excluded.method = "POST".into();
        write_lines(
            &path,
            &[
                line(flow("old", 10.0, "api.example.com", "/keep")),
                line(json!({
                    "type":"tls_error",
                    "ts":30.0,
                    "host":"API.EXAMPLE.COM",
                    "reason":"rejected"
                })),
                line(flow("new", 20.0, "api.example.com", "/keep")),
                line(excluded),
                line(json!({"type":"tls_error","ts":200.0,"host":"other.example"})),
                "not json".into(),
            ],
            true,
        );
        let serial = Serial::new("emulator-5554");
        let matcher = Matcher {
            host: Some("example.com".into()),
            path: Some("/keep".into()),
            method: Some("get".into()),
            status: Some(500),
        };

        // The TLS event ignores path/method/status (which do not exist during a
        // handshake) but shares the final two-event limit with matching flows.
        let recent = read_recent_timeline_from(&path, &serial, &matcher, 2).unwrap();
        let ts = recent
            .iter()
            .map(|value| value["ts"].as_f64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ts, [20.0, 30.0]);
        assert_eq!(recent[0]["type"], "http");
        assert_eq!(recent[0]["id"], "new");
        assert_eq!(recent[1]["type"], "tls_error");
        assert!(recent[0].get("req_body").is_none());
        assert!(recent[0].get("resp_body").is_none());
        assert!(
            read_recent_timeline_from(&path, &serial, &matcher, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn recent_timeline_orders_by_timestamp_and_stably_breaks_ties() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        write_lines(
            &path,
            &[
                line(flow("late-append-old-ts", 1.0, "api.example", "/")),
                line(json!({"type":"tls_error","ts":5.0,"host":"api.example"})),
                line(flow("same-ts", 5.0, "api.example", "/")),
                line(flow("latest", 9.0, "api.example", "/")),
            ],
            true,
        );
        let serial = Serial::new("emulator-5554");
        let timeline = read_recent_timeline_from(&path, &serial, &Matcher::default(), 3).unwrap();

        assert_eq!(
            timeline
                .iter()
                .map(|value| value["ts"].as_f64().unwrap())
                .collect::<Vec<_>>(),
            [5.0, 5.0, 9.0]
        );
        // Matches the old stable merge: equal-ts HTTP records precede TLS.
        assert_eq!(timeline[0]["type"], "http");
        assert_eq!(timeline[1]["type"], "tls_error");
    }

    #[test]
    fn find_by_id_keeps_only_the_latest_generation_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let rotated = path.with_added_extension("1");
        write_lines(
            &rotated,
            &[line(flow("duplicate", 1.0, "old.example", "/old"))],
            true,
        );
        write_lines(
            &path,
            &[
                line(json!({"type":"tls_error","ts":2.0,"host":"api.example"})),
                line(flow("duplicate", 3.0, "new.example", "/new")),
            ],
            true,
        );

        let found = find_by_id_from(&path, "duplicate").unwrap().unwrap();
        assert_eq!(found.host, "new.example");
        assert_eq!(found.path, "/new");
        assert!(find_by_id_from(&path, "missing").unwrap().is_none());

        let missing_path = dir.path().join("missing.jsonl");
        assert!(read_all_from(&missing_path).unwrap().is_empty());
    }

    #[test]
    fn tls_error_recall_is_host_filtered_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        write_lines(
            &path,
            &[
                line(json!({"type":"tls_error","ts":1.0,"host":"api.example.com"})),
                line(json!({"type":"tls_error","ts":2.0,"host":"other.test"})),
                line(json!({"type":"tls_error","ts":3.0,"host":"CDN.EXAMPLE.COM"})),
                line(flow("flow", 4.0, "api.example.com", "/")),
            ],
            true,
        );

        let recent = read_tls_errors_from(&path, Some("example.com"), 1).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0]["ts"], 3.0);
        assert!(read_tls_errors_from(&path, None, 0).unwrap().is_empty());
    }

    #[test]
    fn clear_boundary_excludes_delayed_older_flows_and_preserves_new_queries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let serial = Serial::new("emulator-5554");
        let mut first = flow("f1", 1.0, "api.example", "/old");
        first.flow_sequence = 1;
        first.capture_session_id = "n-one".into();
        first.rule_id = Some("r1".into());
        first.rule_ids = vec!["r1".into()];
        let mut second = flow("f2", 2.0, "api.example", "/old");
        second.flow_sequence = 2;
        second.capture_session_id = "n-one".into();
        let checkpoint = CheckpointRecord {
            kind: "capture_checkpoint".into(),
            checkpoint: "cp1".into(),
            capture_session_id: "n-one".into(),
            created_at: 2.5,
            last_flow_id: Some("f2".into()),
            last_flow_sequence: 2,
        };
        let clear = ClearRecord {
            kind: "capture_clear".into(),
            capture_session_id: "n-one".into(),
            cleared_at: 3.0,
            after_flow_id: Some("f2".into()),
            after_flow_sequence: 2,
        };
        let mut delayed_first = first.clone();
        delayed_first.path = "/completed-after-clear".into();
        let mut third = flow("f3", 4.0, "api.example", "/new");
        third.flow_sequence = 3;
        third.capture_session_id = "n-one".into();
        third.rule_id = Some("r7".into());
        third.rule_ids = vec!["r7".into()];
        let post_clear_checkpoint = CheckpointRecord {
            kind: "capture_checkpoint".into(),
            checkpoint: "cp2".into(),
            capture_session_id: "n-one".into(),
            created_at: 4.5,
            last_flow_id: Some("f3".into()),
            last_flow_sequence: 3,
        };
        write_lines(
            &path,
            &[
                line(first),
                line(second),
                line(checkpoint),
                line(clear),
                line(delayed_first),
                line(third),
                line(json!({
                    "type": "tls_error",
                    "ts": 4.2,
                    "capture_session_id": "n-one",
                    "host": "api.example",
                    "reason": "rejected"
                })),
                line(post_clear_checkpoint),
            ],
            true,
        );

        let result = read_recent_timeline_query_from(
            &path,
            &serial,
            &LogQuery {
                matcher: Matcher::default(),
                limit: 50,
                capture_session_id: Some("n-one".into()),
                after_flow_sequence: Some(2),
                rule_id: Some("r7".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0]["id"], "f3");
        assert!(result.older_events_excluded);
        assert!(result.logical_clear_applied);

        let with_tls = read_recent_timeline_query_from(
            &path,
            &serial,
            &LogQuery {
                matcher: Matcher::default(),
                limit: 50,
                capture_session_id: Some("n-one".into()),
                after_flow_sequence: Some(2),
                after_ts: Some(2.5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            with_tls
                .items
                .iter()
                .map(|item| item["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["http", "tls_error"]
        );
        assert!(find_by_id_from(&path, "f1").unwrap().is_none());
        assert!(read_checkpoint_from(&path, "cp1").unwrap().is_none());
        assert_eq!(
            read_checkpoint_from(&path, "cp2")
                .unwrap()
                .unwrap()
                .last_flow_id
                .as_deref(),
            Some("f3")
        );
    }
}
