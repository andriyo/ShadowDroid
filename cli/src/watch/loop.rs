//! Steady-state watch loop. One emit per real screen change.
//!
//! Wake sources:
//!   - logcat tail (low-latency event signal on Window/Activity transitions,
//!     feeding a dedicated wake channel)
//!   - safety-net poll (default 1s) — catches in-screen mutations
//!   - command nudge (after every dispatched action, direct forced refresh)
//!
//! On wake:
//!   - sleep `debounce_ms` to coalesce a storm
//!   - drain remaining wakes
//!   - GET /v1/screen
//!   - hash compare → emit on change
//!   - run watcher rules → dispatch actions, emit `watcher_fired` events
//!   - update `last_hash`
//!
//! Stdin commands use a separate channel so a wake storm cannot force commands
//! to wait behind every queued wake.

use crate::device::client::ServerClient;
use crate::events::{self, Event, ScreenFormat, emit_action, now_ts};
use crate::ids::Serial;
use crate::proto::{AppRef, Element, SelectorQuery};
use crate::watch::watcher::{PermissionDialogPolicy, WatcherRule, WatcherSet};
use crate::watch::{logcat, stdin};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep};

#[derive(Debug, Clone, Copy)]
pub enum Wake {
    Event,
    Poll,
    Command,
    Init,
}

pub struct WatchConfig {
    pub serial: Serial,
    pub client: ServerClient,
    pub app_filter: Option<String>,
    pub poll_ms: u32,
    pub debounce_ms: u32,
    pub accept_stdin: bool,
    pub detect_crashes: bool,
    pub watcher_files: Vec<String>,
    pub permission_dialog_policy: PermissionDialogPolicy,
    pub screen_format: ScreenFormat,
    /// Interleave live `http` events from a running `net` proxy daemon.
    pub net: bool,
}

#[derive(Default)]
struct WatchState {
    last_hash: Option<String>,
}

pub async fn run(cfg: WatchConfig) -> Result<()> {
    let watchers = WatcherSet::from_files(&cfg.watcher_files)?;
    watchers.set_permission_dialog_policy(cfg.permission_dialog_policy);
    let (wake_tx, mut wake_rx) = mpsc::channel::<Wake>(128);
    let (command_tx, mut command_rx) = mpsc::channel::<String>(128);
    let (event_tx, event_rx) = mpsc::channel::<Event>(64);
    let stopping = Arc::new(AtomicBool::new(false));
    let state = cfg
        .client
        .state()
        .await
        .context("reading initial server state")?;
    events::emit(&Event::Ready {
        device: cfg.serial.to_string(),
        viewport: state.viewport,
        server_version: state.server_version,
        app_filter: cfg.app_filter.clone(),
        detect_crashes: cfg.detect_crashes,
        ts: now_ts(),
    });

    // Network capture is part of the default watch posture: if a proxy daemon is
    // running, HTTP lines join the same stdout timeline as screen/crash lines. If
    // it is not running, emit a structured warning so an agent can decide whether
    // to run `net start` or continue UI-only.
    // println! locks stdout per line, so http/screen lines interleave cleanly.
    let event_emitter = spawn_event_emitter(cfg.serial.clone(), event_rx);
    let mut producers = Vec::new();
    if cfg.net {
        producers.push(spawn_net_events(
            cfg.serial.clone(),
            event_tx.clone(),
            stopping.clone(),
        ));
    }

    producers.push(spawn_wake_logcat(
        cfg.serial.clone(),
        wake_tx.clone(),
        event_tx.clone(),
        stopping.clone(),
    ));
    if cfg.detect_crashes {
        producers.push(spawn_crash_detector(
            cfg.serial.clone(),
            cfg.app_filter.clone(),
            event_tx.clone(),
            stopping.clone(),
        ));
    }
    if cfg.accept_stdin {
        producers.push(spawn_stdin(command_tx.clone()));
    }
    let _ = wake_tx.send(Wake::Init).await;

    let mut poll = interval(Duration::from_millis(cfg.poll_ms as u64));
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut state = WatchState::default();

    loop {
        tokio::select! {
            result = &mut ctrl_c => {
                result.context("waiting for ctrl-c")?;
                stopping.store(true, Ordering::Release);
                break;
            }
            _ = poll.tick() => {
                if handle_screen_wake(&cfg, &watchers, &mut state, Wake::Poll, false, None).await {
                    break;
                }
            }
            Some(wake) = wake_rx.recv() => {
                // Logcat Event and startup Init wakes respect the --app filter
                // and screen-hash de-dup; only an explicit `screen` stdin command
                // forces a filtered-out emit (via command_rx → ScreenOnly).
                if handle_screen_wake(&cfg, &watchers, &mut state, wake, false, Some(&mut wake_rx)).await {
                    break;
                }
            }
            Some(line) = command_rx.recv() => {
                let should_quit = handle_command(&cfg, &watchers, &mut state, &line).await;
                if should_quit {
                    break;
                }
            }
        }
    }

    stopping.store(true, Ordering::Release);
    drop(wake_tx);
    drop(command_tx);
    shutdown_producers_and_drain(&stopping, producers, event_tx, event_emitter).await?;
    emit_action("watch", &json!({"status": "stopped", "device": cfg.serial}));
    Ok(())
}

fn spawn_event_emitter(serial: Serial, mut event_rx: mpsc::Receiver<Event>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(evt) = event_rx.recv().await {
            events::emit_stream_event(&evt, serial.as_str());
        }
    })
}

