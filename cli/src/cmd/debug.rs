//! Agent-first debugging orchestration.
//!
//! Studio-backed debugger commands are the thin Android Studio bridge. The
//! snapshot/timeline commands compose the device server, adb, screenshots,
//! logcat, and optional Studio debugger state into deterministic artifacts an
//! agent can consume or replay.

use crate::cmd::debugger::{self, BridgeClient, DebuggerCmd};
use crate::cmd::studio_contract::{query, route, session_action};
use crate::device::adb;
use crate::device::client::ServerClient;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader as StdBufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Args)]
pub struct DebugArgs {
    /// Android Studio plugin bridge URL. Defaults to the plugin registry, then
    /// http://127.0.0.1:50576.
    #[arg(
        long,
        alias = "url",
        global = true,
        env = "SHADOWDROID_STUDIO_DEBUGGER_URL"
    )]
    pub studio_url: Option<String>,

    #[command(subcommand)]
    pub cmd: DebugCmd,
}

#[derive(Subcommand)]
pub enum DebugCmd {
    /// Capture one bounded debug state object for an agent.
    Snapshot(SnapshotArgs),
    /// Record a JSONL debug timeline: screen/app/logcat/debugger/screenshot events.
    Record(RecordArgs),
    /// Replay action events from a JSONL timeline.
    Replay(ReplayArgs),
    /// Show bridge status, attach, break, step, inspect frames, eval, and watch values.
    #[command(flatten)]
    Studio(DebuggerCmd),
    /// Step over until the screen hash changes, then return a final snapshot.
    StepUntilScreenChange(StudioWaitArgs),
    /// Step over until logcat emits a matching line, then return a final snapshot.
    StepUntilLog(StepUntilLogArgs),
    /// Resume and wait for a Java/native crash or ANR, then return a final snapshot.
    RunUntilCrash(RunUntilCrashArgs),
}

#[derive(Args, Clone)]
pub struct SnapshotArgs {
    /// App package used for log/debugger filtering where possible.
    #[arg(long)]
    pub app: Option<String>,
    /// Write the snapshot JSON to a file instead of stdout.
    #[arg(short = 'o', long)]
    pub out: Option<PathBuf>,
    /// Directory for screenshot artifacts.
    #[arg(long)]
    pub screenshot_dir: Option<PathBuf>,
    /// Skip screenshot capture.
    #[arg(long)]
    pub no_screenshot: bool,
    /// Number of recent logcat lines to include.
    #[arg(long, default_value_t = 200)]
    pub logs: u32,
    /// Include expanded top-frame variables when the debugger is suspended.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
}

#[derive(Args)]
pub struct RecordArgs {
    /// JSONL timeline path.
    #[arg(short = 'o', long)]
    pub out: PathBuf,
    /// Stop automatically after this many milliseconds. Omit to record until Ctrl-C.
    #[arg(long)]
    pub duration_ms: Option<u64>,
    /// Poll interval for screen/debugger snapshots.
    #[arg(long, default_value_t = 500)]
    pub poll_ms: u64,
    /// App package used for annotations and debugger filtering where possible.
    #[arg(long)]
    pub app: Option<String>,
    /// Directory for screenshot artifacts. Defaults beside --out.
    #[arg(long)]
    pub screenshot_dir: Option<PathBuf>,
    /// Skip screenshot capture on screen changes.
    #[arg(long)]
    pub no_screenshots: bool,
    /// Include expanded top-frame variables in debugger timeline events.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
}

#[derive(Args)]
pub struct ReplayArgs {
    /// JSONL timeline path.
    pub file: PathBuf,
    /// Print replayable actions without performing them.
    #[arg(long)]
    pub dry_run: bool,
    /// Fixed delay between replayed actions.
    #[arg(long, default_value_t = 0)]
    pub delay_ms: u64,
    /// Stop at the first unsupported or failed action.
    #[arg(long)]
    pub stop_on_error: bool,
}

