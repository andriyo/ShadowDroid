//! Agent-first debugging orchestration.
//!
//! Studio-backed debugger commands are the thin Android Studio bridge. The
//! snapshot/timeline commands compose the device server, adb, screenshots,
//! logcat, and optional Studio debugger state into deterministic artifacts an
//! agent can consume or replay.

use crate::cmd::debugger::{self, BridgeClient, DebugMode, DebuggerCmd};
use crate::cmd::studio;
use crate::cmd::studio_contract::{query, route, session_action};
use crate::config::{ResolvedApp, ShadowDroidConfig};
use crate::device::adb;
use crate::device::client::ServerClient;
use crate::events::CrashEvent;
use crate::watch::logcat;
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
    /// Start an app, attach Studio debugger when available, and return a snapshot.
    Auto(AutoArgs),
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
    /// Native/mixed-mode readiness and artifact helpers.
    #[command(subcommand)]
    Native(NativeCmd),
    /// List or pull native tombstone files from the device.
    #[command(subcommand)]
    Tombstones(TombstonesCmd),
}

#[derive(Args, Clone)]
pub struct AutoArgs {
    /// App alias, package, or installed app name. Defaults to config, then foreground app.
    pub target: Option<String>,
    /// App alias or installed app name. Useful when the target would be parsed as an option.
    #[arg(long)]
    pub app: Option<String>,
    /// Exact package/process name. Overrides target and --app.
    #[arg(long)]
    pub package: Option<String>,
    /// Project name or absolute project path when multiple projects are open.
    #[arg(long)]
    pub project: Option<String>,
    /// Android debugger id/display name.
    #[arg(long)]
    pub debugger: Option<String>,
    /// Semantic debugger mode. Use --debugger for an exact Studio debugger id/name.
    #[arg(long, value_enum)]
    pub mode: Option<DebugMode>,
    /// Android Studio run configuration whose debugger settings should be reused.
    #[arg(long)]
    pub configuration: Option<String>,
    /// Do not launch the app before attaching/snapshotting.
    #[arg(long)]
    pub no_start: bool,
    /// Do not attach Android Studio's debugger; only resolve, launch, and snapshot.
    #[arg(long)]
    pub no_attach: bool,
    /// App foreground wait timeout after launch.
    #[arg(long, default_value_t = 20000)]
    pub timeout_ms: u32,
    /// Number of recent logcat lines to include.
    #[arg(long, default_value_t = 200)]
    pub logs: u32,
    /// Include expanded top-frame variables when the debugger is suspended.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
    /// Directory for screenshot artifacts.
    #[arg(long)]
    pub screenshot_dir: Option<PathBuf>,
    /// Skip screenshot capture.
    #[arg(long)]
    pub no_screenshot: bool,
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
    /// Number of recent logcat lines to include in the final snapshot/bundle.
    #[arg(long, default_value_t = 120)]
    pub logs: u32,
    /// Include expanded top-frame variables in the final snapshot.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
    /// Write the result JSON to a file instead of stdout.
    #[arg(short = 'o', long)]
    pub out: Option<PathBuf>,
    /// Also write a local crash bundle to this directory.
    #[arg(long)]
    pub bundle: Option<PathBuf>,
    /// Collect best-effort ANR/tombstone artifacts when available.
    #[arg(long)]
    pub native_artifacts: bool,
}

#[derive(Subcommand, Clone)]
pub enum NativeCmd {
    /// Report native/mixed-mode debugger readiness for an app/process.
    Status(NativeStatusArgs),
}

#[derive(Args, Clone)]
pub struct NativeStatusArgs {
    /// App alias, package, or installed app name. Defaults to config, then foreground app.
    pub target: Option<String>,
    /// App alias or installed app name.
    #[arg(long)]
    pub app: Option<String>,
    /// Exact package/process name. Overrides target and --app.
    #[arg(long)]
    pub package: Option<String>,
    /// Project name or absolute project path when multiple projects are open.
    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Subcommand, Clone)]
pub enum TombstonesCmd {
    /// List recent native tombstone files visible through adb.
    List(TombstoneListArgs),
    /// Pull recent native tombstone files into a local directory.
    Pull(TombstonePullArgs),
}