fn spawn_net_events(
    serial: Serial,
    event_tx: mpsc::Sender<Event>,
    stopping: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let req = serde_json::json!({"op": "watch", "matcher": {}});
        if let Err(err) = crate::net::control::request_stream(&serial, req).await {
            if shutdown_in_progress(&stopping).await {
                return;
            }
            let _ = event_tx
                .send(Event::Warning {
                    stage: "net_watch".to_string(),
                    code: "net_events_unavailable".to_string(),
                    msg: format!("network events unavailable: {err}"),
                    detail: serde_json::json!({"ui_and_crash_stream_continues": true}),
                    next_actions: vec![
                        "shadowdroid net start".to_string(),
                        "shadowdroid watch --no-net".to_string(),
                    ],
                    ts: now_ts(),
                })
                .await;
        }
    })
}

fn spawn_crash_detector(
    serial: Serial,
    app_filter: Option<String>,
    event_tx: mpsc::Sender<Event>,
    stopping: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = logcat::run(serial, app_filter, event_tx.clone()).await {
            if shutdown_in_progress(&stopping).await {
                return;
            }
            let _ = event_tx
                .send(Event::Error {
                    stage: "crash_detect".to_string(),
                    code: "crash_detect_failed".to_string(),
                    msg: err.to_string(),
                    input: None,
                    retryable: true,
                    detail: serde_json::json!({}),
                    next_actions: vec![
                        "shadowdroid log --last 5m".to_string(),
                        "shadowdroid doctor --json".to_string(),
                    ],
                    ts: now_ts(),
                })
                .await;
        }
    })
}

fn spawn_wake_logcat(
    serial: Serial,
    wake_tx: mpsc::Sender<Wake>,
    event_tx: mpsc::Sender<Event>,
    stopping: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run_wake_logcat(serial, wake_tx).await {
            if shutdown_in_progress(&stopping).await {
                return;
            }
            let _ = event_tx
                .send(Event::Error {
                    stage: "logcat_wake".to_string(),
                    code: "logcat_wake_failed".to_string(),
                    msg: err.to_string(),
                    input: None,
                    retryable: true,
                    detail: serde_json::json!({}),
                    next_actions: vec![
                        "restart `shadowdroid watch` to restore event-driven UI wakeups"
                            .to_string(),
                        "shadowdroid doctor --json".to_string(),
                    ],
                    ts: now_ts(),
                })
                .await;
        }
    })
}

/// Stop every task that can still write a watch record, wait until cancellation
/// has dropped its channel clones, then let the sole emitter consume everything
/// already queued. Only after this returns is it safe to write the terminal
/// `watch` action directly to stdout.
async fn shutdown_producers_and_drain<T: Send + 'static>(
    stopping: &AtomicBool,
    producers: Vec<JoinHandle<()>>,
    event_tx: mpsc::Sender<T>,
    emitter: JoinHandle<()>,
) -> Result<()> {
    stopping.store(true, Ordering::Release);
    for producer in &producers {
        producer.abort();
    }

    let mut producer_failure = None;
    for producer in producers {
        if let Err(error) = producer.await
            && !error.is_cancelled()
            && producer_failure.is_none()
        {
            producer_failure = Some(error);
        }
    }

    // Closing the last sender is the emitter's drain boundary.
    drop(event_tx);
    emitter.await.context("watch event emitter task panicked")?;
    if let Some(error) = producer_failure {
        return Err(anyhow!("watch producer task failed: {error}"));
    }
    Ok(())
}

async fn shutdown_in_progress(stopping: &AtomicBool) -> bool {
    // SIGINT reaches the adb logcat children and the parent at nearly the same
    // time. Give the parent select branch one scheduler turn to mark intentional
    // shutdown before classifying child exit as a stream failure.
    tokio::time::sleep(Duration::from_millis(250)).await;
    stopping.load(Ordering::Acquire)
}

async fn run_wake_logcat(serial: Serial, wake_tx: mpsc::Sender<Wake>) -> Result<()> {
    let mut child = Command::new("adb")
        .args([
            "-s",
            &serial,
            "logcat",
            "-T",
            "1",
            "-v",
            "raw",
            "ActivityTaskManager:I",
            "ActivityManager:I",
            "WindowManager:I",
            "ViewRootImpl:I",
            "am_focused_activity:I",
            "*:S",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting adb logcat for UI wake events")?;

    let stdout = child
        .stdout
        .take()
        .context("adb logcat did not expose stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if is_ui_wake_line(&line) {
            let _ = wake_tx.send(Wake::Event).await;
        }
    }
    bail!("adb logcat for UI wake events exited")
}

fn is_ui_wake_line(line: &str) -> bool {
    [
        "Activity",
        "Window",
        "focused",
        "addWindow",
        "removeWindow",
        "TYPE_WINDOW",
        "startActivity",
        "resumeActivity",
    ]
    .iter()
    .any(|key| line.contains(key))
}

fn spawn_stdin(command_tx: mpsc::Sender<String>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if command_tx.send(line).await.is_err() {
                break;
            }
        }
    })
}