#[derive(Args, Clone)]
pub struct StudioWaitArgs {
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
    /// Stop waiting after this many milliseconds.
    #[arg(long, default_value_t = 10000)]
    pub timeout_ms: u64,
    /// Poll interval while waiting.
    #[arg(long, default_value_t = 100)]
    pub poll_ms: u64,
    /// App package used for the final snapshot.
    #[arg(long)]
    pub app: Option<String>,
    /// Include expanded top-frame variables in the final snapshot.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
}

#[derive(Args, Clone)]
pub struct StepUntilLogArgs {
    /// Substring that must appear in a logcat line.
    #[arg(long)]
    pub pattern: String,
    #[command(flatten)]
    pub wait: StudioWaitArgs,
}

#[derive(Args, Clone)]
pub struct RunUntilCrashArgs {
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
    /// Stop waiting after this many milliseconds.
    #[arg(long, default_value_t = 30000)]
    pub timeout_ms: u64,
    /// App package used for the final snapshot.
    #[arg(long)]
    pub app: Option<String>,
    /// Include expanded top-frame variables in the final snapshot.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
}

impl DebugArgs {
    pub fn is_host_only(&self) -> bool {
        matches!(self.cmd, DebugCmd::Studio(_))
    }
}

pub async fn run_host_only(args: &DebugArgs) -> Result<()> {
    match &args.cmd {
        DebugCmd::Studio(cmd) => debugger::run(cmd, args.studio_url.as_deref()).await,
        _ => anyhow::bail!("debug command requires an Android device connection"),
    }
}

pub async fn run(serial: &str, client: &ServerClient, args: DebugArgs) -> Result<()> {
    let studio_url = args.studio_url;
    match args.cmd {
        DebugCmd::Snapshot(args) => snapshot_cmd(serial, client, args, studio_url.as_deref()).await,
        DebugCmd::Record(args) => record_cmd(serial, client, args, studio_url.as_deref()).await,
        DebugCmd::Replay(args) => replay_cmd(client, args).await,
        DebugCmd::Studio(cmd) => debugger::run(&cmd, studio_url.as_deref()).await,
        DebugCmd::StepUntilScreenChange(args) => {
            step_until_screen_change(serial, client, args, studio_url.as_deref()).await
        }
        DebugCmd::StepUntilLog(args) => {
            step_until_log(serial, client, args, studio_url.as_deref()).await
        }
        DebugCmd::RunUntilCrash(args) => {
            run_until_crash(serial, client, args, studio_url.as_deref()).await
        }
    }
}

