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
//!
//! ## Adding a command (the canonical recipe)
//!
//! 1. **Declare it.** Add a variant to the relevant `*Cmd` enum (e.g. `UiCmd`),
//!    or a flat `Cmd` variant for a top-level verb. Give every flag a `///` doc
//!    comment — `--help` and `commands --json` (the agent catalog) render from
//!    it, and a `tests/contract.rs`-style reviewer will notice if it's missing.
//! 2. **Route it.** Add a match arm in the relevant dispatcher (`dispatch_ui`,
//!    `dispatch_net`, …) or in `run()` for a host-only verb.
//! 3. **Describe the agent decision.** Add the command to
//!    `cmd/introspect/agent_metadata.rs` with non-empty `next_actions`. The live
//!    catalog test is exhaustive, and terminal JSON emitters reuse that same
//!    metadata as their safe fallback.
//! 4. **Emit the result** with [`crate::events::emit_action`] (enveloped) or
//!    [`crate::events::emit_result`] (raw terminal JSON) — never hand-roll a
//!    JSON `println!`. That keeps one-line output and non-empty `next_actions`
//!    enforceable. Errors propagate through [`report_error`]; expected failures
//!    should use [`crate::diagnostic::DiagnosticError`] with a stable code and
//!    recovery specific to the rejected state.
//! 5. **If it reads config defaults** (app/project/studio-url/…), wire them in
//!    `apply_config_defaults` so flags fall back to `.shadowdroid/config.json`.
//! 6. **Match selectors** through [`crate::selector`] (host side) so `--text`
//!    normalization stays consistent with `ui find`/`tap`.

use crate::ids::Serial;
use anyhow::{Context, Result, anyhow, bail};
use clap::error::{ContextKind, ContextValue};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::cmd::app_install::AppInstallArgs;
use crate::cmd::debug::{DebugArgs, DebugCmd};
use crate::cmd::debugger::{DebugMode, DebuggerCmd};
use crate::cmd::device_profile::ProfileApplyArgs;
use crate::cmd::focus::FocusArgs;
use crate::cmd::layout::{LayoutArgs, LayoutCmd};
use crate::cmd::permissions::AppOpScope;
use crate::cmd::scroll::ScrollArgs;
use crate::cmd::studio::{StudioArgs, StudioCmd};
use crate::config::{ShadowDroidConfig, expand_config_path};
use crate::device::client::{ServerClient, is_transient_transport_error};
use crate::device::{adb, installer};
use crate::events::{CompactElement, ScreenFormat, emit_action, emit_error};
use crate::fusion::{Outcome, top_screen_texts};
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

    /// Named project device target (for example `mobile` or `tv`). Resolves a
    /// configured stable AVD name or physical serial. An explicit --device
    /// takes precedence.
    #[arg(long, global = true, env = "SHADOWDROID_TARGET")]
    pub target: Option<String>,

    /// Reassign an AVD target currently claimed by another project. This is an
    /// explicit isolation override and has no effect on direct --device use.
    #[arg(long, global = true)]
    pub takeover: bool,

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
    //
    // Implementation note (kept as a plain comment so it does NOT leak into every
    // subcommand's `--help`): the flag is `--project-root`, NOT `--project`. This
    // is `global = true`, so clap injects it into every subcommand, and several
    // subcommands define their own `project` arg (`config init`'s scope flag,
    // `debug break`/`eval`'s source root). Two args with the same name on one
    // command make clap panic on a TypeId downcast at access time (the duplicate-id
    // debug_assert is compiled out in release). Keep this name distinct from every
    // per-subcommand `project`.
    #[arg(long, global = true, env = "SHADOWDROID_PROJECT", value_name = "PATH")]
    pub project_root: Option<PathBuf>,

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
        /// Limit command-tree expansion (1 = top-level routing catalog).
        #[arg(long, value_name = "N", conflicts_with = "describe")]
        depth: Option<usize>,
        /// Return one command contract by its space-separated path, e.g. "ui tap".
        #[arg(long, value_name = "COMMAND_PATH", conflicts_with = "depth")]
        describe: Option<String>,
    },
    /// Structured, bounded logcat: app-scoped JSON log lines with crash/ANR
    /// blocks parsed out, windowed (`--last 60s`) and deduplicated. Works
    /// without the on-device server.
    Log(crate::cmd::log::LogArgs),
    /// One bounded triage read: what just went wrong? Fuses the last
    /// crash/ANR, recent error logs, the current screen, and network failures
    /// into a verdict with evidence and next steps.
    Why(crate::cmd::why::WhyArgs),
    /// Opt-in local usage log (verb + duration + error code per invocation;
    /// never argument values, never leaves this machine).
    #[command(subcommand)]
    Usage(crate::cmd::usage::UsageCmd),
    /// Inspect, generate, and validate user/project JSON config.
    Config(crate::cmd::config::ConfigArgs),
    /// Generate, install, or refresh Agent Skills (claude-code / cursor /
    /// codex / gemini / antigravity) at user or project scope.
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
    /// Launch the app's default activity, or an explicit activity with --activity.
    Start {
        /// App package, app alias from config, or installed package name.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
        /// Explicit activity class to launch, e.g. .MainActivity or com.example.MainActivity.
        #[arg(long, value_name = "ACTIVITY")]
        activity: Option<String>,
    },
    /// Force-stop the app.
    Stop {
        /// App package, app alias from config, or installed package name.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
    },
    /// Install an APK and run the app-under-test setup ritual (clear/grant/launch/wait).
    Install(AppInstallArgs),
    /// Like `install`, but uninstall any existing copy first.
    Reinstall(AppInstallArgs),
    /// Clear the app's data.
    Clear {
        /// App package, app alias from config, or installed package name.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
    },
    /// Version name/code + label.
    Info {
        /// App package, app alias from config, or installed package name.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
    },
    /// Wait for the app to launch (or, with --front, reach the foreground).
    Wait {
        /// App package, app alias from config, or installed package name.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
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
    /// Get appop mode(s), preserving UID/package scopes and effective precedence.
    Get { package: String, op: Option<String> },
    /// Set and verify an appop mode at an explicit UID or package scope.
    Set {
        package: String,
        op: String,
        mode: String,
        /// Android policy scope. UID modes govern package modes when both exist.
        #[arg(long, value_enum)]
        scope: AppOpScope,
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
        /// Require Unix permission bits after the push (octal, e.g. 644).
        /// Omit on shared/FUSE storage where Android controls the effective mode.
        #[arg(long, value_parser = parse_octal_mode)]
        mode: Option<u32>,
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
        /// Screen-bound handle from a fresh `ui dump` (i:.../e:...).
        #[arg(long)]
        handle: Option<String>,
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
        /// Require the matched node itself to be clickable instead of resolving
        /// its nearest enabled clickable ancestor.
        #[arg(long)]
        clickable: bool,
        /// Explicitly allow center-coordinate injection when no enabled clickable
        /// target can perform ACTION_CLICK. Selector taps otherwise fail safely.
        #[arg(long)]
        coordinate_fallback: bool,
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
    },
    /// Set a slider/range control through Android accessibility and verify readback.
    SetProgress {
        /// Screen-bound handle from a fresh `ui dump` (i:.../e:...).
        #[arg(long)]
        handle: Option<String>,
        /// Element id from a fresh `ui dump`.
        #[arg(long)]
        id: Option<u32>,
        /// Match by exact/normalized visible text.
        #[arg(long)]
        text: Option<String>,
        /// Match by resource id or Compose test tag.
        #[arg(long)]
        rid: Option<String>,
        /// Match by content description.
        #[arg(long)]
        desc: Option<String>,
        /// Match by supported xpath.
        #[arg(long)]
        xpath: Option<String>,
        /// Match selector values exactly instead of as a substring.
        #[arg(long)]
        exact: bool,
        /// Absolute value in the control's exposed min..max range.
        #[arg(long, allow_hyphen_values = true)]
        value: Option<f64>,
        /// Position from 0 to 100 within the exposed range.
        #[arg(long, allow_hyphen_values = true)]
        percent: Option<f64>,
        /// Clamp an out-of-range value/percent instead of rejecting it.
        #[arg(long)]
        clamp: bool,
        /// Explicitly permit an approximate coordinate track click when semantic mutation is unavailable.
        #[arg(long)]
        coordinate_fallback: bool,
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
    },
    /// Double-tap at <x> <y> coordinates.
    DoubleTap {
        x: i32,
        y: i32,
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
    },
    /// Long-press at <x> <y> coordinates (hold for --duration-ms).
    LongTap {
        x: i32,
        y: i32,
        /// How long to hold the press, in milliseconds.
        #[arg(long, default_value_t = 600)]
        duration_ms: u32,
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
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
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
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
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
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
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
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
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
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
        /// Screen-bound handle from a fresh `ui dump` (i:.../e:...).
        #[arg(long)]
        handle: Option<String>,
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
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
    },
    /// Press a named key or keycode.
    Key {
        name: String,
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
    },
    /// Hide the soft keyboard if ShadowDroid sees it; no-op when already hidden.
    HideKeyboard,
    /// Press the Back button.
    Back {
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
    },
    /// Press the Home button.
    Home {
        #[command(flatten)]
        fusion: crate::fusion::FusionArgs,
    },
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
    Check {
        /// App package, app alias from config, or installed package name.
        #[arg(value_name = "PACKAGE")]
        package: Option<String>,
        /// Ignore `proxy.ca_trusted` and the verify-once cache; probe the device.
        #[arg(long)]
        fresh: bool,
    },
    /// Install / trust the ShadowDroid CA on the device.
    Trust {
        /// Choose the best available install path for this device (default).
        #[arg(long)]
        auto: bool,
        /// Push into the system trust store (emulator/root).
        #[arg(long)]
        system: bool,
        /// Stage the CA in Downloads and open Settings for manual installation.
        /// A device screen-lock credential is required to finish the install.
        #[arg(long)]
        push: bool,
        /// Legacy alias for --push. This does not automate certificate install.
        #[arg(long, hide = true, conflicts_with = "push")]
        ui: bool,
        /// Ignore `proxy.ca_trusted` and the verify-once cache; install/verify
        /// against the device for real.
        #[arg(long)]
        fresh: bool,
    },
    /// Manage the proxy's signing CA (use your own, inspect it, or regenerate).
    #[command(subcommand)]
    Ca(NetCaCmd),
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
        /// Validate the upstream server's TLS certificate. Off by default so
        /// self-signed dev/staging backends work; enable to catch a tampered
        /// upstream (note: it will also surface the app's own pinning failures).
        #[arg(long)]
        verify_upstream: bool,
        /// Redact sensitive headers (authorization, cookie, set-cookie,
        /// proxy-authorization) from captured flows before they're logged.
        #[arg(long)]
        redact: bool,
    },
    /// Stop the proxy and tear down device wiring.
    Stop {
        /// Also remove the ShadowDroid CA from the device trust store.
        #[arg(long)]
        revoke_ca: bool,
        /// Neutral hostname used to verify DNS after teardown.
        #[arg(
            long,
            env = "SHADOWDROID_NET_CANARY_HOST",
            default_value = "example.com"
        )]
        canary_host: String,
    },
    /// Proxy + device-wiring status (daemon, http_proxy, and adb reverse).
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
        /// Write the full response body to this file (avoids inline-JSON
        /// truncation for large responses) instead of printing the flow.
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
        /// Emit the flow as a single-entry HAR object.
        #[arg(long, conflicts_with = "body_file")]
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
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Replace the body with the contents of this file.
        #[arg(long, conflicts_with = "body")]
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
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Response body from this file.
        #[arg(long, conflicts_with = "body")]
        body_file: Option<PathBuf>,
        /// Set a response header; repeatable.
        #[arg(long, value_name = "NAME=VALUE")]
        set_header: Vec<String>,
    },
    /// Declarative response/request rules.
    #[command(subcommand)]
    Rule(NetRuleCmd),
    /// Convenience: map URL-matching requests to a local response file.
    Override {
        /// URL glob to match, e.g. https://api.example.com/v1/dict*.
        #[arg(long, value_name = "GLOB")]
        url: String,
        /// Local file to serve as the response body.
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
    },
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

impl NetCmd {
    fn allows_target_start(&self) -> bool {
        matches!(
            self,
            Self::Check { .. } | Self::Trust { .. } | Self::Start { .. } | Self::Replay { .. }
        )
    }
}

#[derive(Subcommand)]
pub enum NetCaCmd {
    /// Install a user-provided CA as the proxy's signing CA (replaces the
    /// auto-generated one). Everything downstream — `net trust`, `net check`,
    /// leaf signing — then uses your CA. Re-run `net trust` after importing so
    /// the device trusts the new CA, and restart the proxy if it's running.
    Import {
        /// PEM certificate file (your root/intermediate CA). May also contain the
        /// private key (a combined PEM like mitmproxy's `mitmproxy-ca.pem`).
        #[arg(long, value_name = "PATH")]
        cert: PathBuf,
        /// PEM private key file. Omit if the key is in the --cert file. PKCS#1 /
        /// SEC1 keys are converted to PKCS#8 via openssl automatically.
        #[arg(long, value_name = "PATH")]
        key: Option<PathBuf>,
        #[command(flatten)]
        scope: CaScopeArgs,
    },
    /// Show the current signing CA: source, subject, validity, key type, and the
    /// Android trust-store hash.
    Info {
        #[command(flatten)]
        scope: CaScopeArgs,
    },
    /// Discard the current CA (backed up to `.bak`) and generate a fresh
    /// ShadowDroid CA — the way back after an import, and how a per-project CA is
    /// first minted (`--project`).
    Reset {
        #[command(flatten)]
        scope: CaScopeArgs,
    },
}

/// `--project` / `--global` scope selector for the `net ca` verbs. Neither flag
/// = auto: the project `.shadowdroid/` when one exists, else the global CA.
#[derive(clap::Args)]
pub struct CaScopeArgs {
    /// Operate on the per-project CA (`<project>/.shadowdroid/ca.*`).
    #[arg(long, conflicts_with = "global")]
    pub project: bool,
    /// Operate on the global CA (`~/.shadowdroid/net/ca.*`).
    #[arg(long, conflicts_with = "project")]
    pub global: bool,
}

