//! One-shot crash/ANR scanning over `adb logcat -d` dumps — the shared engine
//! behind three agent-facing surfaces:
//!
//!   • `shadowdroid log`     — structured, bounded logcat reads (cmd/log.rs)
//!   • `shadowdroid why`     — the "what just went wrong" triage verb (cmd/why.rs)
//!   • events-since-last     — the probe that attaches crashes/ANRs that happened
//!                             since the previous CLI command to the next command's
//!                             JSON result (`"events":[…]`), so a one-shot loop
//!                             sees a crash without running `watch`.
//!
//! The streaming crash grammar lives in [crate::watch::logcat] (state machine +
//! regexes); this module reuses it against a *dumped* buffer instead of a tail.
//!
//! ## Device-clock timestamps
//!
//! `threadtime` lines carry `MM-DD HH:MM:SS.mmm` in device-local time with no
//! year. All windowing/cursor math therefore runs in "seconds into the year"
//! space (Feb pinned to 29 days so the mapping is stable), with wrap-aware
//! comparisons so a session spanning New Year doesn't mis-order. Device "now" is
//! captured with `date` in the *same* shell call as the dump, so host/device
//! clock skew never enters the math.

use crate::device::adb;
use crate::events::CrashEvent;
use crate::ids::Serial;
use crate::watch::logcat::{CrashCollector, LogLine, package_matches_filter};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Instant;

/// Wrap modulus for seconds-into-year math (Feb=29 basis, see module doc).
const YEAR_SECS: f64 = 366.0 * 86_400.0;
/// Clock-jitter tolerance: a line stamped slightly "after" `now` (same-second
/// races, coarse `date` output) is treated as age 0, not as last year.
const FUTURE_SKEW_TOLERANCE_SECS: f64 = 120.0;
/// The cursor is set this far behind "now" so a crash logged in the same second
/// as the probe read isn't skipped by the strictly-newer comparison; the
/// fingerprint set suppresses the re-report of anything already delivered.
const CURSOR_SLACK_SECS: f64 = 2.0;
/// Fingerprints retained in the cursor for overlap suppression.
const MAX_FINGERPRINTS: usize = 32;
/// Re-scan at `finish_probe` when the command ran longer than this — the entry
/// probe's data would predate a crash that happened *during* the command.
const RESCAN_AFTER_MS: u128 = 1500;

// ── timestamp math ────────────────────────────────────────────────────

/// Cumulative days before month m (1-based), with February fixed at 29 days.
const CUM_DAYS: [u32; 12] = [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335];

/// Parse `MM-DD HH:MM:SS[.mmm]` into seconds-into-year. `None` for garbage.
pub fn parse_ts_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, time) = s.split_once(' ')?;
    let (month, day) = date.split_once('-')?;
    let month: usize = month.parse().ok()?;
    let day: u32 = day.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut parts = time.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let sec_part = parts.next()?;
    let (sec, ms) = match sec_part.split_once('.') {
        Some((s, frac)) => {
            let s: u32 = s.parse().ok()?;
            // Take up to 3 fractional digits as milliseconds.
            let frac: String = frac.chars().take(3).collect();
            let scale = 10u32.pow(3u32.saturating_sub(frac.len() as u32));
            (s, frac.parse::<u32>().ok()? * scale)
        }
        None => (sec_part.parse().ok()?, 0),
    };
    if h > 23 || m > 59 || sec > 60 {
        return None;
    }
    let days = CUM_DAYS[month - 1] + (day - 1);
    Some(
        days as f64 * 86_400.0
            + h as f64 * 3600.0
            + m as f64 * 60.0
            + sec as f64
            + ms as f64 / 1000.0,
    )
}

/// Age of `line` relative to `now`, both in seconds-into-year, wrap-aware.
/// A line slightly in `now`'s future (clock jitter) clamps to 0.
pub fn age_secs(now: f64, line: f64) -> f64 {
    let diff = now - line;
    if diff >= 0.0 {
        return diff;
    }
    if -diff <= FUTURE_SKEW_TOLERANCE_SECS {
        return 0.0;
    }
    // Genuinely "in the future" → interpret as last year (Dec line, Jan now).
    diff.rem_euclid(YEAR_SECS)
}

/// Is `line` strictly newer than `cursor`? Wrap-aware: newer means the circular
/// distance forward from cursor to line is in (0, half a year).
fn newer_than(line: f64, cursor: f64) -> bool {
    let fwd = (line - cursor).rem_euclid(YEAR_SECS);
    fwd > 0.0 && fwd < YEAR_SECS / 2.0
}

