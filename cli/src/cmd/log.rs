//! `shadowdroid log` — structured, bounded, app-scoped logcat.
//!
//! The single most-used Android debugging read, shaped for agents: one JSON
//! object per line instead of a raw text firehose, scoped to the app's
//! processes by default, windowed (`--last 60s`), deduplicated (consecutive
//! identical lines collapse with a `repeat` count), and with crash/ANR blocks
//! lifted out of the noise into parsed `{"type":"crash",…}` events — annotated
//! with `project_frames` (your-code stack frames mapped to files) when a
//! project root is known.
//!
//! Wire shape mirrors `net log`: N line-delimited entries, then one
//! `{"type":"action","cmd":"log",…}` summary. Filter with
//! `jq -c 'select(.type=="crash")'` etc.
//!
//! Scoping honesty: package→pid mapping uses the *current* process list, so
//! main-buffer lines from a process that already died are not attributable and
//! are dropped — except crash blocks, which carry their package name and are
//! matched by it. The summary reports the pid set used.

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::config::ShadowDroidConfig;
use crate::crashscan;
use crate::device::adb;
use crate::events::{emit, emit_action};
use crate::ids::Serial;
use crate::watch::logcat::LogLine;

#[derive(clap::Args)]
pub struct LogArgs {
    /// App package or config alias to scope to (default: the configured app;
    /// `--all` for every process).
    #[arg(long)]
    pub app: Option<String>,
    /// Include every process (ignore the configured default app).
    #[arg(long)]
    pub all: bool,
    /// Minimum level: v, d, i, w, e, or f (default: everything).
    #[arg(long, value_parser = ["v", "d", "i", "w", "e", "f"])]
    pub level: Option<String>,
    /// Only lines whose tag contains this (repeatable; case-insensitive).
    #[arg(long)]
    pub tag: Vec<String>,
    /// Only lines whose tag or message matches this regex.
    #[arg(long)]
    pub grep: Option<String>,
    /// How far back to look: a duration like 30s, 5m, 2h (default 60s).
    #[arg(long, default_value = "60s")]
    pub last: String,
    /// Maximum log entries to emit after filtering (most recent kept).
    #[arg(long, default_value_t = 100)]
    pub max: usize,
    /// Logcat buffers to read (comma-separated: main, system, crash, events).
    #[arg(long, value_delimiter = ',', default_value = "main,system,crash")]
    pub buffer: Vec<String>,
    /// Keep consecutive duplicate lines instead of collapsing them with a
    /// `repeat` count.
    #[arg(long)]
    pub no_dedup: bool,
}

/// Parse "30s" / "5m" / "2h" / bare seconds into seconds.
pub fn parse_duration_secs(s: &str) -> Result<f64> {
    let s = s.trim();
    let (num, unit) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&s[..s.len() - 1], c.to_ascii_lowercase()),
        _ => (s, 's'),
    };
    let value: f64 = num
        .parse()
        .with_context(|| format!("invalid duration `{s}` — use forms like 30s, 5m, 2h"))?;
    let secs = match unit {
        's' => value,
        'm' => value * 60.0,
        'h' => value * 3600.0,
        'd' => value * 86_400.0,
        other => bail!("invalid duration unit `{other}` in `{s}` — use s, m, h, or d"),
    };
    if !(secs.is_finite() && secs > 0.0) {
        bail!("duration `{s}` must be positive");
    }
    Ok(secs)
}

fn level_rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "V" => 0,
        "D" => 1,
        "I" => 2,
        "W" => 3,
        "E" => 4,
        "F" => 5,
        _ => 0,
    }
}

/// One filtered, deduplicated log entry ready to emit.
#[derive(Debug, serde::Serialize)]
struct LogEntry {
    #[serde(rename = "type")]
    kind: &'static str,
    ts: String,
    level: String,
    tag: String,
    pid: i32,
    msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat: Option<u32>,
}

/// Everything one `log`/`why` read needs from the device, fetched in a single
/// shell round-trip: device "now", the process list, and the logcat dump.
pub struct LogSnapshot {
    pub now_secs: Option<f64>,
    pub now_raw: Option<String>,
    /// pids whose process name matches the scoped package (name == pkg or
    /// pkg:suffix). Empty when unscoped.
    pub pids: Vec<i32>,
    pub lines: Vec<String>,
}

const PS_MARKER: &str = "SD_PS";
const LOG_MARKER: &str = "SD_LOG";

