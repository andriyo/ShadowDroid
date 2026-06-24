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
use crate::cmd::focus::FocusArgs;
use crate::cmd::layout::{LayoutArgs, LayoutCmd};
use crate::cmd::scroll::ScrollArgs;
use crate::cmd::studio::{StudioArgs, StudioCmd};
use crate::config::{expand_config_path, ShadowDroidConfig};
use crate::device::client::{is_transient_transport_error, ServerClient};
use crate::device::{adb, installer};
use crate::events::{emit, emit_action, emit_error, CompactElement, ScreenFormat};
use crate::proto::{Element, SelectorQuery};
use crate::watch::watcher::PermissionDialogPolicy;

#[derive(Parser)]
#[command(
    name = "shadowdroid",
    version,
    about = "Drive and debug Android apps from the command line: agent-first UI automation, a streaming JSON event timeline (UI, crashes, network), HTTP(S) interception, and an embeddable in-app debug agent.",
    long_about = "ShadowDroid drives and debugs Android apps over adb — for AI agents and humans.\n\n\
Observe & automate: dump the screen as structured JSON elements; tap, type, swipe, scroll, and wait by selector.\n\
Watch: a live event stream of UI changes, crashes/ANRs, toasts, and network calls.\n\
Network: an on-device MITM proxy to inspect, intercept, modify, and replay HTTP(S) traffic.\n\
In-app agent: embed a debug-only AAR for in-process debugging of apps you build (`aar`).\n\
Plus app & device control, permissions, display profiles, diagnostics (`doctor`), and Android Studio / debugger integration."
)]
pub struct Cli {
    /// ADB serial. Defaults to $SHADOWDROID_DEVICE / $ANDROID_SERIAL / sole attached device.
    #[arg(short, long, global = true, env = "SHADOWDROID_DEVICE")]
    pub device: Option<String>,

    /// Local APK to install instead of normal APK resolution. Can be either:
    ///   • a path to the test APK (e.g., app-debug-androidTest.apk); the
    ///     sibling main APK is auto-discovered in the same directory tree
    ///   • a directory containing both app-debug.apk and app-debug-androidTest.apk
    #[arg(long, global = true, env = "SHADOWDROID_APK", value_name = "PATH")]
    pub apk: Option<PathBuf>,

    /// Skip the version check when installing — assume any provided/discovered APK
    /// is the right one. Implied by --apk; you only need this explicitly to override
    /// the cached download flow during local development without --apk.
    ///
    /// Also settable via the SHADOWDROID_ANY_APK_VERSION env var, which accepts the
    /// usual truthy spellings (1/0, true/false, yes/no, on/off); unset or any other
    /// value means false. The env is resolved in `run()` rather than by clap so it
    /// never dead-ends on a `[possible values: true, false]` parse error.
    #[arg(long, global = true)]
    pub any_apk_version: bool,

    /// Path to an app's source project (Gradle root). Used by `aar` to install
    /// the in-app debug agent and surfaced by `doctor`. Defaults to the
    /// `project` field in config.
    #[arg(long, global = true, env = "SHADOWDROID_PROJECT", value_name = "PATH")]
    pub project: Option<PathBuf>,