async fn snapshot_cmd(
    serial: &str,
    client: &ServerClient,
    args: SnapshotArgs,
    studio_url: Option<&str>,
) -> Result<()> {
    let value = snapshot_value(serial, client, &args, studio_url).await?;
    if let Some(path) = args.out {
        write_json_file(&path, &value)?;
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(())
}

async fn snapshot_value(
    serial: &str,
    client: &ServerClient,
    args: &SnapshotArgs,
    studio_url: Option<&str>,
) -> Result<Value> {
    let state = client.state().await.context("reading server state")?;
    let screen = client.screen().await.context("reading screen tree")?;
    let foreground_activity = adb::foreground_activity(serial).await;
    let device_info = match client.device().await {
        Ok(info) => serde_json::to_value(info).unwrap_or_else(|_| json!({})),
        Err(_) => adb::device_info(serial).await,
    };
    let screenshot = if args.no_screenshot {
        None
    } else {
        Some(write_screenshot(client, args.screenshot_dir.as_deref(), "snapshot").await?)
    };
    let logs = if args.logs == 0 {
        Vec::new()
    } else {
        adb::recent_logcat(serial, args.logs).await
    };
    let debugger = debugger_snapshot(studio_url, args.depth).await;

    Ok(json!({
        "type": "debug_snapshot",
        "schema_version": 1,
        "ts": now_ms(),
        "device": {
            "serial": serial,
            "info": device_info,
        },
        "app": {
            "requested": args.app.clone(),
            "foreground_activity": foreground_activity,
            "server_current": state.current_app,
            "screen_current": screen.current_app,
        },
        "server": {
            "version": state.server_version,
            "api_version": state.api_version,
            "ui_automator_version": state.ui_automator_version,
            "android_sdk": state.android_sdk,
            "android_release": state.android_release,
            "viewport": state.viewport,
        },
        "screen": screen,
        "screenshot": screenshot,
        "debugger": debugger,
        "logs": {
            "format": "threadtime",
            "lines": logs,
        },
    }))
}

async fn debugger_snapshot(studio_url: Option<&str>, depth: u32) -> Value {
    let bridge = match BridgeClient::new(studio_url) {
        Ok(bridge) => bridge,
        Err(err) => return json!({"available": false, "error": err.to_string()}),
    };
    let depth_s = depth.to_string();
    let max_fields_s = "48".to_string();
    let max_array_items_s = "24".to_string();
    let status = match bridge.get(route::STATUS, &[]).await {
        Ok(value) => value,
        Err(err) => return json!({"available": false, "error": err.to_string()}),
    };
    let breakpoints = bridge
        .get(route::BREAKPOINTS, &[])
        .await
        .unwrap_or_else(|err| json!({"ok": false, "error": err.to_string()}));
    let stack = bridge
        .get(route::SESSION_STACK, &[(query::LIMIT, Some("24"))])
        .await
        .unwrap_or_else(|err| json!({"ok": false, "error": err.to_string()}));
    let variables = bridge
        .get(
            route::SESSION_VARIABLES,
            &[
                (query::DEPTH, Some(depth_s.as_str())),
                (query::MAX_FIELDS, Some(max_fields_s.as_str())),
                (query::MAX_ARRAY_ITEMS, Some(max_array_items_s.as_str())),
            ],
        )
        .await
        .unwrap_or_else(|err| json!({"ok": false, "error": err.to_string()}));
    let watches = bridge
        .get(
            route::WATCHES,
            &[
                (query::DEPTH, Some(depth_s.as_str())),
                (query::MAX_FIELDS, Some(max_fields_s.as_str())),
                (query::MAX_ARRAY_ITEMS, Some(max_array_items_s.as_str())),
            ],
        )
        .await
        .unwrap_or_else(|err| json!({"ok": false, "error": err.to_string()}));
    json!({
        "available": true,
        "status": status,
        "breakpoints": breakpoints,
        "stack": stack,
        "variables": variables,
        "watches": watches,
    })
}

async fn record_cmd(
    serial: &str,
    client: &ServerClient,
    args: RecordArgs,
    studio_url: Option<&str>,
) -> Result<()> {
    if let Some(parent) = args.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&args.out)
        .with_context(|| format!("opening {}", args.out.display()))?;

    let screenshot_dir = args.screenshot_dir.clone().or_else(|| {
        args.out
            .parent()
            .map(|p| p.join("shadowdroid-record-screens"))
    });
    if let Some(dir) = &screenshot_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    write_event(
        &mut out,
        &json!({
            "type": "record_start",
            "schema_version": 1,
            "ts": now_ms(),
            "device": serial,
            "app": args.app.clone(),
        }),
    )?;

    let (log_tx, mut log_rx) = mpsc::channel(256);
    spawn_logcat(serial.to_string(), log_tx);

    let mut last_screen_hash: Option<String> = None;
    let mut last_app: Option<Value> = None;
    let mut last_debugger_hash: Option<String> = None;
    let mut last_debugger_suspended: Option<bool> = None;
    let mut last_variables: Option<BTreeMap<String, Value>> = None;
    let started = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_millis(args.poll_ms.max(50)));
    let mut stop = Box::pin(tokio::signal::ctrl_c());

    loop {
        if let Some(duration_ms) = args.duration_ms {
            if started.elapsed() >= Duration::from_millis(duration_ms) {
                break;
            }
        }
        tokio::select! {
            _ = &mut stop => break,
            Some(event) = log_rx.recv() => write_event(&mut out, &event)?,
            _ = ticker.tick() => {
                let screen = client.screen().await.context("record screen")?;
                let app_value = serde_json::to_value(&screen.current_app).unwrap_or_else(|_| json!(null));
                if last_app.as_ref() != Some(&app_value) {
                    write_event(&mut out, &json!({
                        "type": "app_lifecycle",
                        "ts": now_ms(),
                        "current_app": screen.current_app,
                    }))?;
                    last_app = Some(app_value);
                }
                if last_screen_hash.as_deref() != Some(screen.screen_hash.as_str()) {
                    let screenshot = if args.no_screenshots {
                        None
                    } else {
                        Some(write_screenshot(client, screenshot_dir.as_deref(), "record").await?)
                    };
                    write_event(&mut out, &json!({
                        "type": "screen",
                        "ts": now_ms(),
                        "screen_hash": screen.screen_hash,
                        "element_count": screen.element_count,
                        "current_app": screen.current_app,
                        "viewport": screen.viewport,
                        "elements": screen.elements,
                        "screenshot": screenshot,
                    }))?;
                    last_screen_hash = Some(screen.screen_hash);
                }

                let snap_args = SnapshotArgs {
                    app: args.app.clone(),
                    out: None,
                    screenshot_dir: screenshot_dir.clone(),
                    no_screenshot: true,
                    logs: 0,
                    depth: args.depth,
                };
                let debugger = debugger_snapshot(studio_url, snap_args.depth).await;
                let suspended = debugger_suspended(&debugger);
                if suspended == Some(true) && last_debugger_suspended != Some(true) {
                    write_event(&mut out, &json!({
                        "type": "debugger_stop",
                        "ts": now_ms(),
                        "debugger": debugger.clone(),
                    }))?;
                }
                last_debugger_suspended = suspended;

                if let Some(current_variables) = variable_map(&debugger) {
                    if let Some(previous_variables) = &last_variables {
                        let diff = variable_diff(previous_variables, &current_variables);
                        if diff.get("changed").and_then(Value::as_array).map(|v| !v.is_empty()).unwrap_or(false)
                            || diff.get("added").and_then(Value::as_array).map(|v| !v.is_empty()).unwrap_or(false)
                            || diff.get("removed").and_then(Value::as_array).map(|v| !v.is_empty()).unwrap_or(false)
                        {
                            write_event(&mut out, &json!({
                                "type": "variable_diff",
                                "ts": now_ms(),
                                "diff": diff,
                            }))?;
                        }
                    }
                    last_variables = Some(current_variables);
                }

                let debugger_hash = stable_hash(&debugger);
                if last_debugger_hash.as_deref() != Some(debugger_hash.as_str()) {
                    write_event(&mut out, &json!({
                        "type": "debugger_snapshot",
                        "ts": now_ms(),
                        "hash": debugger_hash,
                        "debugger": debugger,
                    }))?;
                    last_debugger_hash = Some(debugger_hash);
                }
            }
        }
    }

    write_event(
        &mut out,
        &json!({
            "type": "record_stop",
            "ts": now_ms(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    )?;
    eprintln!("recorded {}", args.out.display());
    Ok(())
}

fn spawn_logcat(serial: String, out: mpsc::Sender<Value>) {
    tokio::spawn(async move {
        let child = Command::new("adb")
            .args(["-s", &serial, "logcat", "-v", "threadtime", "-T", "1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = child else {
            let _ = out
                .send(json!({"type":"error","stage":"logcat","ts":now_ms(),"msg":"failed to start adb logcat"}))
                .await;
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = out
                .send(json!({"type":"logcat","ts":now_ms(),"format":"threadtime","line":line}))
                .await;
        }
    });
}

fn debugger_suspended(debugger: &Value) -> Option<bool> {
    debugger
        .get("status")
        .and_then(|status| status.get("sessions"))
        .and_then(Value::as_array)
        .and_then(|sessions| sessions.first())
        .and_then(|session| session.get("suspended"))
        .and_then(Value::as_bool)
}

fn variable_map(debugger: &Value) -> Option<BTreeMap<String, Value>> {
    let variables = debugger.get("variables")?.get("variables")?.as_array()?;
    let mut out = BTreeMap::new();
    for variable in variables {
        let Some(name) = variable.get("name").and_then(Value::as_str) else {
            continue;
        };
        out.insert(name.to_string(), variable.clone());
    }
    Some(out)
}

fn variable_diff(before: &BTreeMap<String, Value>, after: &BTreeMap<String, Value>) -> Value {
    let before_keys = before.keys().cloned().collect::<BTreeSet<_>>();
    let after_keys = after.keys().cloned().collect::<BTreeSet<_>>();
    let added = after_keys
        .difference(&before_keys)
        .filter_map(|key| {
            after
                .get(key)
                .map(|value| json!({"name": key, "value": value}))
        })
        .collect::<Vec<_>>();
    let removed = before_keys
        .difference(&after_keys)
        .filter_map(|key| {
            before
                .get(key)
                .map(|value| json!({"name": key, "value": value}))
        })
        .collect::<Vec<_>>();
    let changed = before_keys
        .intersection(&after_keys)
        .filter_map(|key| {
            let before_value = before.get(key)?;
            let after_value = after.get(key)?;
            (before_value != after_value)
                .then(|| json!({"name": key, "before": before_value, "after": after_value}))
        })
        .collect::<Vec<_>>();
    json!({
        "added": added,
        "removed": removed,
        "changed": changed,
    })
}

async fn replay_cmd(client: &ServerClient, args: ReplayArgs) -> Result<()> {
    let file =
        File::open(&args.file).with_context(|| format!("opening {}", args.file.display()))?;
    let reader = StdBufReader::new(file);
    let mut seen = 0u64;
    let mut replayed = 0u64;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        seen += 1;
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("parsing {} line {seen}", args.file.display()))?;
        if !is_replayable_action(&value) {
            continue;
        }
        replayed += 1;
        if args.dry_run {
            println!(
                "{}",
                serde_json::to_string(&json!({"type":"replay_plan","index":seen,"event":value}))?
            );
        } else {
            match perform_action(client, &value).await {
                Ok(()) => println!(
                    "{}",
                    serde_json::to_string(&json!({"type":"replay_action","index":seen,"ok":true}))?
                ),
                Err(err) => {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &json!({"type":"replay_action","index":seen,"ok":false,"error":err.to_string()})
                        )?
                    );
                    if args.stop_on_error {
                        return Err(err);
                    }
                }
            }
            if args.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(args.delay_ms)).await;
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string(&json!({"type":"replay_done","seen":seen,"replayable":replayed}))?
    );
    Ok(())
}