/// Fetch now + ps + logcat dump in one adb call so all clocks agree.
pub async fn fetch_snapshot(
    serial: &Serial,
    buffers: &[String],
    fetch_lines: u32,
    package: Option<&str>,
) -> Result<LogSnapshot> {
    let buffers = if buffers.is_empty() {
        "main,system,crash".to_string()
    } else {
        buffers.join(",")
    };
    let cmd = format!(
        "echo \"SD_NOW=$(date '+%m-%d %H:%M:%S')\"; echo {PS_MARKER}; \
         ps -A -o PID=,NAME= 2>/dev/null; echo {LOG_MARKER}; \
         logcat -d -v threadtime -b {buffers} -t {fetch_lines} 2>&1"
    );
    let out = adb::shell(serial, cmd).await.context("logcat snapshot")?;

    let mut now_raw = None;
    let mut pids = Vec::new();
    let mut lines = Vec::new();
    let mut section = 0u8; // 0 = preamble, 1 = ps, 2 = log
    for line in out.lines() {
        match line.trim() {
            l if section == 0 && l.starts_with("SD_NOW=") => {
                now_raw = Some(l.trim_start_matches("SD_NOW=").trim().to_string());
                continue;
            }
            PS_MARKER => {
                section = 1;
                continue;
            }
            LOG_MARKER => {
                section = 2;
                continue;
            }
            _ => {}
        }
        match section {
            1 => {
                if let Some(package) = package {
                    let mut cols = line.split_whitespace();
                    if let (Some(pid), Some(name)) = (cols.next(), cols.next())
                        && (name == package
                            || name
                                .strip_prefix(package)
                                .is_some_and(|s| s.starts_with(':')))
                        && let Ok(pid) = pid.parse()
                    {
                        pids.push(pid);
                    }
                }
            }
            2 => lines.push(line.to_string()),
            _ => {}
        }
    }
    let now_secs = now_raw.as_deref().and_then(crashscan::parse_ts_secs);
    Ok(LogSnapshot {
        now_secs,
        now_raw,
        pids,
        lines,
    })
}

/// Resolve the `--app`/config-default scoping to a package (None = unscoped).
pub async fn resolve_scope(
    config: &ShadowDroidConfig,
    serial: &Serial,
    app: Option<String>,
    all: bool,
) -> Result<Option<String>> {
    if all {
        return Ok(None);
    }
    let resolved = config
        .resolve_app(Some(serial.as_str()), app.as_deref())
        .await?;
    Ok(resolved.package)
}