#[derive(Subcommand)]
pub enum NetRuleCmd {
    /// Add a rule. Request-phase kinds: block, delay, map-local, map-remote,
    /// set-request-header. Response-phase kinds: set-status, set-response-header,
    /// replace.
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
    /// block | delay | map-local | map-remote | set-request-header | set-status
    /// | set-response-header | replace
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
    /// Kind-specific positionals: block [status], delay <ms>, map-local <file>,
    /// map-remote <host:port>, set-request-header <name> <value>,
    /// set-response-header <name> <value>, set-status <code>, replace <regex> <repl>.
    #[arg(value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(clap::Args)]
pub struct NetDaemonArgs {
    /// ADB serial the daemon wires itself to.
    #[arg(long)]
    pub serial: String,
    /// Identity of the parent `net start` attempt (internal readiness guard).
    #[arg(long)]
    pub startup_id: String,
    /// Device-facing proxy port (`http_proxy` target; reverse-mapped to the host).
    #[arg(long, default_value_t = crate::net::DEFAULT_PROXY_PORT)]
    pub port: u16,
    /// Host loopback port the proxy binds (per-serial; set by the parent `net start`).
    #[arg(long, default_value_t = crate::net::DEFAULT_PROXY_PORT)]
    pub host_port: u16,
    /// Signing CA certificate to load (resolved by the parent `net start`).
    #[arg(long)]
    pub ca_cert: PathBuf,
    /// Signing CA private key to load (resolved by the parent `net start`).
    #[arg(long)]
    pub ca_key: PathBuf,
    /// Host globs to scope capture to (repeatable; empty = all).
    #[arg(long)]
    pub host: Vec<String>,
    /// Strip cache-validation request headers.
    #[arg(long)]
    pub anticache: bool,
    /// Strip Accept-Encoding to force uncompressed responses.
    #[arg(long)]
    pub anticomp: bool,
    /// Validate the upstream server's TLS certificate.
    #[arg(long)]
    pub verify_upstream: bool,
    /// Redact sensitive headers from captured flows.
    #[arg(long)]
    pub redact: bool,
}

/// Parse argv, converting clap's plaintext usage errors into the same
/// `{"type":"error",…}` contract as runtime failures (item: agents shouldn't
/// have to special-case a `try '--help'` plaintext line). `--help`/`--version`
/// are not errors and are rendered exactly as clap would.
fn parse_cli() -> Cli {
    use clap::error::ErrorKind;
    let matches = match Cli::command().try_get_matches_from(std::env::args_os()) {
        Ok(matches) => matches,
        Err(err) => {
            let partial_path = crate::cmd::usage::verb_from_argv();
            let partial_path =
                (!partial_path.is_empty() && partial_path != "(unparsed)").then_some(partial_path);
            if let Some(path) = &partial_path {
                crate::events::set_current_command_path(path.clone());
            }
            let kind = err.kind();
            // Help/version: render to stdout as usual, exit success.
            if matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let _ = err.print();
                std::process::exit(0);
            }
            // Bare invocation is a machine-readable usage failure. Explicit
            // `--help` remains the human help path.
            if kind == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
                crate::cmd::usage::record_parse_error(
                    "MissingSubcommand",
                    false,
                    partial_path.as_deref(),
                );
                emit_error(
                    "usage",
                    "missing_subcommand",
                    "no command was provided",
                    json!({
                        "next_actions": [
                            "run `shadowdroid commands --json --depth 1` for the agent command catalog",
                            "run `shadowdroid --help` for human-readable help"
                        ]
                    }),
                );
                std::process::exit(2);
            }
            // A genuine usage error → structured JSON that *names* the bad flag
            // (and clap's spelling suggestion) instead of a `try '--help'` line.
            let ctx_str = |kind| clap_context_string(&err, kind);
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
            let suggestion = actionable_clap_suggestion(&err);
            if let Some((label, value)) = &suggestion {
                extra.insert("suggestion".into(), json!(value));
                extra.insert("suggestion_kind".into(), json!(label));
            }
            if let Some(hint) = ctx_str(ContextKind::Suggested) {
                extra.insert("hint".into(), json!(hint));
            }
            if let Some(values) = ctx_str(ContextKind::ValidValue) {
                extra.insert("valid_values".into(), json!(values));
            }
            let mut next_actions = Vec::new();
            if let Some((label, value)) = suggestion {
                if label == "subcommand" {
                    if let Some(path) = crate::events::current_command_path() {
                        next_actions.push(format!("shadowdroid {path} {value} --help"));
                    }
                } else {
                    next_actions.push(format!(
                        "replace the rejected {label} with {value:?}, then retry"
                    ));
                }
            }
            if let Some(path) = crate::events::current_command_path() {
                next_actions.push(format!("shadowdroid {path} --help"));
                next_actions.push(format!("shadowdroid commands --json --describe '{path}'"));
            } else {
                next_actions.push("shadowdroid commands --json --depth 2".to_string());
            }
            extra.insert("next_actions".into(), json!(next_actions));
            crate::cmd::usage::record_parse_error(
                &format!("{kind:?}"),
                extra.contains_key("suggestion"),
                partial_path.as_deref(),
            );
            emit_error("usage", "usage", &msg, serde_json::Value::Object(extra));
            std::process::exit(2);
        }
    };
    if let Some(path) = parsed_command_path(&matches) {
        crate::events::set_current_command_path(path);
    }
    Cli::from_arg_matches(&matches).unwrap_or_else(|err| {
        // `matches` was produced by this exact derive-generated command, so a
        // conversion failure is an internal invariant violation rather than a
        // second user-facing parse branch.
        panic!("validated clap matches did not convert to Cli: {err}")
    })
}

fn clap_context_string(err: &clap::Error, kind: ContextKind) -> Option<String> {
    err.get(kind).and_then(|value| match value {
        ContextValue::String(value) => Some(value.clone()),
        ContextValue::Strings(values) => Some(values.join(", ")),
        ContextValue::StyledStr(value) => Some(value.to_string()),
        ContextValue::StyledStrs(values) => Some(
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        _ => None,
    })
}

fn actionable_clap_suggestion(err: &clap::Error) -> Option<(&'static str, String)> {
    [
        (ContextKind::SuggestedCommand, "command"),
        (ContextKind::SuggestedSubcommand, "subcommand"),
        (ContextKind::SuggestedArg, "argument"),
        (ContextKind::SuggestedValue, "value"),
    ]
    .into_iter()
    .find_map(|(context, label)| clap_context_string(err, context).map(|value| (label, value)))
}

fn parsed_command_path(matches: &clap::ArgMatches) -> Option<String> {
    let mut cursor = matches;
    let mut path = Vec::new();
    while let Some((name, child)) = cursor.subcommand() {
        path.push(name.to_string());
        cursor = child;
    }
    (!path.is_empty()).then(|| path.join(" "))
}

/// Lazy device selection keeps passive/host-only commands from starting an
/// emulator. Explicit `--device` wins; otherwise a requested/implicit named
/// target wins over the legacy config.device/sole-attached fallback.
struct DeviceSelection {
    explicit_device: Option<String>,
    requested_target: Option<String>,
    takeover: bool,
}

impl DeviceSelection {
    fn target_name<'a>(&'a self, config: &'a ShadowDroidConfig) -> Option<&'a str> {
        self.requested_target
            .as_deref()
            .or_else(|| config.implicit_target())
    }

    async fn resolve(&self, config: &ShadowDroidConfig) -> Result<Serial> {
        if let Some(device) = self.explicit_device.as_deref() {
            return resolve_serial(Some(device)).await;
        }
        if let Some(target) = self.target_name(config) {
            let serial = crate::device::target::resolve(config, target, self.takeover).await?;
            crate::events::set_current_device(serial.to_string());
            return Ok(serial);
        }
        resolve_serial(config.device.as_deref()).await
    }

    async fn resolve_existing(&self, config: &ShadowDroidConfig) -> Result<Serial> {
        if let Some(device) = self.explicit_device.as_deref() {
            return resolve_serial(Some(device)).await;
        }
        if let Some(target) = self.target_name(config) {
            let serial =
                crate::device::target::resolve_existing(config, target, self.takeover).await?;
            crate::events::set_current_device(serial.to_string());
            return Ok(serial);
        }
        resolve_serial(config.device.as_deref()).await
    }

    /// Preserve doctor's inventory-first behavior for direct/legacy serials;
    /// only named targets need lifecycle resolution and optional startup.
    async fn doctor_device(&self, config: &ShadowDroidConfig) -> Result<Option<String>> {
        if let Some(device) = self.explicit_device.clone() {
            return Ok(Some(device));
        }
        if let Some(name) = self.target_name(config) {
            if let Some((_, target)) = config.target(name)
                && let Some(serial) = target.serial.clone()
            {
                return Ok(Some(serial));
            }
            return self
                .resolve(config)
                .await
                .map(|serial| Some(serial.to_string()));
        }
        Ok(config.device.clone())
    }

    /// Android Studio's bridge accepts either a serial or AVD name. Returning
    /// the stable binding avoids starting an emulator for host-only reads.
    fn debugger_filter(&self, config: &ShadowDroidConfig) -> Result<Option<String>> {
        if let Some(device) = self.explicit_device.clone() {
            return Ok(Some(device));
        }
        if let Some(name) = self.target_name(config) {
            let Some((_, target)) = config.target(name) else {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "target_not_configured",
                    "device_target",
                    format!("device target `{name}` is not configured"),
                )
                .detail(json!({
                    "target_name": name,
                    "available_targets": config.targets.keys().collect::<Vec<_>>(),
                }))
                .next_actions([
                    "run `shadowdroid config schema --json` to inspect the targets shape",
                    "fix the target reference and run `shadowdroid config validate --json`",
                ])
                .into());
            };
            return Ok(target.serial.clone().or_else(|| target.avd.clone()));
        }
        Ok(config.device.clone())
    }
}

/// Entry point: run the command, then (if the user opted in) append one line
/// to the local usage log — verb, duration, outcome. The log write can never
/// fail the command.
pub async fn run() -> Result<()> {
    let started = std::time::Instant::now();
    let result = run_inner().await;
    crate::cmd::usage::record(started, &result);
    result
}