    /// Silence ShadowDroid's own operational logs (the `tracing` lines written to
    /// stderr) so command output on stdout stays clean — handy when piping with
    /// `2>&1` or for the tidiest agent output. Real errors are still reported, and
    /// an explicit `RUST_LOG` still takes precedence.
    ///
    /// Also settable via the SHADOWDROID_QUIET env var (1/true/yes/on). The env is
    /// resolved manually in `main` rather than wired through clap: clap's strict
    /// bool env parser only accepts `true`/`false`, so `SHADOWDROID_QUIET=1` (the
    /// documented spelling) would otherwise dead-end every command on a parse
    /// error — exactly the trap `--any-apk-version` avoids the same way.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

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
    /// Run an instrumentation-test command with the device's UiAutomation slot
    /// freed: disconnects ShadowDroid, runs the given command (stdio inherited),
    /// then reconnects (unless `--no-reconnect`). Exits with the command's status.
    ///
    /// Example: `shadowdroid test -- ./gradlew :app:connectedDebugAndroidTest`
    Test {
        /// Leave ShadowDroid disconnected after the command finishes.
        #[arg(long)]
        no_reconnect: bool,
        /// The command to run, after `--`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            required = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
    /// Check whether this CLI is older than the latest GitHub Release.
    Update {
        /// Only report whether an update is available; don't modify anything.
        #[arg(long)]
        check: bool,
        /// Emit the result as JSON instead of human text.
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
        /// App package to scope the bundle to (defaults to the configured app).
        #[arg(long)]
        app: Option<String>,
        /// Output directory for the bundle (default: a timestamped temp dir).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Skip capturing a screenshot.
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
        /// Safety-net poll interval (ms). Catches in-screen changes that emit no
        /// logcat transition — a counter ticking, async content loading — by
        /// re-dumping on this cadence. Navigation changes are caught immediately
        /// via logcat, independent of this; lower = notice silent changes sooner
        /// at the cost of more dumps.
        #[arg(long, default_value_t = 1000)]
        poll_ms: u32,
        /// Settle delay (ms) after a logcat transition before dumping, so a burst
        /// of transition events (an animation, a multi-step navigation) collapses
        /// into one dump of the final screen instead of every intermediate frame.
        /// Not applied to poll ticks or post-command refreshes.
        #[arg(long, default_value_t = 80)]
        debounce_ms: u32,
        /// Don't read interactive commands from stdin; only stream events.
        #[arg(long)]
        no_stdin: bool,
        /// Don't parse logcat for crashes/ANRs (skip the crash watcher).
        #[arg(long)]
        no_crash_detect: bool,
        /// Emit the full element set (bounds + every UIAutomator flag) instead of
        /// the compact agent shape — the same flag as `ui dump --full`.
        #[arg(long)]
        full: bool,
        /// Built-in Android permission dialog policy.
        ///
        /// `allow` taps PermissionController allow buttons; `deny` taps deny buttons.
        #[arg(long, value_enum, default_value_t = PermissionDialogPolicy::Ignore)]
        permission_dialogs: PermissionDialogPolicy,
        /// Load a JSON watcher-rules file (declarative popup auto-handlers).
        /// Repeatable to stack multiple rule files.
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
    /// Install/manage the in-app debug AAR (the ShadowDroid agent) in an app you
    /// can build: wires one debug-only dependency; the agent auto-installs via a
    /// merged ContentProvider. Host-only (no device needed).
    #[command(subcommand)]
    Aar(crate::cmd::aar::AarCmd),
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
        /// Give up after this many milliseconds.
        #[arg(long, default_value_t = 20000)]
        timeout_ms: u32,
        /// Wait until the app reaches the foreground, not just until it launches.
        #[arg(long)]
        front: bool,
    },
    /// Print the current foreground app (package / activity / pid).
    Current {
        /// Accepted for consistency with other subcommands; output is always JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum PermCmd {
    /// Grant one or more runtime permissions (verify-by-readback).
    Grant {
        package: String,
        /// Permissions to grant (e.g. android.permission.CAMERA); one or more.
        #[arg(required = true)]
        perms: Vec<String>,
    },
    /// Revoke one or more runtime permissions.
    Revoke {
        package: String,
        /// Permissions to revoke (e.g. android.permission.CAMERA); one or more.
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
        /// Write the snapshot to this file instead of stdout.
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
        /// Kill the command and fail if it runs longer than this (milliseconds).
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
        /// Unix permission bits for the pushed file (octal, e.g. 644).
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
    /// Audit the current screen for interactive elements lacking a stable
    /// selector (resource-id / Compose testTag) — the ones that force a test to
    /// match on localized text or element index. Helps before writing a test.
    Audit,
    /// Generate a starting-point Kotlin Screen Object from the current screen,
    /// with stable selectors filled in and un-tagged elements listed as TODOs.
    Gen {
        /// Class-name prefix; the generated class is `<name>Screen`.
        #[arg(long, default_value = "Generated")]
        name: String,
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
        /// Match by visible text (normalized, case-insensitive substring; add --exact for a full match).
        #[arg(long)]
        text: Option<String>,
        /// Match by resource-id (substring).
        #[arg(long)]
        rid: Option<String>,
        /// Match by content-description (normalized substring).
        #[arg(long)]
        desc: Option<String>,
        /// Match by xpath, e.g. //*[@text='Foo'] or //*[contains(@text,'Foo')].
        #[arg(long)]
        xpath: Option<String>,
        /// Return all matches instead of the first.
        #[arg(long)]
        all: bool,
        /// Match selector values exactly instead of as a substring.
        #[arg(long)]
        exact: bool,
        /// Only match clickable elements (skips labels/containers with the same text).
        #[arg(long)]
        clickable: bool,
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
        /// Tap the element matching this visible text (normalized substring; --exact for full).
        #[arg(long)]
        text: Option<String>,
        /// Tap the element matching this resource-id (substring).
        #[arg(long)]
        rid: Option<String>,
        /// Tap the element matching this content-description (substring).
        #[arg(long)]
        desc: Option<String>,
        /// Tap the element matching this xpath.
        #[arg(long)]
        xpath: Option<String>,
        /// Match selector values exactly instead of as a substring. Avoids tapping a
        /// label whose text merely contains the target (e.g. "Allow Disney+…" vs "Allow").
        #[arg(long)]
        exact: bool,
        /// Only tap a clickable element. Skips a non-clickable label/TextView that
        /// shares the target's text in favor of the actual button.
        #[arg(long)]
        clickable: bool,
    },
    /// Double-tap at <x> <y> coordinates.
    DoubleTap { x: i32, y: i32 },
    /// Long-press at <x> <y> coordinates (hold for --duration-ms).
    LongTap {
        x: i32,
        y: i32,
        /// How long to hold the press, in milliseconds.
        #[arg(long, default_value_t = 600)]
        duration_ms: u32,
    },
    /// Swipe from (x1,y1) to (x2,y2).
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        /// Swipe duration in milliseconds (longer = slower, more deliberate).
        #[arg(long, default_value_t = 200)]
        duration_ms: u32,
    },
    /// Drag from (x1,y1) to (x2,y2) — slower than swipe, for drag-and-drop / reorder.
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        /// Drag duration in milliseconds (longer = slower, for drag-and-drop).
        #[arg(long, default_value_t = 500)]
        duration_ms: u32,
    },
    /// Swipe a fraction (--scale) of the screen in a direction (up/down/left/right).
    SwipeExt {
        /// Direction to swipe.
        #[arg(value_parser = ["up", "down", "left", "right"])]
        direction: String,
        /// Fraction of the screen to travel, 0.0–1.0.
        #[arg(long, default_value_t = 0.9)]
        scale: f32,
        /// Swipe duration in milliseconds.
        #[arg(long, default_value_t = 200)]
        duration_ms: u32,
    },
    /// Pinch in (zoom out) or out (zoom in) on the element matched by a selector.
    Pinch {
        /// Pinch `in` (zoom out) or `out` (zoom in).
        #[arg(value_parser = ["in", "out"])]
        direction: String,
        /// Match the target element by resource-id (substring).
        #[arg(long)]
        rid: Option<String>,
        /// Match the target element by visible text (substring).
        #[arg(long)]
        text: Option<String>,
        /// Match the target element by content-description (substring).
        #[arg(long)]
        desc: Option<String>,
        /// Pinch distance as a percent of the element's size (1–100).
        #[arg(long, default_value_t = 50)]
        percent: u32,
    },
    /// Scroll a list until a selector is visible, then optionally tap it.
    ScrollTo(ScrollArgs),
    /// Move D-pad focus to a selector (TV/leanback), then optionally activate it (--center).
    Focus(FocusArgs),
    /// Type into the focused field, or into an element matched by --id/--text/--rid/--desc/--xpath.
    Text {
        value: String,
        /// Clear the field's existing contents before typing.
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
    ///
    /// Selector text matches as a case-insensitive substring by default; add
    /// --exact for a full-string match. --package / --activity wait for the
    /// foreground app to *be* (substring of) the given component; --package-not
    /// waits for it to *leave* a package (e.g. confirm a Custom Tab / share sheet
    /// opened, or that you returned from an external app). The result reports the
    /// resulting `current_app` and, when a selector matched, the `element`.
    Wait {
        /// Wait for an element whose visible text matches (substring; --exact for full).
        #[arg(long)]
        text: Option<String>,
        /// Wait for an element whose resource-id matches (substring).
        #[arg(long)]
        rid: Option<String>,
        /// Wait for an element whose content-description matches (substring).
        #[arg(long)]
        desc: Option<String>,
        /// Wait for an element whose class name matches (substring).
        #[arg(long)]
        klass: Option<String>,
        /// Wait until the foreground activity name contains this (substring).
        #[arg(long)]
        activity: Option<String>,
        /// Wait until the foreground app's package contains this (e.g. `chrome`).
        #[arg(long, visible_alias = "pkg")]
        package: Option<String>,
        /// Wait until the foreground app's package does NOT contain this — i.e.
        /// the screen left this app (a browser/dialer/share-sheet opened, or you
        /// navigated back out).
        #[arg(long, visible_alias = "pkg-not")]
        package_not: Option<String>,
        /// Match selector values (--text/--rid/--desc/--klass) exactly instead of
        /// as a case-insensitive substring.
        #[arg(long)]
        exact: bool,
        /// Invert: wait until the matched element is *gone* instead of present.
        #[arg(long)]
        gone: bool,
        /// Give up after this many milliseconds.
        #[arg(long, default_value_t = 10000)]
        timeout_ms: u32,
        /// How often to re-check, in milliseconds.
        #[arg(long, default_value_t = 200)]
        poll_ms: u32,
    },
    /// Capture recent toast messages.
    Toast {
        /// How long to listen for a toast, in milliseconds.
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
        /// Proxy listen port (wired to the device via `adb reverse`).
        #[arg(long, default_value_t = crate::net::DEFAULT_PROXY_PORT)]
        port: u16,
        /// Limit capture/MITM to these host globs, e.g. '*.livd.app' (repeatable;
        /// empty = all hosts). Same `--host` filter used by `net log`/`intercept`.
        #[arg(long)]
        host: Vec<String>,
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
        /// Filter by host (substring), e.g. api.example.com.
        #[arg(long)]
        host: Option<String>,
        /// Filter by URL path (substring).
        #[arg(long)]
        path: Option<String>,
        /// Filter by HTTP method (GET, POST, …).
        #[arg(long)]
        method: Option<String>,
        /// Filter by response status code.
        #[arg(long)]
        status: Option<u16>,
        /// Max number of flows to return (most recent first).
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
    },
    /// Full headers + bodies for one flow.
    Show {
        id: String,
        /// Include request/response bodies (not just headers).
        #[arg(long)]
        body: bool,
        /// Emit the flow as a single-entry HAR object.
        #[arg(long)]
        har: bool,
    },
    /// Export flows for interop: `har`, `curl`, or `fixtures` (a replayable
    /// response set + `manifest.json` for deterministic instrumentation tests;
    /// GraphQL POSTs are keyed by operationName). Framework-specific setups are
    /// generated from the neutral fixtures manifest by your own tooling.
    Export {
        /// Export format: har, curl, or fixtures.
        #[arg(value_parser = ["har", "curl", "fixtures"])]
        format: String,
        id: Option<String>,
        /// Output directory for `fixtures` (default: ./shadowdroid-fixtures).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Pause matching flows for agent-in-the-loop editing.
    Intercept {
        /// Only intercept flows whose host contains this (substring).
        #[arg(long)]
        host: Option<String>,
        /// Only intercept flows whose URL path contains this (substring).
        #[arg(long)]
        path: Option<String>,
        /// Only intercept flows with this HTTP method.
        #[arg(long)]
        method: Option<String>,
        /// Only intercept flows with this response status (response phase).
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
        /// Override the response status code.
        #[arg(long)]
        set_status: Option<u16>,
        /// Set (or replace) a header; repeatable.
        #[arg(long, value_name = "NAME=VALUE")]
        set_header: Vec<String>,
        /// Remove a header; repeatable.
        #[arg(long, value_name = "NAME")]
        remove_header: Vec<String>,
        /// Replace the body with this literal string.
        #[arg(long)]
        body: Option<String>,
        /// Replace the body with the contents of this file.
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// Regex-replace within the body: --replace <REGEX> <REPL>.
        #[arg(long, num_args = 2, value_names = ["REGEX", "REPL"])]
        replace: Option<Vec<String>>,
        /// Delay the release by this many milliseconds (simulate latency).
        #[arg(long)]
        delay_ms: Option<u32>,
        /// Rewrite the request URL before forwarding (request phase).
        #[arg(long)]
        set_url: Option<String>,
    },
    /// Kill a held flow (device sees a connection error, or the given status).
    Drop {
        id: String,
        /// Return this status to the device instead of a connection error.
        #[arg(long)]
        set_status: Option<u16>,
    },
    /// Short-circuit a held request with a canned response (never hits the server).
    Respond {
        id: String,
        /// Status code of the canned response.
        #[arg(long, default_value_t = 200)]
        set_status: u16,
        /// Response body as a literal string.
        #[arg(long)]
        body: Option<String>,
        /// Response body from this file.
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// Set a response header; repeatable.
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
        /// Directory of saved responses to serve (a `fixtures` export).
        #[arg(long)]
        from: PathBuf,
        /// Only replay for this host; let other hosts pass through.
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
    /// Match flows whose host contains this (substring).
    #[arg(long)]
    pub host: Option<String>,
    /// Match flows whose URL path contains this (substring).
    #[arg(long)]
    pub path: Option<String>,
    /// Match flows with this HTTP method.
    #[arg(long)]
    pub method: Option<String>,
    /// Match flows with this response content-type (substring).
    #[arg(long)]
    pub content_type: Option<String>,
    /// Kind-specific positional args (e.g. set-status <code>, set-header <name> <value>).
    pub args: Vec<String>,
}

#[derive(clap::Args)]
pub struct NetDaemonArgs {
    /// ADB serial the daemon wires itself to.
    #[arg(long)]
    pub serial: String,
    /// Port the daemon listens on.
    #[arg(long, default_value_t = crate::net::DEFAULT_PROXY_PORT)]
    pub port: u16,
    /// Host globs to scope capture to (repeatable; empty = all).
    #[arg(long)]
    pub host: Vec<String>,
    /// Strip cache-validation request headers.
    #[arg(long)]
    pub anticache: bool,
    /// Strip Accept-Encoding to force uncompressed responses.
    #[arg(long)]
    pub anticomp: bool,
}

/// Parse argv, converting clap's plaintext usage errors into the same
/// `{"type":"error",…}` contract as runtime failures (item: agents shouldn't
/// have to special-case a `try '--help'` plaintext line). `--help`/`--version`
/// are not errors and are rendered exactly as clap would.
fn parse_cli() -> Cli {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let kind = err.kind();
            // Help/version: render to stdout as usual, exit success.
            if matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let _ = err.print();
                std::process::exit(0);
            }
            // Bare invocation with no subcommand: clap prints help; keep its exit 2.
            if kind == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
                let _ = err.print();
                std::process::exit(2);
            }
            // A genuine usage error → structured JSON that *names* the bad flag
            // (and clap's spelling suggestion) instead of a `try '--help'` line.
            let ctx_str = |k: ContextKind| -> Option<String> {
                err.get(k).and_then(|v| match v {
                    ContextValue::String(s) => Some(s.clone()),
                    ContextValue::Strings(ss) => Some(ss.join(", ")),
                    _ => None,
                })
            };
            let msg = err
                .to_string()
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("invalid command-line arguments")
                .trim_start_matches("error: ")
                .to_string();
            let mut extra = serde_json::Map::new();
            extra.insert("kind".into(), json!(format!("{kind:?}")));
            if let Some(a) = ctx_str(ContextKind::InvalidArg) {
                extra.insert("arg".into(), json!(a));
            }
            if let Some(s) = ctx_str(ContextKind::SuggestedArg) {
                extra.insert("suggestion".into(), json!(s));
            }
            extra.insert(
                "hint".into(),
                json!("run `shadowdroid <command> --help` for usage"),
            );
            emit_error("usage", "usage", &msg, serde_json::Value::Object(extra));
            std::process::exit(2);
        }
    }
}

