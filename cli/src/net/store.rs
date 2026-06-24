//! Session event log — appends completed [FlowRecord]s to
//! `~/.shadowdroid/net/<serial>.jsonl`, one JSON object per line. Backs `net
//! log` (recall) and `net show` (detail by id). No daemon IPC needed for
//! recall: the log is a plain file any `net` invocation can read.

use crate::ids::Serial;
use anyhow::{Context, Result};
use std::io::Write;

use crate::net::flow::FlowRecord;
use crate::net::{paths, Matcher};

/// Append one completed flow to the session log (creating it if needed).
pub fn append(serial: &Serial, rec: &FlowRecord) -> Result<()> {
    paths::ensure_net_dir()?;
    let path = paths::session_log_path(serial)?;
    let mut line = serde_json::to_string(rec)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append {}", path.display()))?;
    Ok(())
}

/// All flows in the session log, oldest first. Lines that fail to parse (e.g. a
/// partial write) are skipped rather than failing the whole read.
pub fn read_all(serial: &Serial) -> Result<Vec<FlowRecord>> {
    let path = paths::session_log_path(serial)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<FlowRecord>(l).ok())
        .collect())
}

/// The last `limit` flows matching `m`, oldest first.
pub fn read_filtered(serial: &Serial, m: &Matcher, limit: usize) -> Result<Vec<FlowRecord>> {
    let mut all = read_all(serial)?;
    all.retain(|f| f.matches(m));
    let n = all.len();
    if n > limit {
        all = all.split_off(n - limit);
    }
    Ok(all)
}

/// Most recent flow with this id (ids can repeat across daemon runs).
pub fn find_by_id(serial: &Serial, id: &str) -> Result<Option<FlowRecord>> {
    Ok(read_all(serial)?.into_iter().rev().find(|f| f.id == id))
}

/// Drop the session log (called on `net start` so each session is fresh).
pub fn clear(serial: &Serial) -> Result<()> {
    let path = paths::session_log_path(serial)?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}