async fn run_inner() -> Result<()> {
    let cli = parse_cli();

    // Recovery and self-description must remain available when a discovered
    // config file is malformed. These commands either inspect raw config files
    // themselves or do not use config at all, so dispatch them before the normal
    // layered load.
    match &cli.cmd {
        Cmd::Commands {
            json,
            depth,
            describe,
        } => return crate::cmd::introspect::run(*json, *depth, describe.as_deref()),
        Cmd::Config(args) => return crate::cmd::config::run(args),
        Cmd::Skill(args) => return crate::cmd::skill::run(args),
        Cmd::Usage(args) => return crate::cmd::usage::run(args),
        Cmd::Update { check, json } => return crate::update::cmd_update(*check, *json).await,
        _ => {}
    }

    let config = ShadowDroidConfig::load()?;
    let selection = DeviceSelection {
        explicit_device: cli.device.clone(),
        requested_target: cli.target.clone(),
        takeover: cli.takeover,
    };
    let apk = cli.apk;
    let project = cli
        .project_root
        .or_else(|| config.project.as_deref().map(PathBuf::from));
    // Resolve `--any-apk-version` ourselves from the env (clap's strict bool env
    // parsing rejects `1`/`yes` and errors with `[possible values: true, false]`).
    // `env_truthy` accepts 1/true/yes/on; anything else (including 0/no/off) is false.
    let any_apk_version =
        cli.any_apk_version || crate::hostenv::env_truthy("SHADOWDROID_ANY_APK_VERSION");
    let mut cmd = cli.cmd;
    apply_config_defaults(&mut cmd, &config);

    // ── Phase 1: commands that do NOT need the on-device server ──
    // doctor diagnoses the very server `ensure_ready` would start; collect does
    // its own best-effort bring-up so it can degrade; perm/appops/profile and
    // `app install`/`reinstall` are pure host-side `adb`.
    match &cmd {
        Cmd::Devices => return cmd_devices(&config).await,
        Cmd::Init(args) => return crate::cmd::studio::run_init(args).await,
        Cmd::Update { .. }
        | Cmd::Commands { .. }
        | Cmd::Config(_)
        | Cmd::Skill(_)
        | Cmd::Usage(_) => {
            unreachable!("recovery command handled before config load")
        }
        Cmd::Studio(args) => return crate::cmd::studio::run(args).await,
        // `aar` install/status/remove are host-only (Gradle + filesystem); the
        // capture/intercept/resume/drop/agent verbs talk to the running in-app
        // agent and resolve a device serial internally.
        Cmd::Aar(c) => {
            let serial = if c.requires_device() {
                Some(selection.resolve(&config).await?)
            } else {
                None
            };
            return crate::cmd::aar::run(
                c,
                project.as_deref(),
                serial.as_ref().map(Serial::as_str),
            )
            .await;
        }
        Cmd::Debug(args) if args.is_host_only() => {
            // Host-only debugger commands skip device resolution / ensure_ready,
            // but still honor an explicit device/target to pick the matching
            // session. Resolve only here so passive commands stay passive even
            // when a target reference needs repair.
            let debugger_device = selection.debugger_filter(&config)?;
            return crate::cmd::debug::run_host_only(args, debugger_device.as_deref()).await;
        }
        Cmd::Connect => {
            let serial = selection.resolve(&config).await?;
            return cmd_connect(&serial, apk.as_deref(), any_apk_version).await;
        }
        Cmd::Disconnect => {
            let serial = selection.resolve_existing(&config).await?;
            return cmd_disconnect(&serial).await;
        }
        Cmd::Test {
            no_reconnect,
            command,
        } => {
            let serial = selection.resolve(&config).await?;
            return cmd_test(
                &serial,
                apk.as_deref(),
                any_apk_version,
                !*no_reconnect,
                command.clone(),
            )
            .await;
        }
        Cmd::Doctor {
            app,
            fix,
            force,
            json,
        } => {
            let doctor_device = selection.doctor_device(&config).await?;
            return crate::cmd::doctor::run(
                doctor_device.as_deref(),
                *fix,
                *force,
                *json,
                app.as_deref(),
                project.as_deref(),
                &config,
            )
            .await;
        }
        Cmd::Collect {
            app,
            out,
            no_screenshot,
        } => {
            let serial = selection.resolve(&config).await?;
            let app = resolve_app_package(&config, Some(&serial), app.clone()).await?;
            return crate::cmd::collect::run(&serial, app, out.clone(), !*no_screenshot).await;
        }
        // `log` and `why` are host-side reads over adb (+ existing routes for
        // `why`'s screen section) — they must work when the server is down,
        // since "the server is down" is exactly when you need them.
        Cmd::Log(args) => {
            let serial = selection.resolve(&config).await?;
            return crate::cmd::log::run(&serial, &config, project.as_deref(), args).await;
        }
        Cmd::Why(args) => {
            let serial = selection.resolve(&config).await?;
            return crate::cmd::why::run(&serial, &config, project.as_deref(), args).await;
        }
        Cmd::Perm(c) => {
            let serial = selection.resolve(&config).await?;
            return dispatch_perm(c, &serial).await;
        }
        Cmd::Appops(c) => {
            let serial = selection.resolve(&config).await?;
            return dispatch_appops(c, &serial).await;
        }
        Cmd::Profile(c) => {
            let serial = selection.resolve(&config).await?;
            return dispatch_profile(c, &serial).await;
        }
        Cmd::App(AppCmd::Install(a)) => {
            let serial = selection.resolve(&config).await?;
            return crate::cmd::app_install::run(&serial, a, false).await;
        }
        Cmd::App(AppCmd::Reinstall(a)) => {
            let serial = selection.resolve(&config).await?;
            return crate::cmd::app_install::run(&serial, a, true).await;
        }
        // `net` is host-only: the proxy is a host-side daemon driven over adb.
        // (`trust --push` only stages through adb and opens Android Settings.)
        Cmd::Net(c) => {
            // The detached daemon carries its own `--serial` and must not depend
            // on a live device being attached; everything else resolves one.
            if matches!(c, NetCmd::Daemon(_)) {
                // The daemon ignores this arg (it reads its serial from the
                // deserialized DaemonConfig); pass an empty sentinel.
                return dispatch_net(c, &Serial::new(""), &config).await;
            }
            // `net ca` manages host-side CA files and needs no device attached;
            // a serial is used only to enrich the "restart the running proxy"
            // hint, so resolve it best-effort rather than failing without one.
            if matches!(c, NetCmd::Ca(_)) {
                let direct = selection
                    .explicit_device
                    .as_deref()
                    .or(config.device.as_deref());
                let serial = resolve_serial(direct)
                    .await
                    .unwrap_or_else(|_| Serial::new(""));
                return dispatch_net(c, &serial, &config).await;
            }
            let serial = if c.allows_target_start() {
                selection.resolve(&config).await?
            } else {
                selection.resolve_existing(&config).await?
            };
            return dispatch_net(c, &serial, &config).await;
        }
        _ => {}
    }

    // ── Phase 2: server-backed commands share one bring-up ──
    let serial = selection.resolve(&config).await?;
    let client =
        installer::ensure_ready_for_command(&serial, apk.as_deref(), any_apk_version).await?;

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
        | Cmd::Log(_)
        | Cmd::Why(_)
        | Cmd::Usage(_)
        | Cmd::Config(_)
        | Cmd::Skill(_)
        | Cmd::Studio(_)
        | Cmd::Perm(_)
        | Cmd::Appops(_)
        | Cmd::Profile(_)
        | Cmd::Net(_)
        | Cmd::Aar(_) => unreachable!("handled before ensure_ready"),

        // ── namespaces ─────────────────────────────────────────
        Cmd::App(app_cmd) => dispatch_app(app_cmd, &client, &config, &serial).await?,
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

/// Every `ui` verb runs with the since-last-command events probe alongside it
/// (one adb round-trip hidden behind the action), then emits exactly once from
/// here — so crashes/ANRs that happened since the previous CLI command ride
/// the same JSON object the agent was already reading, success or error.
async fn dispatch_ui(
    c: UiCmd,
    client: &ServerClient,
    serial: &Serial,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
) -> Result<()> {
    let probe = crate::crashscan::spawn_probe(serial);
    let result = dispatch_ui_inner(c, client, serial, apk, any_apk_version).await;
    let events = crate::crashscan::finish_probe(probe).await;
    match result {
        Ok(outcome) => {
            outcome.emit(events);
            Ok(())
        }
        Err(err) => {
            // report_error (in main) drains the stash into the error envelope.
            crate::events::stash_events(events);
            Err(err)
        }
    }
}

async fn dispatch_ui_inner(
    c: UiCmd,
    client: &ServerClient,
    serial: &Serial,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
) -> Result<Outcome> {
    match c {
        UiCmd::Dump { full } => cmd_screen(serial, apk, any_apk_version, client, full).await,
        UiCmd::Audit => {
            let screen = read_screen_with_reconnect(serial, apk, any_apk_version, client).await?;
            let mut body = crate::cmd::authoring::audit_elements(&screen.elements);
            if let serde_json::Value::Object(m) = &mut body {
                m.insert("screen_hash".into(), json!(screen.screen_hash));
                m.insert(
                    "screen_hash_version".into(),
                    json!(screen.screen_hash_version),
                );
                m.insert("content_hash".into(), json!(screen.content_hash));
                m.insert("interaction_hash".into(), json!(screen.interaction_hash));
                m.insert(
                    "interaction_hash_version".into(),
                    json!(screen.interaction_hash_version),
                );
            }
            Ok(Outcome::Action("ui_audit", body))
        }
        UiCmd::Gen { name } => {
            let screen = read_screen_with_reconnect(serial, apk, any_apk_version, client).await?;
            print!(
                "{}",
                crate::cmd::authoring::generate_screen_object(&name, &screen.elements)
            );
            Ok(Outcome::Done)
        }
        UiCmd::Screenshot {
            path,
            format,
            scale,
            quality,
        } => cmd_screenshot(client, path, format, scale, quality).await,
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
            let body = if full {
                let found = r.matched.is_some();
                json!({"matched":found,"element":r.matched,"elements":r.elements})
            } else {
                let matched = r.matched.map(CompactElement::from);
                let found = matched.is_some();
                let elements: Vec<CompactElement> =
                    r.elements.into_iter().map(CompactElement::from).collect();
                json!({"matched":found,"element":matched,"elements":elements})
            };
            Ok(Outcome::Action("find", body))
        }
        UiCmd::Tap {
            handle,
            id,
            a,
            b,
            text,
            rid,
            desc,
            xpath,
            exact,
            clickable,
            coordinate_fallback,
            mut fusion,
        } => {
            crate::fusion::bind_element_handle(
                &mut fusion,
                handle,
                id.is_some()
                    || a.is_some()
                    || b.is_some()
                    || text.is_some()
                    || rid.is_some()
                    || desc.is_some()
                    || xpath.is_some(),
            )?;
            let hint = crate::fusion::SelectorHint {
                text: text.clone(),
                rid: rid.clone(),
                desc: desc.clone(),
            };
            crate::fusion::run_fused(client, &fusion, Some(hint), |handle_id| {
                cmd_tap(
                    client,
                    handle_id.or(id),
                    a,
                    b,
                    text,
                    rid,
                    desc,
                    xpath,
                    exact,
                    clickable,
                    coordinate_fallback,
                )
            })
            .await
        }
        UiCmd::SetProgress {
            handle,
            id,
            text,
            rid,
            desc,
            xpath,
            exact,
            value,
            percent,
            clamp,
            coordinate_fallback,
            mut fusion,
        } => {
            validate_set_progress_input(value, percent)?;
            crate::fusion::bind_element_handle(
                &mut fusion,
                handle,
                id.is_some()
                    || text.is_some()
                    || rid.is_some()
                    || desc.is_some()
                    || xpath.is_some(),
            )?;
            let hint = crate::fusion::SelectorHint {
                text: text.clone(),
                rid: rid.clone(),
                desc: desc.clone(),
            };
            crate::fusion::run_fused(client, &fusion, Some(hint), |handle_id| async move {
                let query = SelectorQuery {
                    id: handle_id.or(id),
                    text,
                    rid,
                    desc,
                    xpath,
                    exact,
                    ..Default::default()
                };
                let response = client
                    .set_progress(&query, value, percent, clamp, coordinate_fallback)
                    .await?;
                Ok(("set_progress", serde_json::to_value(response)?))
            })
            .await
        }
        UiCmd::DoubleTap { x, y, fusion } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                client.double_tap(x, y).await?;
                Ok(("double_tap", json!({"x":x,"y":y})))
            })
            .await
        }
        UiCmd::LongTap {
            x,
            y,
            duration_ms,
            fusion,
        } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                client.long_tap(x, y, duration_ms).await?;
                Ok(("long_tap", json!({"x":x,"y":y,"duration_ms":duration_ms})))
            })
            .await
        }
        UiCmd::Swipe {
            x1,
            y1,
            x2,
            y2,
            duration_ms,
            fusion,
        } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                client.swipe(x1, y1, x2, y2, duration_ms).await?;
                Ok((
                    "swipe",
                    json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
                ))
            })
            .await
        }
        UiCmd::Drag {
            x1,
            y1,
            x2,
            y2,
            duration_ms,
            fusion,
        } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                client.drag(x1, y1, x2, y2, duration_ms).await?;
                Ok((
                    "drag",
                    json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
                ))
            })
            .await
        }
        UiCmd::SwipeExt {
            direction,
            scale,
            duration_ms,
            fusion,
        } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                client.swipe_ext(&direction, scale, duration_ms).await?;
                Ok((
                    "swipe_ext",
                    json!({"direction":direction,"scale":scale,"duration_ms":duration_ms}),
                ))
            })
            .await
        }
        UiCmd::Pinch {
            direction,
            rid,
            text,
            desc,
            percent,
            fusion,
        } => {
            let hint = crate::fusion::SelectorHint {
                text: text.clone(),
                rid: rid.clone(),
                desc: desc.clone(),
            };
            crate::fusion::run_fused(client, &fusion, Some(hint), |_| async {
                client
                    .pinch(
                        rid.as_deref(),
                        text.as_deref(),
                        desc.as_deref(),
                        &direction,
                        percent,
                    )
                    .await?;
                Ok((
                    "pinch",
                    json!({"direction":direction,"rid":rid,"text":text,"desc":desc,"percent":percent}),
                ))
            })
            .await
        }
        UiCmd::ScrollTo(args) => crate::cmd::scroll::run(client, &args).await,
        UiCmd::Focus(args) => crate::cmd::focus::run(client, &args).await,
        UiCmd::Text {
            value,
            clear,
            handle,
            id,
            text,
            rid,
            desc,
            xpath,
            exact,
            mut fusion,
        } => {
            crate::fusion::bind_element_handle(
                &mut fusion,
                handle,
                id.is_some()
                    || text.is_some()
                    || rid.is_some()
                    || desc.is_some()
                    || xpath.is_some(),
            )?;
            let hint = crate::fusion::SelectorHint {
                text: text.clone(),
                rid: rid.clone(),
                desc: desc.clone(),
            };
            crate::fusion::run_fused(client, &fusion, Some(hint), |handle_id| async move {
                let target = text_target_query(handle_id.or(id), text, rid, desc, xpath, exact);
                client
                    .text_with_target(&value, clear, target.as_ref())
                    .await?;
                Ok(("text", json!({"value":value,"clear":clear,"target":target})))
            })
            .await
        }
        UiCmd::Key { name, fusion } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                let injected = client.key(&name).await?;
                Ok(("key", json!({"name":name,"injected":injected})))
            })
            .await
        }
        UiCmd::HideKeyboard => {
            let screen = read_screen_with_reconnect(serial, apk, any_apk_version, client).await?;
            let injected = if screen.ime.keyboard_visible {
                client.key("back").await?
            } else {
                false
            };
            Ok(Outcome::Action(
                "hide_keyboard",
                json!({
                    "keyboard_visible": screen.ime.keyboard_visible,
                    "injected": injected,
                    "ime": compact_ime(&screen.ime),
                }),
            ))
        }
        UiCmd::Back { fusion } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                let injected = client.key("back").await?;
                Ok(("key", json!({"name":"back","injected":injected})))
            })
            .await
        }
        UiCmd::Home { fusion } => {
            crate::fusion::run_fused(client, &fusion, None, |_| async {
                let injected = client.key("home").await?;
                Ok(("key", json!({"name":"home","injected":injected})))
            })
            .await
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
                WaitOptions {
                    gone,
                    timeout_ms,
                    poll_ms,
                },
                serial,
                apk,
                any_apk_version,
            )
            .await
        }
        UiCmd::Toast { wait_ms } => cmd_toast(client, wait_ms).await,
    }
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
        Cmd::App(args) => apply_app_config(args, config),
        Cmd::Net(args) => apply_net_config(args, config),
        Cmd::Debug(args) => apply_debug_config(args, config),
        Cmd::Layout(args) => apply_layout_config(args, config),
        _ => {}
    }
}

fn apply_app_config(args: &mut AppCmd, config: &ShadowDroidConfig) {
    match args {
        AppCmd::Start { package, .. }
        | AppCmd::Stop { package }
        | AppCmd::Clear { package }
        | AppCmd::Info { package }
        | AppCmd::Wait { package, .. } => fill_app(package, config),
        AppCmd::Install(_) | AppCmd::Reinstall(_) | AppCmd::Current { .. } => {}
    }
}