pub async fn run() -> Result<()> {
    let cli = parse_cli();
    let config = ShadowDroidConfig::load()?;
    let device = cli.device.or_else(|| config.device.clone());
    let apk = cli.apk;
    let project = cli
        .project
        .or_else(|| config.project.as_deref().map(PathBuf::from));
    // Resolve `--any-apk-version` ourselves from the env (clap's strict bool env
    // parsing rejects `1`/`yes` and errors with `[possible values: true, false]`).
    // `env_truthy` accepts 1/true/yes/on; anything else (including 0/no/off) is false.
    let any_apk_version =
        cli.any_apk_version || installer::env_truthy("SHADOWDROID_ANY_APK_VERSION");
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
        // `aar` install/status/remove are host-only (Gradle + filesystem); the
        // capture/intercept/resume/drop/agent verbs talk to the running in-app
        // agent and resolve a device serial internally.
        Cmd::Aar(c) => return crate::cmd::aar::run(c, project.as_deref(), device.as_deref()).await,
        Cmd::Debug(args) if args.is_host_only() => {
            return crate::cmd::debug::run_host_only(args).await
        }
        Cmd::Connect => {
            return cmd_connect(device.as_deref(), apk.as_deref(), any_apk_version).await
        }
        Cmd::Disconnect => return cmd_disconnect(device.as_deref()).await,
        Cmd::Test {
            no_reconnect,
            command,
        } => {
            return cmd_test(
                device.as_deref(),
                apk.as_deref(),
                any_apk_version,
                !*no_reconnect,
                command.clone(),
            )
            .await
        }
        Cmd::Doctor {
            app,
            fix,
            force,
            json,
        } => {
            return crate::cmd::doctor::run(
                device.as_deref(),
                *fix,
                *force,
                *json,
                app.as_deref(),
                project.as_deref(),
            )
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
        | Cmd::Test { .. }
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
        | Cmd::Net(_)
        | Cmd::Aar(_) => unreachable!("handled before ensure_ready"),

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
            full,
            permission_dialogs,
            watcher_file,
            no_net,
        } => {
            let app = resolve_app_package(&config, Some(&serial), app).await?;
            let screen_format = if full {
                ScreenFormat::Full
            } else {
                ScreenFormat::Compact
            };
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
        UiCmd::Audit => {
            let screen = read_screen_with_reconnect(serial, apk, any_apk_version, client).await?;
            let mut body = crate::cmd::authoring::audit_elements(&screen.elements);
            if let serde_json::Value::Object(m) = &mut body {
                m.insert("screen_hash".into(), json!(screen.screen_hash));
            }
            emit_action("ui_audit", &body);
        }
        UiCmd::Gen { name } => {
            let screen = read_screen_with_reconnect(serial, apk, any_apk_version, client).await?;
            print!(
                "{}",
                crate::cmd::authoring::generate_screen_object(&name, &screen.elements)
            );
        }
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
            exact,
            clickable,
            full,
        } => {
            let query = SelectorQuery {
                text,
                rid,
                desc,
                xpath,
                all,
                exact,
                clickable: clickable.then_some(true),
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
            exact,
            clickable,
        } => cmd_tap(client, id, a, b, text, rid, desc, xpath, exact, clickable).await?,
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
        UiCmd::Focus(args) => crate::cmd::focus::run(client, &args).await?,
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
            package_not,
            exact,
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
                    package_not,
                    exact,
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
            host,
            foreground,
            anticache,
            anticomp,
        } => {
            nc::start(
                serial,
                *port,
                host.clone(),
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
        NetCmd::Export { format, id, out } => {
            nc::export(serial, format, id.clone(), out.clone()).await
        }
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
            delay_ms,
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
                delay_ms: *delay_ms,
                set_url: set_url.clone(),
            };
            nc::resume(serial, id, mutation).await
        }
        NetCmd::Drop { id, set_status } => nc::drop_flow(serial, id, *set_status).await,
        NetCmd::Respond {
            id,
            set_status,
            body,
            body_file,
            set_header,
        } => {
            nc::respond(
                serial,
                id,
                *set_status,
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
                app_filters: a.host.clone(),
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
        AppCmd::Current { json: _ } => {
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

/// Render a failed command as one `{"type":"error",…}` line on stdout. Walks the
/// `anyhow` chain for a [`ServerError`] so the server's machine `code`
/// (`element_not_found`, …) and HTTP `status` survive; otherwise falls back to a
/// generic `error` code with the human message. Called by `main` on `Err`.
pub fn report_error(err: &anyhow::Error) {
    if let Some(se) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::device::client::ServerError>())
    {
        let mut extra = json!({ "status": se.status.as_u16() });
        if let Some(detail) = &se.detail {
            extra["detail"] = detail.clone();
        }
        emit_error("run", &se.code, &se.message, extra);
    } else if let Some(amb) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::selector::AmbiguousMatch>())
    {
        emit_error(
            "run",
            "ambiguous_match",
            &amb.to_string(),
            json!({ "detail": { "candidates": amb.candidates } }),
        );
    } else {
        emit_error("run", "error", &err.to_string(), json!({}));
    }
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
    let mut body = json!({
        "path": p.display().to_string(),
        "bytes": bytes.len() as u64,
        "format": format.as_deref().unwrap_or("png"),
    });
    if let Some((w, h)) = image_dimensions(&bytes) {
        body["width"] = json!(w);
        body["height"] = json!(h);
    }
    // Best-effort structural screen hash so two screenshots are comparable
    // without pixel-diffing — the same value `ui wait` / `ui dump` return. A slow
    // or failed dump must not fail the screenshot, so the error is swallowed.
    if let Ok(screen) = client.screen().await {
        body["screen_hash"] = json!(screen.screen_hash);
    }
    emit_action("screenshot", &body);
    Ok(())
}

/// Parse pixel dimensions from a PNG or JPEG byte stream without pulling in an
/// image-decoding dependency. `None` for anything unrecognized — the screenshot
/// still succeeds; `width`/`height` are simply omitted.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG: 8-byte signature, then an IHDR chunk whose data begins at offset 16
    // with big-endian u32 width, height.
    if bytes.len() >= 24
        && bytes[0..8] == [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        && &bytes[12..16] == b"IHDR"
    {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    // JPEG: SOI (FF D8), then a chain of marker segments. A frame header
    // (SOF0/2/…) carries height then width as big-endian u16.
    if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2usize;
        while i + 9 < bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            // 0xFF padding fill, or standalone markers that have no length field.
            if marker == 0xFF {
                i += 1;
                continue;
            }
            if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker)
            {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            // SOF (frame header) markers carry the size; excludes DHT/JPG/DAC.
            let is_sof = matches!(
                marker,
                0xC0 | 0xC1
                    | 0xC2
                    | 0xC3
                    | 0xC5
                    | 0xC6
                    | 0xC7
                    | 0xC9
                    | 0xCA
                    | 0xCB
                    | 0xCD
                    | 0xCE
                    | 0xCF
            );
            if is_sof {
                let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                return Some((w, h));
            }
            i += 2 + len;
        }
    }
    None
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
    exact: bool,
    clickable: bool,
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
                exact,
                clickable: clickable.then_some(true),
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
    package_not: Option<String>,
    exact: bool,
}

/// Outcome of one `wait_query_matches` probe: whether the screen satisfies the
/// query, and (when a selector was given) the element that satisfied it — so the
/// result can echo the matched node back, like `ui tap` does.
struct WaitOutcome {
    matched: bool,
    element: Option<Element>,
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
        let outcome = wait_query_matches(&query, &screen.current_app, &screen.elements);
        let matched = outcome.matched;
        let screen_hash = screen.screen_hash;
        let current_app = json!({
            "package": screen.current_app.package,
            "activity": screen.current_app.activity,
        });
        if matched != gone {
            let mut body = json!({
                "matched": matched,
                "gone": gone,
                "screen_hash": screen_hash,
                "current_app": current_app,
            });
            if let Some(el) = outcome.element {
                body["element"] = json!(CompactElement::from(el));
            }
            emit_action("wait", &body);
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let hint = if gone {
                "still present after timeout — the selector may be too broad, or the element is re-created each frame"
            } else {
                "never matched — verify the selector against the live screen (`shadowdroid ui find …` or `ui audit`), or the target screen may not have finished loading"
            };
            emit_action(
                "wait",
                &json!({"matched":matched,"gone":gone,"screen_hash":screen_hash,"current_app":current_app,"timeout":true,"hint":hint}),
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

fn wait_query_matches(
    query: &WaitQuery,
    app: &crate::proto::AppRef,
    elements: &[Element],
) -> WaitOutcome {
    let no_match = WaitOutcome {
        matched: false,
        element: None,
    };
    // Foreground-app gates: package (must be), package_not (must have left),
    // activity (must be). Package names are case-sensitive, so these stay
    // substring-but-case-sensitive regardless of --exact.
    if let Some(package) = &query.package {
        if !app.package.as_deref().unwrap_or("").contains(package) {
            return no_match;
        }
    }
    if let Some(package_not) = &query.package_not {
        if app.package.as_deref().unwrap_or("").contains(package_not) {
            return no_match;
        }
    }
    if let Some(activity) = &query.activity {
        if !app.activity.as_deref().unwrap_or("").contains(activity) {
            return no_match;
        }
    }
    let has_element_query = query.text.is_some()
        || query.rid.is_some()
        || query.desc.is_some()
        || query.klass.is_some();
    if !has_element_query {
        // Pure foreground-app wait (package / package_not / activity satisfied).
        return WaitOutcome {
            matched: true,
            element: None,
        };
    }
    // Match against the canonical selector spec ([crate::selector]). When not in
    // exact mode, prefer an exact hit for the *returned* element so the agent
    // sees the most specific node (a pure presence / `--gone` check is
    // unaffected — any match still satisfies the wait).
    let matches = |el: &Element, exact: bool| {
        crate::selector::text_matches(el.text.as_deref(), query.text.as_deref(), exact)
            && crate::selector::text_matches(el.rid.as_deref(), query.rid.as_deref(), exact)
            && crate::selector::text_matches(el.desc.as_deref(), query.desc.as_deref(), exact)
            && crate::selector::text_matches(el.klass.as_deref(), query.klass.as_deref(), exact)
    };
    let element = elements
        .iter()
        .find(|el| !query.exact && matches(el, true))
        .or_else(|| elements.iter().find(|el| matches(el, query.exact)))
        .cloned();
    WaitOutcome {
        matched: element.is_some(),
        element,
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── session subcommands ───────────────────────────────────────

/// `devices` honors the "one JSON object per command" contract (it used to print
/// bare serials). Each entry carries enough to pick `-d <serial>` without a
/// second call: `state`, plus `device_model` / `device_manufacturer` /
/// `android_release` / `android_sdk` for fully-online devices. Offline,
/// unauthorized, or no-perm devices can't be queried over `getprop`, so they
/// report `serial` + `state` only (and are still listed, unlike `connect`'s
/// actionable-only view).
async fn cmd_devices() -> Result<()> {
    let pairs = adb::list_devices_with_state().await?;
    let mut devices = Vec::with_capacity(pairs.len());
    for (serial, state) in pairs {
        let mut obj = serde_json::Map::new();
        obj.insert("serial".into(), json!(serial));
        obj.insert("state".into(), json!(state));
        if state == "device" {
            if let serde_json::Value::Object(info) = adb::device_info(&serial).await {
                for (k, v) in info {
                    obj.insert(k, v);
                }
            }
        }
        devices.push(serde_json::Value::Object(obj));
    }
    let empty = devices.is_empty();
    let mut body = json!({ "count": devices.len(), "devices": devices });
    if empty {
        body["hint"] =
            json!("no devices attached — start an emulator or plug in a device with USB debugging");
    }
    emit_action("devices", &body);
    Ok(())
}

/// Advisory surfaced on `connect`. ShadowDroid hosts its on-device server as an
/// `AndroidJUnitRunner` instrumentation, which occupies the device's single
/// `UiAutomation` slot. While ShadowDroid is connected, a user's own Espresso /
/// UI Automator instrumentation tests cannot acquire that slot and fail at
/// startup with "UiAutomationService ... already registered!". Disconnecting
/// releases it. This is reported (not silently assumed) so agents and humans
/// can plan around it instead of debugging a cryptic instrumentation failure.
fn ui_automation_advisory() -> serde_json::Value {
    json!({
        "owner": "shadowdroid",
        "blocks_instrumentation_tests": true,
        "advisory": "ShadowDroid holds the device's single UiAutomation slot, so Espresso / UI Automator instrumentation tests (AndroidJUnitRunner) fail to start with \"UiAutomationService ... already registered!\" while it is connected.",
        "resolution": "run `shadowdroid disconnect` before launching instrumentation tests, then `shadowdroid connect` again to resume driving",
    })
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
        "ui_automation": ui_automation_advisory(),
    });
    // Surface the UiAutomation-slot implication once, up front, so a later
    // instrumentation-test failure isn't a mystery. Muted by `--quiet`.
    tracing::info!(
        "ShadowDroid now holds the device's single UiAutomation slot — run `shadowdroid disconnect` before launching Espresso / UI Automator instrumentation tests"
    );
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
    free_ui_automation_slot(&serial).await?;
    let out = json!({"type": "disconnected", "device": serial});
    println!("{}", serde_json::to_string(&out).unwrap());
    Ok(())
}

/// Release the device's single UiAutomation slot held by ShadowDroid's
/// instrumentation: force-stop our packages, kill instrument zombies, drop the
/// port forward. Shared by `disconnect` and `test`.
async fn free_ui_automation_slot(serial: &str) -> Result<()> {
    adb::am_force_stop(serial, installer::TEST_PACKAGE).await?;
    adb::am_force_stop(serial, installer::APP_PACKAGE).await?;
    adb::kill_instrument_zombies(serial).await?;
    // Best-effort remove forward; ignore error if it wasn't set.
    let _ = adb::forward_remove(serial, installer::DEFAULT_PORT).await;
    Ok(())
}

/// `shadowdroid test -- <cmd>`: free the UiAutomation slot, run the user's
/// instrumentation-test command with stdio inherited, then reconnect (unless
/// `reconnect` is false). Exits with the command's own status code so CI / agents
/// see pass/fail.
async fn cmd_test(
    device: Option<&str>,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
    reconnect: bool,
    command: Vec<String>,
) -> Result<()> {
    use std::io::Write;

    let serial = resolve_serial(device).await?;
    free_ui_automation_slot(&serial)
        .await
        .context("freeing the UiAutomation slot before the test run")?;

    // Inherit stdio so the test runner's output streams live to the user.
    let program = command
        .first()
        .ok_or_else(|| anyhow!("no command given; use `shadowdroid test -- <command>`"))?;
    let status = std::process::Command::new(program)
        .args(&command[1..])
        .status()
        .with_context(|| format!("failed to launch `{}`", command.join(" ")))?;
    let exit_code = status.code();

    let reconnected = if reconnect {
        installer::ensure_ready(&serial, apk, any_apk_version)
            .await
            .is_ok()
    } else {
        false
    };

    let out = json!({
        "type": "action",
        "cmd": "test",
        "device": serial,
        "command": command,
        "exit_code": exit_code,
        "reconnected": reconnected,
    });
    println!("{}", serde_json::to_string(&out).unwrap());
    let _ = std::io::stdout().flush();

    if status.success() {
        Ok(())
    } else {
        // Propagate the test runner's status so callers (CI/agents) see failure.
        std::process::exit(exit_code.unwrap_or(1));
    }
}

pub(crate) async fn resolve_serial(explicit: Option<&str>) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_defines_global_quiet_flag() {
        // Guards the `--quiet`/`-q` contract that `main` pre-scans before clap runs.
        Cli::command().debug_assert();
        let cmd = Cli::command();
        let quiet = cmd
            .get_arguments()
            .find(|a| a.get_id().as_str() == "quiet")
            .expect("global --quiet flag should be defined");
        assert_eq!(quiet.get_short(), Some('q'));
        assert!(quiet.is_global_set(), "--quiet must be global");
        // The env must NOT be wired through clap: its strict bool parser rejects
        // `SHADOWDROID_QUIET=1`, which would dead-end every command. `main`
        // resolves the env manually instead (truthy spellings).
        assert!(
            quiet.get_env().is_none(),
            "SHADOWDROID_QUIET must be resolved manually, not via clap",
        );
    }

    #[test]
    fn app_current_accepts_json_flag() {
        // `app current` already emits JSON; the flag is accepted for consistency
        // with doctor/update/commands instead of erroring out as an unknown arg.
        let cli = Cli::try_parse_from(["shadowdroid", "app", "current", "--json"])
            .expect("`app current --json` should parse");
        assert!(matches!(cli.cmd, Cmd::App(AppCmd::Current { json: true })));
        // And it still parses without the flag.
        let bare = Cli::try_parse_from(["shadowdroid", "app", "current"]).unwrap();
        assert!(matches!(
            bare.cmd,
            Cmd::App(AppCmd::Current { json: false })
        ));
    }

    #[test]
    fn ui_tap_accepts_exact_and_clickable() {
        let cli = Cli::try_parse_from([
            "shadowdroid",
            "ui",
            "tap",
            "--text",
            "Allow",
            "--exact",
            "--clickable",
        ])
        .expect("`ui tap --exact --clickable` should parse");
        match cli.cmd {
            Cmd::Ui(UiCmd::Tap {
                exact,
                clickable,
                text,
                ..
            }) => {
                assert!(exact, "--exact should be set");
                assert!(clickable, "--clickable should be set");
                assert_eq!(text.as_deref(), Some("Allow"));
            }
            _ => panic!("expected `ui tap`"),
        }
    }

    #[test]
    fn ui_find_accepts_exact_and_clickable() {
        let cli =
            Cli::try_parse_from(["shadowdroid", "ui", "find", "--rid", "btn", "--exact"]).unwrap();
        match cli.cmd {
            Cmd::Ui(UiCmd::Find {
                exact, clickable, ..
            }) => {
                assert!(exact);
                assert!(!clickable);
            }
            _ => panic!("expected `ui find`"),
        }
    }

    #[test]
    fn any_apk_version_is_a_plain_global_flag() {
        // The env var is resolved in `run()` (not by clap) so that `1`/`yes`/etc.
        // don't dead-end on clap's strict bool parser. The CLI flag still works.
        let cli = Cli::try_parse_from(["shadowdroid", "ui", "dump", "--any-apk-version"]).unwrap();
        assert!(cli.any_apk_version);
        let arg = Cli::command()
            .get_arguments()
            .find(|a| a.get_id().as_str() == "any_apk_version")
            .expect("--any-apk-version should be defined")
            .clone();
        assert!(arg.is_global_set(), "--any-apk-version must stay global");
        assert!(
            arg.get_env().is_none(),
            "env must be resolved manually, not wired through clap",
        );
    }

    #[test]
    fn ui_automation_advisory_flags_instrumentation_conflict() {
        let v = ui_automation_advisory();
        assert_eq!(v["owner"], "shadowdroid");
        assert_eq!(v["blocks_instrumentation_tests"], true);
        assert!(
            v["advisory"]
                .as_str()
                .unwrap()
                .contains("already registered"),
            "advisory should name the instrumentation failure symptom",
        );
        assert!(
            v["resolution"]
                .as_str()
                .unwrap()
                .contains("shadowdroid disconnect"),
            "resolution should point at disconnect",
        );
    }

    // ── ui wait: foreground gates + match semantics ────────────────────

    fn wait_query() -> WaitQuery {
        WaitQuery {
            text: None,
            rid: None,
            desc: None,
            klass: None,
            activity: None,
            package: None,
            package_not: None,
            exact: false,
        }
    }

    fn app_ref(package: Option<&str>) -> crate::proto::AppRef {
        crate::proto::AppRef {
            package: package.map(Into::into),
            activity: None,
            pid: None,
        }
    }

    fn text_el(text: &str) -> Element {
        Element {
            id: 0,
            text: Some(text.into()),
            desc: None,
            klass: None,
            rid: None,
            bounds: None,
            tap: None,
            clickable: false,
            long_clickable: false,
            scrollable: false,
            checkable: false,
            focusable: false,
            enabled: true,
            selected: false,
            checked: false,
            focused: false,
            password: false,
            input: false,
        }
    }

    #[test]
    fn ui_wait_accepts_package_not_exact_and_aliases() {
        // --pkg-not is the visible alias for --package-not; --exact parses.
        let cli = Cli::try_parse_from([
            "shadowdroid",
            "ui",
            "wait",
            "--pkg-not",
            "com.livd",
            "--exact",
        ])
        .expect("`ui wait --pkg-not --exact` should parse");
        match cli.cmd {
            Cmd::Ui(UiCmd::Wait {
                package,
                package_not,
                exact,
                ..
            }) => {
                assert_eq!(package_not.as_deref(), Some("com.livd"));
                assert!(exact, "--exact should be set");
                assert!(package.is_none());
            }
            _ => panic!("expected `ui wait`"),
        }
        // --pkg is the visible alias for --package.
        let chrome = Cli::try_parse_from(["shadowdroid", "ui", "wait", "--pkg", "chrome"]).unwrap();
        match chrome.cmd {
            Cmd::Ui(UiCmd::Wait {
                package,
                package_not,
                ..
            }) => {
                assert_eq!(package.as_deref(), Some("chrome"));
                assert!(package_not.is_none());
            }
            _ => panic!("expected `ui wait`"),
        }
    }

    #[test]
    fn wait_exact_distinguishes_substring_from_full_match() {
        let els = [text_el("Done")];
        // Substring (default): "on" hits "Done", and the matched node is returned.
        let sub = WaitQuery {
            text: Some("on".into()),
            ..wait_query()
        };
        let out = wait_query_matches(&sub, &app_ref(Some("com.x")), &els);
        assert!(out.matched);
        assert_eq!(out.element.unwrap().text.as_deref(), Some("Done"));
        // Exact: "on" no longer matches "Done".
        let exact = WaitQuery {
            text: Some("on".into()),
            exact: true,
            ..wait_query()
        };
        assert!(!wait_query_matches(&exact, &app_ref(Some("com.x")), &els).matched);
    }

    #[test]
    fn wait_package_not_matches_only_after_leaving_app() {
        let q = WaitQuery {
            package_not: Some("com.livd".into()),
            ..wait_query()
        };
        // Still in com.livd → not satisfied.
        assert!(!wait_query_matches(&q, &app_ref(Some("com.livd")), &[]).matched);
        // Foreground moved to chrome → satisfied (left the package).
        assert!(wait_query_matches(&q, &app_ref(Some("com.android.chrome")), &[]).matched);
    }

    #[test]
    fn image_dimensions_parses_png_and_rejects_junk() {
        let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR chunk length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&2400u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]); // bit depth / color type / …
        assert_eq!(image_dimensions(&png), Some((1080, 2400)));
        assert_eq!(image_dimensions(b"not an image"), None);
    }
}
