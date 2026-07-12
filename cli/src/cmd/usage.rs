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
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// Usage consent is read only from the user config, never the merged project
/// config. A checked-out repository must not be able to opt the user into even
/// local-only logging.
fn user_usage_config() -> ShadowDroidConfig {
    config::user_config_path()
        .ok()
        .filter(|path| path.is_file())
        .and_then(|path| config::parse_config_file(&path).ok())
        .unwrap_or_default()
}

/// Append one record for this invocation. Called unconditionally from the run
/// wrapper; checks enablement itself and never fails the command (any I/O
/// problem degrades to a debug log line).
pub fn record(started: Instant, result: &Result<()>) {
    let config = user_usage_config();
    if !enabled(&config) {
        return;
    }
    // `usage` subcommands would drown the stats in meta-noise; skip them.
    let verb = verb_from_argv();
    if verb.is_empty() || verb.starts_with("usage") {
        return;
    }
    let mut entry = json!({
        "schema_version": 3,
        "ts_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0),
        "verb": verb,
        "ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "ok": result.is_ok(),
        "v": env!("CARGO_PKG_VERSION"),
    });
    if let Err(err) = result {
        let code = crate::cli::error_code_of(err);
        let stage = crate::cli::error_stage_of(err);
        entry["code"] = json!(code);
        entry["stage"] = json!(stage);
        entry["recovery_id"] = json!(format!("{stage}/{code}"));
        entry["used_fallback"] = json!(crate::cli::error_uses_fallback(err));
        entry["retryable"] = json!(crate::cli::error_retryable_of(err));
    }
    if let Err(io) = append(&entry) {
        tracing::debug!("usage log write failed: {io:#}");
    }
}

/// Record a clap parse failure, which exits before the normal `run` wrapper can
/// observe a `Result`. Only the partial command path and error category are
/// retained; invalid argument names/values are deliberately excluded.
pub fn record_parse_error(kind: &str, had_suggestion: bool, verb: Option<&str>) {
    let config = user_usage_config();
    if !enabled(&config) {
        return;
    }
    let entry = json!({
        "schema_version": 3,
        "ts_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0),
        "verb": verb.unwrap_or("(unparsed)"),
        "ms": 0,
        "ok": false,
        "code": "usage",
        "stage": "parse",
        "retryable": false,
        "parse_kind": kind,
        "had_suggestion": had_suggestion,
        "recovery_id": format!("parse/{kind}"),
        "used_fallback": false,
        "v": env!("CARGO_PKG_VERSION"),
    });
    if let Err(io) = append(&entry) {
        tracing::debug!("usage parse-error log write failed: {io:#}");
    }
}

/// The verb path ("ui tap", "net start", …) recovered by re-parsing argv with
/// clap and walking the subcommand chain — no hand-maintained match to drift.
/// Flag values can't be mistaken for verbs this way (unlike scanning argv).
pub(crate) fn verb_from_argv() -> String {
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
    // Serialize the tiny rotate+append transaction across processes with the
    // standard library's OS-backed file lock. Locks are released automatically
    // on process death, so no stale lockfile reclamation heuristic is needed.
    let _rotation_lock = acquire_usage_lock(&path)?;
    append_locked(&path, entry)
}

fn acquire_usage_lock(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.lock()?;
    Ok(lock)
}

