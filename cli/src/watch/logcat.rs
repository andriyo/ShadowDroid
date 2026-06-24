//! Crash detection via `adb logcat`.
//!
//! Tail with `-v threadtime -T 1 AndroidRuntime:E ActivityManager:E libc:F DEBUG:F *:S`.
//! Parser emits structured crash events from proven AndroidRuntime/libc/DEBUG patterns.
//!
//! State machine: idle → collecting (after `FATAL EXCEPTION`/`Fatal signal`) →
//! finalise after a quiet window (default 1s) or when another crash starts.
//! On finalise, fetch ~60 lines of broader context via `adb logcat -d -t 60`
//! and `adb shell getprop` for device info, then emit a `crash` event.

use crate::device::adb;
use crate::events::{now_ts, CausedBy, CrashEvent, Event};
use crate::ids::Serial;
use anyhow::{Context, Result};
use regex::Regex;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

const CONTEXT_LINES: u32 = 60;
const QUIET_WINDOW: Duration = Duration::from_secs(1);

pub async fn run(
    serial: Serial,
    app_filter: Option<String>,
    out: mpsc::Sender<Event>,
) -> Result<()> {
    let (crash_tx, mut crash_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let _ = run_crashes(serial, app_filter, crash_tx).await;
    });
    while let Some(evt) = crash_rx.recv().await {
        let _ = out.send(Event::Crash(evt)).await;
    }
    Ok(())
}

pub async fn run_crashes(
    serial: Serial,
    app_filter: Option<String>,
    out: mpsc::Sender<CrashEvent>,
) -> Result<()> {
    let mut child = Command::new("adb")
        .args([
            "-s",
            &serial,
            "logcat",
            "-v",
            "threadtime",
            "-T",
            "1",
            "AndroidRuntime:E",
            "ActivityManager:E",
            "libc:F",
            "DEBUG:F",
            "*:S",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting adb logcat for crash detection")?;

    let stdout = child
        .stdout
        .take()
        .context("adb logcat did not expose stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut collector = CrashCollector::default();
    let mut quiet_tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else {
                    break;
                };
                for evt in collector.handle_line(&line) {
                    emit_if_matches(&serial, &app_filter, &out, evt).await;
                }
            }
            _ = quiet_tick.tick() => {
                if let Some(evt) = collector.finalize_if_quiet(QUIET_WINDOW) {
                    emit_if_matches(&serial, &app_filter, &out, evt).await;
                }
            }
        }
    }

    if let Some(evt) = collector.finalize_now() {
        emit_if_matches(&serial, &app_filter, &out, evt).await;
    }
    Ok(())
}

async fn emit_if_matches(
    serial: &Serial,
    app_filter: &Option<String>,
    out: &mpsc::Sender<CrashEvent>,
    mut evt: CrashEvent,
) {
    if let (Some(filter), Some(package)) = (app_filter, &evt.package) {
        if !package_matches_filter(package, filter) {
            return;
        }
    }
    evt.context = fetch_context(serial).await;
    evt.device_info = fetch_device_info(serial).await;
    let _ = out.send(evt).await;
}

fn package_matches_filter(package: &str, filter: &str) -> bool {
    package == filter
        || package
            .strip_prefix(filter)
            .is_some_and(|suffix| suffix.starts_with(':'))
}

#[derive(Default)]
struct CrashCollector {
    buffer: Vec<String>,
    kind: Option<CrashKind>,
    pid: Option<i32>,
    last_seen: Option<Instant>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CrashKind {
    Java,
    Native,
}

impl CrashCollector {
    fn handle_line(&mut self, line: &str) -> Vec<CrashEvent> {
        let Some(parsed) = LogLine::parse(line) else {
            if !self.buffer.is_empty() {
                self.buffer.push(line.to_string());
                self.last_seen = Some(Instant::now());
            }
            return Vec::new();
        };

        let is_java_start =
            parsed.tag == "AndroidRuntime" && parsed.msg.contains("FATAL EXCEPTION");
        let is_native_start = native_fatal_re().is_match(&parsed.msg);
        let is_anr = parsed.tag == "ActivityManager" && parsed.msg.contains("ANR in ");

        if is_anr {
            return vec![build_anr_event(&parsed)];
        }

        if is_java_start || is_native_start {
            let pending = self.finalize_now();
            self.kind = Some(if is_java_start {
                CrashKind::Java
            } else {
                CrashKind::Native
            });
            self.pid = Some(parsed.pid);
            self.buffer = vec![line.to_string()];
            self.last_seen = Some(Instant::now());
            return pending.into_iter().collect();
        }

        if !self.buffer.is_empty() && Some(parsed.pid) == self.pid {
            match self.kind {
                Some(CrashKind::Java) if parsed.tag == "AndroidRuntime" => {
                    self.buffer.push(line.to_string());
                    self.last_seen = Some(Instant::now());
                }
                Some(CrashKind::Native) if parsed.tag == "DEBUG" || parsed.tag == "libc" => {
                    self.buffer.push(line.to_string());
                    self.last_seen = Some(Instant::now());
                }
                _ => {}
            }
        }
        Vec::new()
    }

