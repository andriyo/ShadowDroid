//! Session event log — appends completed [FlowRecord]s to
//! `~/.shadowdroid/net/<serial>.jsonl`, one JSON object per line. Backs `net
//! log` (recall) and `net show` (detail by id). No daemon IPC needed for
//! recall: the log is a plain file any `net` invocation can read.

use crate::ids::Serial;
use anyhow::{Context, Result};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::events::Event;
use crate::net::flow::FlowRecord;
use crate::net::ws::{WsCloseRecord, WsMessageRecord, WsSessionRecord};
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

/// Which protocols `net log` interleaves. WebSocket per-message frames are a
/// firehose, so the default shows session lifecycle (`ws_open`/`ws_close`)
/// alongside HTTP but withholds `ws_msg` until asked (`WebSocket`/`All`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP flows + TLS errors + WebSocket lifecycle (no per-message frames).
    #[default]
    Default,
    /// HTTP flows + TLS errors only.
    Http,
    /// WebSocket only: upgrades, messages, and closes.
    WebSocket,
    /// Everything, including every WebSocket message.
    All,
}

impl Protocol {
    fn include_http(self) -> bool {
        matches!(self, Protocol::Default | Protocol::Http | Protocol::All)
    }
    fn include_ws_lifecycle(self) -> bool {
        matches!(
            self,
            Protocol::Default | Protocol::WebSocket | Protocol::All
        )
    }
    fn include_ws_msg(self) -> bool {
        matches!(self, Protocol::WebSocket | Protocol::All)
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub matcher: Matcher,
    pub limit: usize,
    pub protocol: Protocol,
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

/// Append a WebSocket upgrade (`ws_open`) line.
pub fn append_ws_session(serial: &Serial, rec: &WsSessionRecord) -> Result<()> {
    append_line(serial, &serde_json::to_string(rec)?)
}

/// Append one reassembled WebSocket message (`ws_msg`) line.
pub fn append_ws_message(serial: &Serial, rec: &WsMessageRecord) -> Result<()> {
    append_line(serial, &serde_json::to_string(rec)?)
}

/// Append a WebSocket teardown (`ws_close`) line with per-direction totals.
pub fn append_ws_close(serial: &Serial, rec: &WsCloseRecord) -> Result<()> {
    append_line(serial, &serde_json::to_string(rec)?)
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
    WsOpen(serde_json::Value),
    WsMsg(serde_json::Value),
    WsClose(serde_json::Value),
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
                Some("ws_open") => visit(StoredLine::WsOpen(value)),
                Some("ws_msg") => visit(StoredLine::WsMsg(value)),
                Some("ws_close") => visit(StoredLine::WsClose(value)),
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
        StoredLine::TlsError(_)
        | StoredLine::WsOpen(_)
        | StoredLine::WsMsg(_)
        | StoredLine::WsClose(_)
        | StoredLine::Checkpoint(_) => {}
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
        StoredLine::TlsError(_)
        | StoredLine::WsOpen(_)
        | StoredLine::WsMsg(_)
        | StoredLine::WsClose(_)
        | StoredLine::Checkpoint(_) => {}
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
        StoredLine::TlsError(_)
        | StoredLine::WsOpen(_)
        | StoredLine::WsMsg(_)
        | StoredLine::WsClose(_)
        | StoredLine::Checkpoint(_) => {}
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

/// A WS record joins the timeline only when host (and, for `ws_open`, path)
/// match and no HTTP-only filter (method/status) is set.
fn ws_value_matches(value: &serde_json::Value, matcher: &Matcher, has_path: bool) -> bool {
    if matcher.method.is_some() || matcher.status.is_some() {
        return false;
    }
    let host_ok = matcher.host.as_deref().is_none_or(|needle| {
        value
            .get("host")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|host| host.to_lowercase().contains(&needle.to_lowercase()))
    });
    let path_ok = match matcher.path.as_deref() {
        None => true,
        Some(needle) => {
            has_path
                && value
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|path| path.to_lowercase().contains(&needle.to_lowercase()))
        }
    };
    host_ok && path_ok
}

/// Shared checkpoint/clear/since boundary test for WS records (they carry the
/// same `flow_sequence`/`ts` as flows).
fn ws_before_boundary(value: &serde_json::Value, floor: u64, query: &LogQuery) -> bool {
    let ts = value
        .get("ts")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let sequence = value
        .get("flow_sequence")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    (floor > 0 && sequence <= floor)
        || query
            .after_flow_sequence
            .is_some_and(|after| sequence <= after)
        || query.since_ts.is_some_and(|since| ts < since)
        || query.after_ts.is_some_and(|after| ts <= after)
}

/// WS records never carry a rule id, and honor a `--session` restriction.
fn ws_session_excluded(value: &serde_json::Value, query: &LogQuery) -> bool {
    query.rule_id.is_some()
        || query.capture_session_id.as_deref().is_some_and(|session| {
            value
                .get("capture_session_id")
                .and_then(serde_json::Value::as_str)
                != Some(session)
        })
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
        let entry = match record {
            StoredLine::Flow(value) if query.protocol.include_http() => {
                serde_json::from_value::<FlowRecord>(value)
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
                    })
            }
            StoredLine::TlsError(value)
                if query.protocol.include_http()
                    && tls_host_matches(&value, query.matcher.host.as_deref()) =>
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
            StoredLine::WsOpen(value) if query.protocol.include_ws_lifecycle() => {
                if !ws_value_matches(&value, &query.matcher, true) {
                    None
                } else if ws_before_boundary(&value, floor, query) {
                    excluded_count = excluded_count.saturating_add(1);
                    older_events_excluded = true;
                    None
                } else if ws_session_excluded(&value, query) {
                    None
                } else {
                    serde_json::from_value::<WsSessionRecord>(value)
                        .ok()
                        .and_then(|record| {
                            let ts = record.ts;
                            serde_json::to_value(record.open_event(serial))
                                .ok()
                                .map(|value| RankedEvent {
                                    ts,
                                    kind: 2,
                                    sequence,
                                    value,
                                })
                        })
                }
            }
            StoredLine::WsMsg(value) if query.protocol.include_ws_msg() => {
                if !ws_value_matches(&value, &query.matcher, false) {
                    None
                } else if ws_before_boundary(&value, floor, query) {
                    excluded_count = excluded_count.saturating_add(1);
                    older_events_excluded = true;
                    None
                } else if ws_session_excluded(&value, query) {
                    None
                } else {
                    serde_json::from_value::<WsMessageRecord>(value)
                        .ok()
                        .and_then(|record| {
                            let ts = record.ts;
                            serde_json::to_value(record.msg_event(serial))
                                .ok()
                                .map(|value| RankedEvent {
                                    ts,
                                    kind: 3,
                                    sequence,
                                    value,
                                })
                        })
                }
            }
            StoredLine::WsClose(value) if query.protocol.include_ws_lifecycle() => {
                if !ws_value_matches(&value, &query.matcher, false) {
                    None
                } else if ws_before_boundary(&value, floor, query) {
                    excluded_count = excluded_count.saturating_add(1);
                    older_events_excluded = true;
                    None
                } else if ws_session_excluded(&value, query) {
                    None
                } else {
                    serde_json::from_value::<WsCloseRecord>(value)
                        .ok()
                        .and_then(|record| {
                            let ts = record.ts;
                            serde_json::to_value(record.close_event(serial))
                                .ok()
                                .map(|value| RankedEvent {
                                    ts,
                                    kind: 4,
                                    sequence,
                                    value,
                                })
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
            _ => None,
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
        StoredLine::Flow(_)
        | StoredLine::TlsError(_)
        | StoredLine::WsOpen(_)
        | StoredLine::WsMsg(_)
        | StoredLine::WsClose(_)
        | StoredLine::Checkpoint(_) => {}
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
        StoredLine::Flow(_)
        | StoredLine::TlsError(_)
        | StoredLine::WsOpen(_)
        | StoredLine::WsMsg(_)
        | StoredLine::WsClose(_)
        | StoredLine::Checkpoint(_) => {}
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

// ── WebSocket recall (backs `net ws` and WS `net show`) ───────────────────────

#[derive(Debug, Clone, Default)]
pub struct WsSessionQuery {
    pub host: Option<String>,
    pub capture_session_id: Option<String>,
    pub since_ts: Option<f64>,
    pub limit: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WsMessageQuery {
    pub session_id: Option<String>,
    pub host: Option<String>,
    pub dir: Option<String>,
    pub opcode: Option<String>,
    pub grep: Option<String>,
    pub since_ts: Option<f64>,
    pub after_ts: Option<f64>,
    pub after_flow_sequence: Option<u64>,
    pub capture_session_id: Option<String>,
    pub limit: usize,
}

/// One row of `net ws`: an upgrade plus its running/final per-direction totals.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WsSessionSummary {
    pub id: String,
    pub capture_session_id: String,
    pub ts: f64,
    pub scheme: String,
    pub host: String,
    pub path: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subprotocol: Option<String>,
    pub permessage_deflate: bool,
    /// No `ws_close` seen yet — still open, or the daemon exited mid-session.
    pub open: bool,
    /// The upgrade (`ws_open`) predates a `net log clear`, so only its later
    /// frames survive; url/scheme/path are unknown here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    pub c2s_msgs: u64,
    pub s2c_msgs: u64,
    pub c2s_bytes: u64,
    pub s2c_bytes: u64,
    pub dropped: u64,
}

fn record_after_floor(value: &serde_json::Value, floor: u64) -> bool {
    let sequence = value
        .get("flow_sequence")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    floor == 0 || sequence > floor
}

/// Everything the log tells us about one session id, in first-seen order.
#[derive(Default)]
struct WsSessionAgg {
    open: Option<WsSessionRecord>,
    close: Option<WsCloseRecord>,
    /// Host from the open, else the first message/close (for orphaned sessions).
    host: Option<String>,
    capture_session_id: Option<String>,
    first_ts: f64,
    live: (u64, u64, u64, u64),
}

fn read_ws_sessions_from(path: &Path, query: &WsSessionQuery) -> Result<Vec<WsSessionSummary>> {
    // One aggregate per session id; `order` preserves first-seen order so a
    // session whose `ws_open` was cleared still appears where its frames land.
    let mut aggs: HashMap<String, WsSessionAgg> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut floor = 0_u64;
    let touch = |aggs: &mut HashMap<String, WsSessionAgg>,
                 order: &mut Vec<String>,
                 id: &str,
                 ts: f64|
     -> bool {
        let fresh = !aggs.contains_key(id);
        if fresh {
            order.push(id.to_string());
        }
        let agg = aggs.entry(id.to_string()).or_default();
        if fresh || ts < agg.first_ts {
            agg.first_ts = ts;
        }
        fresh
    };
    visit_records(path, |record| match record {
        StoredLine::WsOpen(value) if record_after_floor(&value, floor) => {
            if let Ok(open) = serde_json::from_value::<WsSessionRecord>(value) {
                touch(&mut aggs, &mut order, &open.id, open.ts);
                let agg = aggs.get_mut(&open.id).unwrap();
                agg.host = Some(open.host.clone());
                agg.capture_session_id = Some(open.capture_session_id.clone());
                agg.open = Some(open);
            }
        }
        StoredLine::WsClose(value) if record_after_floor(&value, floor) => {
            if let Ok(close) = serde_json::from_value::<WsCloseRecord>(value) {
                touch(&mut aggs, &mut order, &close.session_id, close.ts);
                let agg = aggs.get_mut(&close.session_id).unwrap();
                agg.host.get_or_insert_with(|| close.host.clone());
                agg.capture_session_id
                    .get_or_insert_with(|| close.capture_session_id.clone());
                agg.close = Some(close);
            }
        }
        StoredLine::WsMsg(value) if record_after_floor(&value, floor) => {
            let session = value
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let ts = value
                .get("ts")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            touch(&mut aggs, &mut order, &session, ts);
            let agg = aggs.get_mut(&session).unwrap();
            if agg.host.is_none() {
                agg.host = value
                    .get("host")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
            }
            if agg.capture_session_id.is_none() {
                agg.capture_session_id = value
                    .get("capture_session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
            }
            let dir = value.get("dir").and_then(serde_json::Value::as_str);
            let len = value
                .get("payload_len")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if dir == Some("c2s") {
                agg.live.0 += 1;
                agg.live.2 += len;
            } else {
                agg.live.1 += 1;
                agg.live.3 += len;
            }
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            aggs.clear();
            order.clear();
        }
        _ => {}
    })?;

    let host = query.host.as_deref().map(str::to_lowercase);
    let mut summaries: Vec<WsSessionSummary> = order
        .into_iter()
        .filter_map(|id| aggs.remove(&id).map(|agg| (id, agg)))
        .filter(|(_, agg)| {
            host.as_deref().is_none_or(|needle| {
                agg.host
                    .as_deref()
                    .is_some_and(|host| host.to_lowercase().contains(needle))
            }) && query
                .capture_session_id
                .as_deref()
                .is_none_or(|session| agg.capture_session_id.as_deref() == Some(session))
                && query.since_ts.is_none_or(|since| agg.first_ts >= since)
        })
        .map(|(id, agg)| {
            let close = agg.close.as_ref();
            let live = agg.live;
            let host = agg.host.unwrap_or_default();
            let counts = |c: Option<u64>, l: u64| c.unwrap_or(l);
            match agg.open {
                Some(open) => WsSessionSummary {
                    url: open.url(),
                    open: close.is_none(),
                    dur_ms: close.map(|c| c.dur_ms),
                    close_code: close.and_then(|c| c.close_code),
                    close_reason: close.and_then(|c| c.close_reason.clone()),
                    c2s_msgs: counts(close.map(|c| c.c2s_msgs), live.0),
                    s2c_msgs: counts(close.map(|c| c.s2c_msgs), live.1),
                    c2s_bytes: counts(close.map(|c| c.c2s_bytes), live.2),
                    s2c_bytes: counts(close.map(|c| c.s2c_bytes), live.3),
                    dropped: close.map(|c| c.dropped).unwrap_or(0),
                    id: open.id,
                    capture_session_id: open.capture_session_id,
                    ts: open.ts,
                    scheme: open.scheme,
                    host: open.host,
                    path: open.path,
                    subprotocol: open.subprotocol,
                    permessage_deflate: open.permessage_deflate,
                    partial: false,
                },
                // Orphaned: the `ws_open` predates a clear; surface what's left so
                // `net log`'s `net ws <id>` follow-up still resolves.
                None => WsSessionSummary {
                    id,
                    capture_session_id: agg.capture_session_id.unwrap_or_default(),
                    ts: agg.first_ts,
                    scheme: String::new(),
                    host,
                    path: String::new(),
                    url: String::new(),
                    subprotocol: None,
                    permessage_deflate: false,
                    open: close.is_none(),
                    partial: true,
                    dur_ms: close.map(|c| c.dur_ms),
                    close_code: close.and_then(|c| c.close_code),
                    close_reason: close.and_then(|c| c.close_reason.clone()),
                    c2s_msgs: counts(close.map(|c| c.c2s_msgs), live.0),
                    s2c_msgs: counts(close.map(|c| c.s2c_msgs), live.1),
                    c2s_bytes: counts(close.map(|c| c.c2s_bytes), live.2),
                    s2c_bytes: counts(close.map(|c| c.s2c_bytes), live.3),
                    dropped: close.map(|c| c.dropped).unwrap_or(0),
                },
            }
        })
        .collect();
    if query.limit > 0 && summaries.len() > query.limit {
        summaries.drain(..summaries.len() - query.limit);
    }
    Ok(summaries)
}

/// WebSocket session summaries (oldest first), most recent `limit`.
pub fn read_ws_sessions(serial: &Serial, query: &WsSessionQuery) -> Result<Vec<WsSessionSummary>> {
    read_ws_sessions_from(&paths::session_log_path(serial)?, query)
}

fn read_ws_messages_from(path: &Path, query: &WsMessageQuery) -> Result<Vec<WsMessageRecord>> {
    if query.limit == 0 {
        return Ok(Vec::new());
    }
    let host = query.host.as_deref().map(str::to_lowercase);
    let grep = query.grep.as_deref().map(str::to_lowercase);
    let mut recent = VecDeque::new();
    let mut floor = 0_u64;
    visit_records(path, |record| match record {
        StoredLine::WsMsg(value) if record_after_floor(&value, floor) => {
            let Ok(message) = serde_json::from_value::<WsMessageRecord>(value) else {
                return;
            };
            let matches = query
                .session_id
                .as_deref()
                .is_none_or(|id| message.session_id == id)
                && host
                    .as_deref()
                    .is_none_or(|needle| message.host.to_lowercase().contains(needle))
                && query.dir.as_deref().is_none_or(|dir| message.dir == dir)
                && query
                    .opcode
                    .as_deref()
                    .is_none_or(|opcode| message.opcode == opcode)
                && query.since_ts.is_none_or(|since| message.ts >= since)
                && query.after_ts.is_none_or(|after| message.ts > after)
                && query
                    .after_flow_sequence
                    .is_none_or(|after| message.flow_sequence > after)
                && query
                    .capture_session_id
                    .as_deref()
                    .is_none_or(|session| message.capture_session_id == session)
                && grep.as_deref().is_none_or(|needle| {
                    message
                        .text
                        .as_deref()
                        .is_some_and(|text| text.to_lowercase().contains(needle))
                        || message
                            .preview
                            .as_deref()
                            .is_some_and(|preview| preview.to_lowercase().contains(needle))
                });
            if matches {
                recent.push_back(message);
                if recent.len() > query.limit {
                    recent.pop_front();
                }
            }
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            recent.clear();
        }
        _ => {}
    })?;
    Ok(recent.into_iter().collect())
}

/// WebSocket messages (oldest first), most recent `limit` after filtering.
pub fn read_ws_messages(serial: &Serial, query: &WsMessageQuery) -> Result<Vec<WsMessageRecord>> {
    read_ws_messages_from(&paths::session_log_path(serial)?, query)
}

fn find_ws_session_from(
    path: &Path,
    id: &str,
) -> Result<Option<(WsSessionRecord, Option<WsCloseRecord>)>> {
    let mut open = None;
    let mut close = None;
    // Fallback identity (host, ts, capture_session_id) from a surviving
    // message/close when the `ws_open` itself predates a clear, so `net show
    // <id>` resolves the orphan the same way `net ws` lists it.
    let mut orphan: Option<(String, f64, String)> = None;
    let mut floor = 0_u64;
    visit_records(path, |record| match record {
        StoredLine::WsOpen(value) if record_after_floor(&value, floor) => {
            if let Ok(record) = serde_json::from_value::<WsSessionRecord>(value)
                && record.id == id
            {
                open = Some(record);
            }
        }
        StoredLine::WsClose(value) if record_after_floor(&value, floor) => {
            if let Ok(record) = serde_json::from_value::<WsCloseRecord>(value)
                && record.session_id == id
            {
                orphan.get_or_insert((
                    record.host.clone(),
                    record.ts,
                    record.capture_session_id.clone(),
                ));
                close = Some(record);
            }
        }
        StoredLine::WsMsg(value) if record_after_floor(&value, floor) => {
            if value.get("session_id").and_then(serde_json::Value::as_str) == Some(id)
                && orphan.is_none()
            {
                let host = value
                    .get("host")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let ts = value
                    .get("ts")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                let capture_session_id = value
                    .get("capture_session_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                orphan = Some((host, ts, capture_session_id));
            }
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            open = None;
            close = None;
            orphan = None;
        }
        _ => {}
    })?;
    if let Some(open) = open {
        return Ok(Some((open, close)));
    }
    // Synthesize a partial session from the surviving frames.
    Ok(orphan.map(|(host, ts, capture_session_id)| {
        (
            WsSessionRecord {
                kind: "ws_open".to_string(),
                id: id.to_string(),
                flow_sequence: 0,
                capture_session_id,
                ts,
                scheme: String::new(),
                host,
                path: String::new(),
                status: 0,
                subprotocol: None,
                permessage_deflate: false,
                req_headers: Vec::new(),
                resp_headers: Vec::new(),
                redaction_policy: None,
                redaction_policy_version: None,
            },
            close,
        )
    }))
}

/// The session record (with its close, if any) for `net show <session-id>`.
pub fn find_ws_session(
    serial: &Serial,
    id: &str,
) -> Result<Option<(WsSessionRecord, Option<WsCloseRecord>)>> {
    find_ws_session_from(&paths::session_log_path(serial)?, id)
}

fn find_ws_message_from(path: &Path, id: &str) -> Result<Option<WsMessageRecord>> {
    let mut found = None;
    let mut floor = 0_u64;
    visit_records(path, |record| match record {
        StoredLine::WsMsg(value) if record_after_floor(&value, floor) => {
            if let Ok(record) = serde_json::from_value::<WsMessageRecord>(value)
                && record.id == id
            {
                found = Some(record);
            }
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            found = None;
        }
        _ => {}
    })?;
    Ok(found)
}

/// The message record for `net show <message-id>`.
pub fn find_ws_message(serial: &Serial, id: &str) -> Result<Option<WsMessageRecord>> {
    find_ws_message_from(&paths::session_log_path(serial)?, id)
}

fn export_jsonl_from(
    path: &Path,
    protocol: Protocol,
    capture_session: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    let session_ok = |value: &serde_json::Value| {
        capture_session.is_none_or(|session| {
            value
                .get("capture_session_id")
                .and_then(serde_json::Value::as_str)
                == Some(session)
        })
    };
    let mut out = Vec::new();
    let mut floor = 0_u64;
    visit_records(path, |record| match record {
        StoredLine::Flow(value)
            if protocol.include_http()
                && record_after_floor(&value, floor)
                && session_ok(&value) =>
        {
            out.push(value);
        }
        StoredLine::TlsError(value) if protocol.include_http() && session_ok(&value) => {
            out.push(value);
        }
        StoredLine::WsOpen(value)
            if protocol.include_ws_lifecycle()
                && record_after_floor(&value, floor)
                && session_ok(&value) =>
        {
            out.push(value);
        }
        StoredLine::WsMsg(value)
            if protocol.include_ws_msg()
                && record_after_floor(&value, floor)
                && session_ok(&value) =>
        {
            out.push(value);
        }
        StoredLine::WsClose(value)
            if protocol.include_ws_lifecycle()
                && record_after_floor(&value, floor)
                && session_ok(&value) =>
        {
            out.push(value);
        }
        StoredLine::Clear(clear) => {
            floor = clear.after_flow_sequence;
            out.clear();
        }
        _ => {}
    })?;
    Ok(out)
}

/// The durable records (full flows + WS lines) for `net export jsonl`, in append
/// order after the last logical clear, filtered by protocol and capture session.
pub fn read_export_jsonl(
    serial: &Serial,
    protocol: Protocol,
    capture_session: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    export_jsonl_from(&paths::session_log_path(serial)?, protocol, capture_session)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::secure_existing_log;
    use super::{
        CheckpointRecord, ClearRecord, LogQuery, Protocol, WsMessageQuery, WsSessionQuery,
        acquire_log_lock, acquire_log_read_lock, export_jsonl_from, find_by_id_from,
        find_ws_message_from, find_ws_session_from, read_all_from, read_checkpoint_from,
        read_recent_timeline_from, read_recent_timeline_query_from, read_tls_errors_from,
        read_ws_messages_from, read_ws_sessions_from,
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

    // ── WebSocket recall ──────────────────────────────────────────────────

    fn ws_open(id: &str, seq: u64, ts: f64, host: &str, path: &str) -> serde_json::Value {
        json!({
            "type": "ws_open", "id": id, "flow_sequence": seq,
            "capture_session_id": "n-one", "ts": ts, "scheme": "wss",
            "host": host, "path": path, "status": 101,
            "permessage_deflate": true
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn ws_msg(
        id: &str,
        session: &str,
        seq: u64,
        ts: f64,
        host: &str,
        dir: &str,
        opcode: &str,
        text: &str,
    ) -> serde_json::Value {
        json!({
            "type": "ws_msg", "id": id, "session_id": session, "flow_sequence": seq,
            "capture_session_id": "n-one", "ts": ts, "host": host, "dir": dir,
            "seq": 1, "opcode": opcode, "payload_len": text.len(),
            "wire_len": text.len(), "retained_len": text.len(), "frame_count": 1,
            "text": text, "preview": text
        })
    }

    fn ws_close(id: &str, seq: u64, ts: f64, host: &str) -> serde_json::Value {
        json!({
            "type": "ws_close", "id": id, "session_id": id, "flow_sequence": seq,
            "capture_session_id": "n-one", "ts": ts, "started_ts": ts - 1.0,
            "dur_ms": 1000, "host": host, "close_code": 1000, "close_reason": "bye",
            "close_initiator": "c2s", "c2s_msgs": 1, "s2c_msgs": 1,
            "c2s_bytes": 5, "s2c_bytes": 5
        })
    }

    fn ws_log(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("device.jsonl");
        write_lines(
            &path,
            &[
                line(flow("f1", 5.0, "api.example.com", "/rest")),
                line(ws_open("w1", 1, 10.0, "ws.example.com", "/socket")),
                line(ws_msg(
                    "w1.1",
                    "w1",
                    2,
                    11.0,
                    "ws.example.com",
                    "c2s",
                    "text",
                    "hello server",
                )),
                line(ws_msg(
                    "w1.2",
                    "w1",
                    3,
                    12.0,
                    "ws.example.com",
                    "s2c",
                    "text",
                    "hello client",
                )),
                line(ws_msg(
                    "w1.3",
                    "w1",
                    4,
                    13.0,
                    "ws.example.com",
                    "s2c",
                    "binary",
                    "PNGdata",
                )),
                line(ws_close("w1", 5, 20.0, "ws.example.com")),
            ],
            true,
        );
        path
    }

    #[test]
    fn timeline_default_shows_ws_lifecycle_not_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = ws_log(dir.path());
        let serial = Serial::new("emulator-5554");
        let result = read_recent_timeline_query_from(
            &path,
            &serial,
            &LogQuery {
                limit: 50,
                protocol: Protocol::Default,
                ..Default::default()
            },
        )
        .unwrap();
        let types: Vec<&str> = result
            .items
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect();
        // Default: HTTP flow + ws_open + ws_close, but no per-message ws_msg.
        assert_eq!(types, ["http", "ws_open", "ws_close"]);
    }

    #[test]
    fn timeline_protocol_websocket_includes_messages_and_hides_http() {
        let dir = tempfile::tempdir().unwrap();
        let path = ws_log(dir.path());
        let serial = Serial::new("emulator-5554");
        let result = read_recent_timeline_query_from(
            &path,
            &serial,
            &LogQuery {
                limit: 50,
                protocol: Protocol::WebSocket,
                ..Default::default()
            },
        )
        .unwrap();
        let types: Vec<&str> = result
            .items
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["ws_open", "ws_msg", "ws_msg", "ws_msg", "ws_close"]);
        // The compact ws_msg event carries a preview and direction but no payload.
        let msg = result
            .items
            .iter()
            .find(|item| item["type"] == "ws_msg")
            .unwrap();
        assert_eq!(msg["dir"], "c2s");
        assert_eq!(msg["preview"], "hello server");
        assert!(msg.get("text").is_none());
    }

    #[test]
    fn timeline_protocol_http_hides_websockets() {
        let dir = tempfile::tempdir().unwrap();
        let path = ws_log(dir.path());
        let serial = Serial::new("emulator-5554");
        let result = read_recent_timeline_query_from(
            &path,
            &serial,
            &LogQuery {
                limit: 50,
                protocol: Protocol::Http,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0]["type"], "http");
    }

    #[test]
    fn ws_sessions_summarize_closed_and_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        write_lines(
            &path,
            &[
                line(ws_open("w1", 1, 10.0, "ws.example.com", "/a")),
                line(ws_msg(
                    "w1.1",
                    "w1",
                    2,
                    11.0,
                    "ws.example.com",
                    "c2s",
                    "text",
                    "hi",
                )),
                line(ws_close("w1", 3, 20.0, "ws.example.com")),
                // Second session, still open (no ws_close), 2 live messages.
                line(ws_open("w2", 4, 30.0, "other.example.com", "/b")),
                line(ws_msg(
                    "w2.1",
                    "w2",
                    5,
                    31.0,
                    "other.example.com",
                    "s2c",
                    "text",
                    "x",
                )),
                line(ws_msg(
                    "w2.2",
                    "w2",
                    6,
                    32.0,
                    "other.example.com",
                    "s2c",
                    "text",
                    "yy",
                )),
            ],
            true,
        );
        let sessions = read_ws_sessions_from(
            &path,
            &WsSessionQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sessions.len(), 2);
        let closed = &sessions[0];
        assert_eq!(closed.id, "w1");
        assert!(!closed.open);
        assert_eq!(closed.close_code, Some(1000));
        assert_eq!(closed.c2s_msgs, 1);
        let open = &sessions[1];
        assert_eq!(open.id, "w2");
        assert!(open.open);
        // Live counts derived from ws_msg lines (no ws_close yet).
        assert_eq!(open.s2c_msgs, 2);
        assert_eq!(open.s2c_bytes, 3);

        // Host filter narrows the list.
        let filtered = read_ws_sessions_from(
            &path,
            &WsSessionQuery {
                host: Some("other".into()),
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "w2");
    }

    #[test]
    fn ws_messages_filter_by_dir_opcode_and_grep() {
        let dir = tempfile::tempdir().unwrap();
        let path = ws_log(dir.path());
        let all = read_ws_messages_from(
            &path,
            &WsMessageQuery {
                session_id: Some("w1".into()),
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 3);

        let s2c = read_ws_messages_from(
            &path,
            &WsMessageQuery {
                session_id: Some("w1".into()),
                dir: Some("s2c".into()),
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s2c.len(), 2);
        assert!(s2c.iter().all(|m| m.dir == "s2c"));

        let binary = read_ws_messages_from(
            &path,
            &WsMessageQuery {
                session_id: Some("w1".into()),
                opcode: Some("binary".into()),
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(binary.len(), 1);
        assert_eq!(binary[0].id, "w1.3");

        let grepped = read_ws_messages_from(
            &path,
            &WsMessageQuery {
                session_id: Some("w1".into()),
                grep: Some("server".into()),
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(grepped.len(), 1);
        assert_eq!(grepped[0].id, "w1.1");
    }

    #[test]
    fn find_ws_session_and_message_return_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = ws_log(dir.path());
        let (open, close) = find_ws_session_from(&path, "w1").unwrap().unwrap();
        assert_eq!(open.id, "w1");
        assert_eq!(open.status, 101);
        assert_eq!(close.unwrap().close_code, Some(1000));

        let message = find_ws_message_from(&path, "w1.2").unwrap().unwrap();
        assert_eq!(message.dir, "s2c");
        assert_eq!(message.text.as_deref(), Some("hello client"));
        assert!(find_ws_message_from(&path, "w1.99").unwrap().is_none());
    }

    #[test]
    fn ws_records_honor_the_clear_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let clear = ClearRecord {
            kind: "capture_clear".into(),
            capture_session_id: "n-one".into(),
            cleared_at: 15.0,
            after_flow_id: Some("w1.3".into()),
            after_flow_sequence: 3,
        };
        write_lines(
            &path,
            &[
                line(ws_open("w1", 1, 10.0, "ws.example.com", "/socket")),
                line(ws_msg(
                    "w1.1",
                    "w1",
                    2,
                    11.0,
                    "ws.example.com",
                    "c2s",
                    "text",
                    "old",
                )),
                line(clear),
                line(ws_open("w2", 4, 20.0, "ws.example.com", "/socket")),
                line(ws_msg(
                    "w2.1",
                    "w2",
                    5,
                    21.0,
                    "ws.example.com",
                    "c2s",
                    "text",
                    "new",
                )),
            ],
            true,
        );
        // The pre-clear session/message are gone; only the post-clear ones remain.
        let sessions = read_ws_sessions_from(
            &path,
            &WsSessionQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "w2");
        assert!(find_ws_session_from(&path, "w1").unwrap().is_none());
        assert!(find_ws_message_from(&path, "w1.1").unwrap().is_none());
        assert!(find_ws_message_from(&path, "w2.1").unwrap().is_some());
    }

    #[test]
    fn clearing_mid_session_surfaces_the_orphan_as_partial() {
        // Finding #6: ws_open before a clear, but its later frames survive.
        // `net ws` / `net show` must still surface the session so the
        // `net log`→`net ws <id>` follow-up stays consistent.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let clear = ClearRecord {
            kind: "capture_clear".into(),
            capture_session_id: "n-one".into(),
            cleared_at: 11.5,
            after_flow_id: Some("w1.1".into()),
            after_flow_sequence: 2,
        };
        write_lines(
            &path,
            &[
                line(ws_open("w1", 1, 10.0, "chat.example.com", "/s")),
                line(ws_msg(
                    "w1.1",
                    "w1",
                    2,
                    11.0,
                    "chat.example.com",
                    "c2s",
                    "text",
                    "pre",
                )),
                line(clear),
                line(ws_msg(
                    "w1.2",
                    "w1",
                    3,
                    12.0,
                    "chat.example.com",
                    "s2c",
                    "text",
                    "post",
                )),
                line(ws_close("w1", 4, 13.0, "chat.example.com")),
            ],
            true,
        );
        let sessions = read_ws_sessions_from(
            &path,
            &WsSessionQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sessions.len(), 1, "the orphaned session is still listed");
        assert_eq!(sessions[0].id, "w1");
        assert!(sessions[0].partial, "flagged partial (open was cleared)");
        assert_eq!(sessions[0].host, "chat.example.com");
        assert!(!sessions[0].open, "a post-clear ws_close makes it closed");

        // `net show w1` resolves via a synthesized record instead of erroring.
        let (open, close) = find_ws_session_from(&path, "w1").unwrap().unwrap();
        assert_eq!(open.id, "w1");
        assert_eq!(open.status, 0, "synthesized (no real upgrade retained)");
        assert_eq!(open.host, "chat.example.com");
        assert_eq!(open.capture_session_id, "n-one");
        assert!(close.is_some());
    }

    #[test]
    fn message_only_orphan_recovers_capture_session_id() {
        // Finding B: an orphan with a surviving message but no close must recover
        // capture_session_id (from the message), matching `net ws`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.jsonl");
        let clear = ClearRecord {
            kind: "capture_clear".into(),
            capture_session_id: "n-one".into(),
            cleared_at: 10.5,
            after_flow_id: Some("w1".into()),
            after_flow_sequence: 1,
        };
        write_lines(
            &path,
            &[
                line(ws_open("w1", 1, 10.0, "chat.example.com", "/s")),
                line(clear),
                line(ws_msg(
                    "w1.1",
                    "w1",
                    2,
                    11.0,
                    "chat.example.com",
                    "s2c",
                    "text",
                    "x",
                )),
            ],
            true,
        );
        let (open, close) = find_ws_session_from(&path, "w1").unwrap().unwrap();
        assert_eq!(open.status, 0, "synthesized");
        assert_eq!(
            open.capture_session_id, "n-one",
            "recovered from the message"
        );
        assert!(close.is_none());
    }

    #[test]
    fn export_jsonl_filters_by_protocol_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = ws_log(dir.path());
        // websocket protocol → open + 3 msgs + close (no http flow).
        let ws_only = export_jsonl_from(&path, Protocol::WebSocket, None).unwrap();
        let kinds: Vec<&str> = ws_only
            .iter()
            .map(|value| value["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["ws_open", "ws_msg", "ws_msg", "ws_msg", "ws_close"]);
        // The full record keeps the payload (unlike the compact log event).
        let first_msg = ws_only.iter().find(|v| v["type"] == "ws_msg").unwrap();
        assert_eq!(first_msg["text"], "hello server");

        // all protocol → includes the http flow too.
        let all = export_jsonl_from(&path, Protocol::All, None).unwrap();
        assert_eq!(all.len(), 6);

        // capture-session filter that matches nothing yields an empty export.
        let none = export_jsonl_from(&path, Protocol::All, Some("n-other")).unwrap();
        assert!(none.is_empty());
    }
}
