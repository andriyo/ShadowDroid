//! `shadowdroid why` — one bounded JSON answering "what just went wrong?".
//!
//! The verb an agent reaches for after any surprise, replacing the 4–5 command
//! forensic dance (crash buffer? logcat errors? what's on screen? did the
//! network fail?) with a single fused read:
//!
//!   • last crash/ANR in the window (parsed, project frames mapped to files)
//!   • recent error-level logcat for the app, deduplicated and capped
//!   • the current screen: foreground app, visible texts, keyboard state
//!   • recent network failures (4xx/5xx/tls_error) when the `net` daemon runs
//!
//! …then an explicit `verdict` naming the most probable cause, with the
//! runner-ups in `also` and next-step commands in `hints`. Best-effort by
//! design: every section degrades independently (`checked` reports coverage),
//! and the server is used only if already reachable — `why` never mutates
//! device state to answer a question.

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::ShadowDroidConfig;
use crate::crashscan;
use crate::device::{adb, installer};
use crate::events::emit_action;
use crate::fusion::top_screen_texts;
use crate::ids::Serial;
use crate::watch::logcat::LogLine;

#[derive(clap::Args)]
pub struct WhyArgs {
    /// App package or config alias to focus on (default: the configured app).
    #[arg(long)]
    pub app: Option<String>,
    /// How far back to look, e.g. 120s, 5m (default 120s).
    #[arg(long, default_value = "120s")]
    pub last: String,
}