async fn handle_screen_wake(
    cfg: &WatchConfig,
    watchers: &WatcherSet,
    state: &mut WatchState,
    wake: Wake,
    force: bool,
    drain_wakes: Option<&mut mpsc::Receiver<Wake>>,
) -> bool {
    let debounce_ms = debounce_delay_ms(wake, cfg.debounce_ms);
    if debounce_ms > 0 {
        sleep(Duration::from_millis(debounce_ms as u64)).await;
    }
    // Coalesce any wakes that queued during the debounce window into this one
    // screen fetch. The channel only carries Event/Init wakes, neither of which
    // forces a filtered-out emit, so draining them can't change `force`.
    if let Some(wake_rx) = drain_wakes {
        while wake_rx.try_recv().is_ok() {}
    }
    match cfg.client.screen().await {
        Ok(screen) => {
            if !should_emit_screen(
                cfg.app_filter.as_deref(),
                screen.current_app.package.as_deref(),
                force,
            ) {
                return false;
            }
            if !force && state.last_hash.as_deref() == Some(screen.screen_hash.as_str()) {
                return false;
            }
            state.last_hash = Some(screen.screen_hash.clone());
            let screen_hash_version = screen.screen_hash_version;
            let hits = watchers.matches(&screen.screen_hash, &screen.elements);
            events::emit(&events::screen_event(
                &cfg.serial,
                screen,
                cfg.screen_format,
            ));
            for hit in hits {
                events::emit(&Event::WatcherFired {
                    name: hit.name.clone(),
                    screen_hash: hit.screen_hash.clone(),
                    screen_hash_version,
                    matched: hit.matched.clone(),
                    ts: now_ts(),
                });
                for action in hit.then {
                    match dispatch_command(cfg, watchers, state, &action).await {
                        Ok(DispatchOutcome::Handled) => {}
                        Ok(DispatchOutcome::Continue) => {}
                        Ok(DispatchOutcome::ScreenOnly) => {}
                        Ok(DispatchOutcome::Quit) => return true,
                        Err(err) => events::emit_stream_event(
                            &Event::Error {
                                stage: "watcher".to_string(),
                                code: "watcher_action_failed".to_string(),
                                msg: err.to_string(),
                                input: Some(action.to_string()),
                                retryable: false,
                                detail: serde_json::json!({"watcher": hit.name}),
                                next_actions: vec![
                                    "inspect input and correct the watcher action".to_string(),
                                    "shadowdroid ui dump".to_string(),
                                ],
                                ts: now_ts(),
                            },
                            cfg.serial.as_str(),
                        ),
                    }
                }
            }
        }
        Err(err) => events::emit_stream_event(
            &Event::Error {
                stage: "screen".to_string(),
                code: "screen_read_failed".to_string(),
                msg: err.to_string(),
                input: None,
                retryable: true,
                detail: serde_json::json!({}),
                next_actions: vec![
                    "shadowdroid doctor --json".to_string(),
                    "shadowdroid connect".to_string(),
                ],
                ts: now_ts(),
            },
            cfg.serial.as_str(),
        ),
    }
    false
}

async fn handle_command(
    cfg: &WatchConfig,
    watchers: &WatcherSet,
    state: &mut WatchState,
    line: &str,
) -> bool {
    let cmd = match stdin::parse_command(line) {
        Ok(Value::Null) => return false,
        Ok(cmd) => cmd,
        Err(err) => {
            events::emit_stream_event(
                &Event::Error {
                    stage: "parse".to_string(),
                    code: "watch_input_invalid".to_string(),
                    msg: err.to_string(),
                    input: Some(line.to_string()),
                    retryable: false,
                    detail: serde_json::json!({}),
                    next_actions: vec![
                        "correct input using the watch stdin grammar, then retry".to_string(),
                        "shadowdroid commands --json --describe 'watch'".to_string(),
                    ],
                    ts: now_ts(),
                },
                cfg.serial.as_str(),
            );
            return false;
        }
    };

    match dispatch_command(cfg, watchers, state, &cmd).await {
        Ok(DispatchOutcome::Handled) => false,
        Ok(DispatchOutcome::Continue) => {
            handle_screen_wake(cfg, watchers, state, Wake::Command, false, None).await
        }
        Ok(DispatchOutcome::ScreenOnly) => {
            handle_screen_wake(cfg, watchers, state, Wake::Command, true, None).await
        }
        Ok(DispatchOutcome::Quit) => true,
        Err(err) => {
            events::emit_stream_event(
                &Event::Error {
                    stage: "dispatch".to_string(),
                    code: "watch_action_failed".to_string(),
                    msg: err.to_string(),
                    input: Some(cmd.to_string()),
                    retryable: false,
                    detail: serde_json::json!({}),
                    next_actions: vec![
                        "inspect input, correct the failed action, and retry".to_string(),
                        "shadowdroid ui dump".to_string(),
                    ],
                    ts: now_ts(),
                },
                cfg.serial.as_str(),
            );
            false
        }
    }
}

fn debounce_delay_ms(wake: Wake, configured_ms: u32) -> u32 {
    match wake {
        Wake::Event | Wake::Init => configured_ms,
        Wake::Command | Wake::Poll => 0,
    }
}

enum DispatchOutcome {
    Handled,
    Continue,
    ScreenOnly,
    Quit,
}

#[derive(Debug, Clone)]
struct TapOutcome {
    id: Option<u32>,
    x: Option<i32>,
    y: Option<i32>,
    source: String,
}