fn apply_net_config(args: &mut NetCmd, config: &ShadowDroidConfig) {
    if let NetCmd::Check { package, .. } = args {
        fill_app(package, config);
    }
    let Some(proxy) = config.proxy.as_ref() else {
        return;
    };
    // Flag > config: only fill knobs the user left at their clap default.
    match args {
        NetCmd::Start {
            port,
            host,
            anticache,
            anticomp,
            verify_upstream,
            redact,
            ..
        } => {
            if *port == crate::net::DEFAULT_PROXY_PORT
                && let Some(p) = proxy.port
            {
                *port = p;
            }
            if host.is_empty() && !proxy.hosts.is_empty() {
                *host = proxy.hosts.clone();
            }
            // These are opt-in booleans (default off); config can only turn them on.
            *anticache |= proxy.anticache.unwrap_or(false);
            *anticomp |= proxy.anticomp.unwrap_or(false);
            *verify_upstream |= proxy.verify_upstream.unwrap_or(false);
            *redact |= proxy.redact.unwrap_or(false);
        }
        NetCmd::Trust {
            auto,
            system,
            push,
            ui,
            ..
        } if !*auto && !*system && !*push && !*ui => {
            match proxy.trust_store.as_deref() {
                Some("system") => *system = true,
                Some("push" | "ui") => *push = true,
                // "user" has no dedicated flag; the default auto path installs
                // system-then-user, so leave the flags unset.
                _ => {}
            }
        }
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
        DebuggerCmd::Watch(crate::cmd::debugger::WatchCmd::Add { project, .. })
            if project.is_none() =>
        {
            *project = config.project.clone();
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

async fn dispatch_perm(c: &PermCmd, serial: &Serial) -> Result<()> {
    use crate::cmd::permissions;
    match c {
        PermCmd::Grant { package, perms } => permissions::grant(serial, package, perms).await,
        PermCmd::Revoke { package, perms } => permissions::revoke(serial, package, perms).await,
        PermCmd::List { package } => permissions::list(serial, package).await,
        PermCmd::Reset { package } => permissions::reset(serial, package).await,
    }
}

async fn dispatch_appops(c: &AppopsCmd, serial: &Serial) -> Result<()> {
    use crate::cmd::permissions;
    match c {
        AppopsCmd::Get { package, op } => {
            permissions::appop_get(serial, package, op.as_deref()).await
        }
        AppopsCmd::Set {
            package,
            op,
            mode,
            scope,
        } => permissions::appop_set(serial, package, op, mode, *scope).await,
    }
}

async fn dispatch_profile(c: &ProfileCmd, serial: &Serial) -> Result<()> {
    use crate::cmd::device_profile;
    match c {
        ProfileCmd::Snapshot { out } => device_profile::snapshot(serial, out.as_ref()).await,
        ProfileCmd::Apply(args) => device_profile::apply(serial, args).await,
        ProfileCmd::Reset => device_profile::reset(serial).await,
    }
}

async fn resolve_net_check_package(
    serial: &Serial,
    config: &ShadowDroidConfig,
    package: Option<String>,
) -> Result<String> {
    let requested = package
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let resolved = config
        .resolve_app(Some(serial.as_str()), requested.as_deref())
        .await?;
    if let Some(package) = resolved.package {
        return Ok(package);
    }
    if let Some(input) = resolved.input {
        bail!(
            "`{input}` did not resolve to one installed package for `shadowdroid net check` \
             (resolution source: {}). Pass a package explicitly or add an alias with \
             `shadowdroid config init --project --app <name> --package <pkg>`.",
            resolved.source
        );
    }
    if let Some(component) = adb::foreground_activity(serial).await
        && let Some((package, _)) = component.split_once('/')
        && !package.is_empty()
    {
        return Ok(package.to_string());
    }
    bail!(
        "`shadowdroid net check` needs a package, and no default app is configured. \
         Run `shadowdroid app current` to inspect the foreground app, pass `shadowdroid net check <pkg>`, \
         or create a default with `shadowdroid config init --project --app <name> --package <pkg>`."
    )
}

/// Route a parsed `net` command to its host-side handler. `net` owns its own
/// daemon + adb wiring, so (unlike server-backed namespaces) this never touches
/// `ensure_ready`. Clap types stay here; the handlers speak plain structs.
async fn dispatch_net(c: &NetCmd, serial: &Serial, config: &ShadowDroidConfig) -> Result<()> {
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
        NetCmd::Check { package, fresh } => {
            let package = resolve_net_check_package(serial, config, package.clone()).await?;
            let tctx = crate::net::trust::TrustContext::resolve(config, serial, *fresh)?;
            nc::check(serial, &package, &tctx).await
        }
        NetCmd::Trust {
            auto,
            system,
            push,
            ui,
            fresh,
        } => {
            let mut tctx = crate::net::trust::TrustContext::resolve(config, serial, *fresh)?;
            // A genuine install needs the CA on disk; an assertion doesn't touch
            // the device, so don't mint a bogus CA just to assert it's trusted.
            if !tctx.asserted {
                crate::net::ca::ensure_ca(&tctx.ca)?;
                tctx.refresh_ca_lease()?;
            }
            nc::trust(serial, *auto, *system, *push, *ui, &tctx).await
        }
        NetCmd::Ca(sub) => match sub {
            NetCaCmd::Import { cert, key, scope } => {
                let (dir, origin) = crate::net::ca::ca_scope_dir(scope.project, scope.global)?;
                nc::ca_import(serial, &dir, origin, cert, key.as_deref()).await
            }
            NetCaCmd::Info { scope } => {
                let (dir, origin) = crate::net::ca::ca_scope_dir(scope.project, scope.global)?;
                nc::ca_info(&dir, origin).await
            }
            NetCaCmd::Reset { scope } => {
                let (dir, origin) = crate::net::ca::ca_scope_dir(scope.project, scope.global)?;
                nc::ca_reset(serial, &dir, origin).await
            }
        },
        NetCmd::Start {
            port,
            host,
            foreground,
            anticache,
            anticomp,
            verify_upstream,
            redact,
        } => {
            // Resolve which CA to sign with (config override → per-project
            // convention → global) and make it exist before the daemon, which
            // is load-only, tries to read it.
            let ca = crate::net::ca::resolve_ca(config, Some(serial))?;
            crate::net::ca::ensure_ca(&ca)?;
            nc::start(
                serial,
                nc::StartOpts {
                    port: *port,
                    apps: host.clone(),
                    foreground: *foreground,
                    anticache: *anticache,
                    anticomp: *anticomp,
                    verify_upstream: *verify_upstream,
                    redact: *redact,
                    ca_cert: ca.cert,
                    ca_key: ca.key,
                },
            )
            .await
        }
        NetCmd::Stop {
            revoke_ca,
            canary_host,
        } => {
            // Resolve the CA to remove (revoke path); fall back to the global cert
            // if config resolution fails so teardown never blocks on CA config.
            let ca_cert = crate::net::ca::resolve_ca(config, Some(serial))
                .map(|c| c.cert)
                .or_else(|_| crate::net::paths::ca_cert_path())
                .unwrap_or_default();
            nc::stop(serial, *revoke_ca, canary_host, &ca_cert).await
        }
        NetCmd::Status => {
            let ca = crate::net::ca::resolve_ca(config, Some(serial))
                .ok()
                .map(|c| c.cert);
            nc::status(serial, ca.as_deref()).await
        }
        NetCmd::Log {
            host,
            path,
            method,
            status,
            limit,
        } => nc::log(serial, matcher(host, path, method, status), *limit).await,
        NetCmd::Show {
            id,
            body,
            body_file,
            har,
        } => nc::show(serial, id, *body, *har, body_file.as_deref()).await,
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
                Some(values) => {
                    return Err(crate::diagnostic::DiagnosticError::new(
                        "invalid_replace_arity",
                        "input",
                        "--replace expects exactly REGEX and REPL",
                    )
                    .detail(json!({"provided_count": values.len()}))
                    .next_actions(["rerun with `--replace <REGEX> <REPL>` as two adjacent values"])
                    .into());
                }
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
        NetCmd::Override { url, file } => nc::override_local(serial, url, file).await,
        NetCmd::Rules { file } => nc::rules_apply(serial, file).await,
        NetCmd::Replay { from, host } => nc::replay(serial, from, host.clone()).await,
        NetCmd::Daemon(a) => {
            crate::net::daemon::run(DaemonConfig {
                serial: Serial::new(a.serial.clone()),
                startup_id: a.startup_id.clone(),
                ca_cert: a.ca_cert.clone(),
                ca_key: a.ca_key.clone(),
                port: a.port,
                host_port: a.host_port,
                app_filters: a.host.clone(),
                anticache: a.anticache,
                anticomp: a.anticomp,
                verify_upstream: a.verify_upstream,
                redact: a.redact,
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
                .ok_or_else(|| {
                    crate::diagnostic::DiagnosticError::new(
                        "invalid_header_assignment",
                        "input",
                        "--set-header expects NAME=VALUE",
                    )
                    .detail(json!({"assignment": p}))
                    .next_actions(["rerun with each header spelled as `--set-header NAME=VALUE`"])
                    .into()
                })
        })
        .collect()
}

/// Resolve a body from `--body <str>` or `--body-file <path>` (mutually exclusive).
fn read_body_arg(inline: &Option<String>, file: &Option<PathBuf>) -> Result<Option<Vec<u8>>> {
    match (inline, file) {
        (Some(_), Some(path)) => Err(crate::diagnostic::DiagnosticError::new(
            "body_source_conflict",
            "input",
            "--body and --body-file are mutually exclusive",
        )
        .detail(json!({"body_file": path.display().to_string()}))
        .next_actions(["remove either --body or --body-file, then retry"])
        .into()),
        (Some(s), None) => Ok(Some(s.clone().into_bytes())),
        (None, Some(p)) => Ok(Some(
            std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        )),
        (None, None) => Ok(None),
    }
}

/// Server-backed `app` verbs. `Install`/`Reinstall` are handled host-side in
/// phase 1, so they're unreachable here.
/// Like [dispatch_ui], `app` verbs carry the since-last-command events probe:
/// `app start` after a crash-loop, `app current` after an unexpected exit —
/// these are exactly the moments the crash context matters.
async fn dispatch_app(
    c: AppCmd,
    client: &ServerClient,
    config: &ShadowDroidConfig,
    serial: &Serial,
) -> Result<()> {
    let probe = crate::crashscan::spawn_probe(serial);
    let result = dispatch_app_inner(c, client, config, serial).await;
    let events = crate::crashscan::finish_probe(probe).await;
    match result {
        Ok(outcome) => {
            outcome.emit(events);
            Ok(())
        }
        Err(err) => {
            crate::events::stash_events(events);
            Err(err)
        }
    }
}

async fn dispatch_app_inner(
    c: AppCmd,
    client: &ServerClient,
    config: &ShadowDroidConfig,
    serial: &Serial,
) -> Result<Outcome> {
    match c {
        AppCmd::Install(_) | AppCmd::Reinstall(_) => {
            unreachable!("app install/reinstall handled host-side")
        }
        AppCmd::Start { package, activity } => {
            let package = require_app_package(client, config, serial, package, "app start").await?;
            let r =
                app_start_with_transport_recovery(client, serial, &package, activity.as_deref())
                    .await?;
            let mut body = json!({
                "package": package,
                "activity": r.activity,
                "launcher_activities": r.launcher_activities,
                "ok": r.ok,
            });
            if let Some(warning) = r.warning {
                body["warning"] = json!(warning);
            }
            Ok(Outcome::Action("app_start", body))
        }
        AppCmd::Stop { package } => {
            let package = require_app_package(client, config, serial, package, "app stop").await?;
            client.app_stop(&package).await?;
            Ok(Outcome::Action("app_stop", json!({"package":package})))
        }
        AppCmd::Clear { package } => {
            let package = require_app_package(client, config, serial, package, "app clear").await?;
            client.app_clear(&package).await?;
            Ok(Outcome::Action("app_clear", json!({"package":package})))
        }
        AppCmd::Info { package } => {
            let package = require_app_package(client, config, serial, package, "app info").await?;
            let info = client.app_info(&package).await?;
            Ok(Outcome::Action(
                "app_info",
                json!({
                    "package":package,
                    "version_name":info.version_name,
                    "version_code":info.version_code,
                    "label":info.label,
                }),
            ))
        }
        AppCmd::Wait {
            package,
            timeout_ms,
            front,
        } => {
            let package = require_app_package(client, config, serial, package, "app wait").await?;
            let r = client.app_wait(&package, timeout_ms, front).await?;
            if !r.matched {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "app_wait_timeout",
                    "app",
                    format!(
                        "{package} did not reach the requested app state within {timeout_ms}ms"
                    ),
                )
                .retryable(true)
                .detail(json!({
                    "package": package,
                    "timeout_ms": timeout_ms,
                    "front": front,
                    "current": r.current,
                }))
                .next_actions(["shadowdroid app current --json", "shadowdroid why"])
                .into());
            }
            Ok(Outcome::Action(
                "app_wait",
                json!({"package":package,"matched":r.matched,"current":r.current}),
            ))
        }
        AppCmd::Current { json: _ } => {
            let cur = client.app_current().await?;
            Ok(Outcome::Action(
                "app_current",
                serde_json::to_value(&cur).unwrap_or_default(),
            ))
        }
    }
}

async fn app_start_with_transport_recovery(
    client: &ServerClient,
    serial: &Serial,
    package: &str,
    activity: Option<&str>,
) -> Result<crate::proto::AppStartResp> {
    match client.app_start(package, activity).await {
        Ok(response) => Ok(response),
        Err(err) if is_transient_transport_error(&err) => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                if let Some(component) = adb::foreground_activity(serial).await
                    && let Some(foreground_activity) =
                        matching_started_activity(&component, package, activity)
                {
                    return Ok(crate::proto::AppStartResp {
                            ok: true,
                            activity: Some(foreground_activity),
                            launcher_activities: Vec::new(),
                            warning: Some(
                                "the server response was interrupted after launch; ShadowDroid verified the requested foreground activity through ADB"
                                    .into(),
                            ),
                        });
                }
                if std::time::Instant::now() >= deadline {
                    return Err(err);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        Err(err) => Err(err),
    }
}

fn matching_started_activity(
    component: &str,
    package: &str,
    requested_activity: Option<&str>,
) -> Option<String> {
    let (foreground_package, foreground_activity) = component.split_once('/')?;
    if foreground_package != package {
        return None;
    }
    let normalize = |activity: &str| {
        if activity.starts_with('.') {
            format!("{package}{activity}")
        } else {
            activity.to_string()
        }
    };
    let foreground_activity = normalize(foreground_activity);
    if let Some(requested) = requested_activity {
        let requested = requested
            .split_once('/')
            .map(|(component_package, activity)| (component_package == package).then_some(activity))
            .unwrap_or(Some(requested))?;
        if normalize(requested) != foreground_activity {
            return None;
        }
    }
    Some(foreground_activity)
}

async fn require_app_package(
    client: &ServerClient,
    config: &ShadowDroidConfig,
    serial: &Serial,
    package: Option<String>,
    command: &str,
) -> Result<String> {
    let requested = package
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let resolved = config
        .resolve_app(Some(serial.as_str()), requested.as_deref())
        .await?;
    if let Some(package) = resolved.package {
        return Ok(package);
    }
    if let Some(input) = resolved.input {
        return Err(crate::diagnostic::DiagnosticError::new(
            "app_resolution_failed",
            "app",
            format!("{input:?} did not resolve to one installed package for `shadowdroid {command}`"),
        )
        .detail(json!({"input": input, "resolution_source": resolved.source}))
        .next_actions([
            format!("rerun `shadowdroid {command}` with an explicit installed package"),
            "add a project alias with `shadowdroid config init --project --app <name> --package <pkg>`".to_string(),
        ])
        .into());
    }
    let foreground = client
        .app_current()
        .await
        .ok()
        .and_then(|current| current.package);
    let message =
        format!("`shadowdroid {command}` needs a package, and no default app is configured");
    let foreground_command = foreground
        .as_ref()
        .map(|package| format!("shadowdroid {command} {package}"));
    Err(
        crate::diagnostic::DiagnosticError::new("app_required", "app", message)
            .detail(json!({"foreground_package": foreground}))
            .next_actions(foreground_command.into_iter().chain(std::iter::once(
                "shadowdroid config init --project --app <name> --package <pkg>".to_string(),
            )))
            .into(),
    )
}

async fn dispatch_device(c: DeviceCmd, client: &ServerClient, serial: &Serial) -> Result<()> {
    match c {
        DeviceCmd::Info => cmd_device_info(client, serial).await?,
        DeviceCmd::Shell { cmd, timeout_ms } => {
            let r = client.shell(&cmd, timeout_ms).await?;
            if r.exit_code.is_some_and(|code| code != 0) {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "device_shell_nonzero",
                    "device",
                    format!(
                        "device shell command exited with status {}",
                        r.exit_code.unwrap_or_default()
                    ),
                )
                .detail(json!({
                    "input": r.input,
                    "output": r.output,
                    "exit_code": r.exit_code,
                }))
                .next_actions(["inspect detail.output and correct the device shell command"])
                .into());
            }
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

async fn dispatch_files(c: FilesCmd, client: &ServerClient, serial: &Serial) -> Result<()> {
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
            let local_path = Path::new(&local);
            // Server first (app-accessible storage); fall back to `adb push` for
            // paths the instrumentation uid can't write (e.g. /sdcard under
            // scoped storage). A structured client error (4xx, e.g. bad_mode) is
            // a contract violation adb can't fix, so surface it instead.
            match client.push_file(&remote, local_path, mode).await {
                Ok(r) => {
                    if mode.is_some_and(|requested| requested != r.mode) {
                        return Err(file_mode_postcondition_error(
                            &local,
                            &remote,
                            mode,
                            Some(r.mode),
                            "server",
                        ));
                    }
                    emit_action(
                        "push",
                        &json!({"local":local,"remote":remote,"path":r.path,"bytes":r.bytes,"mode":r.mode,"requested_mode":mode,"via":"server"}),
                    );
                }
                Err(err) if should_fall_back_to_adb(&err) => {
                    let bytes_len = adb::push(serial, local_path, remote.clone()).await?;
                    let mode_applied = match mode {
                        Some(requested) => Some(chmod_via_adb(serial, requested, &remote).await),
                        None => None,
                    };
                    if mode_applied == Some(false) {
                        return Err(file_mode_postcondition_error(
                            &local, &remote, mode, None, "adb",
                        ));
                    }
                    emit_action(
                        "push",
                        &json!({"local":local,"remote":remote,"bytes":bytes_len,"requested_mode":mode,"mode_applied":mode_applied,"via":"adb"}),
                    );
                }
                Err(err) => return Err(err),
            }
        }
        FilesCmd::Pull { remote, local } => {
            let local_path = Path::new(&local);
            let server_pull = match client.pull_file_response(&remote).await {
                Ok(response) => {
                    crate::transfer::response_to_path_atomic(response, local_path, None).await
                }
                Err(error) => Err(error),
            };
            let (bytes, via) = match server_pull {
                Ok(receipt) => (receipt.bytes, "server"),
                Err(err) if should_pull_fall_back_to_adb(&err) => (
                    adb::pull_to_path(serial, remote.clone(), local_path).await?,
                    "adb",
                ),
                Err(err) => return Err(err),
            };
            emit_action(
                "pull",
                &json!({"remote":remote,"local":local,"bytes":bytes,"via":via}),
            );
        }
    }
    Ok(())
}

fn file_mode_postcondition_error(
    local: &str,
    remote: &str,
    requested_mode: Option<u32>,
    observed_mode: Option<u32>,
    via: &str,
) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "file_mode_postcondition_failed",
        "files",
        format!("file was pushed, but {remote} did not reach the requested Unix mode"),
    )
    .detail(json!({
        "local": local,
        "remote": remote,
        "requested_mode": requested_mode,
        "observed_mode": observed_mode,
        "via": via,
        "transfer_completed": true,
    }))
    .next_actions([
        "use an app-specific/device filesystem path that supports chmod, then retry with --mode",
        "omit --mode when pushing to Android shared/FUSE storage and accept its platform-managed mode",
    ])
    .into()
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Parse an octal permission string (e.g. `644`, `755`) into its bit value.
/// `files push --mode` documents octal, but clap's stock `u32` parser reads
/// base-10 — so `--mode 644` would mean 644 decimal, overflowing the server's
/// `0..=0o777` range and getting chmod'd as `0o1204` on the adb fallback. This
/// mirrors the octal parse the `watch` stdin `push` grammar already uses.
fn parse_octal_mode(s: &str) -> Result<u32, String> {
    let mode = u32::from_str_radix(s.trim(), 8)
        .map_err(|_| format!("mode must be octal permission bits, e.g. 644 (got {s:?})"))?;
    if mode > 0o777 {
        return Err(format!("mode must be in the range 0..=777 octal (got {s})"));
    }
    Ok(mode)
}

/// Whether a failed server push should fall back to `adb push`. Transport
/// failures and server-side (5xx) errors — e.g. an unwritable scoped-storage
/// path — fall back; structured client errors (4xx, e.g. `bad_mode`) are
/// surfaced, since adb can't fix a rejected request.
fn should_fall_back_to_adb(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<crate::device::client::ServerError>() {
        Some(server) => !server.status.is_client_error(),
        None => true,
    }
}

fn should_pull_fall_back_to_adb(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<crate::device::client::ServerError>() {
        Some(server) => server.status.is_server_error() || server.code == "file_not_found",
        None => crate::transfer::is_remote_response_failure(err),
    }
}

const CHMOD_OK_MARKER: &str = "__shadowdroid_chmod_ok__";

/// Apply `mode` to `remote` via `adb shell chmod`, reporting whether it
/// actually took. `adb::shell` returns `Ok` whenever the transport succeeds and
/// discards the on-device exit code, so the command self-reports success with a
/// marker that only prints when `chmod` returns 0.
async fn chmod_via_adb(serial: &Serial, mode: u32, remote: &str) -> bool {
    let cmd = format!(
        "chmod {:o} {} && echo {CHMOD_OK_MARKER}",
        mode,
        shell_single_quote(remote)
    );
    match adb::shell(serial, cmd).await {
        Ok(out) => out.contains(CHMOD_OK_MARKER),
        Err(_) => false,
    }
}

// ── emit helpers ────────────────────────────────────────────

/// Render a failed command as one `{"type":"error",…}` line on stdout. Walks the
/// `anyhow` chain for a [`ServerError`] so the server's machine `code`
/// (`element_not_found`, …) and HTTP `status` survive; otherwise falls back to a
/// generic `error` code with the human message. Called by `main` on `Err`.
pub fn report_error(err: &anyhow::Error) {
    if let Some(diagnostic) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::diagnostic::DiagnosticError>())
    {
        emit_error(
            &diagnostic.stage,
            &diagnostic.code,
            &diagnostic.message,
            json!({
                "retryable": diagnostic.retryable,
                "detail": diagnostic.detail,
                "next_actions": diagnostic.next_actions,
            }),
        );
    } else if let Some(se) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::device::client::ServerError>())
    {
        let retryable = server_error_retryable(&se.code, se.status);
        let command_path = crate::events::current_command_path();
        let mut extra = json!({
            "status": se.status.as_u16(),
            "retryable": retryable,
            "next_actions": server_error_next_actions(&se.code, command_path).unwrap_or_else(|| vec![
                "shadowdroid collect".to_string(),
                "shadowdroid doctor --json".to_string(),
            ]),
        });
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
            json!({
                "detail": { "candidates": amb.candidates },
                "next_actions": [
                    "choose a candidate's unique resource id or content description; otherwise use its screen-bound handle",
                    "rerun the action with the refined selector"
                ]
            }),
        );
    } else if let Some(sc) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::fusion::ScreenChanged>())
    {
        // The failure carries the re-observe: `detail.screen` is the fresh
        // compact dump, so the agent re-plans without another read.
        emit_error(
            "run",
            "screen_changed",
            &sc.to_string(),
            json!({ "detail": {
                "expected": sc.expected,
                "actual": sc.actual,
                "screen": sc.screen,
            }, "next_actions": [
                "re-plan from detail.screen instead of issuing another dump",
                "retry the action with detail.actual as --if-screen"
            ]}),
        );
    } else if let Some(stale) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::fusion::StaleElement>())
    {
        emit_error(
            "run",
            "stale_element",
            &stale.to_string(),
            json!({
                "retryable": true,
                "detail": {
                    "handle": stale.handle,
                    "expected_interaction": stale.expected,
                    "actual_interaction": stale.actual,
                    "screen": stale.screen,
                },
                "next_actions": [
                    "re-plan from detail.screen and prefer a stable resource id or content description",
                    "otherwise copy the target's new handle from detail.screen and retry"
                ]
            }),
        );
    } else if let Some(changed) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::fusion::InteractionChanged>())
    {
        emit_error(
            "run",
            "interaction_changed",
            &changed.to_string(),
            json!({
                "retryable": true,
                "detail": {
                    "expected": changed.expected,
                    "actual": changed.actual,
                    "screen": changed.screen,
                },
                "next_actions": [
                    "re-plan from detail.screen instead of issuing another dump",
                    "retry with detail.actual as --if-interaction"
                ]
            }),
        );
    } else if let Some(observation) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::fusion::ObservationFailure>())
    {
        // `detail.screen` is deliberately nested diagnostic evidence. Keeping
        // it out of the top-level `screen` field prevents next-action synthesis
        // from proposing ephemeral ids from an unproven destination.
        emit_error(
            "observe",
            observation.code,
            &observation.message,
            json!({
                "retryable": true,
                "detail": observation.detail,
                "next_actions": [
                    "inspect detail.screen as diagnostic evidence only; do not act from its element ids",
                    "retry with the strongest available --expect-* postcondition and a longer --timeout-ms",
                    "run `shadowdroid ui dump` to start a fresh interaction cycle"
                ]
            }),
        );
    } else {
        let class = classify_generic_error(err);
        emit_error(
            class.stage,
            class.code,
            &err.to_string(),
            json!({
                "retryable": class.retryable,
                "detail": {
                    "used_fallback": true,
                    "command": crate::events::current_command_path(),
                },
                "next_actions": generic_error_next_actions(err, class),
            }),
        );
    }
}

