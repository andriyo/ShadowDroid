//! Argument parsing + subcommand dispatch.
//!
//! M1 implements:
//!   - `devices` — list attached emulators / phones
//!   - `connect` — install APK, start server, verify with /v1/state
//!   - `disconnect` — stop server, remove port forward
//!
//! M2 implements one-shot inspection/action verbs. M3 implements `watch`.
//! M4 adds selectors, toasts, files, and declarative popup watchers.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

use crate::device::client::ServerClient;
use crate::device::{adb, installer};
use crate::events::ScreenFormat;
use crate::proto::{Element, SelectorQuery};
use crate::watch::watcher::PermissionDialogPolicy;

#[derive(Parser)]
#[command(
    name = "shadowdroid",
    version,
    about = "Drive Android apps — streaming JSON events, structured crashes, declarative popup watchers"
)]
pub struct Cli {
    /// ADB serial. Defaults to $SHADOWDROID_DEVICE / $ANDROID_SERIAL / sole attached device.
    #[arg(short, long, global = true, env = "SHADOWDROID_DEVICE")]
    pub device: Option<String>,

    /// Local APK to install instead of fetching from GitHub releases. See
    /// docs/development.md for the precedence chain. Can be either:
    ///   • a path to the test APK (e.g., app-debug-androidTest.apk); the
    ///     sibling main APK is auto-discovered in the same directory tree
    ///   • a directory containing both app-debug.apk and app-debug-androidTest.apk
    #[arg(long, global = true, env = "SHADOWDROID_APK", value_name = "PATH")]
    pub apk: Option<PathBuf>,