async fn step_until_screen_change(
    serial: &str,
    client: &ServerClient,
    args: StudioWaitArgs,
    studio_url: Option<&str>,
) -> Result<()> {
    let bridge = BridgeClient::new(studio_url)?;
    let session_s = args.session.map(|s| s.to_string());
    let initial = client.screen().await.context("reading initial screen")?;
    let initial_hash = initial.screen_hash.clone();
    let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
    let mut steps = 0u64;

    loop {
        if Instant::now() >= deadline {
            let snapshot =
                final_snapshot(serial, client, &args.app, studio_url, args.depth).await?;
            emit_json(&json!({
                "type": "step_until_screen_change",
                "ok": false,
                "timeout": true,
                "steps": steps,
                "initial_screen_hash": initial_hash,
                "snapshot": snapshot,
            }))?;
            return Ok(());
        }

        studio_control(&bridge, session_action::STEP_OVER, session_s.as_deref()).await?;
        steps += 1;
        tokio::time::sleep(Duration::from_millis(args.poll_ms.max(25))).await;
        let screen = client.screen().await.context("reading screen after step")?;
        if screen.screen_hash != initial_hash {
            let snapshot =
                final_snapshot(serial, client, &args.app, studio_url, args.depth).await?;
            emit_json(&json!({
                "type": "step_until_screen_change",
                "ok": true,
                "steps": steps,
                "initial_screen_hash": initial_hash,
                "screen_hash": screen.screen_hash,
                "snapshot": snapshot,
            }))?;
            return Ok(());
        }
    }
}

