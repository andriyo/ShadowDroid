//! Argument parsing + subcommand dispatch.
//!
//! Command surface follows a noun-namespace model:
//!   - **Interaction primitives stay flat** — gestures (`tap`, `swipe`, …),
//!     input (`text`, `key`, `back`, `home`), reads (`screen`, `screenshot`),
//!     locate (`find`), `scroll-to`, sync/stream (`wait`, `watch`, `toast`),
//!     and session/diagnostics (`connect`, `doctor`, `collect`, …).
//!   - **Resources are nested** under a noun: `app`, `perm`, `appops`,
//!     `profile`, `device`, `files` (e.g. `app install`, `perm grant`,
//!     `device shell`, `files pull`).
//!
//! Dispatch is two-phase: host-only commands (no on-device server) run first
//! and return; everything else shares one `ensure_ready` bring-up, then routes
//! through per-namespace sub-dispatchers.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

use crate::cmd::app_install::AppInstallArgs;
use crate::cmd::device_profile::ProfileApplyArgs;
use crate::cmd::scroll::ScrollArgs;
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
    // ── session / diagnostics (flat) ──────────────────────────
    /// List attached devices / emulators.
    Devices,
    /// Install the server APK, start it, and verify (also disables the stylus tutorial).
    Connect,
    /// Stop the server and remove the port forward.
    Disconnect,
    /// Check whether this CLI is older than the latest GitHub Release.
    Update {
        #[arg(long)]
        check: bool,
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
        #[arg(long)]
        app: Option<String>,
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        #[arg(long)]
        no_screenshot: bool,
    },
    /// Emit the full command catalog (machine-readable self-introspection for agents).
    Commands {
        /// Emit JSON instead of a human tree.
        #[arg(long)]
        json: bool,
    },
    /// Generate an agent-integration file (Claude Code / Cursor / Codex).
    Skill(crate::cmd::skill::SkillArgs),
    /// Operate Android Studio debugger sessions through the ShadowDroid plugin.
    #[command(alias = "debug")]
    Debugger(crate::cmd::debugger::DebuggerArgs),

    // ── resource namespaces (nested) ──────────────────────────
    /// Application lifecycle, info, and install rituals.
    #[command(subcommand)]
    App(AppCmd),
    /// Runtime permission grants.
    #[command(subcommand)]
    Perm(PermCmd),
    /// App-ops (allow|deny|ignore|default|… per operation).
    #[command(subcommand)]
    Appops(AppopsCmd),
    /// Device display profile (animations, font, density, size, rotation).
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Device & system controls (info, shell, power, orientation, clipboard, …).
    #[command(subcommand)]
    Device(DeviceCmd),
    /// On-device file operations.
    #[command(subcommand)]
    Files(FilesCmd),

    // ── UI read (flat) ────────────────────────────────────────
    /// Dump the current UI as a flat element list.
    Screen,
    /// Capture a screenshot to a file.
    Screenshot {
        path: Option<String>,
        /// Image format: png (default) or jpeg.
        #[arg(long)]
        format: Option<String>,
        /// Server-side downscale factor, e.g. 0.5.
        #[arg(long)]
        scale: Option<f32>,
        /// JPEG quality 1..100 (format=jpeg only).
        #[arg(long)]
        quality: Option<u32>,
    },

    // ── gestures (flat) ───────────────────────────────────────
    /// Tap by element id, coordinates, or a selector (--text/--rid/--desc/--xpath).
    Tap {
        /// Element id (from `screen`) or X coordinate.
        a: Option<i32>,
        /// Y coordinate (with X for a coordinate tap).
        b: Option<i32>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        rid: Option<String>,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long)]
        xpath: Option<String>,
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
    /// Pinch in (zoom out) or out (zoom in) on the element matched by a selector.
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
    /// Scroll a list until a selector is visible, then optionally tap it.
    ScrollTo(ScrollArgs),

    // ── locate (flat) ─────────────────────────────────────────
    /// Find elements by selector (--text/--rid/--desc/--xpath); does not tap.
    Find {
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        rid: Option<String>,
        #[arg(long)]
        desc: Option<String>,
        #[arg(long)]
        xpath: Option<String>,
        /// Return all matches instead of the first.
        #[arg(long)]
        all: bool,
    },

    // ── input (flat) ──────────────────────────────────────────
    /// Type into the focused field.
    Text {
        value: String,
        #[arg(long)]
        clear: bool,
    },
    /// Press a named key or keycode.
    Key {
        name: String,
    },
    Back,
    Home,

    // ── sync / stream / capture (flat) ────────────────────────
    /// Wait for an element / activity / package to appear (or be --gone).
    Wait {
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
    /// Capture recent toast messages.
    Toast {
        #[arg(long, default_value_t = 5000)]
        wait_ms: u32,
    },
    /// Stream UI/crash/toast events as JSON lines.
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

// ── nested namespaces ─────────────────────────────────────────

#[derive(Subcommand)]
pub enum AppCmd {
    /// Launch the app's default activity.
    Start { package: String },
    /// Force-stop the app.
    Stop { package: String },
    /// Install an APK and run the app-under-test setup ritual (clear/grant/launch/wait).
    Install(AppInstallArgs),
    /// Like `install`, but uninstall any existing copy first.
    Reinstall(AppInstallArgs),
    /// Clear the app's data.
    Clear { package: String },
    /// Version name/code + label.
    Info { package: String },
    /// Wait for the app to launch (or, with --front, reach the foreground).
    Wait {
        package: String,
        #[arg(long, default_value_t = 20000)]
        timeout_ms: u32,
        #[arg(long)]
        front: bool,
    },
    /// Print the current foreground app (package / activity / pid).
    Current,
}

#[derive(Subcommand)]
pub enum PermCmd {
    /// Grant one or more runtime permissions (verify-by-readback).
    Grant {
        package: String,
        #[arg(required = true)]
        perms: Vec<String>,
    },
    /// Revoke one or more runtime permissions.
    Revoke {
        package: String,
        #[arg(required = true)]
        perms: Vec<String>,
    },
    /// List a package's runtime permission grant states.
    List { package: String },
    /// Revoke all granted runtime permissions (fresh-install prompt state).
    Reset { package: String },
}

#[derive(Subcommand)]
pub enum AppopsCmd {
    /// Get appop mode(s) for a package (all ops, or one named op).
    Get { package: String, op: Option<String> },
    /// Set an appop mode (allow|deny|ignore|default|foreground|…).
    Set {
        package: String,
        op: String,
        mode: String,
    },
}

#[derive(Subcommand)]
pub enum ProfileCmd {
    /// Capture the display profile as JSON, optionally to a file.
    Snapshot {
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Apply a preset (`automation`), a snapshot file, or individual flags.
    Apply(ProfileApplyArgs),
    /// Reset the display profile to stock defaults.
    Reset,
}

#[derive(Subcommand)]
pub enum DeviceCmd {
    /// Detailed device info (model, fingerprint, locale, density).
    Info,
    /// Run a shell command on the device.
    Shell {
        cmd: String,
        #[arg(long, default_value_t = 30000)]
        timeout_ms: u32,
    },
    /// Wake the screen / turn the display on.
    Wake,
    /// Put the display to sleep.
    Sleep,
    /// Wake and dismiss the keyguard.
    Unlock,
    /// Get (no value) or set the screen orientation.
    Orientation { value: Option<String> },
    /// Get (no value) or set the clipboard.
    Clipboard { value: Option<String> },
    /// Open the notification shade.
    Notifications,
    /// Open quick settings.
    QuickSettings,
    /// Open a URL via an ACTION_VIEW intent.
    OpenUrl { url: String },
}

#[derive(Subcommand)]
pub enum FilesCmd {
    /// List a directory on the device.
    Ls { remote: String },
    /// Push a local file to the device.
    Push {
        local: String,
        remote: String,
        #[arg(long, default_value_t = 0o644)]
        mode: u32,
    },
    /// Pull a device file to the host.
    Pull { remote: String, local: String },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let device = cli.device;
    let apk = cli.apk;
    let any_apk_version = cli.any_apk_version;
    let cmd = cli.cmd;

    // ── Phase 1: commands that do NOT need the on-device server ──
    // doctor diagnoses the very server `ensure_ready` would start; collect does
    // its own best-effort bring-up so it can degrade; perm/appops/profile and
    // `app install`/`reinstall` are pure host-side `adb`.
    match &cmd {
        Cmd::Devices => return cmd_devices().await,
        Cmd::Update { check, json } => return crate::update::cmd_update(*check, *json).await,
        // Pure self-introspection / file generation — no device needed.
        Cmd::Commands { json } => return crate::cmd::introspect::run(*json),
        Cmd::Skill(args) => return crate::cmd::skill::run(args),
        Cmd::Debugger(args) => return crate::cmd::debugger::run(args).await,
        Cmd::Connect => {
            return cmd_connect(device.as_deref(), apk.as_deref(), any_apk_version).await
        }
        Cmd::Disconnect => return cmd_disconnect(device.as_deref()).await,
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
        Cmd::Perm(c) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return dispatch_perm(c, &serial).await;
        }
        Cmd::Appops(c) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return dispatch_appops(c, &serial).await;
        }
        Cmd::Profile(c) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return dispatch_profile(c, &serial).await;
        }
        Cmd::App(AppCmd::Install(a)) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::app_install::run(&serial, a, false).await;
        }
        Cmd::App(AppCmd::Reinstall(a)) => {
            let serial = resolve_serial(device.as_deref()).await?;
            return crate::cmd::app_install::run(&serial, a, true).await;
        }
        _ => {}
    }

    // ── Phase 2: server-backed commands share one bring-up ──
    let serial = resolve_serial(device.as_deref()).await?;
    let client = installer::ensure_ready(&serial, apk.as_deref(), any_apk_version).await?;

    match cmd {
        // handled in phase 1
        Cmd::Devices
        | Cmd::Connect
        | Cmd::Disconnect
        | Cmd::Update { .. }
        | Cmd::Doctor { .. }
        | Cmd::Collect { .. }
        | Cmd::Commands { .. }
        | Cmd::Skill(_)
        | Cmd::Debugger(_)
        | Cmd::Perm(_)
        | Cmd::Appops(_)
        | Cmd::Profile(_) => unreachable!("handled before ensure_ready"),

        // ── namespaces ─────────────────────────────────────────
        Cmd::App(app_cmd) => dispatch_app(app_cmd, &client).await?,
        Cmd::Device(device_cmd) => dispatch_device(device_cmd, &client, &serial).await?,
        Cmd::Files(files_cmd) => dispatch_files(files_cmd, &client, &serial).await?,

        // ── UI read ────────────────────────────────────────────
        Cmd::Screen => emit(&client.screen().await?),
        Cmd::Screenshot {
            path,
            format,
            scale,
            quality,
        } => cmd_screenshot(&client, path, format, scale, quality).await?,

        // ── gestures ───────────────────────────────────────────
        Cmd::Tap {
            a,
            b,
            text,
            rid,
            desc,
            xpath,
        } => cmd_tap(&client, a, b, text, rid, desc, xpath).await?,
        Cmd::DoubleTap { x, y } => {
            client.double_tap(x, y).await?;
            emit_action("double_tap", &json!({"x":x,"y":y}));
        }
        Cmd::LongTap { x, y, duration_ms } => {
            client.long_tap(x, y, duration_ms).await?;
            emit_action("long_tap", &json!({"x":x,"y":y,"duration_ms":duration_ms}));
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
                &json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
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
                &json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
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
                &json!({"direction":direction,"scale":scale,"duration_ms":duration_ms}),
            );
        }
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
                &json!({"direction":direction,"rid":rid,"text":text,"desc":desc,"percent":percent}),
            );
        }
        Cmd::ScrollTo(args) => crate::cmd::scroll::run(&client, &args).await?,

        // ── locate ─────────────────────────────────────────────
        Cmd::Find {
            text,
            rid,
            desc,
            xpath,
            all,
        } => {
            let query = SelectorQuery {
                text,
                rid,
                desc,
                xpath,
                all,
                ..Default::default()
            };
            let r = client.find(&query).await?;
            emit_action("find", &json!({"matched":r.matched,"elements":r.elements}));
        }

        // ── input ──────────────────────────────────────────────
        Cmd::Back => {
            client.key("back").await?;
            emit_action("key", &json!({"name":"back"}));
        }
        Cmd::Home => {
            client.key("home").await?;
            emit_action("key", &json!({"name":"home"}));
        }
        Cmd::Key { name } => {
            client.key(&name).await?;
            emit_action("key", &json!({"name":name}));
        }
        Cmd::Text { value, clear } => {
            client.text(&value, clear).await?;
            emit_action("text", &json!({"value":value,"clear":clear}));
        }

        // ── sync / stream / capture ────────────────────────────
        Cmd::Wait {
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
            cmd_wait(
                &client,
                WaitQuery {
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
        Cmd::Toast { wait_ms } => cmd_toast(&client, wait_ms).await?,
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
        }
    }
    Ok(())
}