    /// Skip the version check when installing — assume any provided/discovered APK
    /// is the right one. Implied by --apk; you only need this explicitly to override
    /// the cached download flow during local development without --apk.
    #[arg(long, global = true, env = "SHADOWDROID_ANY_APK_VERSION")]
    pub any_apk_version: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    // ── M1 ────────────────────────────────────────────────────
    Devices,
    Connect,
    Disconnect,
    Update {
        /// Check whether this CLI is older than the latest GitHub Release.
        #[arg(long)]
        check: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    // ── M2: inspection + gestures ─────────────────────────────
    Screen,
    Screenshot {
        path: Option<String>,
    },
    Tap {
        a: i32,
        b: Option<i32>,
    },
    DoubleTap {
        x: i32,
        y: i32,
    },
    LongTap {
        x: i32,
        y: i32,
        #[arg(long, default_value_t = 600)]
        duration_ms: u32,
    },
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(long, default_value_t = 200)]
        duration_ms: u32,
    },
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(long, default_value_t = 500)]
        duration_ms: u32,
    },
    SwipeExt {
        #[arg(value_parser = ["up", "down", "left", "right"])]
        direction: String,
        #[arg(long, default_value_t = 0.9)]
        scale: f32,
        #[arg(long, default_value_t = 200)]
        duration_ms: u32,
    },
    TapText {
        value: String,
    },
    TapRid {
        value: String,
    },
    TapDesc {
        value: String,
    },
    Xpath {
        query: String,
    },
    XpathTap {
        query: String,
    },
    Back,
    Home,
    Key {
        name: String,
    },
    Text {
        value: String,
        #[arg(long)]
        clear: bool,
    },
    Launch {
        package: String,
    },
    Stop {
        package: String,
    },
    AppClear {
        package: String,
    },
    AppWait {
        package: String,
        #[arg(long, default_value_t = 20000)]
        timeout_ms: u32,
        #[arg(long)]
        front: bool,
    },
    AppInfo {
        package: String,
    },
    WaitActivity {
        name: String,
        #[arg(long, default_value_t = 10000)]
        timeout_ms: u32,
    },
    Shell {
        cmd: String,
        #[arg(long, default_value_t = 30000)]
        timeout_ms: u32,
    },
    ScreenOn,
    ScreenOff,
    Unlock,
    Wakeup,
    Orientation {
        value: Option<String>,
    },
    Clipboard {
        value: Option<String>,
    },
    Notifications,
    QuickSettings,
    OpenUrl {
        url: String,
    },
    Push {
        local: String,
        remote: String,
        #[arg(long, default_value_t = 0o644)]
        mode: u32,
    },
    Pull {
        remote: String,
        local: String,
    },
    Toast {
        #[arg(long, default_value_t = 5000)]
        wait_ms: u32,
    },
    WaitFor {
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        rid: Option<String>,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long)]
        klass: Option<String>,
        #[arg(long)]
        activity: Option<String>,
        #[arg(long)]
        package: Option<String>,
        #[arg(long)]
        gone: bool,
        #[arg(long, default_value_t = 10000)]
        timeout_ms: u32,
        #[arg(long, default_value_t = 200)]
        poll_ms: u32,
    },

    // ── M3: streaming ─────────────────────────────────────────
    Watch {
        #[arg(long)]
        app: Option<String>,
        #[arg(long, default_value_t = 1000)]
        poll_ms: u32,
        #[arg(long, default_value_t = 80)]
        debounce_ms: u32,
        #[arg(long)]
        no_stdin: bool,
        #[arg(long)]
        no_crash_detect: bool,
        /// Screen event payload shape. `compact` is the default for fast agent
        /// parsing; use `full` when you need bounds and every UIAutomator flag.
        #[arg(long, value_enum, default_value_t = ScreenFormat::Compact)]
        screen_format: ScreenFormat,
        /// Built-in Android permission dialog policy.
        ///
        /// `allow` taps PermissionController allow buttons; `deny` taps deny buttons.
        #[arg(long, value_enum, default_value_t = PermissionDialogPolicy::Ignore)]
        permission_dialogs: PermissionDialogPolicy,
        #[arg(long)]
        watcher_file: Vec<String>,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let device = cli.device;
    let apk = cli.apk;
    let any_apk_version = cli.any_apk_version;
    let cmd = cli.cmd;
    // For everything beyond `devices` we need an on-device server. ensure_ready
    // installs/starts as needed (no-op if already running).
    match &cmd {
        Cmd::Devices => return cmd_devices().await,
        Cmd::Connect => {
            return cmd_connect(device.as_deref(), apk.as_deref(), any_apk_version).await
        }
        Cmd::Disconnect => return cmd_disconnect(device.as_deref()).await,
        Cmd::Update { check, json } => return crate::update::cmd_update(*check, *json).await,
        _ => {}
    }

    // Shared setup for all UI verbs.
    let serial = resolve_serial(device.as_deref()).await?;
    let client = installer::ensure_ready(&serial, apk.as_deref(), any_apk_version).await?;

    match cmd {
        Cmd::Devices | Cmd::Connect | Cmd::Disconnect | Cmd::Update { .. } => unreachable!(), // handled above

        // ── inspection ─────────────────────────────────────────
        Cmd::Screen => emit(&client.screen().await?),
        Cmd::Screenshot { path } => cmd_screenshot(&client, path).await?,

        // ── gestures ───────────────────────────────────────────
        Cmd::Tap { a, b } => cmd_tap(&client, a, b).await?,
        Cmd::DoubleTap { x, y } => {
            client.double_tap(x, y).await?;
            emit_action("double_tap", &serde_json::json!({"x":x,"y":y}));
        }
        Cmd::LongTap { x, y, duration_ms } => {
            client.long_tap(x, y, duration_ms).await?;
            emit_action(
                "long_tap",
                &serde_json::json!({"x":x,"y":y,"duration_ms":duration_ms}),
            );
        }
        Cmd::Swipe {
            x1,
            y1,
            x2,
            y2,
            duration_ms,
        } => {
            client.swipe(x1, y1, x2, y2, duration_ms).await?;
            emit_action(
                "swipe",
                &serde_json::json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
            );
        }
        Cmd::Drag {
            x1,
            y1,
            x2,
            y2,
            duration_ms,
        } => {
            client.drag(x1, y1, x2, y2, duration_ms).await?;
            emit_action(
                "drag",
                &serde_json::json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
            );
        }
        Cmd::SwipeExt {
            direction,
            scale,
            duration_ms,
        } => {
            client.swipe_ext(&direction, scale, duration_ms).await?;
            emit_action(
                "swipe_ext",
                &serde_json::json!({"direction":direction,"scale":scale,"duration_ms":duration_ms}),
            );
        }
        Cmd::TapText { value } => {
            cmd_find_tap(
                &client,
                "tap_text",
                SelectorQuery {
                    text: Some(value),
                    ..Default::default()
                },
            )
            .await?
        }
        Cmd::TapRid { value } => {
            cmd_find_tap(
                &client,
                "tap_rid",
                SelectorQuery {
                    rid: Some(value),
                    ..Default::default()
                },
            )
            .await?
        }
        Cmd::TapDesc { value } => {
            cmd_find_tap(
                &client,
                "tap_desc",
                SelectorQuery {
                    desc: Some(value),
                    ..Default::default()
                },
            )
            .await?
        }
        Cmd::Xpath { query } => {
            let r = client.xpath(&query, false).await?;
            emit_action(
                "xpath",
                &serde_json::json!({"query":query,"matched":r.matched,"elements":r.elements}),
            );
        }
        Cmd::XpathTap { query } => {
            let r = client.xpath_tap(&query).await?;
            emit_action(
                "xpath_tap",
                &serde_json::json!({"query":query,"x":r.x,"y":r.y,"matched":r.matched}),
            );
        }

        // ── keys + text ────────────────────────────────────────
        Cmd::Back => {
            client.key("back").await?;
            emit_action("key", &serde_json::json!({"name":"back"}));
        }
        Cmd::Home => {
            client.key("home").await?;
            emit_action("key", &serde_json::json!({"name":"home"}));
        }
        Cmd::Key { name } => {
            client.key(&name).await?;
            emit_action("key", &serde_json::json!({"name":name}));
        }
        Cmd::Text { value, clear } => {
            client.text(&value, clear).await?;
            emit_action("text", &serde_json::json!({"value":value,"clear":clear}));
        }

        // ── app lifecycle ──────────────────────────────────────
        Cmd::Launch { package } => {
            client.app_start(&package).await?;
            emit_action("launch", &serde_json::json!({"package":package}));
        }
        Cmd::Stop { package } => {
            client.app_stop(&package).await?;
            emit_action("stop", &serde_json::json!({"package":package}));
        }
        Cmd::AppClear { package } => {
            client.app_clear(&package).await?;
            emit_action("app_clear", &serde_json::json!({"package":package}));
        }
        Cmd::AppWait {
            package,
            timeout_ms,
            front,
        } => {
            let r = client.app_wait(&package, timeout_ms, front).await?;
            emit_action(
                "app_wait",
                &serde_json::json!({"package":package,"matched":r.matched,"current":r.current}),
            );
        }
        Cmd::AppInfo { package } => {
            let info = client.app_info(&package).await?;
            emit_action(
                "app_info",
                &serde_json::json!({
                    "package":package,
                    "version_name":info.version_name,
                    "version_code":info.version_code,
                    "label":info.label,
                }),
            );
        }
        Cmd::WaitActivity { name, timeout_ms } => {
            cmd_wait_activity(&client, &name, timeout_ms).await?;
        }

        // ── system ─────────────────────────────────────────────
        Cmd::Shell { cmd, timeout_ms } => {
            let r = client.shell(&cmd, timeout_ms).await?;
            emit_action(
                "shell",
                &serde_json::json!({
                    "input":r.input,"output":r.output,"exit_code":r.exit_code
                }),
            );
        }
        Cmd::ScreenOn => {
            client.screen_on().await?;
            emit_action("screen_on", &serde_json::Value::Null);
        }
        Cmd::ScreenOff => {
            client.screen_off().await?;
            emit_action("screen_off", &serde_json::Value::Null);
        }
        Cmd::Unlock => {
            client.unlock().await?;
            emit_action("unlock", &serde_json::Value::Null);
        }
        Cmd::Wakeup => {
            client.wakeup().await?;
            emit_action("wakeup", &serde_json::Value::Null);
        }
        Cmd::Orientation { value } => match value {
            None => emit_action(
                "orientation",
                &serde_json::json!({"value": client.orientation_get().await?}),
            ),
            Some(v) => {
                client.orientation_set(&v).await?;
                emit_action("set_orientation", &serde_json::json!({"value":v}));
            }
        },
        Cmd::Clipboard { value } => match value {
            None => emit_action(
                "clipboard",
                &serde_json::json!({"value": client.clipboard_get().await?}),
            ),
            Some(v) => {
                client.clipboard_set(&v).await?;
                emit_action("set_clipboard", &serde_json::json!({"value":v}));
            }
        },
        Cmd::Notifications => {
            client.open_notifications().await?;
            emit_action("open_notification", &serde_json::Value::Null);
        }
        Cmd::QuickSettings => {
            client.open_quick_settings().await?;
            emit_action("open_quick_settings", &serde_json::Value::Null);
        }
        Cmd::OpenUrl { url } => {
            client.open_url(&url).await?;
            emit_action("open_url", &serde_json::json!({"url":url}));
        }
        Cmd::Push {
            local,
            remote,
            mode,
        } => {
            let bytes = std::fs::read(&local).with_context(|| format!("reading {local}"))?;
            let r = client.push_file(&remote, bytes).await?;
            emit_action(
                "push",
                &serde_json::json!({"local":local,"remote":remote,"path":r.path,"bytes":r.bytes,"mode":r.mode,"requested_mode":mode}),
            );
        }
        Cmd::Pull { remote, local } => {
            let bytes = client.pull_file(&remote).await?;
            std::fs::write(&local, &bytes).with_context(|| format!("writing {local}"))?;
            emit_action(
                "pull",
                &serde_json::json!({"remote":remote,"local":local,"bytes":bytes.len() as u64}),
            );
        }
        Cmd::Toast { wait_ms } => {
            cmd_toast(&client, wait_ms).await?;
        }
        Cmd::WaitFor {
            text,
            rid,
            desc,
            klass,
            activity,
            package,
            gone,
            timeout_ms,
            poll_ms,
        } => {
            cmd_wait_for(
                &client,
                WaitForQuery {
                    text,
                    rid,
                    desc,
                    klass,
                    activity,
                    package,
                },
                gone,
                timeout_ms,
                poll_ms,
            )
            .await?;
        }
        Cmd::Watch {
            app,
            poll_ms,
            debounce_ms,
            no_stdin,
            no_crash_detect,
            screen_format,
            permission_dialogs,
            watcher_file,
        } => {
            crate::watch::r#loop::run(crate::watch::r#loop::WatchConfig {
                serial,
                client,
                app_filter: app,
                poll_ms: poll_ms.max(1),
                debounce_ms,
                accept_stdin: !no_stdin,
                detect_crashes: !no_crash_detect,
                watcher_files: watcher_file,
                permission_dialog_policy: permission_dialogs,
                screen_format,
            })
            .await?;
        } // ── deferred / M2-OUT ──────────────────────────────────
    }
    Ok(())
}

