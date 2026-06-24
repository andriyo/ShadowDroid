//! Steady-state watch loop. One emit per real screen change.
//!
//! Wake sources (all feed a single `tokio::sync::mpsc::Sender<Wake>`):
//!   - logcat tail (low-latency event signal on Window/Activity transitions)
//!   - safety-net poll (default 1s) — catches in-screen mutations
//!   - command nudge (after every dispatched action, force a fresh dump)
//!
//! On wake:
//!   - sleep `debounce_ms` to coalesce a storm
//!   - drain remaining wakes
//!   - GET /v1/screen
//!   - hash compare → emit on change
//!   - run watcher rules → dispatch actions, emit `watcher_fired` events
//!   - update `last_hash`

use crate::device::client::ServerClient;
use crate::events::{self, now_ts, Event, ScreenFormat};
use crate::proto::{AppRef, Element, SelectorQuery};
use crate::watch::watcher::{PermissionDialogPolicy, WatcherRule, WatcherSet};
use crate::watch::{logcat, stdin};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};

#[derive(Debug, Clone, Copy)]
pub enum Wake {
    Event,
    Poll,
    Command,
    Init,
}

pub struct WatchConfig {
    pub serial: String,
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

enum WatchMsg {
    Wake(Wake),
    Command(String),
}

#[derive(Default)]
struct WatchState {
    last_hash: Option<String>,
}

pub async fn run(cfg: WatchConfig) -> Result<()> {
    let watchers = WatcherSet::from_files(&cfg.watcher_files)?;
    watchers.set_permission_dialog_policy(cfg.permission_dialog_policy);
    let (watch_tx, mut watch_rx) = mpsc::channel::<WatchMsg>(128);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(64);
    let state = cfg
        .client
        .state()
        .await
        .context("reading initial server state")?;
    events::emit(&Event::Ready {
        device: cfg.serial.clone(),
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
    if cfg.net {
        spawn_net_events(cfg.serial.clone(), event_tx.clone());
    }

    spawn_wake_logcat(cfg.serial.clone(), watch_tx.clone(), event_tx.clone());
    if cfg.detect_crashes {
        spawn_crash_detector(cfg.serial.clone(), cfg.app_filter.clone(), event_tx.clone());
    }
    if cfg.accept_stdin {
        spawn_stdin(watch_tx.clone());
    }
    let _ = watch_tx.send(WatchMsg::Wake(Wake::Init)).await;

    let mut poll = interval(Duration::from_millis(cfg.poll_ms as u64));
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut state = WatchState::default();

    loop {
        tokio::select! {
            result = &mut ctrl_c => {
                result.context("waiting for ctrl-c")?;
                break;
            }
            _ = poll.tick() => {
                handle_screen_wake(&cfg, &watchers, &mut state, Wake::Poll, false).await;
            }
            Some(evt) = event_rx.recv() => {
                events::emit(&evt);
            }
            Some(msg) = watch_rx.recv() => {
                match msg {
                    WatchMsg::Wake(wake) => {
                        let force = matches!(wake, Wake::Init | Wake::Command);
                        handle_screen_wake(&cfg, &watchers, &mut state, wake, force).await;
                    }
                    WatchMsg::Command(line) => {
                        let should_quit = handle_command(&cfg, &watchers, &mut state, &line).await;
                        if should_quit {
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn spawn_net_events(serial: String, event_tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        let req = serde_json::json!({"op": "watch", "matcher": {}});
        if let Err(err) = crate::net::control::request_stream(&serial, req).await {
            let _ = event_tx
                .send(Event::Warning {
                    stage: "net_watch".to_string(),
                    msg: format!("network events unavailable: {err}"),
                    suggested_command: Some("shadowdroid net start".to_string()),
                    hint: Some(
                        "Run `shadowdroid net start` to add HTTP(S) events to this watch timeline, then restart watch; use `watch --no-net` for UI/crash-only streams."
                            .to_string(),
                    ),
                    ts: now_ts(),
                })
                .await;
        }
    });
}

fn spawn_crash_detector(serial: String, app_filter: Option<String>, event_tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        if let Err(err) = logcat::run(serial, app_filter, event_tx.clone()).await {
            let _ = event_tx
                .send(Event::Error {
                    stage: "crash_detect".to_string(),
                    msg: err.to_string(),
                    input: None,
                    ts: now_ts(),
                })
                .await;
        }
    });
}

fn spawn_wake_logcat(
    serial: String,
    watch_tx: mpsc::Sender<WatchMsg>,
    event_tx: mpsc::Sender<Event>,
) {
    tokio::spawn(async move {
        if let Err(err) = run_wake_logcat(serial, watch_tx).await {
            let _ = event_tx
                .send(Event::Error {
                    stage: "logcat_wake".to_string(),
                    msg: err.to_string(),
                    input: None,
                    ts: now_ts(),
                })
                .await;
        }
    });
}

async fn run_wake_logcat(serial: String, watch_tx: mpsc::Sender<WatchMsg>) -> Result<()> {
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
            let _ = watch_tx.send(WatchMsg::Wake(Wake::Event)).await;
        }
    }
    Ok(())
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

fn spawn_stdin(watch_tx: mpsc::Sender<WatchMsg>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if watch_tx.send(WatchMsg::Command(line)).await.is_err() {
                break;
            }
        }
    });
}

async fn handle_screen_wake(
    cfg: &WatchConfig,
    watchers: &WatcherSet,
    state: &mut WatchState,
    wake: Wake,
    force: bool,
) {
    let debounce_ms = debounce_delay_ms(wake, cfg.debounce_ms);
    if debounce_ms > 0 {
        sleep(Duration::from_millis(debounce_ms as u64)).await;
    }
    match cfg.client.screen().await {
        Ok(screen) => {
            if let Some(filter) = &cfg.app_filter {
                if !should_emit_package(
                    Some(filter.as_str()),
                    screen.current_app.package.as_deref(),
                ) {
                    return;
                }
            }
            if !force && state.last_hash.as_deref() == Some(screen.screen_hash.as_str()) {
                return;
            }
            state.last_hash = Some(screen.screen_hash.clone());
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
                    matched: hit.matched.clone(),
                    ts: now_ts(),
                });
                for action in hit.then {
                    match dispatch_command(cfg, watchers, state, &action).await {
                        Ok(DispatchOutcome::Handled) => {}
                        Ok(DispatchOutcome::Continue) => {}
                        Ok(DispatchOutcome::ScreenOnly) => {}
                        Ok(DispatchOutcome::Quit) => return,
                        Err(err) => events::emit(&Event::Error {
                            stage: "watcher".to_string(),
                            msg: err.to_string(),
                            input: Some(action.to_string()),
                            ts: now_ts(),
                        }),
                    }
                }
            }
        }
        Err(err) => events::emit(&Event::Error {
            stage: "screen".to_string(),
            msg: err.to_string(),
            input: None,
            ts: now_ts(),
        }),
    }
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
            events::emit(&Event::Error {
                stage: "parse".to_string(),
                msg: err.to_string(),
                input: Some(line.to_string()),
                ts: now_ts(),
            });
            return false;
        }
    };

    match dispatch_command(cfg, watchers, state, &cmd).await {
        Ok(DispatchOutcome::Handled) => false,
        Ok(DispatchOutcome::Continue) => {
            handle_screen_wake(cfg, watchers, state, Wake::Command, false).await;
            false
        }
        Ok(DispatchOutcome::ScreenOnly) => {
            handle_screen_wake(cfg, watchers, state, Wake::Command, true).await;
            false
        }
        Ok(DispatchOutcome::Quit) => true,
        Err(err) => {
            events::emit(&Event::Error {
                stage: "dispatch".to_string(),
                msg: err.to_string(),
                input: Some(cmd.to_string()),
                ts: now_ts(),
            });
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
            emit_json(json!({"type":"action","cmd":"quit"}));
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
            emit_json(json!({"type":"action","cmd":"double_tap","x":x,"y":y}));
        }
        "long_tap" => {
            let x = req_i32(cmd, "x")?;
            let y = req_i32(cmd, "y")?;
            let duration_ms = duration_ms(cmd, 600)?;
            cfg.client.long_tap(x, y, duration_ms).await?;
            emit_json(
                json!({"type":"action","cmd":"long_tap","x":x,"y":y,"duration_ms":duration_ms}),
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
            emit_json(
                json!({"type":"action","cmd":op,"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
            );
        }
        "swipe_ext" => {
            let direction = req_str(cmd, "direction")?;
            let scale = opt_f32(cmd, "scale")?.unwrap_or(0.9);
            let duration_ms = duration_ms(cmd, 200)?;
            cfg.client.swipe_ext(direction, scale, duration_ms).await?;
            emit_json(
                json!({"type":"action","cmd":"swipe_ext","direction":direction,"scale":scale,"duration_ms":duration_ms}),
            );
        }
        "key" => {
            let name = req_str(cmd, "name")?;
            let injected = cfg.client.key(name).await?;
            emit_json(json!({"type":"action","cmd":"key","name":name,"injected":injected}));
        }
        "text" => {
            let value = req_str(cmd, "value")?;
            let clear = opt_bool(cmd, "clear").unwrap_or(false);
            let target = selector_query_from_cmd(cmd)?;
            cfg.client
                .text_with_target(value, clear, target.as_ref())
                .await?;
            emit_json(
                json!({"type":"action","cmd":"text","value":value,"clear":clear,"target":target}),
            );
        }
        "launch" => {
            let package = req_str(cmd, "package")?;
            cfg.client.app_start(package).await?;
            emit_json(json!({"type":"action","cmd":"launch","package":package}));
        }
        "stop" => {
            let package = req_str(cmd, "package")?;
            cfg.client.app_stop(package).await?;
            emit_json(json!({"type":"action","cmd":"stop","package":package}));
        }
        "app_clear" => {
            let package = req_str(cmd, "package")?;
            cfg.client.app_clear(package).await?;
            emit_json(json!({"type":"action","cmd":"app_clear","package":package}));
        }
        "app_wait" => {
            let package = req_str(cmd, "package")?;
            let timeout_ms = timeout_ms(cmd, 20_000)?;
            let front = opt_bool(cmd, "front").unwrap_or(false);
            let r = cfg.client.app_wait(package, timeout_ms, front).await?;
            emit_json(
                json!({"type":"action","cmd":"app_wait","package":package,"matched":r.matched,"current":r.current}),
            );
        }
        "app_info" => {
            let package = req_str(cmd, "package")?;
            let info = cfg.client.app_info(package).await?;
            emit_json(json!({
                "type":"action","cmd":"app_info","package":package,
                "version_name":info.version_name,"version_code":info.version_code,"label":info.label
            }));
        }
        "wait_activity" => {
            let name = req_str(cmd, "name")?;
            let timeout_ms = timeout_ms(cmd, 10_000)?;
            wait_activity(&cfg.client, name, timeout_ms).await?;
        }
        "screenshot" => {
            let path = cmd.get("path").and_then(Value::as_str).map(String::from);
            let (path, bytes) = write_screenshot(&cfg.client, path).await?;
            emit_json(json!({"type":"action","cmd":"screenshot","path":path,"bytes":bytes}));
        }
        "shell" => {
            let value = req_any_str(cmd, &["value", "input", "cmdline"])?;
            let timeout_ms = timeout_ms(cmd, 30_000)?;
            let r = cfg.client.shell(value, timeout_ms).await?;
            emit_json(
                json!({"type":"action","cmd":"shell","input":r.input,"output":r.output,"exit_code":r.exit_code}),
            );
        }
        "screen_on" => {
            cfg.client.screen_on().await?;
            emit_json(json!({"type":"action","cmd":"screen_on"}));
        }
        "screen_off" => {
            cfg.client.screen_off().await?;
            emit_json(json!({"type":"action","cmd":"screen_off"}));
        }
        "unlock" => {
            cfg.client.unlock().await?;
            emit_json(json!({"type":"action","cmd":"unlock"}));
        }
        "wakeup" => {
            cfg.client.wakeup().await?;
            emit_json(json!({"type":"action","cmd":"wakeup"}));
        }
        "orientation" => {
            let value = cfg.client.orientation_get().await?;
            emit_json(json!({"type":"action","cmd":"orientation","value":value}));
        }
        "set_orientation" => {
            let value = req_str(cmd, "value")?;
            cfg.client.orientation_set(value).await?;
            emit_json(json!({"type":"action","cmd":"set_orientation","value":value}));
        }
        "clipboard" => {
            let value = cfg.client.clipboard_get().await?;
            emit_json(json!({"type":"action","cmd":"clipboard","value":value}));
        }
        "set_clipboard" => {
            let value = req_str(cmd, "value")?;
            cfg.client.clipboard_set(value).await?;
            emit_json(json!({"type":"action","cmd":"set_clipboard","value":value}));
        }
        "open_notification" => {
            cfg.client.open_notifications().await?;
            emit_json(json!({"type":"action","cmd":"open_notification"}));
        }
        "open_quick_settings" => {
            cfg.client.open_quick_settings().await?;
            emit_json(json!({"type":"action","cmd":"open_quick_settings"}));
        }
        "open_url" => {
            let url = req_str(cmd, "url")?;
            cfg.client.open_url(url).await?;
            emit_json(json!({"type":"action","cmd":"open_url","url":url}));
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
            emit_json(
                json!({"type":"action","cmd":base_op,"value":value,"x":r.x,"y":r.y,"action":r.action,"matched":r.matched}),
            );
            if op.ends_with("_and_wait") {
                wait_after_action(
                    cfg,
                    state,
                    cmd,
                    start_hash,
                    op,
                    Some(TapOutcome {
                        id: Some(r.matched.id),
                        x: r.x,
                        y: r.y,
                        source: r.action.unwrap_or_else(|| "server".to_string()),
                    }),
                )
                .await?;
                return Ok(DispatchOutcome::Handled);
            }
        }
        "xpath" => {
            let query = req_any_str(cmd, &["query", "value"])?;
            let r = cfg.client.xpath(query, false).await?;
            emit_json(
                json!({"type":"action","cmd":"xpath","query":query,"matched":r.matched,"elements":r.elements}),
            );
        }
        "xpath_tap" => {
            let query = req_any_str(cmd, &["query", "value"])?;
            let r = cfg.client.xpath_tap(query).await?;
            emit_json(
                json!({"type":"action","cmd":"xpath_tap","query":query,"x":r.x,"y":r.y,"action":r.action,"matched":r.matched}),
            );
        }
        "toast" => {
            let wait = timeout_ms(cmd, 5_000)?;
            let start = unix_ms();
            cfg.client.toast_start(50).await?;
            let deadline = std::time::Instant::now() + Duration::from_millis(wait as u64);
            loop {
                let recent = cfg.client.toast_recent(start).await?;
                if !recent.toasts.is_empty() || std::time::Instant::now() >= deadline {
                    emit_json(json!({"type":"action","cmd":"toast","toasts":recent.toasts}));
                    break;
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        "push" => {
            let local = req_str(cmd, "local")?;
            let remote = req_str(cmd, "remote")?;
            let bytes = std::fs::read(local).with_context(|| format!("reading {local}"))?;
            let r = cfg.client.push_file(remote, bytes).await?;
            emit_json(
                json!({"type":"action","cmd":"push","local":local,"remote":remote,"path":r.path,"bytes":r.bytes}),
            );
        }
        "pull" => {
            let remote = req_str(cmd, "remote")?;
            let local = req_str(cmd, "local")?;
            let bytes = cfg.client.pull_file(remote).await?;
            std::fs::write(local, &bytes).with_context(|| format!("writing {local}"))?;
            emit_json(
                json!({"type":"action","cmd":"pull","remote":remote,"local":local,"bytes":bytes.len() as u64}),
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
            emit_json(json!({"type":"action","cmd":"add_watcher","name":rule.name}));
        }
        "remove_watcher" => {
            let name = req_str(cmd, "name")?;
            let removed = watchers.remove(name);
            emit_json(
                json!({"type":"action","cmd":"remove_watcher","name":name,"removed":removed}),
            );
        }
        "list_watchers" => {
            emit_json(json!({"type":"action","cmd":"list_watchers","watchers":watchers.list()}));
        }
        "clear_watchers" => {
            watchers.clear();
            emit_json(json!({"type":"action","cmd":"clear_watchers"}));
        }
        "permission_dialogs" => {
            let policy = req_str(cmd, "policy")?;
            let policy = PermissionDialogPolicy::parse(policy)
                .ok_or_else(|| anyhow!("permission_dialogs policy must be ignore|allow|deny"))?;
            watchers.set_permission_dialog_policy(policy);
            emit_json(json!({
                "type":"action",
                "cmd":"permission_dialogs",
                "policy": policy.as_str()
            }));
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
        if let Some(expected_hash) = cmd.get("screen_hash").and_then(Value::as_str) {
            if state.last_hash.as_deref() != Some(expected_hash) {
                bail!(
                    "stale screen id {id}: expected screen_hash {expected_hash}, current cached screen_hash {:?}",
                    state.last_hash
                );
            }
        }
        let r = cfg
            .client
            .find_tap(&SelectorQuery {
                id: Some(id),
                ..Default::default()
            })
            .await?;
        let source = r.action.clone().unwrap_or_else(|| "server".to_string());
        emit_json(json!({
            "type":"action","cmd":action_cmd,"id":id,"x":r.x,"y":r.y,
            "source":source,
            "matched":r.matched
        }));
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
    emit_json(json!({"type":"action","cmd":action_cmd,"x":x,"y":y,"source":"coordinates"}));
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
        if let Some(filter) = &cfg.app_filter {
            if !should_emit_package(Some(filter.as_str()), screen.current_app.package.as_deref()) {
                if std::time::Instant::now() >= deadline {
                    emit_json(json!({
                        "type":"action","cmd":action_cmd,"matched":false,
                        "timeout":true,"reason":"app_filter"
                    }));
                    return Ok(());
                }
                sleep(Duration::from_millis(poll_ms as u64)).await;
                continue;
            }
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
            emit_json(json!({
                "type":"action",
                "cmd":action_cmd,
                "matched":done,
                "timeout": timed_out && !done,
                "screen_hash":screen.screen_hash,
                "hash_changed":hash_changed,
                "tap": tap.map(|tap| json!({
                    "id":tap.id,
                    "x":tap.x,
                    "y":tap.y,
                    "source":tap.source
                }))
            }));
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
            emit_json(
                json!({"type":"action","cmd":"wait_activity","name":name,"matched":true,"current":cur}),
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            emit_json(
                json!({"type":"action","cmd":"wait_activity","name":name,"matched":false,"current":cur}),
            );
            return Ok(());
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
            emit_json(json!({
                "type":"action","cmd":"wait_for","matched":matched,"gone":gone,
                "screen_hash":screen.screen_hash
            }));
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            emit_json(json!({
                "type":"action","cmd":"wait_for","matched":matched,"gone":gone,
                "screen_hash":screen.screen_hash,"timeout":true
            }));
            return Ok(());
        }
        sleep(Duration::from_millis(poll_ms as u64)).await;
    }
}

fn wait_query_matches(cmd: &Value, app: &AppRef, elements: &[Element]) -> bool {
    if let Some(package) = cmd.get("package").and_then(Value::as_str) {
        if !app.package.as_deref().unwrap_or("").contains(package) {
            return false;
        }
    }
    if let Some(activity) = cmd.get("activity").and_then(Value::as_str) {
        if !app.activity.as_deref().unwrap_or("").contains(activity) {
            return false;
        }
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
    let Some(expected) = expected else {
        return true;
    };
    actual
        .map(|actual| actual.to_lowercase().contains(&expected.to_lowercase()))
        .unwrap_or(false)
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
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok((path.display().to_string(), bytes.len() as u64))
}

fn emit_json(value: Value) {
    println!("{}", serde_json::to_string(&value).unwrap());
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
    use super::{debounce_delay_ms, should_emit_package, Wake};

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
    fn command_wakes_do_not_pay_debounce_delay() {
        assert_eq!(debounce_delay_ms(Wake::Command, 80), 0);
        assert_eq!(debounce_delay_ms(Wake::Poll, 80), 0);
        assert_eq!(debounce_delay_ms(Wake::Event, 80), 80);
        assert_eq!(debounce_delay_ms(Wake::Init, 80), 80);
    }
}