async fn step_until_log(
    serial: &str,
    client: &ServerClient,
    args: StepUntilLogArgs,
    studio_url: Option<&str>,
) -> Result<()> {
    let bridge = BridgeClient::new(studio_url)?;
    let session_s = args.wait.session.map(|s| s.to_string());
    let (log_tx, mut log_rx) = mpsc::channel(256);
    spawn_logcat(serial.to_string(), log_tx);
    let deadline = Instant::now() + Duration::from_millis(args.wait.timeout_ms);
    let mut steps = 0u64;

    loop {
        if Instant::now() >= deadline {
            let snapshot =
                final_snapshot(serial, client, &args.wait.app, studio_url, args.wait.depth).await?;
            emit_json(&json!({
                "type": "step_until_log",
                "ok": false,
                "timeout": true,
                "pattern": args.pattern,
                "steps": steps,
                "snapshot": snapshot,
            }))?;
            return Ok(());
        }

        studio_control(&bridge, session_action::STEP_OVER, session_s.as_deref()).await?;
        steps += 1;
        let step_deadline = Instant::now() + Duration::from_millis(args.wait.poll_ms.max(25));
        while Instant::now() < step_deadline {
            match tokio::time::timeout(Duration::from_millis(25), log_rx.recv()).await {
                Ok(Some(event)) => {
                    let line = event
                        .get("line")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if line.contains(&args.pattern) {
                        let snapshot = final_snapshot(
                            serial,
                            client,
                            &args.wait.app,
                            studio_url,
                            args.wait.depth,
                        )
                        .await?;
                        emit_json(&json!({
                            "type": "step_until_log",
                            "ok": true,
                            "pattern": args.pattern,
                            "steps": steps,
                            "logcat": event,
                            "snapshot": snapshot,
                        }))?;
                        return Ok(());
                    }
                }
                Ok(None) | Err(_) => {}
            }
        }
    }
}