    fn finalize_if_quiet(&mut self, quiet: Duration) -> Option<CrashEvent> {
        if self
            .last_seen
            .map(|seen| seen.elapsed() >= quiet)
            .unwrap_or(false)
        {
            self.finalize_now()
        } else {
            None
        }
    }

    fn finalize_now(&mut self) -> Option<CrashEvent> {
        if self.buffer.is_empty() {
            return None;
        }
        let lines = std::mem::take(&mut self.buffer);
        let kind = self.kind.take().unwrap_or(CrashKind::Java);
        let pid = self.pid.take();
        self.last_seen = None;
        Some(match kind {
            CrashKind::Java => build_java_event(&lines, pid),
            CrashKind::Native => build_native_event(&lines, pid),
        })
    }
}

struct LogLine {
    pid: i32,
    tag: String,
    msg: String,
    raw: String,
}

impl LogLine {
    fn parse(line: &str) -> Option<Self> {
        let caps = logcat_re().captures(line)?;
        Some(Self {
            pid: caps.get(2)?.as_str().parse().ok()?,
            tag: caps.get(5)?.as_str().to_string(),
            msg: caps.get(6).map(|m| m.as_str()).unwrap_or("").to_string(),
            raw: line.to_string(),
        })
    }
}

fn build_anr_event(line: &LogLine) -> CrashEvent {
    let package = anr_header_re()
        .captures(&line.msg)
        .and_then(|m| m.get(1))
        .map(|m| m.as_str().to_string());
    CrashEvent {
        kind: "anr".to_string(),
        ts: now_ts(),
        package,
        pid: Some(line.pid),
        thread: None,
        exception: None,
        message: Some(line.msg.clone()),
        stack: Vec::new(),
        caused_by: Vec::new(),
        signal: None,
        signal_name: None,
        backtrace: Vec::new(),
        raw: line.raw.clone(),
        context: Vec::new(),
        device_info: serde_json::Value::Object(Default::default()),
    }
}

fn build_java_event(lines: &[String], pid: Option<i32>) -> CrashEvent {
    let msgs = lines.iter().map(extract_msg).collect::<Vec<_>>();
    let mut thread = None;
    let mut package = None;
    let mut exception = None;
    let mut message = None;
    let mut stack = Vec::new();
    let mut caused_by: Vec<CausedBy> = Vec::new();

    for msg in msgs {
        if let Some(caps) = java_fatal_header_re().captures(&msg) {
            thread = caps.get(1).map(|m| m.as_str().to_string());
            continue;
        }
        if let Some(caps) = java_process_re().captures(&msg) {
            package = caps.get(1).map(|m| m.as_str().to_string());
            continue;
        }
        if let Some(caps) = java_caused_by_re().captures(&msg) {
            if let Some(cause_text) = caps.get(1) {
                if let Some(ex_caps) = java_exception_re().captures(cause_text.as_str()) {
                    caused_by.push(CausedBy {
                        exception: ex_caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string(),
                        message: ex_caps.get(2).map(|m| m.as_str().to_string()),
                        stack: Vec::new(),
                    });
                }
            }
            continue;
        }
        if let Some(caps) = java_at_re().captures(&msg) {
            let frame = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if let Some(last) = caused_by.last_mut() {
                last.stack.push(frame);
            } else {
                stack.push(frame);
            }
            continue;
        }
        if exception.is_none() {
            if let Some(caps) = java_exception_re().captures(&msg) {
                exception = caps.get(1).map(|m| m.as_str().to_string());
                message = caps.get(2).map(|m| m.as_str().to_string());
            }
        }
    }

    CrashEvent {
        kind: "java".to_string(),
        ts: now_ts(),
        package,
        pid,
        thread,
        exception,
        message,
        stack,
        caused_by,
        signal: None,
        signal_name: None,
        backtrace: Vec::new(),
        raw: lines.join("\n"),
        context: Vec::new(),
        device_info: serde_json::Value::Object(Default::default()),
    }
}

fn build_native_event(lines: &[String], pid: Option<i32>) -> CrashEvent {
    let msgs = lines.iter().map(extract_msg).collect::<Vec<_>>();
    let mut signal = None;
    let mut signal_name = None;
    let mut thread = None;
    let mut package = None;
    let mut backtrace = Vec::new();

    for msg in msgs {
        if let Some(caps) = native_fatal_re().captures(&msg) {
            signal = caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok());
            signal_name = caps.get(2).map(|m| m.as_str().to_string());
            if let Some(name) = caps.get(4) {
                thread = Some(name.as_str().to_string());
            }
            continue;
        }
        if let Some(caps) = native_pid_line_re().captures(&msg) {
            if thread.is_none() {
                thread = caps.get(3).map(|m| m.as_str().to_string());
            }
            package = caps.get(4).map(|m| m.as_str().to_string());
            continue;
        }
        let stripped = msg.trim_start();
        if stripped.starts_with('#') {
            backtrace.push(stripped.to_string());
        }
    }