// ── emit helpers ────────────────────────────────────────────

fn emit(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string(v).unwrap());
}
fn emit_action(cmd: &str, body: &serde_json::Value) {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), serde_json::Value::String("action".into()));
    m.insert("cmd".into(), serde_json::Value::String(cmd.into()));
    if let serde_json::Value::Object(b) = body {
        for (k, v) in b {
            m.insert(k.clone(), v.clone());
        }
    }
    println!(
        "{}",
        serde_json::to_string(&serde_json::Value::Object(m)).unwrap()
    );
}

// ── specific handlers ──────────────────────────────────────────

async fn cmd_screenshot(client: &ServerClient, path: Option<String>) -> Result<()> {
    let bytes = client.screenshot_png().await?;
    let p: std::path::PathBuf = match path {
        Some(p) => p.into(),
        None => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            std::env::temp_dir().join(format!("shadowdroid-screenshot-{ts}.png"))
        }
    };
    std::fs::write(&p, &bytes).with_context(|| format!("writing {}", p.display()))?;
    emit_action(
        "screenshot",
        &serde_json::json!({
            "path": p.display().to_string(),
            "bytes": bytes.len() as u64,
        }),
    );
    Ok(())
}

/// `shadowdroid tap N` — id from a fresh dump. `shadowdroid tap X Y` — coords.
async fn cmd_tap(client: &ServerClient, a: i32, b: Option<i32>) -> Result<()> {
    match b {
        Some(y) => {
            client.tap_xy(a, y).await?;
            emit_action("tap", &serde_json::json!({"x":a,"y":y}));
        }
        None => {
            let id = u32::try_from(a).map_err(|_| anyhow!("element id must be >= 0, got {a}"))?;
            let screen = client.screen().await?;
            let el = screen.elements.iter().find(|e| e.id == id).ok_or_else(|| {
                anyhow!("element id {id} out of range (0..{})", screen.element_count)
            })?;
            let [x, y] = el.tap;
            client.tap_xy(x, y).await?;
            emit_action(
                "tap",
                &serde_json::json!({
                    "id": id, "x": x, "y": y,
                    "matched": {"text": el.text, "rid": el.rid, "desc": el.desc}
                }),
            );
        }
    }
    Ok(())
}