#[derive(Clone, Copy)]
struct GenericErrorClass {
    code: &'static str,
    stage: &'static str,
    retryable: bool,
}

fn classify_generic_error(err: &anyhow::Error) -> GenericErrorClass {
    let message = format!("{err:#}").to_ascii_lowercase();
    let io_kind = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind);
    if io_kind == Some(std::io::ErrorKind::NotFound)
        || [
            "apk not found:",
            "plugin zip not found:",
            "aar not found:",
            "--apk path does not exist:",
            "source file not found:",
            "app path does not exist:",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    {
        GenericErrorClass {
            code: "input_not_found",
            stage: "input",
            retryable: false,
        }
    } else if io_kind == Some(std::io::ErrorKind::PermissionDenied) {
        GenericErrorClass {
            code: "host_permission_denied",
            stage: "host",
            retryable: false,
        }
    } else if io_kind == Some(std::io::ErrorKind::TimedOut)
        || message.contains("timed out")
        || message.contains("timeout")
    {
        GenericErrorClass {
            code: "transport_timeout",
            stage: "transport",
            retryable: true,
        }
    } else if matches!(
        io_kind,
        Some(
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::NotConnected
        )
    ) || message.contains("connection refused")
        || message.contains("connection closed")
        || message.contains("broken pipe")
        || message.contains("device offline")
    {
        GenericErrorClass {
            code: "transport_error",
            stage: "transport",
            retryable: true,
        }
    } else if message.starts_with("invalid duration")
        || message.starts_with("duration `")
        || message.contains("specify an agent (")
        || message.starts_with("unknown agent ")
        || (message.starts_with("--")
            && (message.contains(" requires ")
                || message.contains(" expects ")
                || message.contains(" must ")))
    {
        GenericErrorClass {
            code: "invalid_input",
            stage: "input",
            retryable: false,
        }
    } else {
        GenericErrorClass {
            code: "runtime_error",
            stage: "runtime",
            retryable: false,
        }
    }
}

fn generic_error_next_actions(_err: &anyhow::Error, class: GenericErrorClass) -> Vec<String> {
    let mut actions = match class.code {
        "input_not_found" => vec![
            "verify every local path or named resource passed to this command exists and is readable, then retry"
                .to_string(),
        ],
        "host_permission_denied" => vec![
            "fix the reported host file/directory permission without broadening unrelated access, then retry"
                .to_string(),
        ],
        "transport_timeout" => vec![
            "retry once; if it times out again, inspect device/server health before increasing the command timeout"
                .to_string(),
            "shadowdroid doctor --json".to_string(),
        ],
        "transport_error" => vec!["shadowdroid doctor --json".to_string()],
        "invalid_input" => Vec::new(),
        _ => Vec::new(),
    };
    if let Some(path) = crate::events::current_command_path() {
        actions.push(format!("shadowdroid {path} --help"));
    } else {
        actions.push("shadowdroid commands --json --depth 2".to_string());
    }
    actions
}

fn server_error_retryable(code: &str, status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || matches!(
            code,
            "activity_start_not_foreground"
                | "element_not_found"
                | "no_focused_field"
                | "no_scrollable"
                | "screenshot_failed"
                | "shell_timeout"
                | "swipe_failed"
                | "tap_failed"
        )
}

fn command_contract_action(command_path: Option<&str>, fallback: &str) -> String {
    format!(
        "shadowdroid commands --json --describe '{}'",
        command_path.unwrap_or(fallback)
    )
}