    CrashEvent {
        kind: "native".to_string(),
        ts: now_ts(),
        package,
        pid,
        thread,
        exception: None,
        message: None,
        stack: Vec::new(),
        caused_by: Vec::new(),
        signal,
        signal_name,
        backtrace,
        raw: lines.join("\n"),
        context: Vec::new(),
        device_info: serde_json::Value::Object(Default::default()),
    }
}

fn extract_msg(line: &String) -> String {
    LogLine::parse(line)
        .map(|l| l.msg)
        .unwrap_or_else(|| line.clone())
}

async fn fetch_context(serial: &Serial) -> Vec<String> {
    adb::shell(
        serial,
        format!("logcat -d -v threadtime -t {CONTEXT_LINES}"),
    )
    .await
    .map(|out| out.lines().map(str::to_string).collect())
    .unwrap_or_default()
}

async fn fetch_device_info(serial: &Serial) -> serde_json::Value {
    // Shared with `collect` so the device-fact shape stays identical across
    // crash events and diagnostic bundles.
    adb::device_info(serial).await
}

fn logcat_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})\s+(\d+)\s+(\d+)\s+([VDIWEF])\s+([^:]+?):\s?(.*)$",
        )
        .unwrap()
    })
}

fn java_fatal_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"FATAL EXCEPTION:\s*(.+)").unwrap())
}

fn java_process_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"Process:\s*(\S+?),\s*PID:\s*(\d+)").unwrap())
}

fn java_exception_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^((?:[\w$]+\.)+[\w$]+(?:Exception|Error)[\w$]*)(?::\s*(.*))?$").unwrap()
    })
}

fn java_at_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*at\s+(.+)$").unwrap())
}

fn java_caused_by_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Caused by:\s*(.+)$").unwrap())
}

fn native_fatal_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"Fatal signal (\d+) \(([^)]+)\)(?:.*?in tid (\d+) \(([^)]+)\))?").unwrap()
    })
}

fn native_pid_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"pid:\s*(\d+),\s*tid:\s*(\d+),\s*name:\s*(\S+).*?>>>\s*(\S+)").unwrap()
    })
}

fn anr_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"ANR in (\S+)").unwrap())
}

#[cfg(test)]
mod tests {
    use super::{package_matches_filter, CrashCollector};

    #[test]
    fn parses_java_crash_block() {
        let mut c = CrashCollector::default();
        c.handle_line("05-19 12:00:00.000  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main");
        c.handle_line(
            "05-19 12:00:00.001  1234  1234 E AndroidRuntime: Process: com.example, PID: 1234",
        );
        c.handle_line(
            "05-19 12:00:00.002  1234  1234 E AndroidRuntime: java.lang.RuntimeException: boom",
        );
        c.handle_line("05-19 12:00:00.003  1234  1234 E AndroidRuntime: \tat com.example.Main.onCreate(Main.kt:1)");
        let evt = c.finalize_now().unwrap();
        assert_eq!(evt.kind, "java");
        assert_eq!(evt.package.as_deref(), Some("com.example"));
        assert_eq!(evt.exception.as_deref(), Some("java.lang.RuntimeException"));
        assert_eq!(evt.stack.len(), 1);
    }

    #[test]
    fn parses_native_crash_block() {
        let mut c = CrashCollector::default();
        c.handle_line("05-19 12:00:00.000  1234  1234 F libc: Fatal signal 11 (SIGSEGV), code 1, fault addr 0x0 in tid 1234 (main)");
        c.handle_line("05-19 12:00:00.001  1234  1234 F DEBUG: pid: 1234, tid: 1234, name: main  >>> com.example <<<");
        c.handle_line(
            "05-19 12:00:00.002  1234  1234 F DEBUG:       #00 pc 0000000000010000  /apex/lib.so",
        );
        let evt = c.finalize_now().unwrap();
        assert_eq!(evt.kind, "native");
        assert_eq!(evt.package.as_deref(), Some("com.example"));
        assert_eq!(evt.signal, Some(11));
        assert_eq!(evt.signal_name.as_deref(), Some("SIGSEGV"));
        assert_eq!(evt.backtrace.len(), 1);
    }

    #[test]
    fn package_filter_matches_remote_processes() {
        assert!(package_matches_filter("com.example", "com.example"));
        assert!(package_matches_filter("com.example:remote", "com.example"));
        assert!(!package_matches_filter("com.example.other", "com.example"));
        assert!(!package_matches_filter("com.other", "com.example"));
    }

    #[test]
    fn parses_anr_header() {
        let mut c = CrashCollector::default();
        let events = c.handle_line(
            "05-19 12:00:00.000  1234  5678 E ActivityManager: ANR in com.example (com.example/.MainActivity)",
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "anr");
        assert_eq!(events[0].package.as_deref(), Some("com.example"));
        assert_eq!(events[0].pid, Some(1234));
        assert!(events[0]
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("ANR in com.example"));
    }
}