// ── scan ──────────────────────────────────────────────────────────────

/// One crash/ANR event lifted out of a logcat dump, tagged with the device-clock
/// timestamp of its final line (so it can be windowed/cursored) and a stable
/// fingerprint of its raw block (so overlapping scans dedupe).
pub struct ScannedCrash {
    pub event: CrashEvent,
    pub ts_raw: String,
    pub ts_secs: Option<f64>,
    pub fingerprint: String,
}

/// The result of one dump scan: device "now" (captured in the same shell call)
/// plus every crash/ANR block found, in dump order.
pub struct CrashScan {
    pub now_raw: Option<String>,
    pub now_secs: Option<f64>,
    pub crashes: Vec<ScannedCrash>,
}

/// The marker line prefix used to carry device "now" alongside the dump.
const NOW_MARKER: &str = "SD_NOW=";

/// Dump `logcat` (bounded) and lift crash/ANR blocks out of it.
///
/// `filtered=true` restricts the dump to the crash-relevant tags device-side
/// (cheap probe: output is empty when nothing crashed); `filtered=false` dumps
/// everything (the `log` verb wants the raw lines too — it passes the dump in
/// via [scan_lines] instead to avoid a second round-trip).
pub async fn scan(
    serial: &Serial,
    buffers: &str,
    max_lines: u32,
    filtered: bool,
    package: Option<&str>,
) -> Result<CrashScan> {
    let filter_spec = if filtered {
        " AndroidRuntime:E ActivityManager:E libc:F DEBUG:F *:S"
    } else {
        ""
    };
    let cmd = format!(
        "echo \"{NOW_MARKER}$(date '+%m-%d %H:%M:%S')\"; \
         logcat -d -v threadtime -b {buffers} -t {max_lines}{filter_spec} 2>&1"
    );
    let out = adb::shell(serial, cmd).await.context("logcat dump")?;
    Ok(scan_lines(out.lines(), package))
}

/// Run the crash state machine over already-dumped lines. The first
/// `SD_NOW=`-marked line (if present) supplies device "now".
pub fn scan_lines<'a>(lines: impl Iterator<Item = &'a str>, package: Option<&str>) -> CrashScan {
    let mut now_raw = None;
    let mut collector = CrashCollector::default();
    let mut crashes: Vec<ScannedCrash> = Vec::new();

    let mut push = |events: Vec<CrashEvent>| {
        for event in events {
            if let (Some(filter), Some(pkg)) = (package, event.package.as_deref())
                && !package_matches_filter(pkg, filter)
            {
                continue;
            }
            // The event's own raw block carries the exact device-clock time of
            // the crash — the surrounding dump position does not (a block can
            // finalize long after unrelated lines have streamed past).
            let ts_raw = event
                .raw
                .lines()
                .next()
                .and_then(LogLine::parse)
                .map(|l| l.ts)
                .unwrap_or_default();
            let fingerprint = blake3::hash(event.raw.as_bytes()).to_hex()[..16].to_string();
            crashes.push(ScannedCrash {
                ts_secs: parse_ts_secs(&ts_raw),
                ts_raw,
                event,
                fingerprint,
            });
        }
    };

    for line in lines {
        if now_raw.is_none()
            && let Some(now) = line.trim().strip_prefix(NOW_MARKER)
        {
            now_raw = Some(now.trim().to_string());
            continue;
        }
        push(collector.handle_line(line));
    }
    push(collector.finalize_now().into_iter().collect());

    let now_secs = now_raw.as_deref().and_then(parse_ts_secs);
    CrashScan {
        now_raw,
        now_secs,
        crashes,
    }
}

// ── per-device host state (the events cursor) ─────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct DeviceState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    crash_cursor: Option<CrashCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrashCursor {
    /// Seconds-into-year of the last probe's "now" minus slack.
    ts_secs: f64,
    /// Human-debuggable form of the same instant.
    ts_raw: String,
    /// Fingerprints of recently delivered events (overlap suppression).
    #[serde(default)]
    fingerprints: Vec<String>,
}

fn state_path(serial: &Serial) -> Result<PathBuf> {
    let sanitized: String = serial
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(crate::hostenv::shadowdroid_home()?
        .join("state")
        .join(format!("{sanitized}.json")))
}

