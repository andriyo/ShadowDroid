//! Argument parsing + subcommand dispatch.
//!
//! Command surface follows a noun-namespace model:
//!   - **Live UI automation is nested** under `ui`: reads (`ui dump`),
//!     locate (`ui find`), gestures (`ui tap`, `ui swipe`, …), input
//!     (`ui text`, `ui key`), and sync (`ui wait`).
//!   - **Resources are nested** under a noun: `app`, `perm`, `appops`, `profile`,
//!     `device`, `files`, `net` (e.g. `app install`, `perm grant`,
//!     `device shell`, `files pull`, `net intercept`).
//!
//! Dispatch is two-phase: host-only commands (no on-device server) run first
//! and return; everything else shares one `ensure_ready` bring-up, then routes
//! through per-namespace sub-dispatchers.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

use crate::cmd::app_install::AppInstallArgs;
use crate::cmd::debug::{DebugArgs, DebugCmd};
use crate::cmd::debugger::{DebugMode, DebuggerCmd};
use crate::cmd::device_profile::ProfileApplyArgs;
use crate::cmd::layout::{LayoutArgs, LayoutCmd};
use crate::cmd::scroll::ScrollArgs;
use crate::cmd::studio::{StudioArgs, StudioCmd};
use crate::config::{expand_config_path, ShadowDroidConfig};
use crate::device::client::{is_transient_transport_error, ServerClient};
use crate::device::{adb, installer};
use crate::events::{CompactElement, ScreenFormat};
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
    /// Initialize host setup: agent skills, Android Studio plugin, and bridge diagnostics.
    Init(crate::cmd::studio::InitArgs),
    /// Diagnose (and optionally repair) the host↔device pipe: device state,
    /// APK version, port forward, server reachability, UiAutomation owners.
    Doctor {
        /// Also run the per-app interceptability verdict (`net check <app>`).
        #[arg(long)]
        app: Option<String>,
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
    /// Inspect, generate, and validate user/project JSON config.
    Config(crate::cmd::config::ConfigArgs),
    /// Generate or refresh an agent-integration file (claude-code / cursor /
    /// codex / gemini / antigravity); `skill --sync` updates installed ones.
    Skill(crate::cmd::skill::SkillArgs),
    /// Detect Android Studio and install the ShadowDroid Android Studio plugin.
    Studio(crate::cmd::studio::StudioArgs),
    /// Agent-first debug snapshots, timelines, replays, and Studio-backed debugger control.
    Debug(crate::cmd::debug::DebugArgs),
    /// Watch the app timeline: UI changes, crashes, toasts, watchers, and network events when available.
    Watch {
        /// Only emit app-scoped events for this package. Permission dialogs are still allowed.
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
        /// Do not try to attach live HTTP events from a running `net` proxy daemon.
        #[arg(long)]
        no_net: bool,
    },

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
    /// Network MITM proxy: enable, inspect, intercept, and modify HTTP(S) traffic.
    #[command(subcommand)]
    Net(NetCmd),
    /// Live UI automation: dump, find, tap, type, and wait for screen state.
    #[command(subcommand)]
    Ui(UiCmd),
    /// Agent-first layout snapshots and diffs.
    Layout(crate::cmd::layout::LayoutArgs),
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

// ── live UI automation (`ui`) ─────────────────────────────────

#[derive(Subcommand)]
pub enum UiCmd {
    /// Dump the current UI as a flat element list.
    Dump {
        /// Emit the full element set (bounds + every UIAutomator flag). Default
        /// is the compact agent shape: selector fields + tap, false flags omitted.
        #[arg(long)]
        full: bool,
    },
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
        /// Emit the full element set instead of the compact agent shape.
        #[arg(long)]
        full: bool,
    },
    /// Tap by element id, coordinates, or a selector (--text/--rid/--desc/--xpath).
    Tap {
        /// Element id from a fresh `ui dump`. Equivalent to positional `ui tap <id>`.
        #[arg(long)]
        id: Option<u32>,
        /// Element id (from `ui dump`) or X coordinate.
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
    /// Double-tap at <x> <y> coordinates.
    DoubleTap { x: i32, y: i32 },
    /// Long-press at <x> <y> coordinates (hold for --duration-ms).
    LongTap {
        x: i32,
        y: i32,
        #[arg(long, default_value_t = 600)]
        duration_ms: u32,
    },
    /// Swipe from (x1,y1) to (x2,y2).
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(long, default_value_t = 200)]
        duration_ms: u32,
    },
    /// Drag from (x1,y1) to (x2,y2) — slower than swipe, for drag-and-drop / reorder.
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(long, default_value_t = 500)]
        duration_ms: u32,
    },
    /// Swipe a fraction (--scale) of the screen in a direction (up/down/left/right).
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
    /// Type into the focused field, or into an element matched by --id/--text/--rid/--desc/--xpath.
    Text {
        value: String,
        #[arg(long)]
        clear: bool,
        /// Element id from a fresh `ui dump` to receive text.
        #[arg(long)]
        id: Option<u32>,
        /// Match a text-bearing/editable element to receive text.
        #[arg(long)]
        text: Option<String>,
        /// Match by resource-id substring.
        #[arg(long)]
        rid: Option<String>,
        /// Match by content-description substring.
        #[arg(long)]
        desc: Option<String>,
        /// Match by xpath.
        #[arg(long)]
        xpath: Option<String>,
        /// Use exact selector matching instead of substring matching.
        #[arg(long)]
        exact: bool,
    },
    /// Press a named key or keycode.
    Key { name: String },
    /// Press the Back button.
    Back,
    /// Press the Home button.
    Home,
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
}