pub async fn run(
    serial: &Serial,
    config: &ShadowDroidConfig,
    project_root: Option<&std::path::Path>,
    args: &LogArgs,
) -> Result<()> {
    let window_secs = parse_duration_secs(&args.last)?;
    let grep = args
        .grep
        .as_deref()
        .map(regex::Regex::new)
        .transpose()
        .context("invalid --grep regex")?;
    let min_rank = args.level.as_deref().map(level_rank);
    let package = resolve_scope(config, serial, args.app.clone(), args.all).await?;

    // Fetch enough raw lines that filtering still fills --max; a fixed generous
    // cap keeps one bad flag from dumping a 16 MiB buffer through the parser.
    let fetch_lines = 4000.max(args.max.saturating_mul(20).min(20_000)) as u32;
    let snapshot = fetch_snapshot(serial, &args.buffer, fetch_lines, package.as_deref()).await?;
    let now_secs = snapshot.now_secs;

    // Crash/ANR blocks first (package-matched, so dead pids still attribute).
    let scan = crashscan::scan_lines(
        snapshot.lines.iter().map(String::as_str),
        package.as_deref(),
    );
    let in_window = |ts_secs: Option<f64>| match (ts_secs, now_secs) {
        (Some(ts), Some(now)) => crashscan::age_secs(now, ts) <= window_secs,
        // No comparable clock — keep the entry rather than silently dropping it.
        _ => true,
    };
    let crashes: Vec<&crashscan::ScannedCrash> = scan
        .crashes
        .iter()
        .filter(|c| in_window(c.ts_secs))
        .collect();
    let crash_pids: Vec<i32> = crashes.iter().filter_map(|c| c.event.pid).collect();

    // Line filter pipeline: parse → window → scope → level → tag → grep.
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut truncated_scope = false;
    for raw in &snapshot.lines {
        let Some(line) = LogLine::parse(raw) else {
            continue;
        };
        let ts_secs = crashscan::parse_ts_secs(&line.ts);
        if !in_window(ts_secs) {
            continue;
        }
        if package.is_some() {
            let known = snapshot.pids.contains(&line.pid) || crash_pids.contains(&line.pid);
            if !known {
                truncated_scope = true;
                continue;
            }
        }
        if let Some(min) = min_rank
            && level_rank(&line.level) < min
        {
            continue;
        }
        if !args.tag.is_empty()
            && !args.tag.iter().any(|t| {
                line.tag
                    .to_ascii_lowercase()
                    .contains(&t.to_ascii_lowercase())
            })
        {
            continue;
        }
        if let Some(re) = &grep
            && !re.is_match(&line.msg)
            && !re.is_match(&line.tag)
        {
            continue;
        }
        // Crash-block constituents are represented by the parsed crash event;
        // don't pay for them twice.
        if crash_pids.contains(&line.pid)
            && matches!(line.tag.as_str(), "AndroidRuntime" | "DEBUG" | "libc")
        {
            continue;
        }
        if line.tag == "ActivityManager" && line.msg.contains("ANR in ") {
            continue;
        }
        // Dedup: consecutive identical (level, tag, msg) collapse.
        if !args.no_dedup
            && let Some(last) = entries.last_mut()
            && last.level == line.level
            && last.tag == line.tag
            && last.msg == line.msg
        {
            last.repeat = Some(last.repeat.unwrap_or(1) + 1);
            last.ts = line.ts; // keep the newest timestamp
            continue;
        }
        entries.push(LogEntry {
            kind: "log",
            ts: line.ts,
            level: line.level,
            tag: line.tag,
            pid: line.pid,
            msg: line.msg,
            repeat: None,
        });
    }

    // Keep the most recent --max entries.
    let dropped = entries.len().saturating_sub(args.max);
    if dropped > 0 {
        entries.drain(..dropped);
    }

    for entry in &entries {
        emit(entry);
    }
    for c in &crashes {
        let mut v = serde_json::to_value(&c.event).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("type".into(), json!("crash"));
            obj.insert("ts_device".into(), json!(c.ts_raw));
            // The streaming enrichments (context/device_info) don't apply to a
            // one-shot read; drop the empty placeholders.
            obj.remove("context");
            obj.remove("device_info");
            obj.remove("ts");
            if let Some(root) = project_root {
                let frames = crashscan::project_frames(&c.event, root);
                if !frames.is_empty() {
                    obj.insert("project_frames".into(), json!(frames));
                }
            }
        }
        emit(&v);
    }

    // Did the buffer reach back over the whole window? If the oldest raw line
    // is younger than the window start, history was evicted before we read it.
    let buffer_covers_window = match (
        snapshot
            .lines
            .iter()
            .find_map(|l| LogLine::parse(l))
            .and_then(|l| crashscan::parse_ts_secs(&l.ts)),
        now_secs,
    ) {
        (Some(oldest), Some(now)) => crashscan::age_secs(now, oldest) >= window_secs,
        _ => true,
    };

    let mut summary = json!({
        "count": entries.len(),
        "crashes": crashes.len(),
        "window": args.last,
        "package": package,
        "pids": snapshot.pids,
        "buffers": args.buffer.join(","),
        "device_now": snapshot.now_raw,
    });
    let s = summary.as_object_mut().expect("literal object");
    if dropped > 0 {
        s.insert("dropped_over_max".into(), json!(dropped));
        s.insert(
            "hint".into(),
            json!(
                "older matching lines dropped by --max; raise --max or narrow with --level/--grep"
            ),
        );
    }
    if !buffer_covers_window {
        s.insert("window_truncated".into(), json!(true));
        s.insert(
            "window_hint".into(),
            json!("logcat buffer does not reach back over the full --last window; older lines are gone"),
        );
    }
    if truncated_scope && entries.is_empty() && crashes.is_empty() {
        s.insert(
            "scope_hint".into(),
            json!("no lines attributable to the app's current pids; if the process restarted, its old lines can't be attributed — crashes are still matched by package"),
        );
    }
    emit_action("log", &summary);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30.0);
        assert_eq!(parse_duration_secs("5m").unwrap(), 300.0);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200.0);
        assert_eq!(parse_duration_secs("45").unwrap(), 45.0);
        assert!(parse_duration_secs("0s").is_err());
        assert!(parse_duration_secs("-5s").is_err());
        assert!(parse_duration_secs("abc").is_err());
        assert!(parse_duration_secs("5x").is_err());
    }

    #[test]
    fn ranks_levels() {
        assert!(level_rank("E") > level_rank("W"));
        assert!(level_rank("F") > level_rank("E"));
        assert!(level_rank("V") < level_rank("D"));
    }
}