/// Poll `app_current` until the activity (or its substring) matches.
async fn cmd_wait_activity(client: &ServerClient, name: &str, timeout_ms: u32) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    let mut last_app: Option<crate::proto::AppRef>;
    loop {
        let cur = client.app_current().await?;
        let activity = cur.activity.as_deref().unwrap_or("");
        if activity.contains(name) {
            emit_action(
                "wait_activity",
                &serde_json::json!({
                    "name":name,"matched":true,"current":cur,
                }),
            );
            return Ok(());
        }
        last_app = Some(cur);
        if std::time::Instant::now() >= deadline {
            emit_action(
                "wait_activity",
                &serde_json::json!({
                    "name":name,"matched":false,"current":last_app,
                }),
            );
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

async fn cmd_find_tap(client: &ServerClient, cmd: &str, query: SelectorQuery) -> Result<()> {
    let r = client.find_tap(&query).await?;
    emit_action(
        cmd,
        &serde_json::json!({"x":r.x,"y":r.y,"matched":r.matched}),
    );
    Ok(())
}

async fn cmd_toast(client: &ServerClient, wait_ms: u32) -> Result<()> {
    let start = unix_ms();
    client.toast_start(50).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms as u64);
    loop {
        let recent = client.toast_recent(start).await?;
        if !recent.toasts.is_empty() || std::time::Instant::now() >= deadline {
            emit_action("toast", &serde_json::json!({"toasts":recent.toasts}));
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

struct WaitForQuery {
    text: Option<String>,
    rid: Option<String>,
    desc: Option<String>,
    klass: Option<String>,
    activity: Option<String>,
    package: Option<String>,
}

async fn cmd_wait_for(
    client: &ServerClient,
    query: WaitForQuery,
    gone: bool,
    timeout_ms: u32,
    poll_ms: u32,
) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    loop {
        let screen = client.screen().await?;
        let matched = wait_query_matches(&query, &screen.current_app, &screen.elements);
        let screen_hash = screen.screen_hash;
        if matched != gone {
            emit_action(
                "wait_for",
                &serde_json::json!({"matched":matched,"gone":gone,"screen_hash":screen_hash}),
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            emit_action(
                "wait_for",
                &serde_json::json!({"matched":matched,"gone":gone,"screen_hash":screen_hash,"timeout":true}),
            );
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms.max(1) as u64)).await;
    }
}

fn wait_query_matches(
    query: &WaitForQuery,
    app: &crate::proto::AppRef,
    elements: &[Element],
) -> bool {
    if let Some(package) = &query.package {
        if !app.package.as_deref().unwrap_or("").contains(package) {
            return false;
        }
    }
    if let Some(activity) = &query.activity {
        if !app.activity.as_deref().unwrap_or("").contains(activity) {
            return false;
        }
    }
    let has_element_query = query.text.is_some()
        || query.rid.is_some()
        || query.desc.is_some()
        || query.klass.is_some();
    if !has_element_query {
        return true;
    }
    elements.iter().any(|el| {
        selector_string_matches(el.text.as_deref(), query.text.as_deref())
            && selector_string_matches(el.rid.as_deref(), query.rid.as_deref())
            && selector_string_matches(el.desc.as_deref(), query.desc.as_deref())
            && selector_string_matches(el.klass.as_deref(), query.klass.as_deref())
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

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── M1 subcommands ───────────────────────────────────────────

async fn cmd_devices() -> Result<()> {
    let devices = adb::list_devices().await?;
    if devices.is_empty() {
        eprintln!("no devices attached (start an emulator or plug in a phone)");
    } else {
        for d in devices {
            println!("{d}");
        }
    }
    Ok(())
}

async fn cmd_connect(
    device: Option<&str>,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
) -> Result<()> {
    let serial = resolve_serial(device).await?;
    let client = installer::ensure_ready(&serial, apk, any_apk_version).await?;
    let state = client.state().await?;
    let out = json!({
        "type": "connected",
        "device": serial,
        "server_version": state.server_version,
        "api_version": state.api_version,
        "ui_automator_version": state.ui_automator_version,
        "android_sdk": state.android_sdk,
        "android_release": state.android_release,
        "viewport": {"w": state.viewport.w, "h": state.viewport.h},
        "current_app": state.current_app,
    });
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(())
}

async fn cmd_disconnect(device: Option<&str>) -> Result<()> {
    let serial = resolve_serial(device).await?;
    adb::am_force_stop(&serial, installer::TEST_PACKAGE).await?;
    adb::am_force_stop(&serial, installer::APP_PACKAGE).await?;
    adb::kill_instrument_zombies(&serial).await?;
    // Best-effort remove forward; ignore error if it wasn't set.
    let _ = adb::forward_remove(&serial, installer::DEFAULT_PORT).await;
    let out = json!({"type": "disconnected", "device": serial});
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(())
}

async fn resolve_serial(explicit: Option<&str>) -> Result<String> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    let devices = adb::list_devices().await.context("listing devices")?;
    match devices.len() {
        0 => Err(anyhow!("no Android devices attached. Run `shadowdroid devices` to check.")),
        1 => Ok(devices.into_iter().next().unwrap()),
        _ => Err(anyhow!(
            "multiple devices attached ({}). Pass --device <serial> or set $SHADOWDROID_DEVICE.\nattached: {}",
            devices.len(),
            devices.join(", ")
        )),
    }
}