async fn run_until_crash(
    serial: &str,
    client: &ServerClient,
    args: RunUntilCrashArgs,
    studio_url: Option<&str>,
) -> Result<()> {
    let bridge = BridgeClient::new(studio_url)?;
    let session_s = args.session.map(|s| s.to_string());
    let (log_tx, mut log_rx) = mpsc::channel(256);
    spawn_logcat(serial.to_string(), log_tx);
    let _ = studio_control(&bridge, session_action::RESUME, session_s.as_deref()).await;
    let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

    loop {
        if Instant::now() >= deadline {
            let snapshot =
                final_snapshot(serial, client, &args.app, studio_url, args.depth).await?;
            emit_json(&json!({
                "type": "run_until_crash",
                "ok": false,
                "timeout": true,
                "snapshot": snapshot,
            }))?;
            return Ok(());
        }

        match tokio::time::timeout(Duration::from_millis(100), log_rx.recv()).await {
            Ok(Some(event)) => {
                let line = event
                    .get("line")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if is_crash_line(line) {
                    let snapshot =
                        final_snapshot(serial, client, &args.app, studio_url, args.depth).await?;
                    emit_json(&json!({
                        "type": "run_until_crash",
                        "ok": true,
                        "logcat": event,
                        "snapshot": snapshot,
                    }))?;
                    return Ok(());
                }
            }
            Ok(None) | Err(_) => {}
        }
    }
}

async fn final_snapshot(
    serial: &str,
    client: &ServerClient,
    app: &Option<String>,
    studio_url: Option<&str>,
    depth: u32,
) -> Result<Value> {
    snapshot_value(
        serial,
        client,
        &SnapshotArgs {
            app: app.clone(),
            out: None,
            screenshot_dir: None,
            no_screenshot: false,
            logs: 120,
            depth,
        },
        studio_url,
    )
    .await
}

async fn studio_control(
    bridge: &BridgeClient,
    action: &str,
    session: Option<&str>,
) -> Result<Value> {
    bridge
        .get(
            route::SESSION_CONTROL,
            &[(query::ACTION, Some(action)), (query::SESSION, session)],
        )
        .await
}