#[derive(Args, Clone)]
pub struct TombstoneListArgs {
    /// App alias/package label for output context.
    #[arg(long)]
    pub app: Option<String>,
}

#[derive(Args, Clone)]
pub struct TombstonePullArgs {
    /// Output directory.
    #[arg(short = 'o', long)]
    pub out: PathBuf,
    /// App alias/package label for output context.
    #[arg(long)]
    pub app: Option<String>,
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
        DebugCmd::Auto(args) => debug_auto(serial, client, args, studio_url.as_deref()).await,
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
        DebugCmd::Native(cmd) => native_cmd(serial, client, cmd, studio_url.as_deref()).await,
        DebugCmd::Tombstones(cmd) => tombstones_cmd(serial, cmd).await,
    }
}

async fn debug_auto(
    serial: &str,
    client: &ServerClient,
    args: AutoArgs,
    studio_url: Option<&str>,
) -> Result<()> {
    let config = ShadowDroidConfig::load()?;
    let requested = args
        .package
        .as_deref()
        .or(args.app.as_deref())
        .or(args.target.as_deref());
    let (resolved, app_label) = resolve_auto_app(serial, client, &config, requested).await?;
    let package = resolved.package.clone();
    let mut steps = Vec::new();
    let mut ok = package.is_some();

    steps.push(json!({
        "step": "resolve_app",
        "ok": package.is_some(),
        "requested": requested,
        "resolved": resolved,
        "label": app_label,
    }));

    if let Some(package) = &package {
        if args.no_start {
            steps.push(json!({
                "step": "app_start",
                "skipped": true,
                "reason": "--no-start",
                "package": package,
            }));
        } else {
            match client.app_start(package).await {
                Ok(()) => steps.push(json!({
                    "step": "app_start",
                    "ok": true,
                    "package": package,
                })),
                Err(err) => {
                    ok = false;
                    steps.push(json!({
                        "step": "app_start",
                        "ok": false,
                        "package": package,
                        "error": err.to_string(),
                    }));
                }
            }

            match client.app_wait(package, args.timeout_ms, true).await {
                Ok(wait) => {
                    ok &= wait.matched;
                    steps.push(json!({
                        "step": "app_wait",
                        "ok": wait.matched,
                        "package": package,
                        "timeout_ms": args.timeout_ms,
                        "current": wait.current,
                    }));
                }
                Err(err) => {
                    ok = false;
                    steps.push(json!({
                        "step": "app_wait",
                        "ok": false,
                        "package": package,
                        "timeout_ms": args.timeout_ms,
                        "error": err.to_string(),
                    }));
                }
            }
        }
    }

    let attach = if args.no_attach {
        json!({
            "skipped": true,
            "reason": "--no-attach",
        })
    } else if let Some(package) = &package {
        let value = auto_attach_debugger(serial, package, &resolved, &args, studio_url).await;
        ok &= value.get("ok").and_then(Value::as_bool).unwrap_or(false);
        value
    } else {
        json!({
            "ok": false,
            "skipped": true,
            "reason": "no package resolved",
            "next_command": "shadowdroid debug auto --app <app alias or package>",
        })
    };

    let snapshot = snapshot_value(
        serial,
        client,
        &SnapshotArgs {
            app: package.clone().or_else(|| requested.map(str::to_string)),
            out: None,
            screenshot_dir: args.screenshot_dir.clone(),
            no_screenshot: args.no_screenshot,
            logs: args.logs,
            depth: args.depth,
        },
        studio_url,
    )
    .await?;

    emit_json(&json!({
        "type": "debug_auto",
        "schema_version": 1,
        "ok": ok,
        "device": serial,
        "app": {
            "requested": requested,
            "package": package,
            "label": app_label,
            "resolution": resolved,
        },
        "steps": steps,
        "attach": attach,
        "snapshot": snapshot,
    }))
}