pub async fn run(
    serial: &Serial,
    config: &ShadowDroidConfig,
    project_root: Option<&std::path::Path>,
    args: &WhyArgs,
) -> Result<()> {
    let window_secs = crate::cmd::log::parse_duration_secs(&args.last)?;
    let package = crate::cmd::log::resolve_scope(config, serial, args.app.clone(), false).await?;
    let mut checked: Vec<&'static str> = Vec::new();
    let mut evidence = serde_json::Map::new();

    // ── crashes / ANRs / recent error log, one adb round-trip ────────────
    let snapshot = crate::cmd::log::fetch_snapshot(serial, &[], 4000, package.as_deref()).await?;
    checked.push("logcat");
    let now_secs = snapshot.now_secs;
    let in_window = |ts: Option<f64>| match (ts, now_secs) {
        (Some(ts), Some(now)) => crashscan::age_secs(now, ts) <= window_secs,
        _ => true,
    };

    let scan = crashscan::scan_lines(
        snapshot.lines.iter().map(String::as_str),
        package.as_deref(),
    );
    let crashes: Vec<&crashscan::ScannedCrash> = scan
        .crashes
        .iter()
        .filter(|c| in_window(c.ts_secs))
        .collect();
    let last_crash = crashes.iter().rev().find(|c| c.event.kind != "anr");
    let last_anr = crashes.iter().rev().find(|c| c.event.kind == "anr");

    if let Some(c) = last_crash {
        let mut v = serde_json::to_value(&c.event).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.remove("context");
            obj.remove("device_info");
            obj.remove("ts");
            obj.insert("ts_device".into(), json!(c.ts_raw));
            if let Some(root) = project_root {
                let frames = crashscan::project_frames(&c.event, root);
                if !frames.is_empty() {
                    obj.insert("project_frames".into(), json!(frames));
                }
            }
        }
        evidence.insert("crash".into(), v);
    }
    if let Some(a) = last_anr {
        evidence.insert(
            "anr".into(),
            json!({
                "package": a.event.package,
                "message": a.event.message,
                "ts_device": a.ts_raw,
            }),
        );
    }

    // Recent E/F lines for the app's pids (deduped, capped) — the "what was it
    // complaining about" trail even when nothing crashed.
    let crash_pids: Vec<i32> = crashes.iter().filter_map(|c| c.event.pid).collect();
    let mut log_errors: Vec<Value> = Vec::new();
    let mut last_err: Option<(String, String)> = None;
    for raw in &snapshot.lines {
        let Some(line) = LogLine::parse(raw) else {
            continue;
        };
        if !matches!(line.level.as_str(), "E" | "F") {
            continue;
        }
        if !in_window(crashscan::parse_ts_secs(&line.ts)) {
            continue;
        }
        if package.is_some()
            && !snapshot.pids.contains(&line.pid)
            && !crash_pids.contains(&line.pid)
        {
            continue;
        }
        // Crash blocks are already parsed above.
        if matches!(line.tag.as_str(), "AndroidRuntime" | "DEBUG" | "libc") {
            continue;
        }
        if last_err.as_ref() == Some(&(line.tag.clone(), line.msg.clone())) {
            continue;
        }
        last_err = Some((line.tag.clone(), line.msg.clone()));
        log_errors.push(json!({"ts": line.ts, "tag": line.tag, "msg": line.msg}));
    }
    let dropped_errors = log_errors.len().saturating_sub(12);
    if dropped_errors > 0 {
        log_errors.drain(..dropped_errors);
    }
    if !log_errors.is_empty() {
        evidence.insert("log_errors".into(), json!(log_errors));
    }

    // ── the screen, only if the server is already reachable ─────────────
    let mut foreground: Option<String> = None;
    let mut screen_summary: Option<Value> = None;
    let mut server_ok = false;
    match installer::probe_existing(serial, true).await {
        Ok(Some(client)) => {
            server_ok = true;
            checked.push("screen");
            if let Ok(screen) = client.screen().await {
                foreground = screen.current_app.package.clone();
                screen_summary = Some(json!({
                    "current_app": screen.current_app,
                    "screen_hash": screen.screen_hash,
                    "screen_hash_version": screen.screen_hash_version,
                    "top_texts": top_screen_texts(&screen.elements, 12),
                    "keyboard_visible": screen.ime.keyboard_visible,
                }));
            }
        }
        Ok(None) => {
            checked.push("screen_unavailable");
            evidence.insert(
                "server_error".into(),
                json!("no already-established ShadowDroid server session is reachable; read-only diagnostics did not start one"),
            );
            // Host-side fallback: at least name the foreground component.
            if let Some(component) = adb::foreground_activity(serial).await {
                foreground = component.split('/').next().map(str::to_string);
                evidence.insert("foreground_component".into(), json!(component));
            }
        }
        Err(err) => {
            checked.push("screen_unavailable");
            evidence.insert("server_error".into(), json!(err.to_string()));
            if let Some(component) = adb::foreground_activity(serial).await {
                foreground = component.split('/').next().map(str::to_string);
                evidence.insert("foreground_component".into(), json!(component));
            }
        }
    }
    if let Some(s) = &screen_summary {
        evidence.insert("screen".into(), s.clone());
    }

    // ── network failures, only if the `net` daemon is running ───────────
    let mut net_failed: Vec<Value> = Vec::new();
    let mut tls_errors: Vec<Value> = Vec::new();
    if crate::net::control::is_running(serial).await {
        checked.push("net");
        if let Ok(flows) =
            crate::net::store::read_filtered(serial, &crate::net::Matcher::default(), 200)
        {
            for f in flows.iter().rev() {
                let evt = f.http_event(serial);
                let v = serde_json::to_value(&evt).unwrap_or_default();
                let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(0);
                let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(true);
                if !ok || status >= 400 {
                    net_failed.push(json!({
                        "method": v.get("method"),
                        "host": v.get("host"),
                        "path": v.get("path"),
                        "status": v.get("status"),
                        "error": v.get("error"),
                        "id": v.get("id"),
                    }));
                    if net_failed.len() >= 5 {
                        break;
                    }
                }
            }
        }
        if let Ok(errs) = crate::net::store::read_tls_errors(serial, None, 3) {
            tls_errors = errs.into_iter().rev().collect();
        }
        if !net_failed.is_empty() {
            evidence.insert("net_failed".into(), json!(net_failed));
        }
        if !tls_errors.is_empty() {
            evidence.insert("tls_errors".into(), json!(tls_errors));
        }
    } else {
        checked.push("net_daemon_not_running");
    }

    // ── verdict ───────────────────────────────────────────────────────
    let app_left_foreground = match (&package, &foreground) {
        (Some(pkg), Some(fg)) => !fg.contains(pkg.as_str()),
        _ => false,
    };
    let mut verdicts: Vec<(&'static str, &'static str)> = Vec::new();
    if last_crash.is_some() {
        verdicts.push((
            "app_crashed",
            "the app process crashed — see evidence.crash (project_frames point into your code)",
        ));
    }
    if last_anr.is_some() {
        verdicts.push((
            "app_not_responding",
            "the system reported an ANR — the main thread was blocked; see evidence.anr",
        ));
    }
    if !tls_errors.is_empty() {
        verdicts.push((
            "tls_rejected",
            "the app rejected the proxy CA during TLS — see evidence.tls_errors and `net check`",
        ));
    }
    if net_failed.iter().any(|f| {
        f.get("status")
            .and_then(|s| s.as_u64())
            .is_some_and(|s| s >= 500)
    }) {
        verdicts.push((
            "backend_errors",
            "recent 5xx responses from the backend — see evidence.net_failed",
        ));
    } else if net_failed.len() >= 2 {
        verdicts.push((
            "request_failures",
            "several failed requests — see evidence.net_failed",
        ));
    }
    if app_left_foreground {
        verdicts.push((
            "app_not_foreground",
            "the target app is not the foreground app — a dialog, launcher, or another app took over; see evidence.screen",
        ));
    }
    if verdicts.is_empty() && !log_errors.is_empty() {
        verdicts.push((
            "log_errors_only",
            "no crash/ANR/network failure — but the app logged errors; see evidence.log_errors",
        ));
    }
    if verdicts.is_empty() {
        verdicts.push((
            "no_obvious_cause",
            "no crash, ANR, network failure, or error logs in the window — the screen state itself may be the answer",
        ));
    }

    let (verdict, explanation) = verdicts[0];
    let also: Vec<&str> = verdicts.iter().skip(1).map(|(v, _)| *v).collect();

    let hints: Vec<String> = match verdict {
        "app_crashed" => vec![
            "shadowdroid log --last 5m   # full crash context".into(),
            "shadowdroid app start       # relaunch after the fix".into(),
        ],
        "app_not_responding" => vec![
            "shadowdroid log --last 5m --grep ANR".into(),
            "shadowdroid debug snapshot --depth 1   # where the main thread is stuck".into(),
        ],
        "tls_rejected" => vec![
            format!(
                "shadowdroid net check {}",
                package.as_deref().unwrap_or("<pkg>")
            ),
            "shadowdroid net trust --auto".into(),
        ],
        "backend_errors" | "request_failures" => vec![
            "shadowdroid net log | jq -c 'select(.type==\"http\" and (.ok==false or .status>=400))'".into(),
            "shadowdroid net show <id> --body".into(),
        ],
        "app_not_foreground" => vec![
            "shadowdroid ui dump   # see what took over".into(),
            "shadowdroid ui wait --pkg <expected> --timeout-ms 5000".into(),
        ],
        _ => vec![
            "shadowdroid ui dump".into(),
            "shadowdroid log --last 5m --level w".into(),
            "shadowdroid collect   # full offline bundle".into(),
        ],
    };

    let mut body = json!({
        "verdict": verdict,
        "explanation": explanation,
        "window": args.last,
        "package": package,
        "server_ok": server_ok,
        "checked": checked,
        "evidence": Value::Object(evidence),
        "hints": hints,
    });
    if !also.is_empty() {
        body["also"] = json!(also);
    }
    emit_action("why", &body);
    Ok(())
}
