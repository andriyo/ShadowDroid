//! Opt-in local usage log — treat the agent as a user you can't interview.
//!
//! When enabled, every CLI invocation appends one JSON line to
//! `~/.shadowdroid/usage.jsonl`: which verb ran, how long it took, and the
//! machine error code if it failed. **No argument values are recorded** (no
//! selectors, no paths, no package names), and nothing ever leaves the
//! machine — the file exists so `shadowdroid usage report` can tell you which
//! verbs agents actually use and which error codes they trip over: that
//! ranking is the UX roadmap.
//!
//! Enable with `shadowdroid usage enable` (writes `"usage_log": true` to the
//! user config) or per-invocation with SHADOWDROID_USAGE_LOG=1.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use crate::config::{self, ShadowDroidConfig};
use crate::events::emit_action;

/// Rotate at ~5 MiB: usage.jsonl → usage.jsonl.1 (one generation kept).
const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(clap::Subcommand)]
pub enum UsageCmd {
    /// Is usage logging on, where does it write, how big is the log?
    Status,
    /// Turn usage logging on (writes `"usage_log": true` to the user config).
    Enable,
    /// Turn usage logging off.
    Disable,
    /// Aggregate the log: verb frequencies, error codes, durations.
    Report {
        /// Only include entries from the last N days.
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
    /// Delete the usage log files.
    Clear,
}

pub fn usage_log_path() -> Result<PathBuf> {
    Ok(crate::hostenv::shadowdroid_home()?.join("usage.jsonl"))
}

fn enabled(config: &ShadowDroidConfig) -> bool {
    crate::hostenv::env_truthy("SHADOWDROID_USAGE_LOG") || config.usage_log == Some(true)
}

/// Append one record for this invocation. Called unconditionally from the run
/// wrapper; checks enablement itself and never fails the command (any I/O
/// problem degrades to a debug log line).
pub fn record(started: Instant, result: &Result<()>) {
    let config = ShadowDroidConfig::load().unwrap_or_default();
    if !enabled(&config) {
        return;
    }
    // `usage` subcommands would drown the stats in meta-noise; skip them.
    let verb = verb_from_argv();
    if verb.is_empty() || verb.starts_with("usage") {
        return;
    }
    let mut entry = json!({
        "ts_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "verb": verb,
        "ms": started.elapsed().as_millis() as u64,
        "ok": result.is_ok(),
        "v": env!("CARGO_PKG_VERSION"),
    });
    if let Err(err) = result {
        entry["code"] = json!(crate::cli::error_code_of(err));
    }
    if let Err(io) = append(&entry) {
        tracing::debug!("usage log write failed: {io:#}");
    }
}

/// The verb path ("ui tap", "net start", …) recovered by re-parsing argv with
/// clap and walking the subcommand chain — no hand-maintained match to drift.
/// Flag values can't be mistaken for verbs this way (unlike scanning argv).
fn verb_from_argv() -> String {
    use clap::CommandFactory;
    let cmd = crate::cli::Cli::command().ignore_errors(true);
    let Ok(matches) = cmd.try_get_matches_from(std::env::args_os()) else {
        return "(unparsed)".into();
    };
    let mut parts = Vec::new();
    let mut current = &matches;
    while let Some((name, sub)) = current.subcommand() {
        parts.push(name.to_string());
        current = sub;
    }
    parts.join(" ")
}

fn append(entry: &Value) -> Result<()> {
    let path = usage_log_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Size-capped: rotate once past the cap, keep one prior generation.
    if std::fs::metadata(&path)
        .map(|m| m.len() > ROTATE_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{entry}")?;
    Ok(())
}

pub fn run(cmd: &UsageCmd) -> Result<()> {
    match cmd {
        UsageCmd::Status => {
            let config = ShadowDroidConfig::load().unwrap_or_default();
            let path = usage_log_path()?;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            emit_action(
                "usage_status",
                &json!({
                    "enabled": enabled(&config),
                    "config_value": config.usage_log,
                    "env_override": crate::hostenv::env_truthy("SHADOWDROID_USAGE_LOG"),
                    "path": path.display().to_string(),
                    "bytes": size,
                    "note": "local only — records verb, duration, and error code; never argument values",
                }),
            );
        }
        UsageCmd::Enable => set_enabled(true)?,
        UsageCmd::Disable => set_enabled(false)?,
        UsageCmd::Report { days } => report(*days)?,
        UsageCmd::Clear => {
            let path = usage_log_path()?;
            let existed = std::fs::remove_file(&path).is_ok();
            let _ = std::fs::remove_file(path.with_extension("jsonl.1"));
            emit_action(
                "usage_clear",
                &json!({"path": path.display().to_string(), "removed": existed}),
            );
        }
    }
    Ok(())
}

/// Flip `usage_log` in the *user* config file only — never a project file, so
/// enabling telemetry for yourself can't end up committed to a repo.
fn set_enabled(value: bool) -> Result<()> {
    let path = config::user_config_path()?;
    let mut user = if path.is_file() {
        config::parse_config_file(&path)?
    } else {
        ShadowDroidConfig::default()
    };
    user.usage_log = Some(value);
    config::write_config_file(&path, &user).context("write user config")?;
    emit_action(
        "usage_set",
        &json!({
            "enabled": value,
            "config": path.display().to_string(),
            "log": usage_log_path()?.display().to_string(),
        }),
    );
    Ok(())
}

fn report(days: u32) -> Result<()> {
    let path = usage_log_path()?;
    let cutoff_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        .saturating_sub(days as u64 * 86_400_000);

    // Read current + one rotated generation.
    let mut text = std::fs::read_to_string(path.with_extension("jsonl.1")).unwrap_or_default();
    text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());

    struct VerbStats {
        count: u64,
        errors: u64,
        durations: Vec<u64>,
    }
    let mut by_verb: std::collections::BTreeMap<String, VerbStats> = Default::default();
    let mut by_code: std::collections::BTreeMap<String, u64> = Default::default();
    let mut total = 0u64;
    let mut oldest_ms: Option<u64> = None;

    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = v.get("ts_ms").and_then(|t| t.as_u64()).unwrap_or(0);
        if ts < cutoff_ms {
            continue;
        }
        total += 1;
        oldest_ms = Some(oldest_ms.map_or(ts, |o| o.min(ts)));
        let verb = v
            .get("verb")
            .and_then(|s| s.as_str())
            .unwrap_or("(unknown)")
            .to_string();
        let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(true);
        let ms = v.get("ms").and_then(|m| m.as_u64()).unwrap_or(0);
        let stats = by_verb.entry(verb).or_insert(VerbStats {
            count: 0,
            errors: 0,
            durations: Vec::new(),
        });
        stats.count += 1;
        stats.durations.push(ms);
        if !ok {
            stats.errors += 1;
            let code = v
                .get("code")
                .and_then(|s| s.as_str())
                .unwrap_or("error")
                .to_string();
            *by_code.entry(code).or_insert(0) += 1;
        }
    }

    let mut verbs: Vec<Value> = by_verb
        .into_iter()
        .map(|(verb, mut s)| {
            s.durations.sort_unstable();
            json!({
                "verb": verb,
                "count": s.count,
                "errors": s.errors,
                "p50_ms": percentile(&s.durations, 0.5),
                "p95_ms": percentile(&s.durations, 0.95),
            })
        })
        .collect();
    verbs.sort_by_key(|v| std::cmp::Reverse(v["count"].as_u64().unwrap_or(0)));

    let mut codes: Vec<Value> = by_code
        .into_iter()
        .map(|(code, count)| json!({"code": code, "count": count}))
        .collect();
    codes.sort_by_key(|v| std::cmp::Reverse(v["count"].as_u64().unwrap_or(0)));

    emit_action(
        "usage_report",
        &json!({
            "days": days,
            "total": total,
            "oldest_ts_ms": oldest_ms,
            "verbs": verbs,
            "error_codes": codes,
            "path": path.display().to_string(),
        }),
    );
    Ok(())
}

/// Nearest-rank percentile over an ascending-sorted slice.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_math_holds() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.5), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 0.5), 3);
        assert_eq!(percentile(&[1, 2, 3, 4, 100], 0.95), 100);
    }
}