async fn resolve_auto_app(
    serial: &str,
    client: &ServerClient,
    config: &ShadowDroidConfig,
    requested: Option<&str>,
) -> Result<(ResolvedApp, Option<String>)> {
    let mut resolved = config.resolve_app(Some(serial), requested).await?;
    let mut app_label = None;

    if resolved.package.is_none() {
        if let Some(input) = resolved.input.clone() {
            if let Some((package, label)) = resolve_app_by_label(serial, client, &input).await? {
                resolved.package = Some(package);
                resolved.source = "installed_app_label_match".into();
                app_label = Some(label);
            }
        }
    }

    if resolved.package.is_none() && resolved.input.is_none() {
        if let Ok(current) = client.app_current().await {
            if let Some(package) = current.package {
                app_label = client.app_info(&package).await.ok().map(|info| info.label);
                resolved.package = Some(package);
                resolved.source = "foreground_app".into();
            }
        }
    }

    Ok((resolved, app_label))
}

async fn resolve_app_by_label(
    serial: &str,
    client: &ServerClient,
    requested: &str,
) -> Result<Option<(String, String)>> {
    let needle = lookup_key(requested);
    if needle.is_empty() {
        return Ok(None);
    }
    let packages = adb::list_packages(serial).await?;
    let mut matches = Vec::new();
    for package in packages {
        let Ok(info) = client.app_info(&package).await else {
            continue;
        };
        let label_key = lookup_key(&info.label);
        if label_key == needle || label_key.contains(&needle) {
            matches.push((package, info.label));
        }
    }

    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        many => anyhow::bail!(
            "app name `{}` matched multiple installed labels: {}. Add an alias to .shadowdroid.json.",
            requested,
            many.iter()
                .map(|(package, label)| format!("{label} ({package})"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

async fn auto_attach_debugger(
    serial: &str,
    package: &str,
    resolved: &ResolvedApp,
    args: &AutoArgs,
    studio_url: Option<&str>,
) -> Value {
    let bridge = match BridgeClient::new(studio_url) {
        Ok(bridge) => bridge,
        Err(err) => return studio_problem_value("resolve_bridge", err),
    };
    let project = args.project.as_deref().or(resolved.project.as_deref());
    let debugger = args.debugger.as_deref().or(resolved.debugger.as_deref());
    let mode = args.mode.map(DebugMode::as_str);
    let configuration = args
        .configuration
        .as_deref()
        .or(resolved.run_configuration.as_deref());
    match bridge
        .get(
            route::ATTACH,
            &[
                (query::PACKAGE, Some(package)),
                (query::DEVICE, Some(serial)),
                (query::PROJECT, project),
                (query::DEBUGGER, debugger),
                (query::MODE, mode),
                (query::CONFIGURATION, configuration),
                (query::DIALOG, Some("false")),
            ],
        )
        .await
    {
        Ok(response) => {
            let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(true);
            json!({
                "ok": ok,
                "package": package,
                "project": project,
                "debugger": debugger,
                "mode": mode,
                "configuration": configuration,
                "response": response,
            })
        }
        Err(err) => studio_problem_value("attach", err),
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
        Err(err) => return studio_problem_value("resolve_bridge", err),
    };
    let depth_s = depth.to_string();
    let max_fields_s = "48".to_string();
    let max_array_items_s = "24".to_string();
    let status = match bridge.get(route::STATUS, &[]).await {
        Ok(value) => value,
        Err(err) => return studio_problem_value("status", err),
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
    let coroutine_depth_s = depth.to_string();
    let coroutines = bridge
        .get(
            route::SESSION_COROUTINES,
            &[
                (query::LIMIT, Some("32")),
                (query::DEPTH, Some(coroutine_depth_s.as_str())),
                (query::TIMEOUT_MS, Some("2500")),
            ],
        )
        .await
        .unwrap_or_else(|err| json!({"ok": false, "available": false, "error": err.to_string()}));
    json!({
        "available": true,
        "status": status,
        "breakpoints": breakpoints,
        "stack": stack,
        "variables": variables,
        "watches": watches,
        "coroutines": coroutines,
    })
}

fn studio_problem_value(stage: &str, err: anyhow::Error) -> Value {
    json!({
        "available": false,
        "ok": false,
        "type": "studio_debugger_unavailable",
        "stage": stage,
        "error": err.to_string(),
        "next_command": "shadowdroid doctor",
        "setup_command": "shadowdroid init",
        "studio": studio::status_report(None).ok(),
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
                final_snapshot(serial, client, &args.app, studio_url, args.depth, 120).await?;
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
                final_snapshot(serial, client, &args.app, studio_url, args.depth, 120).await?;
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
            let snapshot = final_snapshot(
                serial,
                client,
                &args.wait.app,
                studio_url,
                args.wait.depth,
                120,
            )
            .await?;
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
                            120,
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
    let started = Instant::now();
    let (bridge, bridge_error) = match BridgeClient::new(studio_url) {
        Ok(bridge) => (Some(bridge), None),
        Err(err) => (None, Some(err.to_string())),
    };
    let session_s = args.session.map(|s| s.to_string());
    let (crash_tx, mut crash_rx) = mpsc::channel(32);
    spawn_crash_logcat(serial.to_string(), args.app.clone(), crash_tx);
    let resume = if let Some(bridge) = &bridge {
        match studio_control(bridge, session_action::RESUME, session_s.as_deref()).await {
            Ok(value) => json!({"attempted": true, "ok": true, "result": value}),
            Err(err) => json!({"attempted": true, "ok": false, "error": err.to_string()}),
        }
    } else {
        json!({"attempted": false, "ok": false, "error": bridge_error})
    };
    let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

    loop {
        if Instant::now() >= deadline {
            let (snapshot, snapshot_error) = final_snapshot_best_effort(
                serial, client, &args.app, studio_url, args.depth, args.logs,
            )
            .await;
            let result = json!({
                "type": "run_until_crash",
                "schema_version": 1,
                "ok": false,
                "timeout": true,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "studio": {
                    "resume": resume,
                },
                "snapshot": snapshot,
            });
            let mut result = result;
            if let Some(error) = snapshot_error {
                result["snapshot_error"] = json!(error);
            }
            emit_or_write_json(args.out.as_deref(), &result)?;
            return Ok(());
        }

        match tokio::time::timeout(Duration::from_millis(100), crash_rx.recv()).await {
            Ok(Some(crash)) => {
                let (snapshot, snapshot_error) = final_snapshot_best_effort(
                    serial, client, &args.app, studio_url, args.depth, args.logs,
                )
                .await;
                let correlation = crash_correlation(&crash, &snapshot);
                let bundle = if args.bundle.is_some() {
                    Some(
                        write_crash_bundle(
                            serial,
                            &args.app,
                            &crash,
                            &snapshot,
                            args.bundle.as_deref(),
                            args.native_artifacts,
                            args.logs.max(200),
                        )
                        .await,
                    )
                } else {
                    None
                };
                let mut result = json!({
                    "type": "run_until_crash",
                    "schema_version": 1,
                    "ok": true,
                    "timeout": false,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "app": {
                        "requested": args.app.clone(),
                        "package": crash.package.clone(),
                    },
                    "studio": {
                        "resume": resume,
                    },
                    "crash": crash.clone(),
                    "correlation": correlation,
                    "snapshot": snapshot,
                });
                if let Some(error) = snapshot_error {
                    result["snapshot_error"] = json!(error);
                }
                if let Some(bundle) = bundle {
                    result["bundle"] = bundle;
                }
                emit_or_write_json(args.out.as_deref(), &result)?;
                return Ok(());
            }
            Ok(None) | Err(_) => {}
        }
    }
}

async fn final_snapshot_best_effort(
    serial: &str,
    client: &ServerClient,
    app: &Option<String>,
    studio_url: Option<&str>,
    depth: u32,
    logs: u32,
) -> (Value, Option<String>) {
    match final_snapshot(serial, client, app, studio_url, depth, logs).await {
        Ok(snapshot) => (snapshot, None),
        Err(err) => {
            let error = err.to_string();
            (
                json!({
                    "type": "debug_snapshot",
                    "schema_version": 1,
                    "ok": false,
                    "error": error,
                    "device": {
                        "serial": serial,
                    },
                    "app": {
                        "requested": app.clone(),
                    },
                }),
                Some(error),
            )
        }
    }
}

async fn native_cmd(
    serial: &str,
    client: &ServerClient,
    cmd: NativeCmd,
    studio_url: Option<&str>,
) -> Result<()> {
    match cmd {
        NativeCmd::Status(args) => native_status(serial, client, args, studio_url).await,
    }
}

async fn native_status(
    serial: &str,
    client: &ServerClient,
    args: NativeStatusArgs,
    studio_url: Option<&str>,
) -> Result<()> {
    let config = ShadowDroidConfig::load()?;
    let requested = args
        .package
        .as_deref()
        .or(args.app.as_deref())
        .or(args.target.as_deref());
    let (resolved, label) = resolve_auto_app(serial, client, &config, requested).await?;
    let package = args.package.clone().or_else(|| resolved.package.clone());
    let project = args.project.as_deref().or(resolved.project.as_deref());
    let requested_value = requested.map(str::to_string);
    let project_value = project.map(str::to_string);
    let studio = match BridgeClient::new(studio_url) {
        Ok(bridge) => {
            let clients = bridge
                .get(
                    route::CLIENTS,
                    &[
                        (query::PACKAGE, package.as_deref()),
                        (query::DEVICE, Some(serial)),
                        (query::PROJECT, project),
                    ],
                )
                .await
                .unwrap_or_else(|err| json!({"ok": false, "error": err.to_string()}));
            let sessions = bridge
                .get(route::SESSIONS, &[])
                .await
                .unwrap_or_else(|err| json!({"ok": false, "error": err.to_string()}));
            json!({
                "available": true,
                "clients": clients,
                "sessions": sessions,
            })
        }
        Err(err) => studio_problem_value("resolve_bridge", err),
    };
    let tombstones = tombstone_status(serial).await;
    let native_debuggable = studio
        .get("clients")
        .and_then(|clients| clients.get("clients"))
        .and_then(Value::as_array)
        .map(|clients| {
            clients.iter().any(|client| {
                client.get("native_debuggable").and_then(Value::as_bool) == Some(true)
            })
        })
        .unwrap_or(false);
    emit_json(&json!({
        "type": "debug_native_status",
        "schema_version": 1,
        "device": serial,
        "app": {
            "requested": requested_value,
            "label": label,
            "resolved": resolved,
            "package": package,
        },
        "project": project_value,
        "native_debuggable": native_debuggable,
        "studio": studio,
        "artifacts": {
            "tombstones": tombstones,
        },
        "limits": {
            "lldb_control": false,
            "native_variables": false,
            "note": "native live control is not exposed until Android Studio LLDB APIs are proven stable",
        },
    }))
}

async fn tombstones_cmd(serial: &str, cmd: TombstonesCmd) -> Result<()> {
    match cmd {
        TombstonesCmd::List(args) => {
            let status = tombstone_status(serial).await;
            emit_json(&json!({
                "type": "debug_tombstones_list",
                "schema_version": 1,
                "device": serial,
                "app": args.app,
                "tombstones": status,
            }))
        }
        TombstonesCmd::Pull(args) => {
            let mut bundle = CrashBundle::new(args.out);
            collect_matching_device_files(
                serial,
                &mut bundle,
                "tombstone",
                "ls -1t /data/tombstones/tombstone_* 2>/dev/null | head -n 5",
            )
            .await;
            emit_json(&json!({
                "type": "debug_tombstones_pull",
                "schema_version": 1,
                "device": serial,
                "app": args.app,
                "bundle": bundle.summary(),
            }))
        }
    }
}

async fn tombstone_status(serial: &str) -> Value {
    match device_file_list(
        serial,
        "ls -1t /data/tombstones/tombstone_* 2>/dev/null | head -n 5",
    )
    .await
    {
        Ok(paths) => json!({
            "available": !paths.is_empty(),
            "paths": paths,
        }),
        Err(err) => json!({
            "available": false,
            "error": err.to_string(),
        }),
    }
}

async fn final_snapshot(
    serial: &str,
    client: &ServerClient,
    app: &Option<String>,
    studio_url: Option<&str>,
    depth: u32,
    logs: u32,
) -> Result<Value> {
    snapshot_value(
        serial,
        client,
        &SnapshotArgs {
            app: app.clone(),
            out: None,
            screenshot_dir: None,
            no_screenshot: false,
            logs,
            depth,
        },
        studio_url,
    )
    .await
}

async fn device_file_list(serial: &str, list_cmd: &str) -> Result<Vec<String>> {
    let out = adb::shell(serial, list_cmd).await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(20)
        .map(str::to_string)
        .collect())
}

fn spawn_crash_logcat(serial: String, app_filter: Option<String>, out: mpsc::Sender<CrashEvent>) {
    tokio::spawn(async move {
        let _ = logcat::run_crashes(serial, app_filter, out).await;
    });
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

fn crash_correlation(crash: &CrashEvent, snapshot: &Value) -> Value {
    let debugger = snapshot.get("debugger").unwrap_or(&Value::Null);
    let debugger_available = debugger
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let session_suspended = debugger
        .get("status")
        .and_then(|status| status.get("sessions"))
        .and_then(Value::as_array)
        .map(|sessions| {
            sessions
                .iter()
                .any(|session| session.get("suspended").and_then(Value::as_bool) == Some(true))
        })
        .unwrap_or(false);
    json!({
        "debugger_available": debugger_available,
        "session_suspended": session_suspended,
        "source_locations": crash_source_locations(crash),
    })
}

fn crash_source_locations(crash: &CrashEvent) -> Vec<Value> {
    crash
        .stack
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| java_stack_location(index, frame))
        .collect()
}

fn java_stack_location(index: usize, frame: &str) -> Option<Value> {
    let open = frame.rfind('(')?;
    let close = frame.rfind(')')?;
    if close <= open {
        return None;
    }
    let symbol = frame[..open].trim();
    let source = frame[open + 1..close].trim();
    if source.is_empty()
        || source == "Native Method"
        || source == "Unknown Source"
        || source.starts_with("SourceFile:")
    {
        return None;
    }
    let (file, line) = match source.rsplit_once(':') {
        Some((file, line)) => (file, line.parse::<u32>().ok()),
        None => (source, None),
    };
    let (class, method) = symbol
        .rsplit_once('.')
        .map(|(class, method)| (Some(class), Some(method)))
        .unwrap_or((None, Some(symbol)));
    Some(json!({
        "crash_frame": index,
        "class": class,
        "method": method,
        "file": file,
        "line": line,
    }))
}

struct CrashBundle {
    dir: PathBuf,
    captured: Vec<String>,
    errors: Vec<String>,
}

impl CrashBundle {
    fn new(dir: PathBuf) -> Self {
        let mut bundle = Self {
            dir,
            captured: Vec::new(),
            errors: Vec::new(),
        };
        if let Err(err) = std::fs::create_dir_all(&bundle.dir) {
            bundle
                .errors
                .push(format!("bundle_dir: create failed: {err}"));
        }
        bundle
    }

    fn write_text(&mut self, name: &str, content: &str) {
        match std::fs::write(self.dir.join(name), content) {
            Ok(()) => self.captured.push(name.to_string()),
            Err(err) => self.errors.push(format!("{name}: write failed: {err}")),
        }
    }

    fn write_bytes(&mut self, name: &str, content: &[u8]) {
        match std::fs::write(self.dir.join(name), content) {
            Ok(()) => self.captured.push(name.to_string()),
            Err(err) => self.errors.push(format!("{name}: write failed: {err}")),
        }
    }

    fn write_json(&mut self, name: &str, value: &Value) {
        match serde_json::to_vec_pretty(value) {
            Ok(bytes) => self.write_bytes(name, &bytes),
            Err(err) => self.errors.push(format!("{name}: serialize failed: {err}")),
        }
    }

    fn summary(&self) -> Value {
        json!({
            "path": self.dir.display().to_string(),
            "captured": self.captured,
            "errors": self.errors,
        })
    }
}

async fn write_crash_bundle(
    serial: &str,
    app: &Option<String>,
    crash: &CrashEvent,
    snapshot: &Value,
    out: Option<&Path>,
    native_artifacts: bool,
    log_lines: u32,
) -> Value {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("shadowdroid-crash-{ts}")));
    let mut bundle = CrashBundle::new(dir);
    let crash_value = serde_json::to_value(crash).unwrap_or(Value::Null);
    bundle.write_json("crash.json", &crash_value);
    bundle.write_json("snapshot.json", snapshot);
    bundle.write_json("device_info.json", &adb::device_info(serial).await);
    bundle.write_text(
        "logcat_main.txt",
        &adb::recent_logcat(serial, log_lines).await.join("\n"),
    );
    match adb::shell(serial, "logcat -d -b crash -v threadtime -t 300").await {
        Ok(out) if !out.trim().is_empty() => bundle.write_text("logcat_crash.txt", &out),
        Ok(_) => bundle
            .errors
            .push("logcat_crash.txt: crash buffer empty".to_string()),
        Err(err) => bundle.errors.push(format!("logcat_crash.txt: {err}")),
    }
    if native_artifacts || crash.kind == "native" {
        collect_matching_device_files(
            serial,
            &mut bundle,
            "tombstone",
            "ls -1t /data/tombstones/tombstone_* 2>/dev/null | head -n 5",
        )
        .await;
    }
    if native_artifacts || crash.kind == "anr" {
        collect_matching_device_files(
            serial,
            &mut bundle,
            "anr",
            "ls -1t /data/anr/* 2>/dev/null | head -n 5",
        )
        .await;
        match adb::shell(serial, "logcat -d -b events -v threadtime -t 300").await {
            Ok(out) if !out.trim().is_empty() => bundle.write_text("logcat_events.txt", &out),
            Ok(_) => bundle
                .errors
                .push("logcat_events.txt: events buffer empty".to_string()),
            Err(err) => bundle.errors.push(format!("logcat_events.txt: {err}")),
        }
    }
    let manifest = json!({
        "type": "crash_bundle",
        "schema_version": 1,
        "ts": ts,
        "device": serial,
        "app": {
            "requested": app,
            "package": crash.package.clone(),
        },
        "crash_kind": crash.kind.clone(),
        "captured": bundle.captured,
        "errors": bundle.errors,
    });
    bundle.write_json("collect.json", &manifest);
    bundle.summary()
}

async fn collect_matching_device_files(
    serial: &str,
    bundle: &mut CrashBundle,
    prefix: &str,
    list_cmd: &str,
) {
    match adb::shell(serial, list_cmd).await {
        Ok(out) => {
            let paths = out
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(5)
                .map(str::to_string)
                .collect::<Vec<_>>();
            if paths.is_empty() {
                bundle.errors.push(format!("{prefix}: no matching files"));
                return;
            }
            for remote in paths {
                let name = device_artifact_name(prefix, &remote);
                match adb::pull(serial, &remote).await {
                    Ok(bytes) => bundle.write_bytes(&name, &bytes),
                    Err(pull_err) => match adb::shell(serial, format!("cat {remote}")).await {
                        Ok(text) if !text.trim().is_empty() => bundle.write_text(&name, &text),
                        Ok(_) => bundle.errors.push(format!("{remote}: empty/unreadable")),
                        Err(cat_err) => bundle.errors.push(format!(
                            "{remote}: pull failed: {pull_err}; cat failed: {cat_err}"
                        )),
                    },
                }
            }
        }
        Err(err) => bundle.errors.push(format!("{prefix}: list failed: {err}")),
    }
}

fn device_artifact_name(prefix: &str, remote: &str) -> String {
    let basename = remote
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(prefix)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{prefix}_{basename}")
}

fn lookup_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
            client.key(name).await.map(|_| ())
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

fn emit_or_write_json(path: Option<&Path>, value: &Value) -> Result<()> {
    if let Some(path) = path {
        write_json_file(path, value)
    } else {
        emit_json(value)
    }
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
    fn parses_java_crash_source_locations() {
        let location =
            java_stack_location(2, "com.example.MainActivity.onCreate(MainActivity.kt:42)")
                .unwrap();
        assert_eq!(location["crash_frame"], 2);
        assert_eq!(location["class"], "com.example.MainActivity");
        assert_eq!(location["method"], "onCreate");
        assert_eq!(location["file"], "MainActivity.kt");
        assert_eq!(location["line"], 42);
        assert!(java_stack_location(0, "java.lang.Thread.sleep(Native Method)").is_none());
    }

    #[test]
    fn sanitizes_device_artifact_names() {
        assert_eq!(
            device_artifact_name("tombstone", "/data/tombstones/tombstone_01"),
            "tombstone_tombstone_01"
        );
        assert_eq!(
            device_artifact_name("anr", "/data/anr/traces 1.txt"),
            "anr_traces_1.txt"
        );
    }
}
