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
    /// Diagnose (and optionally repair) the host↔device pipe: device state,
    /// APK version, port forward, server reachability, UiAutomation owners.
    Doctor {
        /// Attempt to repair the issues found.
        #[arg(long)]
        fix: bool,
        /// Allow --fix to kill a competing (non-ShadowDroid) UiAutomation owner.
        #[arg(long)]
        force: bool,
        /// Emit the report as a single JSON object instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Gather a self-contained diagnostic bundle (doctor report, device info,
    /// recent logcat + crash buffer, and — if the server is up — screen dump,
    /// screenshot, current activity, app info) into a directory.
    Collect {
        /// App package to include version/info for.
        #[arg(long)]
        app: Option<String>,
        /// Output directory (default: a temp dir under the OS temp path).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Skip the screenshot capture.
        #[arg(long)]
        no_screenshot: bool,
    },
    /// Grant one or more runtime permissions to a package, then report the
    /// observed state (verify-by-readback).
    PermGrant {
        package: String,
        #[arg(required = true)]
        perms: Vec<String>,
    },
    /// Revoke one or more runtime permissions from a package.
    PermRevoke {
        package: String,
        #[arg(required = true)]
        perms: Vec<String>,
    },
    /// List a package's runtime permission grant states.
    PermList {
        package: String,
    },
    /// Revoke all granted runtime permissions, returning the package to a
    /// fresh-install prompt state.
    PermReset {
        package: String,
    },
    /// Get appop mode(s) for a package (all ops, or one named op).
    AppopGet {
        package: String,
        op: Option<String>,
    },
    /// Set an appop mode (allow|deny|ignore|default|foreground|…).
    AppopSet {
        package: String,
        op: String,
        mode: String,
    },
    /// Install an APK and run the app-under-test setup ritual: optional clear /
    /// grant / launch / wait-for-front. Emits a structured per-step summary.
    AppInstall(crate::cmd::app_install::AppInstallArgs),
    /// Like app-install, but uninstall any existing copy first (crosses a
    /// signature change or wipes state).
    AppReinstall(crate::cmd::app_install::AppInstallArgs),
    /// Capture the device display profile (animation scales, font scale,
    /// density, size, rotation) as JSON, optionally to a file.
    ProfileSnapshot {
        /// Write the profile to this file (default: stdout only).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Apply a display profile: a preset (`automation` = animations off), a
    /// snapshot file, or individual flags.
    ProfileApply(crate::cmd::device_profile::ProfileApplyArgs),
    /// Reset the display profile to stock defaults (animations on, size/density
    /// reset, auto-rotate on).
    ProfileReset,

    // ── M2: inspection + gestures ─────────────────────────────
    Screen,
    Screenshot {
        path: Option<String>,
        /// Image format: png (default) or jpeg. Requires a 0.1.4+ server.
        #[arg(long)]
        format: Option<String>,
        /// Server-side downscale factor, e.g. 0.5. Requires a 0.1.4+ server.
        #[arg(long)]
        scale: Option<f32>,
        /// JPEG quality 1..100 (format=jpeg only). Requires a 0.1.4+ server.
        #[arg(long)]
        quality: Option<u32>,
    },
    /// One-shot detailed device info (model, fingerprint, locale, density).
    Device,
    /// Pinch in (zoom out) or out (zoom in) on the element matched by a
    /// selector. Requires a 0.1.4+ server.
    Pinch {
        #[arg(value_parser = ["in", "out"])]
        direction: String,
        #[arg(long)]
        rid: Option<String>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long, default_value_t = 50)]
        percent: u32,
    },
    /// Print the current foreground app (package / activity / pid).
    Current,
    /// List a directory on the device (within accessible storage).
    Ls {
        remote: String,
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
    /// Scroll a list until an element matching a selector is visible, then
    /// optionally tap it.
    ScrollTo(crate::cmd::scroll::ScrollArgs),
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
        // doctor must NOT go through ensure_ready — it diagnoses the very server
        // ensure_ready would start. collect handles its own (best-effort)
        // bring-up so it can degrade to host-side diagnostics when the server
        // can't start.
        Cmd::Doctor { fix, force, json } => {
            return crate::cmd::doctor::run(device.as_deref(), *fix, *force, *json).await
        }
        Cmd::Collect {
            app,
            out,
            no_screenshot,
        } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::collect::run(&serial, app.clone(), out.clone(), !*no_screenshot)
                .await;
        }
        // perm-*/appop-* are host-only (plain `adb shell`); they need a device
        // but not the on-device server.
        Cmd::PermGrant { package, perms } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::permissions::grant(&serial, package, perms).await;
        }
        Cmd::PermRevoke { package, perms } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::permissions::revoke(&serial, package, perms).await;
        }
        Cmd::PermList { package } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::permissions::list(&serial, package).await;
        }
        Cmd::PermReset { package } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::permissions::reset(&serial, package).await;
        }
        Cmd::AppopGet { package, op } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::permissions::appop_get(&serial, package, op.as_deref()).await;
        }
        Cmd::AppopSet { package, op, mode } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::permissions::appop_set(&serial, package, op, mode).await;
        }
        Cmd::AppInstall(args) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::app_install::run(&serial, args, false).await;
        }
        Cmd::AppReinstall(args) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::app_install::run(&serial, args, true).await;
        }
        Cmd::ProfileSnapshot { out } => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::device_profile::snapshot(&serial, out.as_ref()).await;
        }
        Cmd::ProfileApply(args) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::device_profile::apply(&serial, args).await;
        }
        Cmd::ProfileReset => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::device_profile::reset(&serial).await;
        }
        _ => {}
    }

    // Shared setup for all UI verbs.
    let serial = resolve_serial(device.as_deref()).await?;
    let client = installer::ensure_ready(&serial, apk.as_deref(), any_apk_version).await?;

    match cmd {
        Cmd::Devices
        | Cmd::Connect
        | Cmd::Disconnect
        | Cmd::Update { .. }
        | Cmd::Doctor { .. }
        | Cmd::Collect { .. }
        | Cmd::PermGrant { .. }
        | Cmd::PermRevoke { .. }
        | Cmd::PermList { .. }
        | Cmd::PermReset { .. }
        | Cmd::AppopGet { .. }
        | Cmd::AppopSet { .. }
        | Cmd::AppInstall(..)
        | Cmd::AppReinstall(..)
        | Cmd::ProfileSnapshot { .. }
        | Cmd::ProfileApply(..)
        | Cmd::ProfileReset => unreachable!(), // handled above

        // ── inspection ─────────────────────────────────────────
        Cmd::Screen => emit(&client.screen().await?),
        Cmd::Screenshot {
            path,
            format,
            scale,
            quality,
        } => cmd_screenshot(&client, path, format, scale, quality).await?,
        Cmd::Device => cmd_device(&client, &serial).await?,
        Cmd::Pinch {
            direction,
            rid,
            text,
            desc,
            percent,
        } => {
            client
                .pinch(
                    rid.as_deref(),
                    text.as_deref(),
                    desc.as_deref(),
                    &direction,
                    percent,
                )
                .await?;
            emit_action(
                "pinch",
                &serde_json::json!({"direction":direction,"rid":rid,"text":text,"desc":desc,"percent":percent}),
            );
        }
        Cmd::Current => {
            let cur = client.app_current().await?;
            emit_action("current", &serde_json::to_value(&cur).unwrap_or_default());
        }
        Cmd::Ls { remote } => {
            let r = client.list_dir(&remote).await?;
            emit_action(
                "ls",
                &serde_json::json!({"remote":remote,"entries":r.entries}),
            );
        }

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
        Cmd::ScrollTo(args) => crate::cmd::scroll::run(&client, &args).await?,

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
            let bytes_len = bytes.len() as u64;
            // Server first (app-accessible storage); fall back to `adb push` for
            // paths the instrumentation uid can't write (e.g. /sdcard under
            // scoped storage).
            match client.push_file(&remote, bytes).await {
                Ok(r) => emit_action(
                    "push",
                    &serde_json::json!({"local":local,"remote":remote,"path":r.path,"bytes":r.bytes,"mode":r.mode,"requested_mode":mode,"via":"server"}),
                ),
                Err(_) => {
                    adb::push(&serial, std::path::PathBuf::from(&local), remote.clone()).await?;
                    emit_action(
                        "push",
                        &serde_json::json!({"local":local,"remote":remote,"bytes":bytes_len,"via":"adb"}),
                    );
                }
            }
        }
        Cmd::Pull { remote, local } => {
            // Server first; fall back to `adb pull` for paths it can't read.
            let (bytes, via) = match client.pull_file(&remote).await {
                Ok(b) => (b, "server"),
                Err(_) => (adb::pull(&serial, remote.clone()).await?, "adb"),
            };
            std::fs::write(&local, &bytes).with_context(|| format!("writing {local}"))?;
            emit_action(
                "pull",
                &serde_json::json!({"remote":remote,"local":local,"bytes":bytes.len() as u64,"via":via}),
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

async fn cmd_device(client: &ServerClient, serial: &str) -> Result<()> {
    match client.device().await {
        // 0.1.4+ server: rich device facts.
        Ok(d) => emit_action("device", &serde_json::to_value(&d).unwrap_or_default()),
        // Older server without /v1/device: fall back to /state + getprop.
        Err(_) => {
            let state = client.state().await?;
            let getprop = adb::device_info(serial).await;
            emit_action(
                "device",
                &serde_json::json!({
                    "source": "fallback",
                    "android_release": state.android_release,
                    "android_sdk": state.android_sdk,
                    "getprop": getprop,
                }),
            );
        }
    }
    Ok(())
}

async fn cmd_screenshot(
    client: &ServerClient,
    path: Option<String>,
    format: Option<String>,
    scale: Option<f32>,
    quality: Option<u32>,
) -> Result<()> {
    let bytes = client.screenshot(format.as_deref(), scale, quality).await?;
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
    // Device prep: disable the Android 14+ stylus-handwriting tutorial that
    // otherwise hijacks the first text-field focus and breaks `text` input.
    // Best-effort + idempotent; surfaced in the output rather than done silently.
    let stylus_tutorial_disabled =
        crate::cmd::device_profile::disable_stylus_tutorial(&serial).await;
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
        "device_prep": {"stylus_tutorial_disabled": stylus_tutorial_disabled},
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