fn load_state(serial: &Serial) -> DeviceState {
    state_path(serial)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_state(serial: &Serial, state: &DeviceState) {
    let Ok(path) = state_path(serial) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(state) {
        // Best-effort: a failed cursor write means an event may be re-reported
        // next command, never lost.
        let _ = std::fs::write(path, text);
    }
}

// ── the events-since-last probe ───────────────────────────────────────

/// Probe toggle: on by default; disabled with SHADOWDROID_NO_EVENTS
/// (1/true/yes/on) for latency-critical scripting.
pub fn probe_enabled() -> bool {
    !crate::hostenv::env_truthy("SHADOWDROID_NO_EVENTS")
}

pub struct Probe {
    handle: tokio::task::JoinHandle<Vec<Value>>,
    started: Instant,
    serial: Serial,
}

/// Spawn the probe concurrently with the command's real work; the one adb
/// round-trip (~40 ms) hides behind the action. `None` when disabled.
pub fn spawn_probe(serial: &Serial) -> Option<Probe> {
    if !probe_enabled() {
        return None;
    }
    let owned = serial.clone();
    let task_serial = owned.clone();
    Some(Probe {
        handle: tokio::spawn(async move { probe_once(&task_serial).await }),
        started: Instant::now(),
        serial: owned,
    })
}

/// Await the probe. If the command ran long (e.g. `ui wait`), the entry probe
/// predates anything that happened *during* the command — re-scan once so a
/// crash mid-wait still surfaces on this same response.
pub async fn finish_probe(probe: Option<Probe>) -> Vec<Value> {
    let Some(probe) = probe else {
        return Vec::new();
    };
    let events = probe.handle.await.unwrap_or_default();
    if events.is_empty() && probe.started.elapsed().as_millis() > RESCAN_AFTER_MS {
        return probe_once(&probe.serial).await;
    }
    events
}

/// One cursor-advancing scan: report crash/ANR events newer than the cursor,
/// then move the cursor to (device now − slack). First run initializes the
/// cursor without reporting history — `log`/`why` are the history verbs.
async fn probe_once(serial: &Serial) -> Vec<Value> {
    let scan = match scan(serial, "crash,system", 200, true, None).await {
        Ok(scan) => scan,
        Err(err) => {
            tracing::debug!("events probe failed: {err:#}");
            return Vec::new();
        }
    };
    let Some(now_secs) = scan.now_secs else {
        return Vec::new();
    };

    let mut state = load_state(serial);
    let new_cursor_ts = now_secs - CURSOR_SLACK_SECS;
    let new_cursor_raw = scan.now_raw.clone().unwrap_or_default();

    let Some(cursor) = state.crash_cursor.clone() else {
        state.crash_cursor = Some(CrashCursor {
            ts_secs: new_cursor_ts,
            ts_raw: new_cursor_raw,
            fingerprints: Vec::new(),
        });
        save_state(serial, &state);
        return Vec::new();
    };

    let fresh: Vec<&ScannedCrash> = scan
        .crashes
        .iter()
        .filter(|c| {
            c.ts_secs
                .map(|ts| newer_than(ts, cursor.ts_secs))
                .unwrap_or(false)
                && !cursor.fingerprints.contains(&c.fingerprint)
        })
        .collect();

    let mut fingerprints = cursor.fingerprints.clone();
    for c in &fresh {
        fingerprints.push(c.fingerprint.clone());
    }
    // Also remember boundary-window events we did NOT deliver this time (they
    // were older than the cursor) so slack overlap can't resurrect them.
    if fingerprints.len() > MAX_FINGERPRINTS {
        let excess = fingerprints.len() - MAX_FINGERPRINTS;
        fingerprints.drain(..excess);
    }
    state.crash_cursor = Some(CrashCursor {
        ts_secs: new_cursor_ts,
        ts_raw: new_cursor_raw,
        fingerprints,
    });
    save_state(serial, &state);

    fresh.iter().map(|c| compact_event(c)).collect()
}

/// The in-band shape: enough to act on (what died, why, where), no raw block —
/// `log`/`why` carry the full detail.
fn compact_event(c: &ScannedCrash) -> Value {
    let e = &c.event;
    let mut v = json!({
        "type": "crash",
        "kind": e.kind,
        "ts": c.ts_raw,
    });
    let obj = v.as_object_mut().expect("literal object");
    if let Some(p) = &e.package {
        obj.insert("package".into(), json!(p));
    }
    if let Some(x) = &e.exception {
        obj.insert("exception".into(), json!(x));
    }
    if let Some(m) = &e.message {
        obj.insert("message".into(), json!(m));
    }
    if let Some(sig) = &e.signal_name {
        obj.insert("signal".into(), json!(sig));
    }
    if !e.stack.is_empty() {
        let top: Vec<&String> = e.stack.iter().take(5).collect();
        obj.insert("stack".into(), json!(top));
    }
    if e.kind == "anr" {
        obj.insert(
            "hint".into(),
            json!("main thread blocked >5s; `shadowdroid why` for detail"),
        );
    } else {
        obj.insert(
            "hint".into(),
            json!("app process died; `shadowdroid why` or `shadowdroid log --last 2m` for detail"),
        );
    }
    v
}

// ── project-frame annotation ──────────────────────────────────────────

/// Map crash stack frames onto the app's source tree so the agent sees
/// *your-code* frames first: returns `{frame, file, line, path}` for each stack
/// entry whose `(File.kt:42)` suffix names a file that exists under
/// `project_root`. Cheap single-pass index; bounded walk.
pub fn project_frames(event: &CrashEvent, project_root: &std::path::Path) -> Vec<Value> {
    let mut frames: Vec<&String> = event.stack.iter().collect();
    for cause in &event.caused_by {
        frames.extend(cause.stack.iter());
    }
    if frames.is_empty() {
        return Vec::new();
    }
    let index = source_index(project_root);
    if index.is_empty() {
        return Vec::new();
    }
    let re = frame_file_re();
    let mut out = Vec::new();
    for frame in frames {
        if out.len() >= 8 {
            break;
        }
        let Some(caps) = re.captures(frame) else {
            continue;
        };
        let file = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let line: u64 = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        if let Some(paths) = index.get(file) {
            // Shortest relative path wins when a filename exists in several modules.
            if let Some(path) = paths.iter().min_by_key(|p| p.len()) {
                out.push(json!({
                    "frame": frame,
                    "file": file,
                    "line": line,
                    "path": path,
                }));
            }
        }
    }
    out
}

fn frame_file_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\(([\w$.-]+\.(?:kt|java)):(\d+)\)").unwrap())
}