// ── namespace sub-dispatchers ─────────────────────────────────

async fn dispatch_perm(c: &PermCmd, serial: &str) -> Result<()> {
    use crate::cmd::permissions;
    match c {
        PermCmd::Grant { package, perms } => permissions::grant(serial, package, perms).await,
        PermCmd::Revoke { package, perms } => permissions::revoke(serial, package, perms).await,
        PermCmd::List { package } => permissions::list(serial, package).await,
        PermCmd::Reset { package } => permissions::reset(serial, package).await,
    }
}

async fn dispatch_appops(c: &AppopsCmd, serial: &str) -> Result<()> {
    use crate::cmd::permissions;
    match c {
        AppopsCmd::Get { package, op } => {
            permissions::appop_get(serial, package, op.as_deref()).await
        }
        AppopsCmd::Set { package, op, mode } => {
            permissions::appop_set(serial, package, op, mode).await
        }
    }
}

async fn dispatch_profile(c: &ProfileCmd, serial: &str) -> Result<()> {
    use crate::cmd::device_profile;
    match c {
        ProfileCmd::Snapshot { out } => device_profile::snapshot(serial, out.as_ref()).await,
        ProfileCmd::Apply(args) => device_profile::apply(serial, args).await,
        ProfileCmd::Reset => device_profile::reset(serial).await,
    }
}