async fn dispatch_command(
    cfg: &WatchConfig,
    watchers: &WatcherSet,
    state: &mut WatchState,
    cmd: &Value,
) -> Result<DispatchOutcome> {
    let op = req_str(cmd, "cmd")?;
    match op {
        "quit" => {
            emit_action("quit", &json!({}));
            return Ok(DispatchOutcome::Quit);
        }
        "screen" => {
            return Ok(DispatchOutcome::ScreenOnly);
        }
        "tap" => {
            dispatch_tap(cfg, state, cmd, "tap").await?;
        }
        "tap_and_wait" => {
            let start_hash = state.last_hash.clone();
            let tapped = dispatch_tap(cfg, state, cmd, "tap_and_wait").await?;
            wait_after_action(cfg, state, cmd, start_hash, "tap_and_wait", Some(tapped)).await?;
            return Ok(DispatchOutcome::Handled);
        }
        "double_tap" => {
            let x = req_i32(cmd, "x")?;
            let y = req_i32(cmd, "y")?;
            cfg.client.double_tap(x, y).await?;
            emit_action("double_tap", &json!({"x":x, "y":y}));
        }
        "long_tap" => {
            let x = req_i32(cmd, "x")?;
            let y = req_i32(cmd, "y")?;
            let duration_ms = duration_ms(cmd, 600)?;
            cfg.client.long_tap(x, y, duration_ms).await?;
            emit_action(
                "long_tap",
                &json!({"x":x, "y":y, "duration_ms":duration_ms}),
            );
        }
        "swipe" | "drag" => {
            let [x1, y1] = req_pair(cmd, "from")?;
            let [x2, y2] = req_pair(cmd, "to")?;
            let duration_ms = duration_ms(cmd, if op == "drag" { 500 } else { 200 })?;
            if op == "drag" {
                cfg.client.drag(x1, y1, x2, y2, duration_ms).await?;
            } else {
                cfg.client.swipe(x1, y1, x2, y2, duration_ms).await?;
            }
            emit_action(
                op,
                &json!({"from":[x1,y1], "to":[x2,y2], "duration_ms":duration_ms}),
            );
        }
        "swipe_ext" => {
            let direction = req_str(cmd, "direction")?;
            let scale = opt_f32(cmd, "scale")?.unwrap_or(0.9);
            let duration_ms = duration_ms(cmd, 200)?;
            cfg.client.swipe_ext(direction, scale, duration_ms).await?;
            emit_action(
                "swipe_ext",
                &json!({"direction":direction, "scale":scale, "duration_ms":duration_ms}),
            );
        }
        "key" => {
            let name = req_str(cmd, "name")?;
            let injected = cfg.client.key(name).await?;
            emit_action("key", &json!({"name":name, "injected":injected}));
        }
        "text" => {
            let value = req_str(cmd, "value")?;
            let clear = opt_bool(cmd, "clear").unwrap_or(false);
            let target = selector_query_from_cmd(cmd)?;
            cfg.client
                .text_with_target(value, clear, target.as_ref())
                .await?;
            emit_action(
                "text",
                &json!({"value":value, "clear":clear, "target":target}),
            );
        }
        "launch" => {
            let package = req_str(cmd, "package")?;
            cfg.client.app_start(package, None).await?;
            emit_action("launch", &json!({"package":package}));
        }
        "stop" => {
            let package = req_str(cmd, "package")?;
            cfg.client.app_stop(package).await?;
            emit_action("stop", &json!({"package":package}));
        }
        "app_clear" => {
            let package = req_str(cmd, "package")?;
            cfg.client.app_clear(package).await?;
            emit_action("app_clear", &json!({"package":package}));
        }
        "app_wait" => {
            let package = req_str(cmd, "package")?;
            let timeout_ms = timeout_ms(cmd, 20_000)?;
            let front = opt_bool(cmd, "front").unwrap_or(false);
            let r = cfg.client.app_wait(package, timeout_ms, front).await?;
            if !r.matched {
                bail!(
                    "app_wait timed out after {timeout_ms}ms for {package}; current app: {:?}. Inspect `screen` and correct the package/state before retrying",
                    r.current
                );
            }
            emit_action(
                "app_wait",
                &json!({"package":package, "matched":r.matched, "current":r.current}),
            );
        }
        "app_info" => {
            let package = req_str(cmd, "package")?;
            let info = cfg.client.app_info(package).await?;
            emit_action(
                "app_info",
                &json!({"package":package, "version_name":info.version_name, "version_code":info.version_code, "label":info.label}),
            );
        }
        "wait_activity" => {
            let name = req_str(cmd, "name")?;
            let timeout_ms = timeout_ms(cmd, 10_000)?;
            wait_activity(&cfg.client, name, timeout_ms).await?;
        }
        "screenshot" => {
            let path = cmd.get("path").and_then(Value::as_str).map(String::from);
            let (path, bytes) = write_screenshot(&cfg.client, path).await?;
            emit_action("screenshot", &json!({"path":path, "bytes":bytes}));
        }
        "shell" => {
            let value = req_any_str(cmd, &["value", "input", "cmdline"])?;
            let timeout_ms = timeout_ms(cmd, 30_000)?;
            let r = cfg.client.shell(value, timeout_ms).await?;
            if r.exit_code.is_some_and(|code| code != 0) {
                bail!(
                    "device shell exited {} for {:?}; output: {}. Correct the command before retrying",
                    r.exit_code.unwrap_or_default(),
                    r.input,
                    r.output.trim()
                );
            }
            emit_action(
                "shell",
                &json!({"input":r.input, "output":r.output, "exit_code":r.exit_code}),
            );
        }
        "screen_on" => {
            cfg.client.screen_on().await?;
            emit_action("screen_on", &json!({}));
        }
        "screen_off" => {
            cfg.client.screen_off().await?;
            emit_action("screen_off", &json!({}));
        }
        "unlock" => {
            cfg.client.unlock().await?;
            emit_action("unlock", &json!({}));
        }
        "wakeup" => {
            cfg.client.wakeup().await?;
            emit_action("wakeup", &json!({}));
        }
        "orientation" => {
            let value = cfg.client.orientation_get().await?;
            emit_action("orientation", &json!({"value":value}));
        }
        "set_orientation" => {
            let value = req_str(cmd, "value")?;
            cfg.client.orientation_set(value).await?;
            emit_action("set_orientation", &json!({"value":value}));
        }
        "clipboard" => {
            let value = cfg.client.clipboard_get().await?;
            emit_action("clipboard", &json!({"value":value}));
        }
        "set_clipboard" => {
            let value = req_str(cmd, "value")?;
            cfg.client.clipboard_set(value).await?;
            emit_action("set_clipboard", &json!({"value":value}));
        }
        "open_notification" => {
            cfg.client.open_notifications().await?;
            emit_action("open_notification", &json!({}));
        }
        "open_quick_settings" => {
            cfg.client.open_quick_settings().await?;
            emit_action("open_quick_settings", &json!({}));
        }
        "open_url" => {
            let url = req_str(cmd, "url")?;
            cfg.client.open_url(url).await?;
            emit_action("open_url", &json!({"url":url}));
        }
        "tap_text" | "tap_rid" | "tap_desc" | "tap_text_and_wait" | "tap_rid_and_wait"
        | "tap_desc_and_wait" => {
            let value = req_str(cmd, "value")?;
            let base_op = op.strip_suffix("_and_wait").unwrap_or(op);
            let query = match op {
                "tap_text" | "tap_text_and_wait" => SelectorQuery {
                    text: Some(value.to_string()),
                    ..Default::default()
                },
                "tap_rid" | "tap_rid_and_wait" => SelectorQuery {
                    rid: Some(value.to_string()),
                    ..Default::default()
                },
                "tap_desc" | "tap_desc_and_wait" => SelectorQuery {
                    desc: Some(value.to_string()),
                    ..Default::default()
                },
                _ => unreachable!(),
            };
            let start_hash = state.last_hash.clone();
            let r = cfg.client.find_tap(&query).await?;
            let matched_id = r.matched.id;
            let source = r.action.clone().unwrap_or_else(|| "server".to_string());
            emit_action(
                base_op,
                &json!({
                    "value":value,
                    "x":r.x,
                    "y":r.y,
                    "action":r.action,
                    "selector_matched":true,
                    "actionable_resolved":r.actionable_resolved,
                    "input_delivered":r.input_delivered,
                    "matched_element":r.matched,
                    "activated_element":r.activated_element
                }),
            );
            if op.ends_with("_and_wait") {
                wait_after_action(
                    cfg,
                    state,
                    cmd,
                    start_hash,
                    op,
                    Some(TapOutcome {
                        id: Some(matched_id),
                        x: r.x,
                        y: r.y,
                        source,
                    }),
                )
                .await?;
                return Ok(DispatchOutcome::Handled);
            }
        }
        "xpath" => {
            let query = req_any_str(cmd, &["query", "value"])?;
            let r = cfg.client.xpath(query, false).await?;
            emit_action(
                "xpath",
                &json!({"query":query, "matched":r.matched, "elements":r.elements}),
            );
        }
        "xpath_tap" => {
            let query = req_any_str(cmd, &["query", "value"])?;
            let r = cfg.client.xpath_tap(query, false).await?;
            emit_action(
                "xpath_tap",
                &json!({
                    "query":query,
                    "x":r.x,
                    "y":r.y,
                    "action":r.action,
                    "selector_matched":true,
                    "actionable_resolved":r.actionable_resolved,
                    "input_delivered":r.input_delivered,
                    "matched_element":r.matched,
                    "activated_element":r.activated_element
                }),
            );
        }
        "toast" => {
            let wait = toast_timeout_ms(cmd)?;
            let start = unix_ms();
            cfg.client.toast_start(50).await?;
            let deadline = std::time::Instant::now() + Duration::from_millis(wait as u64);
            loop {
                let recent = cfg.client.toast_recent(start).await?;
                if !recent.toasts.is_empty() || std::time::Instant::now() >= deadline {
                    emit_action("toast", &json!({"toasts":recent.toasts}));
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        "push" => {
            let local = req_str(cmd, "local")?;
            let remote = req_str(cmd, "remote")?;
            let mode = opt_u32(cmd, "mode")?;
            let r = cfg.client.push_file(remote, Path::new(local), mode).await?;
            if mode.is_some_and(|requested| requested != r.mode) {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "file_mode_postcondition_failed",
                    "files",
                    format!("file was pushed, but {remote} did not reach the requested Unix mode"),
                )
                .detail(json!({
                    "local": local,
                    "remote": remote,
                    "requested_mode": mode,
                    "observed_mode": r.mode,
                    "transfer_completed": true,
                }))
                .next_actions([
                    "use a path that supports chmod, or omit mode on shared/FUSE storage",
                ])
                .into());
            }
            let mut payload = json!({"local":local, "remote":remote, "path":r.path, "bytes":r.bytes, "mode":r.mode});
            if let Some(mode) = mode {
                payload["requested_mode"] = json!(mode);
            }
            emit_action("push", &payload);
        }
        "pull" => {
            let remote = req_str(cmd, "remote")?;
            let local = req_str(cmd, "local")?;
            let response = cfg.client.pull_file_response(remote).await?;
            let receipt =
                crate::transfer::response_to_path_atomic(response, Path::new(local), None).await?;
            emit_action(
                "pull",
                &json!({"remote":remote, "local":local, "bytes":receipt.bytes}),
            );
        }
        "wait_for" => {
            let timeout = timeout_ms(cmd, 10_000)?;
            let poll_ms = opt_u32(cmd, "poll_ms")?.unwrap_or(200).max(1);
            let gone = opt_bool(cmd, "gone").unwrap_or(false);
            wait_for(cfg, cmd, gone, timeout, poll_ms).await?;
        }
        "add_watcher" => {
            let rule_value = cmd.get("rule").cloned().unwrap_or_else(|| {
                let mut copy = cmd.clone();
                if let Value::Object(map) = &mut copy {
                    map.remove("cmd");
                }
                copy
            });
            let rule: WatcherRule = serde_json::from_value(rule_value)?;
            watchers.add(rule.clone());
            emit_action("add_watcher", &json!({"name":rule.name}));
        }
        "remove_watcher" => {
            let name = req_str(cmd, "name")?;
            let removed = watchers.remove(name);
            emit_action("remove_watcher", &json!({"name":name, "removed":removed}));
        }
        "list_watchers" => {
            emit_action("list_watchers", &json!({"watchers":watchers.list()}));
        }
        "clear_watchers" => {
            watchers.clear();
            emit_action("clear_watchers", &json!({}));
        }
        "permission_dialogs" => {
            let policy = req_str(cmd, "policy")?;
            let policy = PermissionDialogPolicy::parse(policy)
                .ok_or_else(|| anyhow!("permission_dialogs policy must be ignore|allow|deny"))?;
            watchers.set_permission_dialog_policy(policy);
            emit_action("permission_dialogs", &json!({"policy": policy.as_str()}));
        }
        _ => bail!("unknown cmd: {op}"),
    }
    Ok(DispatchOutcome::Continue)
}

async fn dispatch_tap(
    cfg: &WatchConfig,
    state: &mut WatchState,
    cmd: &Value,
    action_cmd: &str,
) -> Result<TapOutcome> {
    if let Some(id) = opt_u32(cmd, "id")? {
        if let Some(expected_hash) = cmd.get("screen_hash").and_then(Value::as_str)
            && state.last_hash.as_deref() != Some(expected_hash)
        {
            bail!(
                "stale screen id {id}: expected screen_hash {expected_hash}, current cached screen_hash {:?}",
                state.last_hash
            );
        }
        let r = cfg
            .client
            .find_tap(&SelectorQuery {
                id: Some(id),
                ..Default::default()
            })
            .await?;
        let source = r.action.clone().unwrap_or_else(|| "server".to_string());
        emit_action(
            action_cmd,
            &json!({
                "id":id,
                "x":r.x,
                "y":r.y,
                "source":source,
                "selector_matched":true,
                "actionable_resolved":r.actionable_resolved,
                "input_delivered":r.input_delivered,
                "matched_element":r.matched,
                "activated_element":r.activated_element
            }),
        );
        return Ok(TapOutcome {
            id: Some(id),
            x: r.x,
            y: r.y,
            source,
        });
    }

    let x = req_i32(cmd, "x")?;
    let y = req_i32(cmd, "y")?;
    cfg.client.tap_xy(x, y).await?;
    emit_action(action_cmd, &json!({"x":x, "y":y, "source":"coordinates"}));
    Ok(TapOutcome {
        id: None,
        x: Some(x),
        y: Some(y),
        source: "coordinates".to_string(),
    })
}

async fn wait_after_action(
    cfg: &WatchConfig,
    state: &mut WatchState,
    cmd: &Value,
    start_hash: Option<String>,
    action_cmd: &str,
    tap: Option<TapOutcome>,
) -> Result<()> {
    let timeout = timeout_ms(cmd, 2_000)?;
    let poll_ms = opt_u32(cmd, "poll_ms")?.unwrap_or(25).max(1);
    let gone = opt_bool(cmd, "gone").unwrap_or(false);
    let has_query = has_wait_query(cmd);
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout as u64);

    loop {
        let screen = cfg.client.screen().await?;
        if let Some(filter) = &cfg.app_filter
            && !should_emit_package(Some(filter.as_str()), screen.current_app.package.as_deref())
        {
            if std::time::Instant::now() >= deadline {
                bail!(
                    "{action_cmd} timed out after {timeout}ms because the foreground app stayed outside the watch app filter; inspect `screen` or change the filter"
                );
            }
            sleep(Duration::from_millis(poll_ms as u64)).await;
            continue;
        }

        let query_matched = wait_query_matches(cmd, &screen.current_app, &screen.elements);
        let hash_changed = start_hash
            .as_deref()
            .map(|hash| hash != screen.screen_hash.as_str())
            .unwrap_or(true);
        let done = if has_query {
            query_matched != gone
        } else {
            hash_changed
        };
        let timed_out = std::time::Instant::now() >= deadline;

        if done || timed_out {
            state.last_hash = Some(screen.screen_hash.clone());
            events::emit(&events::screen_event(
                &cfg.serial,
                screen.clone(),
                cfg.screen_format,
            ));
            if timed_out && !done {
                bail!(
                    "{action_cmd} wait timed out after {timeout}ms; current screen_hash={} (v{}). Inspect the emitted screen and refine the wait condition",
                    screen.screen_hash,
                    screen.screen_hash_version
                );
            }
            emit_action(
                action_cmd,
                &json!({"matched":true, "timeout": false, "screen_hash":screen.screen_hash, "screen_hash_version":screen.screen_hash_version, "hash_changed":hash_changed, "tap": tap.map(|tap| json!({
                    "id":tap.id,
                    "x":tap.x,
                    "y":tap.y,
                    "source":tap.source
                }))}),
            );
            return Ok(());
        }

        sleep(Duration::from_millis(poll_ms as u64)).await;
    }
}