/// filename → project-relative paths for every .kt/.java under src/ dirs.
/// Bounded (depth + file count) so a pathological tree can't stall a probe.
fn source_index(root: &std::path::Path) -> std::collections::HashMap<String, Vec<String>> {
    const MAX_FILES: usize = 30_000;
    const MAX_DEPTH: usize = 14;
    let mut index: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH || seen > MAX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                // Skip build output and VCS noise — sources live under src/.
                if matches!(
                    name.as_ref(),
                    "build" | ".git" | ".gradle" | ".idea" | "node_modules" | "out"
                ) {
                    continue;
                }
                stack.push((path, depth + 1));
            } else if name.ends_with(".kt") || name.ends_with(".java") {
                seen += 1;
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                index.entry(name.to_string()).or_default().push(rel);
            }
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_threadtime_timestamps() {
        // June 5: 31+29+31+30+31 = 152 days before June, +4 elapsed days = 156.
        let secs = parse_ts_secs("06-05 00:00:00.000").unwrap();
        assert_eq!(secs, 156.0 * 86_400.0);
        let with_time = parse_ts_secs("06-05 12:30:15.500").unwrap();
        assert_eq!(
            with_time,
            156.0 * 86_400.0 + 12.0 * 3600.0 + 30.0 * 60.0 + 15.5
        );
        // Second-resolution (the `date` marker has no millis).
        assert!(parse_ts_secs("01-01 00:00:01").is_some());
        assert!(parse_ts_secs("garbage").is_none());
        assert!(parse_ts_secs("13-01 00:00:00.000").is_none());
    }

    #[test]
    fn age_is_wrap_aware() {
        let dec31 = parse_ts_secs("12-31 23:59:00.000").unwrap();
        let jan01 = parse_ts_secs("01-01 00:01:00.000").unwrap();
        // A Dec 31 line viewed from Jan 1 is two minutes old, not negative.
        assert!((age_secs(jan01, dec31) - 120.0).abs() < 1.0);
        // Trivial same-day case.
        let a = parse_ts_secs("06-05 12:00:00.000").unwrap();
        let b = parse_ts_secs("06-05 12:00:30.000").unwrap();
        assert_eq!(age_secs(b, a), 30.0);
        // Slight future (clock jitter) clamps to 0.
        assert_eq!(age_secs(a, a + 1.5), 0.0);
    }

    #[test]
    fn newer_than_is_wrap_aware() {
        let dec31 = parse_ts_secs("12-31 23:59:00.000").unwrap();
        let jan01 = parse_ts_secs("01-01 00:01:00.000").unwrap();
        assert!(newer_than(jan01, dec31));
        assert!(!newer_than(dec31, jan01));
        let a = parse_ts_secs("06-05 12:00:00.000").unwrap();
        assert!(!newer_than(a, a));
        assert!(newer_than(a + 1.0, a));
    }

    #[test]
    fn scan_lines_lifts_crash_with_timestamp_and_now() {
        let dump = "\
SD_NOW=06-05 12:00:05
--------- beginning of crash
06-05 12:00:00.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main
06-05 12:00:00.101  1234  1234 E AndroidRuntime: Process: com.example, PID: 1234
06-05 12:00:00.102  1234  1234 E AndroidRuntime: java.lang.RuntimeException: boom
06-05 12:00:00.103  1234  1234 E AndroidRuntime: \tat com.example.Main.onCreate(Main.kt:10)
";
        let scan = scan_lines(dump.lines(), None);
        assert_eq!(scan.now_raw.as_deref(), Some("06-05 12:00:05"));
        assert!(scan.now_secs.is_some());
        assert_eq!(scan.crashes.len(), 1);
        let c = &scan.crashes[0];
        assert_eq!(c.event.kind, "java");
        assert_eq!(c.event.package.as_deref(), Some("com.example"));
        // The block's own first line, not the dump's newest line.
        assert_eq!(c.ts_raw, "06-05 12:00:00.100");
        assert_eq!(c.fingerprint.len(), 16);
    }

    #[test]
    fn scan_lines_filters_by_package() {
        let dump = "\
06-05 12:00:00.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main
06-05 12:00:00.101  1234  1234 E AndroidRuntime: Process: com.other, PID: 1234
06-05 12:00:00.102  1234  1234 E AndroidRuntime: java.lang.RuntimeException: boom
";
        assert_eq!(
            scan_lines(dump.lines(), Some("com.example")).crashes.len(),
            0
        );
        assert_eq!(scan_lines(dump.lines(), Some("com.other")).crashes.len(), 1);
    }

    #[test]
    fn scan_lines_lifts_anr() {
        let dump = "\
06-05 12:00:00.100  1000  1000 E ActivityManager: ANR in com.example (com.example/.Main)
";
        let scan = scan_lines(dump.lines(), None);
        assert_eq!(scan.crashes.len(), 1);
        assert_eq!(scan.crashes[0].event.kind, "anr");
    }

    #[test]
    fn compact_event_is_lean() {
        let dump = "\
06-05 12:00:00.100  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main
06-05 12:00:00.101  1234  1234 E AndroidRuntime: Process: com.example, PID: 1234
06-05 12:00:00.102  1234  1234 E AndroidRuntime: java.lang.RuntimeException: boom
06-05 12:00:00.103  1234  1234 E AndroidRuntime: \tat com.example.A.a(A.kt:1)
06-05 12:00:00.104  1234  1234 E AndroidRuntime: \tat com.example.B.b(B.kt:2)
";
        let scan = scan_lines(dump.lines(), None);
        let v = compact_event(&scan.crashes[0]);
        assert_eq!(v["type"], "crash");
        assert_eq!(v["kind"], "java");
        assert_eq!(v["package"], "com.example");
        assert_eq!(v["exception"], "java.lang.RuntimeException");
        // No raw block in the in-band shape.
        assert!(v.get("raw").is_none());
        assert!(v.get("context").is_none());
    }

    #[test]
    fn project_frames_maps_files_under_root() {
        let dir = std::env::temp_dir().join(format!("sd-crashscan-test-{}", std::process::id()));
        let src = dir.join("app/src/main/java/com/example");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Main.kt"), "fun main() {}").unwrap();

        let event = CrashEvent {
            kind: "java".into(),
            ts: 0.0,
            package: Some("com.example".into()),
            pid: Some(1),
            thread: None,
            exception: Some("java.lang.RuntimeException".into()),
            message: None,
            stack: vec![
                "com.example.Main.onCreate(Main.kt:10)".into(),
                "android.app.Activity.performCreate(Activity.java:8000)".into(),
            ],
            caused_by: Vec::new(),
            signal: None,
            signal_name: None,
            backtrace: Vec::new(),
            raw: String::new(),
            context: Vec::new(),
            device_info: serde_json::Value::Object(Default::default()),
        };
        let frames = project_frames(&event, &dir);
        std::fs::remove_dir_all(&dir).ok();
        // Main.kt is in the project; Activity.java is not.
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["file"], "Main.kt");
        assert_eq!(frames[0]["line"], 10);
        assert!(frames[0]["path"].as_str().unwrap().ends_with("Main.kt"));
    }
}