// ── network proxy (`net`) ─────────────────────────────────────

#[derive(Subcommand)]
pub enum NetCmd {
    /// Verdict: is this app interceptable (debuggable, NSC trusts user CA, engine proxy-aware)?
    Check { package: String },
    /// Install / trust the ShadowDroid CA on the device.
    Trust {
        /// Push into the system trust store (emulator/root).
        #[arg(long)]
        system: bool,
        /// Drive the Settings "Install a certificate" UI (real device, non-root).
        #[arg(long)]
        ui: bool,
    },
    /// Start the MITM proxy: spawn the daemon, `adb reverse`, set `http_proxy`.
    Start {
        #[arg(long, default_value_t = crate::net::DEFAULT_PROXY_PORT)]
        port: u16,
        /// Limit capture/MITM to these host globs, e.g. '*.livd.app' (repeatable;
        /// empty = all hosts). NB: matches by host today — per-app uid attribution
        /// is planned but not yet wired, so this is host-scoping, not app-scoping.
        #[arg(long)]
        app: Vec<String>,
        /// Run the proxy in the foreground instead of detaching a daemon.
        #[arg(long)]
        foreground: bool,
        /// Strip cache-validation request headers (force fresh responses).
        #[arg(long)]
        anticache: bool,
        /// Strip Accept-Encoding (force uncompressed responses).
        #[arg(long)]
        anticomp: bool,
    },
    /// Stop the proxy and tear down device wiring.
    Stop {
        /// Also remove the ShadowDroid CA from the device trust store.
        #[arg(long)]
        revoke_ca: bool,
    },
    /// Proxy + device-wiring status (running? pointed at us? held flows).
    Status,
    /// Recall past flows from the session log.
    Log {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        method: Option<String>,
        #[arg(long)]
        status: Option<u16>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
    },
    /// Full headers + bodies for one flow.
    Show {
        id: String,
        #[arg(long)]
        body: bool,
        #[arg(long)]
        har: bool,
    },
    /// Export flows for interop (har | curl).
    Export {
        #[arg(value_parser = ["har", "curl"])]
        format: String,
        id: Option<String>,
    },
    /// Pause matching flows for agent-in-the-loop editing.
    Intercept {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        method: Option<String>,
        #[arg(long)]
        status: Option<u16>,
        /// Hold at the request phase, response phase, or both.
        #[arg(long, value_parser = ["request", "response", "both"], default_value = "response")]
        at: String,
        /// Auto-act after this long if the agent doesn't (apps time out their own client).
        #[arg(long, default_value_t = 30000)]
        hold_ms: u32,
        /// What to do when the hold deadline passes (fail-open by default).
        #[arg(long, value_parser = ["resume", "drop"], default_value = "resume")]
        on_timeout: String,
    },
    /// Release a held flow (optionally mutated).
    Resume {
        id: String,
        #[arg(long)]
        set_status: Option<u16>,
        #[arg(long, value_name = "NAME=VALUE")]
        set_header: Vec<String>,
        #[arg(long, value_name = "NAME")]
        remove_header: Vec<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long, num_args = 2, value_names = ["REGEX", "REPL"])]
        replace: Option<Vec<String>>,
        #[arg(long)]
        delay: Option<u32>,
        #[arg(long)]
        set_url: Option<String>,
    },
    /// Kill a held flow (device sees a connection error, or the given status).
    Drop {
        id: String,
        #[arg(long)]
        status: Option<u16>,
    },
    /// Short-circuit a held request with a canned response (never hits the server).
    Respond {
        id: String,
        #[arg(long, default_value_t = 200)]
        status: u16,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long, value_name = "NAME=VALUE")]
        set_header: Vec<String>,
    },
    /// Declarative response/request rules.
    #[command(subcommand)]
    Rule(NetRuleCmd),
    /// Apply a bulk rules file (JSON array of rules).
    Rules { file: PathBuf },
    /// Serve saved responses without a backend.
    Replay {
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        host: Option<String>,
    },
    /// Internal: run the proxy daemon in the foreground (spawned by `net start`).
    #[command(hide = true)]
    Daemon(NetDaemonArgs),
}