fn is_crash_line(line: &str) -> bool {
    line.contains("FATAL EXCEPTION") || line.contains("Fatal signal") || line.contains(" ANR in ")
}

fn is_replayable_action(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("action")
        || value.get("type").and_then(Value::as_str) == Some("replay_action")
}

async fn perform_action(client: &ServerClient, value: &Value) -> Result<()> {
    let cmd = value.get("cmd").and_then(Value::as_str).unwrap_or_default();
    match cmd {
        "tap" => {
            let x = int_field(value, "x")?;
            let y = int_field(value, "y")?;
            client.tap_xy(x, y).await
        }
        "text" => {
            let text = value
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let clear = value.get("clear").and_then(Value::as_bool).unwrap_or(false);
            client.text(text, clear).await
        }
        "key" => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            client.key(name).await
        }
        "swipe" | "drag" => {
            let from = value.get("from").and_then(Value::as_array);
            let to = value.get("to").and_then(Value::as_array);
            let (Some(from), Some(to)) = (from, to) else {
                anyhow::bail!("{cmd} event needs from/to arrays");
            };
            let x1 = from.first().and_then(Value::as_i64).unwrap_or(0) as i32;
            let y1 = from.get(1).and_then(Value::as_i64).unwrap_or(0) as i32;
            let x2 = to.first().and_then(Value::as_i64).unwrap_or(0) as i32;
            let y2 = to.get(1).and_then(Value::as_i64).unwrap_or(0) as i32;
            let duration_ms = value
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(200) as u32;
            if cmd == "drag" {
                client.drag(x1, y1, x2, y2, duration_ms).await
            } else {
                client.swipe(x1, y1, x2, y2, duration_ms).await
            }
        }
        "app_start" => {
            let package = value
                .get("package")
                .and_then(Value::as_str)
                .context("app_start event needs package")?;
            client.app_start(package).await
        }
        other => anyhow::bail!("unsupported replay action: {other}"),
    }
}

fn int_field(value: &Value, key: &str) -> Result<i32> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .map(|v| v as i32)
        .with_context(|| format!("event needs integer {key}"))
}

fn emit_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

async fn write_screenshot(
    client: &ServerClient,
    dir: Option<&Path>,
    prefix: &str,
) -> Result<Value> {
    let bytes = client
        .screenshot_png()
        .await
        .context("capturing screenshot")?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let base = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("shadowdroid-debug"));
    std::fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let path = base.join(format!("{prefix}-{}-{}.png", now_ms(), &hash[..12]));
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": bytes.len() as u64,
        "hash": hash,
        "hash_algorithm": "blake3",
    }))
}

fn write_event(out: &mut File, value: &Value) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string(value)?)?;
    out.flush()?;
    Ok(())
}

fn write_json_file(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("writing {}", path.display()))
}

fn stable_hash(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_variables_by_name() {
        let before = BTreeMap::from([
            ("same".to_string(), json!({"name":"same","value":"1"})),
            ("old".to_string(), json!({"name":"old","value":"gone"})),
            ("changed".to_string(), json!({"name":"changed","value":"1"})),
        ]);
        let after = BTreeMap::from([
            ("same".to_string(), json!({"name":"same","value":"1"})),
            ("new".to_string(), json!({"name":"new","value":"hi"})),
            ("changed".to_string(), json!({"name":"changed","value":"2"})),
        ]);

        let diff = variable_diff(&before, &after);
        assert_eq!(diff["added"].as_array().unwrap().len(), 1);
        assert_eq!(diff["removed"].as_array().unwrap().len(), 1);
        assert_eq!(diff["changed"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn detects_crash_logcat_lines() {
        assert!(is_crash_line("AndroidRuntime: FATAL EXCEPTION: main"));
        assert!(is_crash_line("libc: Fatal signal 11 (SIGSEGV)"));
        assert!(is_crash_line("ActivityManager: ANR in com.example"));
        assert!(!is_crash_line(
            "I ActivityTaskManager: Displayed com.example/.Main"
        ));
    }
}
