//! Session event log — appends completed [FlowRecord]s to
//! `~/.shadowdroid/net/<serial>.jsonl`, one JSON object per line. Backs `net
//! log` (recall) and `net show` (detail by id). No daemon IPC needed for
//! recall: the log is a plain file any `net` invocation can read.

use crate::ids::Serial;
use anyhow::{Context, Result};
use std::io::Write;

use crate::events::Event;
use crate::net::flow::FlowRecord;
use crate::net::{paths, Matcher};

/// Append one completed flow to the session log (creating it if needed).
pub fn append(serial: &Serial, rec: &FlowRecord) -> Result<()> {
    append_line(serial, &serde_json::to_string(rec)?)
}

/// Append a non-flow [Event] (currently only `tls_error`) as its own JSON line.
/// `read_all`/`net show` parse lines as [FlowRecord] and skip these; they're
/// picked back out by [read_tls_errors] for `net log`.
pub fn append_event(serial: &Serial, ev: &Event) -> Result<()> {
    append_line(serial, &serde_json::to_string(ev)?)
}

fn append_line(serial: &Serial, line: &str) -> Result<()> {
    paths::ensure_net_dir()?;
    let path = paths::session_log_path(serial)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("append {}", path.display()))?;
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

/// `tls_error` events from the session log (in log order), optionally limited to
/// hosts containing `host` (case-insensitive substring, matching the flow
/// matcher). Returned as raw JSON values since [Event] is serialize-only.
pub fn read_tls_errors(serial: &Serial, host: Option<&str>) -> Result<Vec<serde_json::Value>> {
    let path = paths::session_log_path(serial)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(filter_tls_errors(&text, host))
}

/// Pure core of [read_tls_errors]: pick `tls_error` lines matching `host`. Flow
/// lines have no top-level `type` key, so they're excluded.
fn filter_tls_errors(text: &str, host: Option<&str>) -> Vec<serde_json::Value> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("tls_error"))
        .filter(|v| match host {
            Some(h) => v
                .get("host")
                .and_then(|x| x.as_str())
                .map(|hn| hn.to_lowercase().contains(&h.to_lowercase()))
                .unwrap_or(false),
            None => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::filter_tls_errors;

    #[test]
    fn filter_tls_errors_picks_events_and_skips_flows() {
        // A flow line (no top-level `type`) interleaved with two tls_error lines.
        let log = concat!(
            r#"{"id":"f1","ts":1.0,"method":"GET","scheme":"https","host":"api.example.com","path":"/x","req_len":0,"resp_len":0}"#,
            "\n",
            r#"{"type":"tls_error","ts":2.0,"host":"api.example.com","reason":"rejected"}"#,
            "\n",
            r#"{"type":"tls_error","ts":3.0,"host":"cdn.other.net","reason":"rejected"}"#,
            "\n",
        );
        // No filter: both tls_errors, no flow.
        let all = filter_tls_errors(log, None);
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|v| v["type"] == "tls_error"));
        // Host filter is a case-insensitive substring.
        let one = filter_tls_errors(log, Some("EXAMPLE"));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0]["host"], "api.example.com");
        // Malformed lines are skipped, not fatal.
        assert!(filter_tls_errors("not json\n\n", None).is_empty());
    }
}