#[derive(Subcommand)]
pub enum NetRuleCmd {
    /// Add a rule (kind = map-local|map-remote|set-status|set-header|replace|block|delay).
    Add(NetRuleAddArgs),
    /// List active rules.
    List,
    /// Remove a rule by id.
    Rm { id: String },
    /// Remove all rules.
    Clear,
}

#[derive(clap::Args)]
pub struct NetRuleAddArgs {
    /// map-local | map-remote | set-status | set-header | replace | block | delay
    pub kind: String,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub path: Option<String>,
    #[arg(long)]
    pub method: Option<String>,
    #[arg(long)]
    pub content_type: Option<String>,
    /// Kind-specific positional args (e.g. set-status <code>, set-header <name> <value>).
    pub args: Vec<String>,
}

#[derive(clap::Args)]
pub struct NetDaemonArgs {
    #[arg(long)]
    pub serial: String,
    #[arg(long, default_value_t = crate::net::DEFAULT_PROXY_PORT)]
    pub port: u16,
    #[arg(long)]
    pub app: Vec<String>,
    #[arg(long)]
    pub anticache: bool,
    #[arg(long)]
    pub anticomp: bool,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = ShadowDroidConfig::load()?;
    let device = cli.device.or_else(|| config.device.clone());
    let apk = cli.apk;
    let any_apk_version = cli.any_apk_version;
    let mut cmd = cli.cmd;
    apply_config_defaults(&mut cmd, &config);

    // ── Phase 1: commands that do NOT need the on-device server ──
    // doctor diagnoses the very server `ensure_ready` would start; collect does
    // its own best-effort bring-up so it can degrade; perm/appops/profile and
    // `app install`/`reinstall` are pure host-side `adb`.
    match &cmd {
        Cmd::Devices => return cmd_devices().await,
        Cmd::Update { check, json } => return crate::update::cmd_update(*check, *json).await,
        Cmd::Init(args) => return crate::cmd::studio::run_init(args).await,
        // Pure self-introspection / file generation — no device needed.
        Cmd::Commands { json } => return crate::cmd::introspect::run(*json),
        Cmd::Config(args) => return crate::cmd::config::run(args),
        Cmd::Skill(args) => return crate::cmd::skill::run(args),
        Cmd::Studio(args) => return crate::cmd::studio::run(args).await,
        Cmd::Debug(args) if args.is_host_only() => {
            return crate::cmd::debug::run_host_only(args).await
        }
        Cmd::Connect => {
            return cmd_connect(device.as_deref(), apk.as_deref(), any_apk_version).await
        }
        Cmd::Disconnect => return cmd_disconnect(device.as_deref()).await,
        Cmd::Doctor {
            app,
            fix,
            force,
            json,
        } => {
            return crate::cmd::doctor::run(device.as_deref(), *fix, *force, *json, app.as_deref())
                .await
        }
        Cmd::Collect {
            app,
            out,
            no_screenshot,
        } => {
            let serial = resolve_serial(device.as_deref()).await?;
            let app = resolve_app_package(&config, Some(&serial), app.clone()).await?;
            return crate::cmd::collect::run(&serial, app, out.clone(), !*no_screenshot).await;
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
        // `net` is host-only: the proxy is a host-side daemon driven over adb.
        // (`trust --ui` brings up the UI server on demand internally.)
        Cmd::Net(c) => {
            // The detached daemon carries its own `--serial` and must not depend
            // on a live device being attached; everything else resolves one.
            if matches!(c, NetCmd::Daemon(_)) {
                return dispatch_net(c, "").await;
            }
            let serial = resolve_serial(device.as_deref()).await?;
            return dispatch_net(c, &serial).await;
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
        | Cmd::Init(_)
        | Cmd::Doctor { .. }
        | Cmd::Collect { .. }
        | Cmd::Commands { .. }
        | Cmd::Config(_)
        | Cmd::Skill(_)
        | Cmd::Studio(_)
        | Cmd::Perm(_)
        | Cmd::Appops(_)
        | Cmd::Profile(_)
        | Cmd::Net(_) => unreachable!("handled before ensure_ready"),

        // ── namespaces ─────────────────────────────────────────
        Cmd::App(app_cmd) => dispatch_app(app_cmd, &client).await?,
        Cmd::Device(device_cmd) => dispatch_device(device_cmd, &client, &serial).await?,
        Cmd::Files(files_cmd) => dispatch_files(files_cmd, &client, &serial).await?,
        Cmd::Debug(args) => crate::cmd::debug::run(&serial, &client, args).await?,
        Cmd::Layout(args) => crate::cmd::layout::run(&serial, &client, args).await?,
        Cmd::Ui(ui_cmd) => {
            dispatch_ui(ui_cmd, &client, &serial, apk.as_deref(), any_apk_version).await?
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
            no_net,
        } => {
            let app = resolve_app_package(&config, Some(&serial), app).await?;
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
                net: !no_net,
            })
            .await?;
        }
    }
    Ok(())
}