fn has_wait_query(cmd: &Value) -> bool {
    ["text", "rid", "desc", "klass", "package", "activity"]
        .iter()
        .any(|key| cmd.get(*key).and_then(Value::as_str).is_some())
}

async fn wait_activity(client: &ServerClient, name: &str, timeout_ms: u32) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        let cur = client.app_current().await?;
        if cur.activity.as_deref().unwrap_or("").contains(name) {
            emit_action(
                "wait_activity",
                &json!({"name":name, "matched":true, "current":cur}),
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "wait_activity timed out after {timeout_ms}ms waiting for {name:?}; current activity: {:?}. Correct the activity or app state before retrying",
                cur.activity
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for(
    cfg: &WatchConfig,
    cmd: &Value,
    gone: bool,
    timeout_ms: u32,
    poll_ms: u32,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        let screen = cfg.client.screen().await?;
        let matched = wait_query_matches(cmd, &screen.current_app, &screen.elements);
        if matched != gone {
            emit_action(
                "wait_for",
                &json!({"matched":matched, "gone":gone, "screen_hash":screen.screen_hash, "screen_hash_version":screen.screen_hash_version}),
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "wait_for timed out after {timeout_ms}ms (matched={matched}, gone={gone}); current screen_hash={} (v{}). Inspect `screen` and refine the selector/state",
                screen.screen_hash,
                screen.screen_hash_version
            );
        }
        sleep(Duration::from_millis(poll_ms as u64)).await;
    }
}

fn wait_query_matches(cmd: &Value, app: &AppRef, elements: &[Element]) -> bool {
    if let Some(package) = cmd.get("package").and_then(Value::as_str)
        && !app.package.as_deref().unwrap_or("").contains(package)
    {
        return false;
    }
    if let Some(activity) = cmd.get("activity").and_then(Value::as_str)
        && !app.activity.as_deref().unwrap_or("").contains(activity)
    {
        return false;
    }
    let text = cmd.get("text").and_then(Value::as_str);
    let rid = cmd.get("rid").and_then(Value::as_str);
    let desc = cmd.get("desc").and_then(Value::as_str);
    let klass = cmd.get("klass").and_then(Value::as_str);
    if text.is_none() && rid.is_none() && desc.is_none() && klass.is_none() {
        return true;
    }
    elements.iter().any(|el| {
        selector_string_matches(el.text.as_deref(), text)
            && selector_string_matches(el.rid.as_deref(), rid)
            && selector_string_matches(el.desc.as_deref(), desc)
            && selector_string_matches(el.klass.as_deref(), klass)
    })
}

fn selector_string_matches(actual: Option<&str>, expected: Option<&str>) -> bool {
    crate::selector::text_matches(actual, expected, false)
}

fn selector_query_from_cmd(cmd: &Value) -> Result<Option<SelectorQuery>> {
    let id = opt_u32(cmd, "id")?;
    let text = cmd.get("text").and_then(Value::as_str).map(String::from);
    let rid = cmd.get("rid").and_then(Value::as_str).map(String::from);
    let desc = cmd.get("desc").and_then(Value::as_str).map(String::from);
    let klass = cmd.get("klass").and_then(Value::as_str).map(String::from);
    let xpath = cmd.get("xpath").and_then(Value::as_str).map(String::from);
    if id.is_none()
        && text.is_none()
        && rid.is_none()
        && desc.is_none()
        && klass.is_none()
        && xpath.is_none()
    {
        return Ok(None);
    }
    Ok(Some(SelectorQuery {
        id,
        text,
        rid,
        desc,
        klass,
        xpath,
        exact: opt_bool(cmd, "exact").unwrap_or(false),
        ..Default::default()
    }))
}

async fn write_screenshot(client: &ServerClient, path: Option<String>) -> Result<(String, u64)> {
    let bytes = client.screenshot_png().await?;
    let path = match path {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            std::env::temp_dir().join(format!("shadowdroid-screenshot-{ts}.png"))
        }
    };
    tokio::fs::write(&path, &bytes)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    Ok((path.display().to_string(), bytes.len() as u64))
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn req_str<'a>(cmd: &'a Value, key: &str) -> Result<&'a str> {
    cmd.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing string field `{key}`"))
}