/// Server-backed `app` verbs. `Install`/`Reinstall` are handled host-side in
/// phase 1, so they're unreachable here.
async fn dispatch_app(c: AppCmd, client: &ServerClient) -> Result<()> {
    match c {
        AppCmd::Install(_) | AppCmd::Reinstall(_) => {
            unreachable!("app install/reinstall handled host-side")
        }
        AppCmd::Start { package } => {
            client.app_start(&package).await?;
            emit_action("app_start", &json!({"package":package}));
        }
        AppCmd::Stop { package } => {
            client.app_stop(&package).await?;
            emit_action("app_stop", &json!({"package":package}));
        }
        AppCmd::Clear { package } => {
            client.app_clear(&package).await?;
            emit_action("app_clear", &json!({"package":package}));
        }
        AppCmd::Info { package } => {
            let info = client.app_info(&package).await?;
            emit_action(
                "app_info",
                &json!({
                    "package":package,
                    "version_name":info.version_name,
                    "version_code":info.version_code,
                    "label":info.label,
                }),
            );
        }
        AppCmd::Wait {
            package,
            timeout_ms,
            front,
        } => {
            let r = client.app_wait(&package, timeout_ms, front).await?;
            emit_action(
                "app_wait",
                &json!({"package":package,"matched":r.matched,"current":r.current}),
            );
        }
        AppCmd::Current => {
            let cur = client.app_current().await?;
            emit_action(
                "app_current",
                &serde_json::to_value(&cur).unwrap_or_default(),
            );
        }
    }
    Ok(())
}