fn server_error_next_actions(code: &str, command_path: Option<&str>) -> Option<Vec<String>> {
    let current_contract = |fallback: &str| command_contract_action(command_path, fallback);
    match code {
        "element_not_found" => Some(vec![
            "inspect detail.top_texts and detail.closest when present".to_string(),
            "shadowdroid ui dump".to_string(),
            current_contract("ui find"),
        ]),
        "ambiguous_match" => Some(vec![
            "choose a candidate's unique resource id or content description; otherwise use its screen-bound handle"
                .to_string(),
            current_contract("ui find"),
        ]),
        "element_not_clickable" => Some(vec![
            "inspect detail.matched_element and target its enabled clickable control or ancestor"
                .to_string(),
            "use --coordinate-fallback only when raw center injection is explicitly intended"
                .to_string(),
            current_contract("ui tap"),
        ]),
        "element_disabled" => Some(vec![
            "do not retry the same disabled control; inspect the current screen for its enabling condition"
                .to_string(),
            "shadowdroid ui dump".to_string(),
            current_contract("ui tap"),
        ]),
        "range_semantics_unavailable" => Some(vec![
            "inspect the matched element's range/actions in `shadowdroid ui dump --full`"
                .to_string(),
            "use --coordinate-fallback only when approximate track injection is explicitly acceptable"
                .to_string(),
            current_contract("ui set-progress"),
        ]),
        "set_progress_unsupported" | "set_progress_failed" | "set_progress_unverified" => {
            Some(vec![
                "inspect the matched element's range/actions and current value in `shadowdroid ui dump --full`"
                    .to_string(),
                current_contract("ui set-progress"),
                "shadowdroid why".to_string(),
            ])
        }
        "progress_target_required" | "progress_target_conflict" | "progress_value_invalid"
        | "progress_value_out_of_range" => Some(vec![
            "rerun with exactly one finite --value NUMBER or --percent 0..100; add --clamp only when bounding is intended"
                .to_string(),
            current_contract("ui set-progress"),
        ]),
        "server_version_mismatch" | "server_unavailable" => Some(vec![
            "shadowdroid connect".to_string(),
            "shadowdroid doctor --fix --json".to_string(),
        ]),
        "package_not_found" => Some(vec![
            "shadowdroid app current".to_string(),
            current_contract("app info"),
        ]),
        "no_launch_intent" => Some(vec![
            current_contract("app start"),
            "shadowdroid app current".to_string(),
        ]),
        "app_stop_failed" => Some(vec![
            "inspect detail.exit_code, detail.output, and detail.remaining_pid".to_string(),
            current_contract("app stop"),
            "shadowdroid doctor --json".to_string(),
        ]),
        "app_clear_failed" => Some(vec![
            "inspect detail.exit_code and detail.output".to_string(),
            current_contract("app clear"),
        ]),
        "activity_package_mismatch"
        | "activity_start_failed"
        | "activity_start_not_foreground"
        | "missing_activity"
        | "missing_package" => Some(vec![
            "shadowdroid app current".to_string(),
            current_contract("app start"),
        ]),
        "bad_direction" | "swipe_failed" => Some(vec![
            "shadowdroid ui dump".to_string(),
            current_contract(if code == "bad_direction" {
                "ui swipe-ext"
            } else {
                "ui swipe"
            }),
        ]),
        "tap_failed" => Some(vec![
            "shadowdroid ui dump".to_string(),
            current_contract("ui tap"),
        ]),
        "text_failed" | "no_focused_field" => Some(vec![
            "shadowdroid ui dump".to_string(),
            current_contract("ui text"),
        ]),
        "empty_selector" | "xpath_invalid" => Some(vec![
            "shadowdroid ui dump".to_string(),
            current_contract("ui find"),
        ]),
        "no_scrollable" => Some(vec![
            "shadowdroid ui dump".to_string(),
            current_contract("ui scroll-to"),
        ]),
        "bad_mode" | "bad_path" | "file_not_found" | "is_directory" | "not_directory"
        | "missing_path" => Some(vec![current_contract("files ls")]),
        "missing_key" | "unknown_key" => Some(vec![
            current_contract("ui key"),
            "shadowdroid ui dump".to_string(),
        ]),
        "bad_orientation" => Some(vec![
            current_contract("device orientation"),
            "shadowdroid device orientation".to_string(),
        ]),
        "shell_failed" => Some(vec![
            current_contract("device shell"),
            "shadowdroid device info".to_string(),
        ]),
        "shell_timeout" => Some(vec![
            current_contract("device shell"),
            "shadowdroid device info".to_string(),
        ]),
        "screenshot_failed" => Some(vec![
            "shadowdroid doctor --json".to_string(),
            "shadowdroid collect".to_string(),
        ]),
        "internal" => Some(vec![
            "shadowdroid collect".to_string(),
            "shadowdroid doctor --json".to_string(),
        ]),
        "invalid_activity" | "invalid_package" => Some(vec![current_contract("app start")]),
        _ => None,
    }
}

/// The machine error code `report_error` would assign — shared with the usage
/// log so failure statistics use the same vocabulary the agent sees.
pub fn error_code_of(err: &anyhow::Error) -> String {
    if let Some(diagnostic) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::diagnostic::DiagnosticError>())
    {
        diagnostic.code.clone()
    } else if let Some(se) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::device::client::ServerError>())
    {
        se.code.clone()
    } else if err.chain().any(|e| {
        e.downcast_ref::<crate::selector::AmbiguousMatch>()
            .is_some()
    }) {
        "ambiguous_match".into()
    } else if err
        .chain()
        .any(|e| e.downcast_ref::<crate::fusion::ScreenChanged>().is_some())
    {
        "screen_changed".into()
    } else if err
        .chain()
        .any(|e| e.downcast_ref::<crate::fusion::StaleElement>().is_some())
    {
        "stale_element".into()
    } else if err.chain().any(|e| {
        e.downcast_ref::<crate::fusion::InteractionChanged>()
            .is_some()
    }) {
        "interaction_changed".into()
    } else if let Some(observation) = err
        .chain()
        .find_map(|e| e.downcast_ref::<crate::fusion::ObservationFailure>())
    {
        observation.code.into()
    } else {
        classify_generic_error(err).code.into()
    }
}

pub fn error_stage_of(err: &anyhow::Error) -> String {
    if let Some(diagnostic) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::diagnostic::DiagnosticError>())
    {
        diagnostic.stage.clone()
    } else if err.chain().any(|cause| {
        cause
            .downcast_ref::<crate::device::client::ServerError>()
            .is_some()
    }) {
        "run".to_string()
    } else if err.chain().any(|cause| {
        cause
            .downcast_ref::<crate::fusion::ObservationFailure>()
            .is_some()
    }) {
        "observe".to_string()
    } else if err.chain().any(|cause| {
        cause
            .downcast_ref::<crate::fusion::StaleElement>()
            .is_some()
            || cause
                .downcast_ref::<crate::fusion::InteractionChanged>()
                .is_some()
    }) {
        "run".to_string()
    } else {
        classify_generic_error(err).stage.to_string()
    }
}

pub fn error_retryable_of(err: &anyhow::Error) -> bool {
    if let Some(diagnostic) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::diagnostic::DiagnosticError>())
    {
        diagnostic.retryable
    } else if let Some(server) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::device::client::ServerError>())
    {
        server_error_retryable(&server.code, server.status)
    } else if err.chain().any(|cause| {
        cause
            .downcast_ref::<crate::fusion::ScreenChanged>()
            .is_some()
            || cause
                .downcast_ref::<crate::fusion::ObservationFailure>()
                .is_some()
            || cause
                .downcast_ref::<crate::fusion::StaleElement>()
                .is_some()
            || cause
                .downcast_ref::<crate::fusion::InteractionChanged>()
                .is_some()
    }) {
        true
    } else {
        classify_generic_error(err).retryable
    }
}

pub fn error_uses_fallback(err: &anyhow::Error) -> bool {
    if err.chain().any(|cause| {
        cause
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .is_some()
    }) {
        false
    } else if let Some(server) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::device::client::ServerError>())
    {
        server_error_next_actions(&server.code, crate::events::current_command_path()).is_none()
    } else {
        !err.chain().any(|cause| {
            cause
                .downcast_ref::<crate::selector::AmbiguousMatch>()
                .is_some()
                || cause
                    .downcast_ref::<crate::fusion::ScreenChanged>()
                    .is_some()
                || cause
                    .downcast_ref::<crate::fusion::ObservationFailure>()
                    .is_some()
                || cause
                    .downcast_ref::<crate::fusion::StaleElement>()
                    .is_some()
                || cause
                    .downcast_ref::<crate::fusion::InteractionChanged>()
                    .is_some()
        })
    }
}

/// A domain error may preserve a child command's status while still flowing
/// through the normal usage recorder and single structured-error boundary.
pub fn process_exit_code_of(err: &anyhow::Error) -> Option<i32> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<crate::diagnostic::DiagnosticError>())
        .and_then(|diagnostic| diagnostic.process_exit_code)
}

// ── specific handlers ──────────────────────────────────────────

async fn cmd_device_info(client: &ServerClient, serial: &Serial) -> Result<()> {
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
    serial: &Serial,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
    client: &ServerClient,
    full: bool,
) -> Result<Outcome> {
    let screen = read_screen_with_reconnect(serial, apk, any_apk_version, client).await?;
    if full {
        return Ok(Outcome::Raw(serde_json::to_value(&screen)?));
    }
    Ok(Outcome::Raw(crate::fusion::compact_screen_value(&screen)))
}

fn compact_ime(ime: &crate::proto::ImeState) -> serde_json::Value {
    let mut value = json!({
        "keyboard_visible": ime.keyboard_visible,
    });
    if let serde_json::Value::Object(map) = &mut value {
        if let Some(element) = ime.focused_element.clone() {
            map.insert(
                "focused_element".into(),
                json!(CompactElement::from(element)),
            );
        }
        if let Some(element) = ime.focused_input.clone() {
            map.insert("focused_input".into(), json!(CompactElement::from(element)));
        }
        if let Some(detection) = &ime.detection {
            map.insert("detection".into(), json!(detection));
        }
        if let Some(reason) = &ime.reason {
            map.insert("reason".into(), json!(reason));
        }
        if !ime.suggested_actions.is_empty() {
            map.insert("suggested_actions".into(), json!(ime.suggested_actions));
        }
    }
    value
}

async fn cmd_screenshot(
    client: &ServerClient,
    path: Option<String>,
    format: Option<String>,
    scale: Option<f32>,
    quality: Option<u32>,
) -> Result<Outcome> {
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
        body["screen_hash_version"] = json!(screen.screen_hash_version);
        body["content_hash"] = json!(screen.content_hash);
        body["interaction_hash"] = json!(screen.interaction_hash);
        body["interaction_hash_version"] = json!(screen.interaction_hash_version);
    }
    Ok(Outcome::Action("screenshot", body))
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
    coordinate_fallback: bool,
) -> Result<(&'static str, serde_json::Value)> {
    // Selector modes take priority.
    if let Some(query) = xpath {
        let r = client.xpath_tap(&query, coordinate_fallback).await?;
        let body = selector_tap_body("xpath", Some(("xpath", json!(query))), r);
        return Ok(("tap", body));
    }
    if text.is_some() || rid.is_some() || desc.is_some() {
        let r = client
            .find_tap(&SelectorQuery {
                text,
                rid,
                desc,
                exact,
                clickable: clickable.then_some(true),
                coordinate_fallback,
                ..Default::default()
            })
            .await?;
        return Ok(("tap", selector_tap_body("selector", None, r)));
    }
    // Coordinate / id modes.
    match (id, a, b) {
        (Some(id), None, None) => tap_element_id(client, id, coordinate_fallback).await,
        (Some(_), Some(_), _) | (Some(_), None, Some(_)) => {
            Err(crate::diagnostic::DiagnosticError::new(
                "tap_target_conflict",
                "input",
                "tap --id cannot be combined with positional coordinates or an element id",
            )
            .detail(json!({"id": id, "first_positional": a, "second_positional": b}))
            .next_actions([
                "choose exactly one target form: --id ID, positional ID, X Y, or one selector",
            ])
            .into())
        }
        (None, Some(x), Some(y)) => {
            client.tap_xy(x, y).await?;
            Ok((
                "tap",
                json!({
                    "via":"coords",
                    "x":x,
                    "y":y,
                    "action":"coordinate",
                    "selector_matched":false,
                    "actionable_resolved":false,
                    "input_delivered":true,
                    "screen_changed":null,
                    "postcondition_satisfied":null
                }),
            ))
        }
        (None, Some(a), None) => {
            let id = u32::try_from(a).map_err(|_| {
                crate::diagnostic::DiagnosticError::new(
                    "invalid_element_id",
                    "input",
                    format!("element id must be non-negative, got {a}"),
                )
                .detail(json!({"element_id": a}))
                .next_actions(["use a non-negative id from a fresh `shadowdroid ui dump`"])
            })?;
            tap_element_id(client, id, coordinate_fallback).await
        }
        (None, None, _) => {
            Err(crate::diagnostic::DiagnosticError::new(
                "tap_target_required",
                "input",
                "tap needs a target",
            )
            .detail(json!({"accepted_forms": ["ID", "X Y", "--id ID", "--text VALUE", "--rid VALUE", "--desc VALUE", "--xpath VALUE"]}))
            .next_actions([
                "read a fresh `shadowdroid ui dump`, then retry with one id, coordinate pair, or selector",
            ])
            .into())
        }
    }
}

fn validate_set_progress_input(value: Option<f64>, percent: Option<f64>) -> Result<()> {
    let (code, message) = match (value, percent) {
        (None, None) => (
            "progress_target_required",
            "set-progress needs exactly one of --value or --percent",
        ),
        (Some(_), Some(_)) => (
            "progress_target_conflict",
            "set-progress accepts --value or --percent, not both",
        ),
        (Some(v), None) | (None, Some(v)) if !v.is_finite() => (
            "progress_value_invalid",
            "progress value must be a finite number",
        ),
        _ => return Ok(()),
    };
    Err(
        crate::diagnostic::DiagnosticError::new(code, "input", message)
            .next_actions([
                "rerun with one finite --value NUMBER or --percent 0..100",
                "shadowdroid commands --json --describe 'ui set-progress'",
            ])
            .into(),
    )
}

async fn tap_element_id(
    client: &ServerClient,
    id: u32,
    coordinate_fallback: bool,
) -> Result<(&'static str, serde_json::Value)> {
    let r = client
        .find_tap(&SelectorQuery {
            id: Some(id),
            coordinate_fallback,
            ..Default::default()
        })
        .await?;
    Ok(("tap", selector_tap_body("id", Some(("id", json!(id))), r)))
}