async fn dispatch_ui(
    c: UiCmd,
    client: &ServerClient,
    serial: &str,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
) -> Result<()> {
    match c {
        UiCmd::Dump { full } => cmd_screen(serial, apk, any_apk_version, client, full).await?,
        UiCmd::Screenshot {
            path,
            format,
            scale,
            quality,
        } => cmd_screenshot(client, path, format, scale, quality).await?,
        UiCmd::Find {
            text,
            rid,
            desc,
            xpath,
            all,
            full,
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
            if full {
                emit_action("find", &json!({"matched":r.matched,"elements":r.elements}));
            } else {
                let matched = r.matched.map(CompactElement::from);
                let elements: Vec<CompactElement> =
                    r.elements.into_iter().map(CompactElement::from).collect();
                emit_action("find", &json!({"matched":matched,"elements":elements}));
            }
        }
        UiCmd::Tap {
            id,
            a,
            b,
            text,
            rid,
            desc,
            xpath,
        } => cmd_tap(client, id, a, b, text, rid, desc, xpath).await?,
        UiCmd::DoubleTap { x, y } => {
            client.double_tap(x, y).await?;
            emit_action("double_tap", &json!({"x":x,"y":y}));
        }
        UiCmd::LongTap { x, y, duration_ms } => {
            client.long_tap(x, y, duration_ms).await?;
            emit_action("long_tap", &json!({"x":x,"y":y,"duration_ms":duration_ms}));
        }
        UiCmd::Swipe {
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
        UiCmd::Drag {
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
        UiCmd::SwipeExt {
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
        UiCmd::Pinch {
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
        UiCmd::ScrollTo(args) => crate::cmd::scroll::run(client, &args).await?,
        UiCmd::Text {
            value,
            clear,
            id,
            text,
            rid,
            desc,
            xpath,
            exact,
        } => {
            let target = text_target_query(id, text, rid, desc, xpath, exact);
            client
                .text_with_target(&value, clear, target.as_ref())
                .await?;
            emit_action(
                "text",
                &json!({"value":value,"clear":clear,"target":target}),
            );
        }
        UiCmd::Key { name } => {
            let injected = client.key(&name).await?;
            emit_action("key", &json!({"name":name,"injected":injected}));
        }
        UiCmd::Back => {
            let injected = client.key("back").await?;
            emit_action("key", &json!({"name":"back","injected":injected}));
        }
        UiCmd::Home => {
            let injected = client.key("home").await?;
            emit_action("key", &json!({"name":"home","injected":injected}));
        }
        UiCmd::Wait {
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
                client,
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
                serial,
                apk,
                any_apk_version,
            )
            .await?;
        }
        UiCmd::Toast { wait_ms } => cmd_toast(client, wait_ms).await?,
    }
    Ok(())
}

fn apply_config_defaults(cmd: &mut Cmd, config: &ShadowDroidConfig) {
    match cmd {
        Cmd::Init(args) => {
            if args.studio.is_none() {
                args.studio = expand_config_path(&config.android_studio);
            }
            if args.plugin.is_none() {
                args.plugin = expand_config_path(&config.studio_plugin);
            }
        }
        Cmd::Studio(args) => apply_studio_config(args, config),
        Cmd::Collect { app, .. } => fill_app(app, config),
        Cmd::Doctor { app, .. } => fill_app(app, config),
        Cmd::Debug(args) => apply_debug_config(args, config),
        Cmd::Layout(args) => apply_layout_config(args, config),
        _ => {}
    }
}

fn apply_studio_config(args: &mut StudioArgs, config: &ShadowDroidConfig) {
    match &mut args.cmd {
        StudioCmd::Status(args) => {
            if args.studio.is_none() {
                args.studio = expand_config_path(&config.android_studio);
            }
        }
        StudioCmd::Install(args) => {
            if args.studio.is_none() {
                args.studio = expand_config_path(&config.android_studio);
            }
            if args.plugin.is_none() {
                args.plugin = expand_config_path(&config.studio_plugin);
            }
        }
    }
}

fn apply_layout_config(args: &mut LayoutArgs, config: &ShadowDroidConfig) {
    match &mut args.cmd {
        LayoutCmd::Snapshot(args) => fill_studio_url(&mut args.studio_url, config),
        LayoutCmd::Recompositions(args) => fill_studio_url(&mut args.studio_url, config),
        LayoutCmd::Source(args) => fill_studio_url(&mut args.studio_url, config),
        LayoutCmd::Diff(_) => {}
    }
}

fn apply_debug_config(args: &mut DebugArgs, config: &ShadowDroidConfig) {
    fill_studio_url(&mut args.studio_url, config);
    match &mut args.cmd {
        DebugCmd::Auto(args) => {
            if args.package.is_none() && args.app.is_none() && args.target.is_none() {
                fill_app(&mut args.app, config);
            }
            if args.project.is_none() {
                args.project = args
                    .package
                    .as_deref()
                    .or(args.app.as_deref())
                    .or(args.target.as_deref())
                    .and_then(|app| config.default_project_for(Some(app)))
                    .or_else(|| config.project.clone());
            }
            if args.debugger.is_none() {
                args.debugger = args
                    .package
                    .as_deref()
                    .or(args.app.as_deref())
                    .or(args.target.as_deref())
                    .and_then(|app| config.default_debugger_for(Some(app)))
                    .or_else(|| config.debugger.clone());
            }
            if args.mode.is_none() {
                args.mode = args
                    .package
                    .as_deref()
                    .or(args.app.as_deref())
                    .or(args.target.as_deref())
                    .and_then(|app| config.default_debug_mode_for(Some(app)))
                    .or_else(|| config.debug_mode.clone())
                    .and_then(|mode| DebugMode::from_config(&mode));
            }
            if args.configuration.is_none() {
                args.configuration = args
                    .package
                    .as_deref()
                    .or(args.app.as_deref())
                    .or(args.target.as_deref())
                    .and_then(|app| config.default_run_configuration_for(Some(app)))
                    .or_else(|| config.run_configuration.clone());
            }
        }
        DebugCmd::Snapshot(args) => fill_app(&mut args.app, config),
        DebugCmd::Record(args) => fill_app(&mut args.app, config),
        DebugCmd::StepUntilScreenChange(args) => fill_app(&mut args.app, config),
        DebugCmd::StepUntilLog(args) => fill_app(&mut args.wait.app, config),
        DebugCmd::RunUntilCrash(args) => fill_app(&mut args.app, config),
        DebugCmd::Studio(cmd) => apply_debugger_config(cmd, config),
        DebugCmd::Native(cmd) => match cmd {
            crate::cmd::debug::NativeCmd::Status(args) => {
                if args.package.is_none() && args.app.is_none() && args.target.is_none() {
                    fill_app(&mut args.app, config);
                }
            }
        },
        DebugCmd::Tombstones(cmd) => match cmd {
            crate::cmd::debug::TombstonesCmd::List(args) => fill_app(&mut args.app, config),
            crate::cmd::debug::TombstonesCmd::Pull(args) => fill_app(&mut args.app, config),
        },
        DebugCmd::Replay(_) => {}
    }
}

fn apply_debugger_config(cmd: &mut DebuggerCmd, config: &ShadowDroidConfig) {
    match cmd {
        DebuggerCmd::Clients(filter) => {
            if filter.project.is_none() {
                filter.project = config.project.clone();
            }
            if filter.package.is_none() {
                filter.package = config
                    .default_app()
                    .and_then(|app| config.configured_package_for(&app));
            }
            if filter.device.is_none() {
                filter.device = config.device.clone();
            }
        }
        DebuggerCmd::Attach {
            project,
            package,
            device,
            debugger,
            mode,
            configuration,
            ..
        } => {
            if package.is_none() {
                *package = config
                    .default_app()
                    .and_then(|app| config.configured_package_for(&app));
            }
            if project.is_none() {
                *project = package
                    .as_deref()
                    .and_then(|app| config.default_project_for(Some(app)))
                    .or_else(|| config.project.clone());
            }
            if device.is_none() {
                *device = config.device.clone();
            }
            if debugger.is_none() {
                *debugger = package
                    .as_deref()
                    .and_then(|app| config.default_debugger_for(Some(app)))
                    .or_else(|| config.debugger.clone());
            }
            if mode.is_none() {
                *mode = package
                    .as_deref()
                    .and_then(|app| config.default_debug_mode_for(Some(app)))
                    .or_else(|| config.debug_mode.clone())
                    .and_then(|mode| DebugMode::from_config(&mode));
            }
            if configuration.is_none() {
                *configuration = package
                    .as_deref()
                    .and_then(|app| config.default_run_configuration_for(Some(app)))
                    .or_else(|| config.run_configuration.clone());
            }
        }
        DebuggerCmd::Break(break_cmd) => match break_cmd {
            crate::cmd::debugger::BreakCmd::Line { project, .. }
            | crate::cmd::debugger::BreakCmd::Exception { project, .. }
            | crate::cmd::debugger::BreakCmd::Method { project, .. }
            | crate::cmd::debugger::BreakCmd::Field { project, .. } => {
                if project.is_none() {
                    *project = config.project.clone();
                }
            }
            crate::cmd::debugger::BreakCmd::Update(args) => {
                if args.project.is_none() {
                    args.project = config.project.clone();
                }
            }
            crate::cmd::debugger::BreakCmd::Remove { project, .. } => {
                if project.is_none() {
                    *project = config.project.clone();
                }
            }
        },
        DebuggerCmd::Watch(crate::cmd::debugger::WatchCmd::Add { project, .. }) => {
            if project.is_none() {
                *project = config.project.clone();
            }
        }
        _ => {}
    }
}

fn fill_app(app: &mut Option<String>, config: &ShadowDroidConfig) {
    if app.is_none() {
        *app = config.default_app();
    }
}

fn fill_studio_url(studio_url: &mut Option<String>, config: &ShadowDroidConfig) {
    if studio_url.is_none() {
        *studio_url = config.studio_url.clone();
    }
}

async fn resolve_app_package(
    config: &ShadowDroidConfig,
    serial: Option<&str>,
    app: Option<String>,
) -> Result<Option<String>> {
    let Some(app) = app else {
        return Ok(None);
    };
    let resolved = config.resolve_app(serial, Some(&app)).await?;
    Ok(resolved.package.or(Some(app)))
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

/// Route a parsed `net` command to its host-side handler. `net` owns its own
/// daemon + adb wiring, so (unlike server-backed namespaces) this never touches
/// `ensure_ready`. Clap types stay here; the handlers speak plain structs.
async fn dispatch_net(c: &NetCmd, serial: &str) -> Result<()> {
    use crate::net::commands as nc;
    use crate::net::{DaemonConfig, Matcher, RuleSpec};

    let matcher = |host: &Option<String>,
                   path: &Option<String>,
                   method: &Option<String>,
                   status: &Option<u16>| Matcher {
        host: host.clone(),
        path: path.clone(),
        method: method.clone(),
        status: *status,
    };

    match c {
        NetCmd::Check { package } => nc::check(serial, package).await,
        NetCmd::Trust { system, ui } => nc::trust(serial, *system, *ui).await,
        NetCmd::Start {
            port,
            app,
            foreground,
            anticache,
            anticomp,
        } => {
            nc::start(
                serial,
                *port,
                app.clone(),
                *foreground,
                *anticache,
                *anticomp,
            )
            .await
        }
        NetCmd::Stop { revoke_ca } => nc::stop(serial, *revoke_ca).await,
        NetCmd::Status => nc::status(serial).await,
        NetCmd::Log {
            host,
            path,
            method,
            status,
            limit,
        } => nc::log(serial, matcher(host, path, method, status), *limit).await,
        NetCmd::Show { id, body, har } => nc::show(serial, id, *body, *har).await,
        NetCmd::Export { format, id } => nc::export(serial, format, id.clone()).await,
        NetCmd::Intercept {
            host,
            path,
            method,
            status,
            at,
            hold_ms,
            on_timeout,
        } => {
            nc::intercept(
                serial,
                matcher(host, path, method, status),
                at.clone(),
                *hold_ms,
                on_timeout.clone(),
            )
            .await
        }
        NetCmd::Resume {
            id,
            set_status,
            set_header,
            remove_header,
            body,
            body_file,
            replace,
            delay,
            set_url,
        } => {
            let replace = match replace {
                Some(v) if v.len() == 2 => Some((v[0].clone(), v[1].clone())),
                Some(_) => bail!("--replace expects exactly REGEX and REPL"),
                None => None,
            };
            let mutation = crate::net::Mutation {
                set_status: *set_status,
                set_headers: parse_header_pairs(set_header)?,
                remove_headers: remove_header.clone(),
                body: read_body_arg(body, body_file)?,
                replace,
                delay_ms: *delay,
                set_url: set_url.clone(),
            };
            nc::resume(serial, id, mutation).await
        }
        NetCmd::Drop { id, status } => nc::drop_flow(serial, id, *status).await,
        NetCmd::Respond {
            id,
            status,
            body,
            body_file,
            set_header,
        } => {
            nc::respond(
                serial,
                id,
                *status,
                read_body_arg(body, body_file)?,
                parse_header_pairs(set_header)?,
            )
            .await
        }
        NetCmd::Rule(rc) => match rc {
            NetRuleCmd::Add(a) => {
                let spec = RuleSpec {
                    kind: a.kind.clone(),
                    matcher: Matcher {
                        host: a.host.clone(),
                        path: a.path.clone(),
                        method: a.method.clone(),
                        status: None,
                    },
                    content_type: a.content_type.clone(),
                    args: a.args.clone(),
                };
                nc::rule_add(serial, spec).await
            }
            NetRuleCmd::List => nc::rule_list(serial).await,
            NetRuleCmd::Rm { id } => nc::rule_rm(serial, id).await,
            NetRuleCmd::Clear => nc::rule_clear(serial).await,
        },
        NetCmd::Rules { file } => nc::rules_apply(serial, file).await,
        NetCmd::Replay { from, host } => nc::replay(serial, from, host.clone()).await,
        NetCmd::Daemon(a) => {
            crate::net::daemon::run(DaemonConfig {
                serial: a.serial.clone(),
                port: a.port,
                app_filters: a.app.clone(),
                anticache: a.anticache,
                anticomp: a.anticomp,
            })
            .await
        }
    }
}

/// Parse `--set-header NAME=VALUE` pairs.
fn parse_header_pairs(pairs: &[String]) -> Result<Vec<(String, String)>> {
    pairs
        .iter()
        .map(|p| {
            p.split_once('=')
                .map(|(n, v)| (n.trim().to_string(), v.to_string()))
                .ok_or_else(|| anyhow!("--set-header expects NAME=VALUE, got {p:?}"))
        })
        .collect()
}

/// Resolve a body from `--body <str>` or `--body-file <path>` (mutually exclusive).
fn read_body_arg(inline: &Option<String>, file: &Option<PathBuf>) -> Result<Option<Vec<u8>>> {
    match (inline, file) {
        (Some(_), Some(_)) => bail!("--body and --body-file are mutually exclusive"),
        (Some(s), None) => Ok(Some(s.clone().into_bytes())),
        (None, Some(p)) => Ok(Some(
            std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        )),
        (None, None) => Ok(None),
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
            // Server first (app-accessible storage); fall back to `adb shell ls`
            // for paths the instrumentation uid can't see (e.g. /sdcard under
            // scoped storage) — mirrors the push/pull fallback below.
            match client.list_dir(&remote).await {
                Ok(r) => emit_action(
                    "ls",
                    &json!({"remote":remote,"entries":r.entries,"via":"server"}),
                ),
                Err(_) => {
                    let entries = adb::list_dir(serial, &remote).await?;
                    emit_action(
                        "ls",
                        &json!({"remote":remote,"entries":entries,"via":"adb"}),
                    );
                }
            }
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

/// `ui dump` defaults to the compact agent shape (no bounds, false flags
/// omitted) — the loop reads this every iteration, so it pays for itself in
/// tokens. `--full` restores the complete UIAutomator element set.
async fn cmd_screen(
    serial: &str,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
    client: &ServerClient,
    full: bool,
) -> Result<()> {
    let screen = read_screen_with_reconnect(serial, apk, any_apk_version, client).await?;
    if full {
        emit(&screen);
        return Ok(());
    }
    let elements: Vec<CompactElement> = screen
        .elements
        .into_iter()
        .map(CompactElement::from)
        .collect();
    emit(&json!({
        "screen_hash": screen.screen_hash,
        "viewport": screen.viewport,
        "current_app": screen.current_app,
        "element_count": screen.element_count,
        "elements": elements,
    }));
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

/// `ui tap` covers four targeting modes: a selector (`--text/--rid/--desc/--xpath`),
/// an element id from a fresh `ui dump`, or `<x> <y>` coordinates.
#[allow(clippy::too_many_arguments)]
async fn cmd_tap(
    client: &ServerClient,
    id: Option<u32>,
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
            &json!({"via":"xpath","xpath":query,"x":r.x,"y":r.y,"action":r.action,"matched":r.matched}),
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
            &json!({"via":"selector","x":r.x,"y":r.y,"action":r.action,"matched":r.matched}),
        );
        return Ok(());
    }
    // Coordinate / id modes.
    match (id, a, b) {
        (Some(id), None, None) => {
            tap_element_id(client, id).await?;
        }
        (Some(_), Some(_), _) | (Some(_), None, Some(_)) => {
            bail!("tap --id cannot be combined with positional coordinates or element id")
        }
        (None, Some(x), Some(y)) => {
            client.tap_xy(x, y).await?;
            emit_action("tap", &json!({"via":"coords","x":x,"y":y}));
        }
        (None, Some(a), None) => {
            let id = u32::try_from(a).map_err(|_| anyhow!("element id must be >= 0, got {a}"))?;
            tap_element_id(client, id).await?;
        }
        (None, None, _) => {
            bail!("tap needs a target: <id>, <x> <y>, or --text/--rid/--desc/--xpath <value>")
        }
    }
    Ok(())
}

async fn tap_element_id(client: &ServerClient, id: u32) -> Result<()> {
    let r = client
        .find_tap(&SelectorQuery {
            id: Some(id),
            ..Default::default()
        })
        .await?;
    emit_action(
        "tap",
        &json!({
            "via":"id","id": id, "x": r.x, "y": r.y, "action": r.action,
            "matched": r.matched
        }),
    );
    Ok(())
}

fn text_target_query(
    id: Option<u32>,
    text: Option<String>,
    rid: Option<String>,
    desc: Option<String>,
    xpath: Option<String>,
    exact: bool,
) -> Option<SelectorQuery> {
    if id.is_none() && text.is_none() && rid.is_none() && desc.is_none() && xpath.is_none() {
        return None;
    }
    Some(SelectorQuery {
        id,
        text,
        rid,
        desc,
        xpath,
        exact,
        ..Default::default()
    })
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
    serial: &str,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    let mut client = client.clone();
    loop {
        let screen = match client.screen().await {
            Ok(screen) => screen,
            Err(err)
                if is_transient_transport_error(&err) && std::time::Instant::now() < deadline =>
            {
                client = reconnect_after_screen_error(serial, apk, any_apk_version, &err).await?;
                continue;
            }
            Err(err) => return Err(err),
        };
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

async fn read_screen_with_reconnect(
    serial: &str,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
    client: &ServerClient,
) -> Result<crate::proto::ScreenResponse> {
    match client.screen().await {
        Ok(screen) => Ok(screen),
        Err(err) if is_transient_transport_error(&err) => {
            let client = reconnect_after_screen_error(serial, apk, any_apk_version, &err).await?;
            client.screen().await
        }
        Err(err) => Err(err),
    }
}

async fn reconnect_after_screen_error(
    serial: &str,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
    err: &anyhow::Error,
) -> Result<ServerClient> {
    installer::ensure_ready(serial, apk, any_apk_version)
        .await
        .with_context(|| format!("screen request failed ({err}); reconnect failed"))
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
    let mut out = json!({
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
    // After a CLI upgrade, bring installed skills up to date — pristine ones are
    // rewritten silently; anything hand-edited is flagged for `skill sync`.
    if let Some(skills) = crate::cmd::skill::refresh_for_connect() {
        out["skills"] = skills;
    }
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