async fn dispatch_device(c: DeviceCmd, client: &ServerClient, serial: &str) -> Result<()> {
    match c {
        DeviceCmd::Info => cmd_device_info(client, serial).await?,
        DeviceCmd::Shell { cmd, timeout_ms } => {
            let r = client.shell(&cmd, timeout_ms).await?;
            emit_action(
                "shell",
                &json!({"input":r.input,"output":r.output,"exit_code":r.exit_code}),
            );
        }
        DeviceCmd::Wake => {
            client.wakeup().await?;
            emit_action("wake", &serde_json::Value::Null);
        }
        DeviceCmd::Sleep => {
            client.screen_off().await?;
            emit_action("sleep", &serde_json::Value::Null);
        }
        DeviceCmd::Unlock => {
            client.unlock().await?;
            emit_action("unlock", &serde_json::Value::Null);
        }
        DeviceCmd::Orientation { value } => match value {
            None => emit_action(
                "orientation",
                &json!({"value": client.orientation_get().await?}),
            ),
            Some(v) => {
                client.orientation_set(&v).await?;
                emit_action("set_orientation", &json!({"value":v}));
            }
        },
        DeviceCmd::Clipboard { value } => match value {
            None => emit_action(
                "clipboard",
                &json!({"value": client.clipboard_get().await?}),
            ),
            Some(v) => {
                client.clipboard_set(&v).await?;
                emit_action("set_clipboard", &json!({"value":v}));
            }
        },
        DeviceCmd::Notifications => {
            client.open_notifications().await?;
            emit_action("notifications", &serde_json::Value::Null);
        }
        DeviceCmd::QuickSettings => {
            client.open_quick_settings().await?;
            emit_action("quick_settings", &serde_json::Value::Null);
        }
        DeviceCmd::OpenUrl { url } => {
            client.open_url(&url).await?;
            emit_action("open_url", &json!({"url":url}));
        }
    }
    Ok(())
}