fn selector_tap_body(
    via: &'static str,
    extra: Option<(&'static str, serde_json::Value)>,
    response: crate::proto::FindTapResp,
) -> serde_json::Value {
    let matched = response.matched;
    let activated = response.activated_element;
    let actionable_resolved = response
        .actionable_resolved
        .unwrap_or_else(|| activated.is_some() || (matched.clickable && matched.enabled));
    let input_delivered = response.input_delivered.unwrap_or(true);
    // Older servers called their implicit center fallback simply
    // `coordinate`; surface it honestly even though only newer servers enforce
    // the explicit opt-in request field.
    let coordinate_fallback = matches!(
        response.action.as_deref(),
        Some("coordinate_fallback" | "coordinate")
    );
    let mut body = json!({
        "via": via,
        "x": response.x,
        "y": response.y,
        "action": response.action,
        "matched": true,
        "selector_matched": true,
        "actionable_resolved": actionable_resolved,
        "input_delivered": input_delivered,
        "coordinate_fallback": coordinate_fallback,
        "screen_changed": null,
        "postcondition_satisfied": null,
        "element": matched,
        "matched_element": matched,
        "activated_element": activated,
    });
    if let Some((key, value)) = extra {
        body[key] = value;
    }
    body
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

async fn cmd_toast(client: &ServerClient, wait_ms: u32) -> Result<Outcome> {
    let start = unix_ms();
    client.toast_start(50).await?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms as u64);
    loop {
        let recent = client.toast_recent(start).await?;
        if !recent.toasts.is_empty() || std::time::Instant::now() >= deadline {
            return Ok(Outcome::Action("toast", json!({"toasts":recent.toasts})));
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

struct WaitOptions {
    gone: bool,
    timeout_ms: u32,
    poll_ms: u32,
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
    options: WaitOptions,
    serial: &Serial,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
) -> Result<Outcome> {
    let WaitOptions {
        gone,
        timeout_ms,
        poll_ms,
    } = options;
    let started = tokio::time::Instant::now();
    let deadline = started + std::time::Duration::from_millis(timeout_ms as u64);
    let mut client = client.clone();
    loop {
        let screen = match tokio::time::timeout_at(deadline, client.screen()).await {
            Err(_) => {
                return Err(wait_timeout_error(timeout_ms, gone, None, Vec::new()));
            }
            Ok(result) => match result {
                Ok(screen) => screen,
                Err(err)
                    if is_transient_transport_error(&err)
                        && tokio::time::Instant::now() < deadline =>
                {
                    client = tokio::time::timeout_at(
                        deadline,
                        reconnect_after_screen_error(serial, apk, any_apk_version, &err),
                    )
                    .await
                    .map_err(|_| wait_timeout_error(timeout_ms, gone, None, Vec::new()))??;
                    continue;
                }
                Err(err) => return Err(err),
            },
        };
        let outcome = wait_query_matches(&query, &screen.current_app, &screen.elements);
        let matched = outcome.matched;
        let content_hash = screen.content_hash.clone();
        let interaction_hash = screen.interaction_hash.clone();
        let interaction_hash_version = screen.interaction_hash_version;
        let screen_hash = screen.screen_hash;
        let screen_hash_version = screen.screen_hash_version;
        let current_app = json!({
            "package": screen.current_app.package,
            "activity": screen.current_app.activity,
        });
        if matched != gone {
            let mut body = json!({
                "matched": matched,
                "gone": gone,
                "screen_hash": screen_hash,
                "screen_hash_version": screen_hash_version,
                "content_hash": content_hash,
                "interaction_hash": interaction_hash,
                "interaction_hash_version": interaction_hash_version,
                "current_app": current_app,
            });
            if let Some(el) = outcome.element {
                body["element"] = json!(CompactElement::from(el));
            }
            return Ok(Outcome::Action("wait", body));
        }
        if tokio::time::Instant::now() >= deadline {
            // The most common timeout cause is the screen having *changed* to
            // something unexpected (e.g. an error page). Echo the visible texts
            // so the caller sees what the screen became without a second probe.
            let top_texts = top_screen_texts(&screen.elements, 12);
            return Err(wait_timeout_error(
                timeout_ms,
                gone,
                Some(WaitScreenEvidence {
                    screen_hash,
                    screen_hash_version,
                    content_hash,
                    interaction_hash,
                    interaction_hash_version,
                    current_app,
                }),
                top_texts,
            ));
        }
        let sleep = std::time::Duration::from_millis(poll_ms.max(1) as u64);
        tokio::time::sleep_until((tokio::time::Instant::now() + sleep).min(deadline)).await;
    }
}

struct WaitScreenEvidence {
    screen_hash: String,
    screen_hash_version: u32,
    content_hash: Option<String>,
    interaction_hash: Option<String>,
    interaction_hash_version: u32,
    current_app: serde_json::Value,
}

fn wait_timeout_error(
    timeout_ms: u32,
    gone: bool,
    screen: Option<WaitScreenEvidence>,
    top_texts: Vec<String>,
) -> anyhow::Error {
    let screen_detail = match screen {
        Some(screen) => json!({
            "screen_hash": screen.screen_hash,
            "screen_hash_version": screen.screen_hash_version,
            "content_hash": screen.content_hash,
            "interaction_hash": screen.interaction_hash,
            "interaction_hash_version": screen.interaction_hash_version,
            "current_app": screen.current_app,
        }),
        None => json!({
            "screen_hash": null,
            "screen_hash_version": null,
            "content_hash": null,
            "interaction_hash": null,
            "interaction_hash_version": null,
            "current_app": null,
        }),
    };
    crate::diagnostic::DiagnosticError::new(
        "wait_timeout",
        "ui",
        if gone {
            format!("element remained present after {timeout_ms}ms")
        } else {
            format!("element did not appear within {timeout_ms}ms")
        },
    )
    .retryable(true)
    .detail({
        let mut detail = screen_detail;
        detail["timeout_ms"] = json!(timeout_ms);
        detail["gone"] = json!(gone);
        detail["top_texts"] = json!(top_texts);
        detail
    })
    .next_actions(if gone {
        [
            "inspect detail.top_texts; refine an overly broad selector or wait for a stable replacement",
            "run `shadowdroid why` if navigation appears stuck",
        ]
    } else {
        [
            "inspect detail.top_texts and current_app, then correct the selector or expected screen",
            "run `shadowdroid why` if the app reached an unexpected state",
        ]
    })
    .into()
}

async fn read_screen_with_reconnect(
    serial: &Serial,
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
    serial: &Serial,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
    err: &anyhow::Error,
) -> Result<ServerClient> {
    installer::ensure_ready_for_command(serial, apk, any_apk_version)
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
    if let Some(package) = &query.package
        && !app.package.as_deref().unwrap_or("").contains(package)
    {
        return no_match;
    }
    if let Some(package_not) = &query.package_not
        && app.package.as_deref().unwrap_or("").contains(package_not)
    {
        return no_match;
    }
    if let Some(activity) = &query.activity
        && !app.activity.as_deref().unwrap_or("").contains(activity)
    {
        return no_match;
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
async fn cmd_devices(config: &ShadowDroidConfig) -> Result<()> {
    let pairs = adb::list_devices_with_state().await?;
    let mut devices = Vec::with_capacity(pairs.len());
    for (serial, state) in pairs {
        let mut obj = serde_json::Map::new();
        obj.insert("serial".into(), json!(serial));
        obj.insert("state".into(), json!(state));
        if state == "device"
            && let serde_json::Value::Object(info) = adb::device_info(&serial).await
        {
            for (k, v) in info {
                obj.insert(k, v);
            }
        }
        let avd = obj.get("avd").and_then(serde_json::Value::as_str);
        let configured_targets = config
            .targets
            .iter()
            .filter(|(_, target)| {
                target.serial.as_deref() == Some(serial.as_str())
                    || avd.is_some_and(|avd| target.avd.as_deref() == Some(avd))
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if !configured_targets.is_empty() {
            obj.insert("configured_targets".into(), json!(configured_targets));
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
    serial: &Serial,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
) -> Result<()> {
    let before_version = installer::running_server_version(serial)
        .await
        .ok()
        .flatten();
    let client = installer::ensure_ready(serial, apk, any_apk_version).await?;
    // Device prep: disable the Android 14+ stylus-handwriting tutorial that
    // otherwise hijacks the first text-field focus and breaks `text` input.
    // Best-effort + idempotent; surfaced in the output rather than done silently.
    let stylus_tutorial_disabled =
        crate::cmd::device_profile::disable_stylus_tutorial(serial).await;
    let state = client.state().await?;
    let mut out = json!({
        "status": "connected",
        "device": serial,
        "server_version": state.server_version.clone(),
        "api_version": state.api_version,
        "ui_automator_version": state.ui_automator_version,
        "android_sdk": state.android_sdk,
        "android_release": state.android_release,
        "viewport": {"w": state.viewport.w, "h": state.viewport.h},
        "current_app": state.current_app,
        "device_prep": {"stylus_tutorial_disabled": stylus_tutorial_disabled},
        "ui_automation": ui_automation_advisory(),
    });
    if let Some(previous) = before_version.filter(|previous| previous != &state.server_version) {
        out["server_reconciled"] = json!({
            "previous_version": previous,
            "current_version": state.server_version,
            "hint": "server version was reconciled by this explicit connect; implicit UI/app commands fail fast on stale live servers",
        });
    }
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
    emit_action("connect", &out);
    Ok(())
}

async fn cmd_disconnect(serial: &Serial) -> Result<()> {
    let _guard = installer::acquire_lifecycle_lock(serial)?;
    free_ui_automation_slot(serial).await?;
    emit_action(
        "disconnect",
        &json!({"status": "disconnected", "device": serial}),
    );
    Ok(())
}

/// Release the device's single UiAutomation slot held by ShadowDroid's
/// instrumentation: force-stop our packages, kill instrument zombies, drop the
/// port forward. Shared by `disconnect` and `test`.
async fn free_ui_automation_slot(serial: &Serial) -> Result<()> {
    adb::am_force_stop(serial, installer::TEST_PACKAGE).await?;
    adb::am_force_stop(serial, installer::APP_PACKAGE).await?;
    adb::kill_instrument_zombies(serial).await?;
    // Best-effort remove forward; ignore error if it wasn't set. Drop the
    // per-serial host port mapping so the next session reallocates cleanly, and
    // also clear any legacy fixed-port forward left by an older CLI.
    if let Some(host_port) = crate::device::portmap::release(serial, installer::UI_CHANNEL) {
        let _ = adb::forward_remove(serial, host_port).await;
    }
    let _ = adb::forward_remove(serial, installer::DEFAULT_PORT).await;
    Ok(())
}

/// `shadowdroid test -- <cmd>`: free the UiAutomation slot, run the user's
/// instrumentation-test command with stdio inherited, then reconnect (unless
/// `reconnect` is false). A failed child flows through the structured error
/// boundary while preserving the child's status code for CI and agents.
async fn cmd_test(
    serial: &Serial,
    apk: Option<&std::path::Path>,
    any_apk_version: bool,
    reconnect: bool,
    command: Vec<String>,
) -> Result<()> {
    {
        let _guard = installer::acquire_lifecycle_lock(serial)?;
        free_ui_automation_slot(serial)
            .await
            .context("freeing the UiAutomation slot before the test run")?;
    }

    // Inherit stdio so the test runner's output streams live to the user.
    let program = command
        .first()
        .ok_or_else(|| anyhow!("no command given; use `shadowdroid test -- <command>`"))?;
    let status = std::process::Command::new(program)
        .args(&command[1..])
        .status()
        .with_context(|| format!("failed to launch `{}`", command.join(" ")))?;
    let exit_code = status.code();

    let reconnect_result = if reconnect {
        Some(installer::ensure_ready(serial, apk, any_apk_version).await)
    } else {
        None
    };
    let reconnect_error = reconnect_result
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .map(ToString::to_string);
    let reconnected = reconnect_result.as_ref().map(Result::is_ok);

    if !status.success() {
        let mut next_actions =
            vec!["inspect the test command output above, fix the failing test, and rerun"];
        if reconnect_error.is_some() {
            next_actions.push("run `shadowdroid doctor` to repair the failed post-test reconnect");
        }
        return Err(crate::diagnostic::DiagnosticError::new(
            "test_command_failed",
            "test",
            format!("test command exited with status {}", exit_code.unwrap_or(1)),
        )
        .detail(json!({
            "device": serial,
            "command": command,
            "exit_code": exit_code,
            "reconnect_requested": reconnect,
            "reconnected": reconnected,
            "reconnect_error": reconnect_error,
        }))
        .next_actions(next_actions)
        .process_exit_code(exit_code.filter(|code| *code > 0).unwrap_or(1))
        .into());
    }

    // A successful user test followed by a failed reconnect is still a failed
    // ShadowDroid operation: the requested postcondition was not reached. Let
    // the normal error boundary emit the single terminal error envelope.
    if let Some(error) = reconnect_error {
        return Err(crate::diagnostic::DiagnosticError::new(
            "test_reconnect_failed",
            "lifecycle",
            format!("test command passed, but ShadowDroid could not reconnect: {error}"),
        )
        .retryable(true)
        .detail(json!({
            "device": serial,
            "command": command,
            "test_exit_code": exit_code,
        }))
        .next_actions([
            "run `shadowdroid doctor` to inspect the failed reconnect",
            "run `shadowdroid connect` after resolving the reported lifecycle issue",
        ])
        .into());
    }

    let out = json!({
        "device": serial,
        "command": command,
        "exit_code": exit_code,
        "reconnect_requested": reconnect,
        "reconnected": reconnected,
    });
    emit_action("test", &out);
    Ok(())
}

pub(crate) async fn resolve_serial(explicit: Option<&str>) -> Result<Serial> {
    let serial = if let Some(s) = explicit {
        Serial::from(s)
    } else {
        let devices = adb::list_devices().await.context("listing devices")?;
        match devices.len() {
            0 => {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "no_device",
                    "device",
                    "no usable Android device is attached",
                )
                .retryable(true)
                .next_actions([
                    "run `shadowdroid devices` to inspect offline or unauthorized devices",
                    "start an emulator or authorize USB debugging, then retry",
                ])
                .into());
            }
            1 => Serial::from(devices.into_iter().next().unwrap()),
            _ => {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "multiple_devices",
                    "device",
                    format!(
                        "multiple usable Android devices are attached ({})",
                        devices.len()
                    ),
                )
                .detail(json!({"devices": devices}))
                .next_actions([
                    "choose a serial from detail.devices and pass `--device <serial>`",
                    "set SHADOWDROID_DEVICE or config.device for subsequent commands",
                ])
                .into());
            }
        }
    };
    crate::events::set_current_device(serial.to_string());
    Ok(serial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn net_trust_push_is_honest_and_ui_remains_hidden_legacy_alias() {
        let mut command = Cli::command();
        let trust = command
            .find_subcommand_mut("net")
            .unwrap()
            .find_subcommand_mut("trust")
            .unwrap();
        let help = trust.render_long_help().to_string();
        assert!(help.contains("--push"), "{help}");
        assert!(help.contains("screen-lock credential"), "{help}");
        assert!(!help.contains("--ui"), "{help}");

        let parsed = Cli::try_parse_from(["shadowdroid", "net", "trust", "--push"]).unwrap();
        assert!(matches!(
            parsed.cmd,
            Cmd::Net(NetCmd::Trust {
                push: true,
                ui: false,
                ..
            })
        ));

        let legacy = Cli::try_parse_from(["shadowdroid", "net", "trust", "--ui"]).unwrap();
        assert!(matches!(
            legacy.cmd,
            Cmd::Net(NetCmd::Trust {
                push: false,
                ui: true,
                ..
            })
        ));
    }

    #[test]
    fn device_selection_precedence_and_host_filter_use_stable_target_identity() {
        let mut config = ShadowDroidConfig {
            device: Some("legacy-serial".into()),
            default_target: Some("mobile".into()),
            ..Default::default()
        };
        config.targets.insert(
            "mobile".into(),
            crate::config::DeviceTargetConfig {
                avd: Some("Project_Pixel_9".into()),
                ..Default::default()
            },
        );
        let target = DeviceSelection {
            explicit_device: None,
            requested_target: None,
            takeover: false,
        };
        assert_eq!(
            target.debugger_filter(&config).unwrap().as_deref(),
            Some("Project_Pixel_9")
        );

        let explicit = DeviceSelection {
            explicit_device: Some("emulator-7777".into()),
            requested_target: Some("mobile".into()),
            takeover: false,
        };
        assert_eq!(
            explicit.debugger_filter(&config).unwrap().as_deref(),
            Some("emulator-7777")
        );
    }

    #[test]
    fn host_only_target_selection_rejects_unknown_target() {
        let config = ShadowDroidConfig {
            default_target: Some("missing".into()),
            ..Default::default()
        };
        let selection = DeviceSelection {
            explicit_device: None,
            requested_target: None,
            takeover: false,
        };
        let error = selection.debugger_filter(&config).unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.code, "target_not_configured");
    }

    #[test]
    fn push_mode_parses_as_octal() {
        // `--mode 644` must mean 0o644 (rw-r--r-- = 420), matching the help text
        // and the `watch` stdin `push` grammar — not 644 decimal.
        assert_eq!(parse_octal_mode("644").unwrap(), 0o644);
        assert_eq!(parse_octal_mode("755").unwrap(), 0o755);
        assert_eq!(parse_octal_mode(" 600 ").unwrap(), 0o600);
        // Out of the server's 0..=0o777 range, and non-octal digits, are rejected.
        assert!(parse_octal_mode("999").is_err());
        assert!(parse_octal_mode("1644").is_err());
        assert!(parse_octal_mode("rwx").is_err());
    }

    #[test]
    fn push_mode_is_optional_on_platform_managed_storage() {
        let cli =
            Cli::try_parse_from(["shadowdroid", "files", "push", "a.txt", "/data/local/tmp/a"])
                .unwrap();
        match cli.cmd {
            Cmd::Files(FilesCmd::Push { mode, .. }) => assert_eq!(mode, None),
            _ => panic!("expected files push"),
        }
    }

    #[test]
    fn adb_fallback_skips_client_errors_but_takes_server_and_transport() {
        use crate::device::client::ServerError;
        // A 4xx (e.g. bad_mode) is a contract error adb can't fix → surface it.
        let client_err = anyhow::Error::new(ServerError {
            status: reqwest::StatusCode::BAD_REQUEST,
            code: "bad_mode".into(),
            message: "mode must be between 0 and 511".into(),
            detail: None,
        });
        assert!(!should_fall_back_to_adb(&client_err));
        // A 5xx (e.g. unwritable scoped-storage path) → fall back, the reason the
        // adb path exists.
        let server_err = anyhow::Error::new(ServerError {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal".into(),
            message: "EACCES".into(),
            detail: None,
        });
        assert!(should_fall_back_to_adb(&server_err));
        // A non-structured (transport) error → fall back (server unreachable).
        assert!(should_fall_back_to_adb(&anyhow::anyhow!(
            "connection refused"
        )));

        let missing = anyhow::Error::new(ServerError {
            status: reqwest::StatusCode::NOT_FOUND,
            code: "file_not_found".into(),
            message: "not visible to server uid".into(),
            detail: None,
        });
        assert!(should_pull_fall_back_to_adb(&missing));
        let bad_path = anyhow::Error::new(ServerError {
            status: reqwest::StatusCode::BAD_REQUEST,
            code: "bad_path".into(),
            message: "path traversal is not allowed".into(),
            detail: None,
        });
        assert!(!should_pull_fall_back_to_adb(&bad_path));
        let local_disk_error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "destination is read-only",
        ));
        assert!(!should_pull_fall_back_to_adb(&local_disk_error));
    }

    #[tokio::test]
    async fn truncated_server_pull_remains_eligible_for_adb_fallback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort",
                )
                .await
                .unwrap();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/file"))
            .send()
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("pull.bin");
        let error = crate::transfer::response_to_path_atomic(response, &destination, None)
            .await
            .unwrap_err();
        assert!(should_pull_fall_back_to_adb(&error));
        assert!(!destination.exists());
    }

    #[test]
    fn interrupted_app_start_recovery_requires_the_requested_foreground() {
        let package = "io.github.andriyo.shadowdroid.sample";
        assert_eq!(
            matching_started_activity(
                "io.github.andriyo.shadowdroid.sample/.MainActivity",
                package,
                Some(".MainActivity")
            )
            .as_deref(),
            Some("io.github.andriyo.shadowdroid.sample.MainActivity")
        );
        assert!(
            matching_started_activity(
                "io.github.andriyo.shadowdroid.sample/.AltLauncherActivity",
                package,
                Some(".MainActivity")
            )
            .is_none()
        );
        assert!(
            matching_started_activity("com.android.launcher/.Launcher", package, None).is_none()
        );
        assert!(
            matching_started_activity(
                "io.github.andriyo.shadowdroid.sample/.MainActivity",
                package,
                Some("other.package/.MainActivity")
            )
            .is_none()
        );
    }

    #[test]
    fn server_error_families_have_domain_specific_recovery() {
        let cases = [
            ("activity_start_failed", Some("app start"), "app current"),
            ("bad_direction", Some("ui pinch"), "ui pinch"),
            ("swipe_failed", Some("ui swipe"), "ui swipe"),
            ("file_not_found", Some("files pull"), "files pull"),
            ("no_focused_field", Some("ui text"), "ui text"),
            ("no_scrollable", Some("ui scroll-to"), "ui scroll-to"),
            ("shell_timeout", Some("device shell"), "device shell"),
            ("screenshot_failed", Some("ui screenshot"), "doctor"),
            ("xpath_invalid", Some("ui find"), "ui find"),
            ("unknown_key", Some("ui key"), "ui key"),
            ("internal", Some("ui dump"), "collect"),
        ];
        for (code, path, expected) in cases {
            let actions = server_error_next_actions(code, path).unwrap().join("\n");
            assert!(
                actions.contains(expected),
                "{code} recovery should mention {expected:?}: {actions}"
            );
        }
    }

    #[test]
    fn every_server_wire_error_code_has_a_named_recovery() {
        let codes = [
            "activity_package_mismatch",
            "activity_start_failed",
            "activity_start_not_foreground",
            "ambiguous_match",
            "app_clear_failed",
            "app_stop_failed",
            "bad_direction",
            "bad_mode",
            "bad_orientation",
            "bad_path",
            "element_not_found",
            "element_not_clickable",
            "element_disabled",
            "empty_selector",
            "file_not_found",
            "internal",
            "invalid_activity",
            "invalid_package",
            "is_directory",
            "missing_activity",
            "missing_key",
            "missing_package",
            "missing_path",
            "no_focused_field",
            "no_launch_intent",
            "no_scrollable",
            "not_directory",
            "package_not_found",
            "progress_target_conflict",
            "progress_target_required",
            "progress_value_invalid",
            "progress_value_out_of_range",
            "range_semantics_unavailable",
            "screenshot_failed",
            "shell_failed",
            "shell_timeout",
            "swipe_failed",
            "tap_failed",
            "text_failed",
            "set_progress_failed",
            "set_progress_unsupported",
            "set_progress_unverified",
            "unknown_key",
            "xpath_invalid",
        ];
        for code in codes {
            let actions = server_error_next_actions(code, None);
            assert!(
                actions.as_ref().is_some_and(|actions| !actions.is_empty()),
                "server code {code:?} must have an explicit recovery recipe"
            );
        }
        assert!(server_error_next_actions("new_unmapped_code", None).is_none());
    }

    #[test]
    fn generic_errors_are_classified_for_the_feedback_loop() {
        let missing = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "reading /tmp/missing",
        ));
        assert_eq!(classify_generic_error(&missing).code, "input_not_found");
        assert_eq!(error_code_of(&missing), "input_not_found");
        assert_eq!(error_stage_of(&missing), "input");
        assert!(error_uses_fallback(&missing));

        let timeout = anyhow::anyhow!("adb command timed out after 30s");
        let class = classify_generic_error(&timeout);
        assert_eq!(class.code, "transport_timeout");
        assert!(class.retryable);

        let duration = anyhow::anyhow!("invalid duration `later` — use forms like 30s");
        assert_eq!(classify_generic_error(&duration).code, "invalid_input");

        let checksum = anyhow::anyhow!("no checksum found for server.apk in SHA256SUMS");
        assert_eq!(classify_generic_error(&checksum).code, "runtime_error");

        let device_permission = anyhow::anyhow!("adb command failed: Permission denied");
        assert_eq!(
            classify_generic_error(&device_permission).code,
            "runtime_error"
        );

        let known_server = anyhow::Error::new(crate::device::client::ServerError {
            status: reqwest::StatusCode::BAD_REQUEST,
            code: "bad_direction".to_string(),
            message: "bad direction".to_string(),
            detail: None,
        });
        assert!(!error_uses_fallback(&known_server));
        let unknown_server = anyhow::Error::new(crate::device::client::ServerError {
            status: reqwest::StatusCode::BAD_REQUEST,
            code: "future_server_error".to_string(),
            message: "new failure".to_string(),
            detail: None,
        });
        assert!(error_uses_fallback(&unknown_server));
    }

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
    fn clap_prose_hint_is_not_advertised_as_a_replacement() {
        let error = Cli::command()
            .try_get_matches_from(["shadowdroid", "device", "shell", "--wat"])
            .expect_err("--wat should be rejected as an option, not treated as a shell command");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(clap_context_string(&error, ContextKind::Suggested).is_some());
        assert!(actionable_clap_suggestion(&error).is_none());
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
    fn ui_tap_accepts_exact_clickable_and_coordinate_fallback() {
        let cli = Cli::try_parse_from([
            "shadowdroid",
            "ui",
            "tap",
            "--text",
            "Allow",
            "--exact",
            "--clickable",
            "--coordinate-fallback",
        ])
        .expect("`ui tap --exact --clickable` should parse");
        match cli.cmd {
            Cmd::Ui(UiCmd::Tap {
                exact,
                clickable,
                coordinate_fallback,
                text,
                ..
            }) => {
                assert!(exact, "--exact should be set");
                assert!(clickable, "--clickable should be set");
                assert!(coordinate_fallback, "--coordinate-fallback should be set");
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
            sampled_at_ms: None,
        }
    }

    fn text_el(text: &str) -> Element {
        Element {
            id: 0,
            handle: None,
            text: Some(text.into()),
            desc: None,
            klass: None,
            rid: None,
            bounds: None,
            tap: None,
            range: None,
            actions: Vec::new(),
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
    fn top_screen_texts_dedupes_trims_and_caps() {
        let els = [
            text_el("  Something went wrong  "),
            text_el(""),
            text_el("Retry"),
            text_el("Something went wrong"), // dup after trim — dropped
            text_el("Contact support"),
        ];
        let got = top_screen_texts(&els, 12);
        assert_eq!(got, ["Something went wrong", "Retry", "Contact support"]);
        // Cap is honored.
        assert_eq!(top_screen_texts(&els, 2), ["Something went wrong", "Retry"]);
    }

    #[test]
    fn set_progress_input_requires_one_finite_target() {
        assert!(validate_set_progress_input(Some(0.5), None).is_ok());
        assert!(validate_set_progress_input(None, Some(80.0)).is_ok());

        for (value, percent, code) in [
            (None, None, "progress_target_required"),
            (Some(1.0), Some(50.0), "progress_target_conflict"),
            (Some(f64::NAN), None, "progress_value_invalid"),
            (None, Some(f64::INFINITY), "progress_value_invalid"),
        ] {
            let error = validate_set_progress_input(value, percent).unwrap_err();
            assert_eq!(error_code_of(&error), code);
        }
    }

    #[test]
    fn selector_tap_result_separates_match_resolution_and_delivery() {
        let matched = text_el("Nested child");
        let mut activated = text_el("Card");
        activated.id = 1;
        activated.clickable = true;
        let body = selector_tap_body(
            "selector",
            None,
            crate::proto::FindTapResp {
                matched,
                activated_element: Some(activated),
                actionable_resolved: Some(true),
                input_delivered: Some(true),
                x: None,
                y: None,
                action: Some("accessibility_click".into()),
            },
        );
        assert_eq!(body["selector_matched"], true);
        assert_eq!(body["actionable_resolved"], true);
        assert_eq!(body["input_delivered"], true);
        assert_eq!(body["matched_element"]["id"], 0);
        assert_eq!(body["activated_element"]["id"], 1);
        assert!(body["screen_changed"].is_null());
        assert!(body["postcondition_satisfied"].is_null());
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