fn req_any_str<'a>(cmd: &'a Value, keys: &[&str]) -> Result<&'a str> {
    for key in keys {
        if let Some(value) = cmd.get(*key).and_then(Value::as_str) {
            return Ok(value);
        }
    }
    bail!("missing string field: one of {}", keys.join(", "))
}

fn req_i32(cmd: &Value, key: &str) -> Result<i32> {
    cmd.get(key)
        .and_then(as_i32)
        .ok_or_else(|| anyhow!("missing integer field `{key}`"))
}

fn opt_u32(cmd: &Value, key: &str) -> Result<Option<u32>> {
    cmd.get(key)
        .map(|v| {
            as_i64(v)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| anyhow!("field `{key}` must be a non-negative integer"))
        })
        .transpose()
}

fn opt_f32(cmd: &Value, key: &str) -> Result<Option<f32>> {
    cmd.get(key)
        .map(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
                .map(|f| f as f32)
                .ok_or_else(|| anyhow!("field `{key}` must be a number"))
        })
        .transpose()
}

fn opt_bool(cmd: &Value, key: &str) -> Option<bool> {
    cmd.get(key).and_then(Value::as_bool)
}

fn req_pair(cmd: &Value, key: &str) -> Result<[i32; 2]> {
    let arr = cmd
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing array field `{key}`"))?;
    if arr.len() != 2 {
        bail!("field `{key}` must have exactly two integers");
    }
    let x = as_i32(&arr[0]).ok_or_else(|| anyhow!("field `{key}`[0] must be an integer"))?;
    let y = as_i32(&arr[1]).ok_or_else(|| anyhow!("field `{key}`[1] must be an integer"))?;
    Ok([x, y])
}