fn append_locked(path: &Path, entry: &Value) -> Result<()> {
    if std::fs::metadata(path)
        .map(|m| m.len() > ROTATE_BYTES)
        .unwrap_or(false)
    {
        let rotated = path.with_added_extension("1");
        match std::fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove rotated usage log"),
        }
        std::fs::rename(path, &rotated)
            .with_context(|| format!("rotate {} to {}", path.display(), rotated.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "{entry}")?;
    Ok(())
}

pub fn run(cmd: &UsageCmd) -> Result<()> {
    match cmd {
        UsageCmd::Status => {
            let config = user_usage_config();
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
                    "note": "local only — records verb, duration, typed recovery id, and whether fallback guidance was needed; never argument values",
                }),
            );
        }
        UsageCmd::Enable => set_enabled(true)?,
        UsageCmd::Disable => set_enabled(false)?,
        UsageCmd::Report { days } => report(*days)?,
        UsageCmd::Clear => {
            let path = usage_log_path()?;
            let _rotation_lock = acquire_usage_lock(&path)?;
            let existed = remove_if_exists(&path)?;
            remove_if_exists(&path.with_added_extension("1"))?;
            emit_action(
                "usage_clear",
                &json!({"path": path.display().to_string(), "removed": existed}),
            );
        }
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
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
    let _rotation_lock = acquire_usage_lock(&path)?;
    let cutoff_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
        .saturating_sub(u64::from(days) * 86_400_000);

    // Read current + one rotated generation.
    let mut text = std::fs::read_to_string(path.with_added_extension("1")).unwrap_or_default();
    text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());

    struct VerbStats {
        count: u64,
        errors: u64,
        durations: Vec<u64>,
    }
    let mut by_verb: std::collections::BTreeMap<String, VerbStats> = Default::default();
    let mut by_code: std::collections::BTreeMap<String, u64> = Default::default();
    let mut by_stage: std::collections::BTreeMap<String, u64> = Default::default();
    let mut by_recovery: std::collections::BTreeMap<String, u64> = Default::default();
    let mut by_recovery_path: std::collections::BTreeMap<(String, String, bool), u64> =
        Default::default();
    let mut by_version: std::collections::BTreeMap<String, u64> = Default::default();
    let mut fallback_errors = 0u64;
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
        let version = v
            .get("v")
            .and_then(|s| s.as_str())
            .unwrap_or("(unknown)")
            .to_string();
        *by_version.entry(version).or_insert(0) += 1;
        let ms = v.get("ms").and_then(|m| m.as_u64()).unwrap_or(0);
        let stats = by_verb.entry(verb.clone()).or_insert(VerbStats {
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
            let stage = v
                .get("stage")
                .and_then(|s| s.as_str())
                .unwrap_or("runtime")
                .to_string();
            *by_stage.entry(stage).or_insert(0) += 1;
            let recovery_id = v
                .get("recovery_id")
                .and_then(|s| s.as_str())
                .unwrap_or("legacy/unclassified")
                .to_string();
            *by_recovery.entry(recovery_id.clone()).or_insert(0) += 1;
            let used_fallback = v
                .get("used_fallback")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            *by_recovery_path
                .entry((verb.clone(), recovery_id.clone(), used_fallback))
                .or_insert(0) += 1;
            if used_fallback {
                fallback_errors += 1;
            }
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
                "error_rate": if s.count == 0 { 0.0 } else { s.errors as f64 / s.count as f64 },
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
    let stages: Vec<Value> = by_stage
        .into_iter()
        .map(|(stage, errors)| json!({"stage": stage, "errors": errors}))
        .collect();
    let versions: Vec<Value> = by_version
        .into_iter()
        .map(|(version, count)| json!({"version": version, "count": count}))
        .collect();
    let mut recoveries: Vec<Value> = by_recovery
        .into_iter()
        .map(|(recovery_id, count)| json!({"recovery_id": recovery_id, "count": count}))
        .collect();
    recoveries.sort_by_key(|value| std::cmp::Reverse(value["count"].as_u64().unwrap_or_default()));
    let mut recovery_paths: Vec<Value> = by_recovery_path
        .into_iter()
        .map(|((verb, recovery_id, used_fallback), count)| {
            json!({
                "verb": verb,
                "recovery_id": recovery_id,
                "used_fallback": used_fallback,
                "count": count,
            })
        })
        .collect();
    recovery_paths
        .sort_by_key(|value| std::cmp::Reverse(value["count"].as_u64().unwrap_or_default()));
    let recommendations = build_recommendations(&verbs, &codes, &recovery_paths);

    emit_action(
        "usage_report",
        &json!({
            "days": days,
            "total": total,
            "oldest_ts_ms": oldest_ms,
            "verbs": verbs,
            "error_codes": codes,
            "error_stages": stages,
            "recovery_ids": recoveries,
            "recovery_paths": recovery_paths,
            "fallback_errors": fallback_errors,
            "versions": versions,
            "recommendations": recommendations,
            "feedback_loop": "prioritize recommendations, add a regression test, implement, then compare error rate and p95 by version",
            "path": path.display().to_string(),
        }),
    );
    Ok(())
}

/// Turn observed friction into an agent-ready, evidence-backed improvement
/// queue. This deliberately recommends work; it never edits code or sends data.
fn build_recommendations(verbs: &[Value], codes: &[Value], recovery_paths: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let fallback_errors = recovery_paths
        .iter()
        .filter(|path| path["used_fallback"] == true)
        .map(|path| path["count"].as_u64().unwrap_or_default())
        .sum::<u64>();
    if let Some(path) = recovery_paths
        .iter()
        .find(|path| path["used_fallback"] == true)
    {
        let verb = path["verb"].as_str().unwrap_or("(unknown)");
        out.push(json!({
            "priority": "highest",
            "kind": "missing_typed_recovery",
            "verb": verb,
            "recovery_id": path["recovery_id"].clone(),
            "evidence": {
                "path_count": path["count"],
                "fallback_errors": fallback_errors,
            },
            "next_action": format!("reproduce the fallback recovery for `{verb}`, convert it to a typed diagnostic, and add a state-specific next_actions regression"),
        }));
    }
    for verb in verbs {
        let count = verb.get("count").and_then(Value::as_u64).unwrap_or(0);
        let errors = verb.get("errors").and_then(Value::as_u64).unwrap_or(0);
        let p95 = verb.get("p95_ms").and_then(Value::as_u64).unwrap_or(0);
        let name = verb
            .get("verb")
            .and_then(Value::as_str)
            .unwrap_or("(unknown)");
        if count >= 4 && errors * 4 >= count {
            out.push(json!({
                "priority": "high",
                "kind": "reliability",
                "verb": name,
                "evidence": {"count": count, "errors": errors},
                "next_action": format!("reproduce the dominant failure for `{name}`, add a contract regression, and make its diagnostic recovery path executable"),
            }));
        }
        if count >= 3 && p95 >= 1_000 {
            out.push(json!({
                "priority": "medium",
                "kind": "latency",
                "verb": name,
                "evidence": {"count": count, "p95_ms": p95},
                "next_action": format!("profile `{name}` and add a warm-path latency budget regression"),
            }));
        }
    }
    for code in codes.iter().take(3) {
        let count = code.get("count").and_then(Value::as_u64).unwrap_or(0);
        if count < 2 {
            continue;
        }
        let name = code.get("code").and_then(Value::as_str).unwrap_or("error");
        out.push(json!({
            "priority": "high",
            "kind": "error_code",
            "code": name,
            "evidence": {"count": count},
            "next_action": format!("audit `{name}` failures for a deterministic next_actions command and an automated recovery test"),
        }));
    }
    out.truncate(10);
    out
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
    fn usage_lock_serializes_append_and_clear_transactions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        let first = acquire_usage_lock(&path).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let contender_path = path.clone();
        let contender = std::thread::spawn(move || {
            let lock = acquire_usage_lock(&contender_path).unwrap();
            sender.send(()).unwrap();
            drop(lock);
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        drop(first);
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        contender.join().unwrap();
    }

    #[test]
    fn percentile_math_holds() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.5), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 0.5), 3);
        assert_eq!(percentile(&[1, 2, 3, 4, 100], 0.95), 100);
    }

    #[test]
    fn recommendations_require_repeated_evidence() {
        let verbs = vec![json!({
            "verb": "ui dump", "count": 4, "errors": 1, "p95_ms": 1200
        })];
        let codes = vec![json!({"code": "wait_timeout", "count": 2})];
        let result = build_recommendations(&verbs, &codes, &[]);
        assert!(result.iter().any(|v| v["kind"] == "reliability"));
        assert!(result.iter().any(|v| v["kind"] == "latency"));
        assert!(result.iter().any(|v| v["code"] == "wait_timeout"));
    }

    #[test]
    fn fallback_recovery_is_the_highest_priority_self_improvement_signal() {
        let recovery_paths = vec![json!({
            "verb": "app install",
            "recovery_id": "input/input_not_found",
            "used_fallback": true,
            "count": 3,
        })];
        let result = build_recommendations(&[], &[], &recovery_paths);
        assert_eq!(result[0]["priority"], "highest");
        assert_eq!(result[0]["kind"], "missing_typed_recovery");
        assert_eq!(result[0]["evidence"]["fallback_errors"], 3);
        assert_eq!(result[0]["verb"], "app install");
        assert_eq!(result[0]["recovery_id"], "input/input_not_found");
    }
}