async fn dispatch_files(c: FilesCmd, client: &ServerClient, serial: &str) -> Result<()> {
    match c {
        FilesCmd::Ls { remote } => {
            let r = client.list_dir(&remote).await?;
            emit_action("ls", &json!({"remote":remote,"entries":r.entries}));
        }
        FilesCmd::Push {
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
                    &json!({"local":local,"remote":remote,"path":r.path,"bytes":r.bytes,"mode":r.mode,"requested_mode":mode,"via":"server"}),
                ),
                Err(_) => {
                    adb::push(serial, std::path::PathBuf::from(&local), remote.clone()).await?;
                    emit_action(
                        "push",
                        &json!({"local":local,"remote":remote,"bytes":bytes_len,"via":"adb"}),
                    );
                }
            }
        }
        FilesCmd::Pull { remote, local } => {
            let (bytes, via) = match client.pull_file(&remote).await {
                Ok(b) => (b, "server"),
                Err(_) => (adb::pull(serial, remote.clone()).await?, "adb"),
            };
            std::fs::write(&local, &bytes).with_context(|| format!("writing {local}"))?;
            emit_action(
                "pull",
                &json!({"remote":remote,"local":local,"bytes":bytes.len() as u64,"via":via}),
            );
        }
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

async fn cmd_device_info(client: &ServerClient, serial: &str) -> Result<()> {
    match client.device().await {
        // 0.1.4+ server: rich device facts.
        Ok(d) => emit_action("device_info", &serde_json::to_value(&d).unwrap_or_default()),
        // Older server without /v1/device: fall back to /state + getprop.
        Err(_) => {
            let state = client.state().await?;
            let getprop = adb::device_info(serial).await;
            emit_action(
                "device_info",
                &json!({
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
        &json!({
            "path": p.display().to_string(),
            "bytes": bytes.len() as u64,
        }),
    );
    Ok(())
}

/// `tap` covers four targeting modes: a selector (`--text/--rid/--desc/--xpath`),
/// an element id from a fresh `screen` dump, or `<x> <y>` coordinates.
#[allow(clippy::too_many_arguments)]
async fn cmd_tap(
    client: &ServerClient,
    a: Option<i32>,
    b: Option<i32>,
    text: Option<String>,
    rid: Option<String>,
    desc: Option<String>,
    xpath: Option<String>,
) -> Result<()> {
    // Selector modes take priority.
    if let Some(query) = xpath {
        let r = client.xpath_tap(&query).await?;
        emit_action(
            "tap",
            &json!({"via":"xpath","xpath":query,"x":r.x,"y":r.y,"matched":r.matched}),
        );
        return Ok(());
    }
    if text.is_some() || rid.is_some() || desc.is_some() {
        let r = client
            .find_tap(&SelectorQuery {
                text,
                rid,
                desc,
                ..Default::default()
            })
            .await?;
        emit_action(
            "tap",
            &json!({"via":"selector","x":r.x,"y":r.y,"matched":r.matched}),
        );
        return Ok(());
    }
    // Coordinate / id modes.
    match (a, b) {
        (Some(x), Some(y)) => {
            client.tap_xy(x, y).await?;
            emit_action("tap", &json!({"via":"coords","x":x,"y":y}));
        }
        (Some(a), None) => {
            let id = u32::try_from(a).map_err(|_| anyhow!("element id must be >= 0, got {a}"))?;
            let screen = client.screen().await?;
            let el = screen.elements.iter().find(|e| e.id == id).ok_or_else(|| {
                anyhow!("element id {id} out of range (0..{})", screen.element_count)
            })?;
            let [x, y] = el.tap;
            client.tap_xy(x, y).await?;
            emit_action(
                "tap",
                &json!({
                    "via":"id","id": id, "x": x, "y": y,
                    "matched": {"text": el.text, "rid": el.rid, "desc": el.desc}
                }),
            );
        }
        (None, _) => {
            bail!("tap needs a target: <id>, <x> <y>, or --text/--rid/--desc/--xpath <value>")
        }
    }
    Ok(())
}

async fn cmd_toast(client: &ServerClient, wait_ms: u32) -> Result<()> {
    let start = unix_ms();
    client.toast_start(50).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms as u64);
    loop {
        let recent = client.toast_recent(start).await?;
        if !recent.toasts.is_empty() || std::time::Instant::now() >= deadline {
            emit_action("toast", &json!({"toasts":recent.toasts}));
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

struct WaitQuery {
    text: Option<String>,
    rid: Option<String>,
    desc: Option<String>,
    klass: Option<String>,
    activity: Option<String>,
    package: Option<String>,
}

async fn cmd_wait(
    client: &ServerClient,
    query: WaitQuery,
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
                "wait",
                &json!({"matched":matched,"gone":gone,"screen_hash":screen_hash}),
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            emit_action(
                "wait",
                &json!({"matched":matched,"gone":gone,"screen_hash":screen_hash,"timeout":true}),
            );
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms.max(1) as u64)).await;
    }
}

fn wait_query_matches(query: &WaitQuery, app: &crate::proto::AppRef, elements: &[Element]) -> bool {
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

// ── session subcommands ───────────────────────────────────────

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