fn duration_ms(cmd: &Value, default_ms: u32) -> Result<u32> {
    if let Some(v) = cmd.get("duration_ms") {
        return as_u32(v)
            .ok_or_else(|| anyhow!("field `duration_ms` must be a non-negative integer"));
    }
    if let Some(v) = cmd.get("duration") {
        let secs = v
            .as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
            .ok_or_else(|| anyhow!("field `duration` must be a number"))?;
        return Ok((secs * 1000.0).round().max(0.0) as u32);
    }
    Ok(default_ms)
}

fn timeout_ms(cmd: &Value, default_ms: u32) -> Result<u32> {
    if let Some(v) = cmd.get("timeout_ms") {
        return as_u32(v)
            .ok_or_else(|| anyhow!("field `timeout_ms` must be a non-negative integer"));
    }
    if let Some(v) = cmd.get("timeout") {
        let secs = v
            .as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
            .ok_or_else(|| anyhow!("field `timeout` must be a number"))?;
        return Ok((secs * 1000.0).round().max(0.0) as u32);
    }
    Ok(default_ms)
}

fn toast_timeout_ms(cmd: &Value) -> Result<u32> {
    if cmd.get("timeout_ms").is_some() || cmd.get("timeout").is_some() {
        return timeout_ms(cmd, 5_000);
    }
    if let Some(v) = cmd.get("wait") {
        let secs = v
            .as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
            .ok_or_else(|| anyhow!("field `wait` must be a number"))?;
        return Ok((secs * 1000.0).round().max(0.0) as u32);
    }
    Ok(5_000)
}

fn as_i32(v: &Value) -> Option<i32> {
    as_i64(v).and_then(|n| i32::try_from(n).ok())
}

fn as_u32(v: &Value) -> Option<u32> {
    as_i64(v).and_then(|n| u32::try_from(n).ok())
}

fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn should_emit_package(app_filter: Option<&str>, package: Option<&str>) -> bool {
    let Some(filter) = app_filter else {
        return true;
    };
    let Some(package) = package else {
        return false;
    };
    package == filter || is_system_interruption(package)
}

fn should_emit_screen(app_filter: Option<&str>, package: Option<&str>, force: bool) -> bool {
    force || should_emit_package(app_filter, package)
}

fn is_system_interruption(package: &str) -> bool {
    matches!(
        package,
        "com.android.permissioncontroller"
            | "com.google.android.permissioncontroller"
            | "com.android.systemui"
            | "com.google.android.packageinstaller"
            | "com.android.packageinstaller"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Wake, debounce_delay_ms, selector_string_matches, should_emit_package, should_emit_screen,
        shutdown_producers_and_drain, toast_timeout_ms,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[test]
    fn app_filter_allows_target_package() {
        assert!(should_emit_package(Some("com.livd"), Some("com.livd")));
    }

    #[test]
    fn app_filter_allows_permission_dialogs() {
        assert!(should_emit_package(
            Some("com.livd"),
            Some("com.android.permissioncontroller")
        ));
    }

    #[test]
    fn app_filter_suppresses_unrelated_apps() {
        assert!(!should_emit_package(
            Some("com.livd"),
            Some("com.google.android.apps.nexuslauncher")
        ));
    }

    #[test]
    fn forced_screen_bypasses_app_filter() {
        assert!(!should_emit_screen(
            Some("com.livd"),
            Some("com.google.android.apps.nexuslauncher"),
            false
        ));
        assert!(should_emit_screen(
            Some("com.livd"),
            Some("com.google.android.apps.nexuslauncher"),
            true
        ));
    }

    #[test]
    fn command_wakes_do_not_pay_debounce_delay() {
        assert_eq!(debounce_delay_ms(Wake::Command, 80), 0);
        assert_eq!(debounce_delay_ms(Wake::Poll, 80), 0);
        assert_eq!(debounce_delay_ms(Wake::Event, 80), 80);
        assert_eq!(debounce_delay_ms(Wake::Init, 80), 80);
    }

    #[test]
    fn wait_selector_matching_uses_canonical_normalization() {
        assert!(selector_string_matches(
            Some("Sign\u{00A0}\u{200B}in"),
            Some("sign in")
        ));
        assert!(selector_string_matches(
            Some("Don\u{2019}t allow"),
            Some("don't")
        ));
    }

    #[test]
    fn toast_timeout_accepts_legacy_wait_alias() {
        assert_eq!(
            toast_timeout_ms(&json!({"cmd":"toast","wait":30})).unwrap(),
            30_000
        );
        assert_eq!(
            toast_timeout_ms(&json!({"cmd":"toast","timeout":1.5})).unwrap(),
            1_500
        );
    }

    #[tokio::test]
    async fn shutdown_drains_queued_events_before_terminal_record() {
        let stopping = AtomicBool::new(false);
        let order = Arc::new(Mutex::new(Vec::new()));
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let emitter_order = order.clone();
        let emitter = tokio::spawn(async move {
            while event_rx.recv().await.is_some() {
                emitter_order.lock().unwrap().push("event");
            }
        });
        let producer_tx = event_tx.clone();
        let producer = tokio::spawn(async move {
            let _keep_channel_open = producer_tx;
            std::future::pending::<()>().await;
        });
        event_tx.send(1_u8).await.unwrap();

        shutdown_producers_and_drain(&stopping, vec![producer], event_tx, emitter)
            .await
            .unwrap();
        order.lock().unwrap().push("terminal");

        assert!(stopping.load(Ordering::Acquire));
        assert_eq!(*order.lock().unwrap(), ["event", "terminal"]);
    }
}
