//! Host-side Android Studio debugger bridge commands.
//!
//! These commands talk to the ShadowDroid Android Studio plugin over its local
//! loopback HTTP bridge. They do not need the on-device ShadowDroid server.

use anyhow::{Context, Result};
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cmd::studio_contract::{self, query, route, session_action};
use crate::hostenv::shadowdroid_home;

pub(crate) const DEFAULT_BRIDGE_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DebugMode {
    Auto,
    Java,
    Native,
    Mixed,
}

impl DebugMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DebugMode::Auto => "auto",
            DebugMode::Java => "java",
            DebugMode::Native => "native",
            DebugMode::Mixed => "mixed",
        }
    }

    /// Parse a config-file value. Delegates to the ValueEnum derive so config
    /// validation and `--mode` accept exactly the same spellings.
    pub fn from_config(value: &str) -> Option<Self> {
        <Self as clap::ValueEnum>::from_str(value.trim(), true).ok()
    }

    /// The accepted spellings, for error messages ("auto, java, native, mixed").
    pub fn allowed_values() -> String {
        <Self as clap::ValueEnum>::value_variants()
            .iter()
            .filter_map(|v| v.to_possible_value())
            .map(|p| p.get_name().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Subcommand)]
pub enum DebuggerCmd {
    /// Show bridge status, open projects, and active debugger sessions.
    Status,
    /// List active Android Studio debugger sessions.
    Sessions,
    /// List attachable Android processes visible to Android Studio.
    Clients(AndroidClientArgs),
    /// Ask Android Studio to attach its debugger to a running app.
    Attach {
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
        /// App package/process to attach to.
        #[arg(long)]
        package: Option<String>,
        /// Process id to attach to.
        #[arg(long)]
        pid: Option<i32>,
        /// Device serial to prefer when Studio has several devices.
        #[arg(long)]
        device: Option<String>,
        /// Android debugger id/display name. Defaults to Studio's Java Android debugger.
        #[arg(long)]
        debugger: Option<String>,
        /// Semantic debugger mode. Use --debugger for an exact Studio debugger id/name.
        #[arg(long, value_enum)]
        mode: Option<DebugMode>,
        /// Android Studio run configuration whose debugger settings should be reused.
        #[arg(long)]
        configuration: Option<String>,
        /// Open Android Studio's built-in attach dialog instead of attaching headlessly.
        #[arg(long)]
        dialog: bool,
    },
    /// Breakpoint commands.
    #[command(subcommand)]
    Break(BreakCmd),
    /// Non-suspending debugger logpoints and their structured hit stream.
    #[command(subcommand)]
    Logpoint(LogpointCmd),
    /// List breakpoints known to Android Studio.
    Breakpoints,
    /// Pause the selected debug session.
    Pause(SessionSelector),
    /// Resume the selected debug session.
    Resume(SessionSelector),
    /// Step into from the selected suspended session.
    StepIn(SessionSelector),
    /// Step over from the selected suspended session.
    StepOver(SessionSelector),
    /// Step out from the selected suspended session.
    StepOut(SessionSelector),
    /// Stop the selected debug session.
    Stop(SessionSelector),
    /// Print stack frames for the selected suspended session.
    Stack(StackArgs),
    /// Print debugger threads and their stack frames.
    Threads(StackArgs),
    /// Print visible variables for the selected suspended frame.
    Variables(VariablesArgs),
    /// Evaluate a deterministic JDI path expression in the selected frame.
    Eval(EvalArgs),
    /// Inspect a deterministic expression or a previously returned object handle.
    Inspect(InspectArgs),
    /// Coroutine and async-state inspection for suspended sessions.
    #[command(subcommand)]
    Coroutines(CoroutinesCmd),
    /// Resume until a source location or deterministic JDI condition matches.
    ContinueUntil(ContinueUntilArgs),
    /// Watch expression commands.
    #[command(subcommand)]
    Watch(WatchCmd),
}

#[derive(Subcommand)]
pub enum BreakCmd {
    /// Add a Java/Kotlin line breakpoint.
    Line {
        /// Source file path.
        #[arg(long)]
        file: PathBuf,
        /// One-based source line number.
        #[arg(long)]
        line: u32,
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
        /// Create the breakpoint disabled.
        #[arg(long)]
        disabled: bool,
        /// Create a temporary breakpoint.
        #[arg(long)]
        temporary: bool,
        /// Breakpoint condition expression evaluated by Android Studio.
        #[arg(long, conflicts_with = "clear_condition")]
        condition: Option<String>,
        /// Clear any condition on an existing breakpoint at this file/line.
        #[arg(long)]
        clear_condition: bool,
        /// Set the condition even if Android Studio's validation rejects it.
        #[arg(long, requires = "condition")]
        force: bool,
    },
    /// Add a Java exception breakpoint.
    Exception {
        /// Fully-qualified exception class, e.g. java.lang.IllegalStateException.
        exception: String,
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
        /// Create the breakpoint disabled.
        #[arg(long)]
        disabled: bool,
        /// Break on caught exceptions.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        caught: bool,
        /// Break on uncaught exceptions.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        uncaught: bool,
    },
    /// Add a Java/Kotlin wildcard method breakpoint.
    Method {
        /// Class name or pattern.
        #[arg(long)]
        class: String,
        /// Method name.
        #[arg(long)]
        method: String,
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
        /// Create the breakpoint disabled.
        #[arg(long)]
        disabled: bool,
        /// Break on method entry.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        entry: bool,
        /// Break on method exit.
        #[arg(long)]
        exit: bool,
    },
    /// Add a Java/Kotlin field watchpoint at a source location.
    Field {
        /// Source file path containing the field.
        #[arg(long)]
        file: PathBuf,
        /// One-based source line number.
        #[arg(long)]
        line: u32,
        /// Declaring class name.
        #[arg(long)]
        class: String,
        /// Field name.
        #[arg(long)]
        field: String,
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
        /// Create the breakpoint disabled.
        #[arg(long)]
        disabled: bool,
        /// Create a temporary breakpoint.
        #[arg(long)]
        temporary: bool,
        /// Break on field access.
        #[arg(long)]
        access: bool,
        /// Break on field modification.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        modification: bool,
    },
    /// Update a breakpoint by stable id.
    Update(BreakpointUpdateArgs),
    /// Remove a breakpoint by stable id.
    Remove {
        /// Breakpoint id from `debug breakpoints`.
        #[arg(long)]
        id: String,
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum LogpointCmd {
    /// Transactionally configure a non-suspending line logpoint.
    Add(LogpointAddArgs),
    /// List configured logpoints.
    List(LogpointListArgs),
    /// Read a bounded page of structured logpoint-hit events.
    Events(LogpointEventsArgs),
    /// Follow structured logpoint-hit events as JSONL.
    Follow(LogpointFollowArgs),
    /// Remove one ShadowDroid-owned logpoint by stable id.
    Remove(LogpointRemoveArgs),
    /// Remove ShadowDroid-owned logpoints matching the supplied scope.
    Clear(LogpointClearArgs),
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("logpoint_output")
        .required(true)
        .multiple(true)
        .args(["expression", "log_message", "log_stack"])
))]
pub struct LogpointAddArgs {
    /// Source file path.
    #[arg(long)]
    pub file: PathBuf,
    /// One-based source line number.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=2_147_483_647))]
    pub line: u32,
    /// Project name or absolute project path when multiple projects are open.
    #[arg(long)]
    pub project: Option<String>,
    /// Create the logpoint disabled.
    #[arg(long)]
    pub disabled: bool,
    /// Remove the logpoint after its first hit.
    #[arg(long)]
    pub temporary: bool,
    /// Expression whose rendered value is logged at each matching hit.
    #[arg(long, value_parser = parse_nonblank_expression)]
    pub expression: Option<String>,
    /// Include Android Studio's default breakpoint-hit message.
    #[arg(long)]
    pub log_message: bool,
    /// Include a stack trace in Android Studio's rendered logpoint message.
    #[arg(long)]
    pub log_stack: bool,
    /// Boolean condition evaluated before logging.
    #[arg(long)]
    pub condition: Option<String>,
    /// Log only after this many matching passes. Use 0 to disable filtering.
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=2_147_483_647))]
    pub pass_count: Option<u32>,
    /// Set expressions even if Android Studio's validation rejects them.
    #[arg(long)]
    pub force: bool,
    /// Ownership label used for safe remove/clear operations.
    #[arg(long, default_value = "shadowdroid", value_parser = parse_nonblank_owner)]
    pub owner: String,
    /// Maximum structured events captured per second for this logpoint.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub max_events_per_second: Option<u32>,
    /// Maximum rendered-message characters retained per structured event.
    #[arg(long, value_parser = clap::value_parser!(u32).range(256..=65_536))]
    pub max_message_chars: Option<u32>,
}

#[derive(Args, Clone, Debug, Default)]
pub struct LogpointFilterArgs {
    /// Project name or absolute project path.
    #[arg(long)]
    pub project: Option<String>,
    /// Stable debugger session id.
    #[arg(long)]
    pub session: Option<String>,
    /// Stable logpoint/breakpoint id.
    #[arg(long = "id")]
    pub breakpoint_id: Option<String>,
    /// Ownership label to filter by.
    #[arg(long, value_parser = parse_nonblank_owner)]
    pub owner: Option<String>,
}

#[derive(Args)]
pub struct LogpointListArgs {
    #[command(flatten)]
    pub filters: LogpointFilterArgs,
}

#[derive(Args)]
pub struct LogpointEventsArgs {
    /// Return events strictly newer than this cursor.
    #[arg(long, requires = "stream_id")]
    pub after: Option<u64>,
    /// Event-stream id that issued --after; both options must be supplied together.
    #[arg(long, requires = "after", value_parser = parse_nonblank_stream_id)]
    pub stream_id: Option<String>,
    /// Maximum number of events to return.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=200))]
    pub limit: u32,
    #[command(flatten)]
    pub filters: LogpointFilterArgs,
}

#[derive(Args)]
pub struct LogpointFollowArgs {
    /// Resume strictly after this cursor instead of starting at the live tail.
    #[arg(long, conflicts_with = "replay_existing", requires = "stream_id")]
    pub after: Option<u64>,
    /// Event-stream id that issued --after; both options must be supplied together.
    #[arg(long, requires = "after", value_parser = parse_nonblank_stream_id)]
    pub stream_id: Option<String>,
    /// Maximum events requested from the bridge in one page.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=200))]
    pub limit: u32,
    /// Maximum bridge long-poll wait in milliseconds.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(50..=5_000))]
    pub poll_ms: u32,
    /// Stop after this many milliseconds. Omit to follow until Ctrl-C.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub duration_ms: Option<u64>,
    /// Stop after emitting this many logpoint events.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub max_events: Option<u64>,
    /// Emit the current bounded event history before following new hits.
    #[arg(long)]
    pub replay_existing: bool,
    #[command(flatten)]
    pub filters: LogpointFilterArgs,
}

#[derive(Args)]
pub struct LogpointRemoveArgs {
    /// Stable id from `debug logpoint list` or `debug logpoint add`.
    #[arg(long)]
    pub id: String,
    /// Project name or absolute project path.
    #[arg(long)]
    pub project: Option<String>,
    /// Ownership label that must match the logpoint.
    #[arg(long, default_value = "shadowdroid", value_parser = parse_nonblank_owner)]
    pub owner: String,
}

#[derive(Args)]
pub struct LogpointClearArgs {
    /// Project name or absolute project path.
    #[arg(long)]
    pub project: Option<String>,
    /// Remove only logpoints with this ownership label.
    #[arg(long, default_value = "shadowdroid", value_parser = parse_nonblank_owner)]
    pub owner: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LogpointEventFilters {
    pub project: Option<String>,
    pub session: Option<String>,
    pub breakpoint_id: Option<String>,
    pub owner: Option<String>,
}

impl From<&LogpointFilterArgs> for LogpointEventFilters {
    fn from(filters: &LogpointFilterArgs) -> Self {
        Self {
            project: filters.project.clone(),
            session: filters.session.clone(),
            breakpoint_id: filters.breakpoint_id.clone(),
            owner: filters.owner.clone(),
        }
    }
}

fn parse_nonblank_owner(value: &str) -> std::result::Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        Err("owner must not be blank".to_string())
    } else {
        Ok(value.to_string())
    }
}

fn parse_nonblank_expression(value: &str) -> std::result::Result<String, String> {
    if value.trim().is_empty() {
        Err("expression must not be blank".to_string())
    } else {
        Ok(value.to_string())
    }
}

fn parse_nonblank_stream_id(value: &str) -> std::result::Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        Err("stream id must not be blank".to_string())
    } else {
        Ok(value.to_string())
    }
}

#[derive(Args)]
pub struct SessionSelector {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Args)]
pub struct StackArgs {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Maximum number of frames per stack.
    #[arg(long, default_value_t = 64)]
    pub limit: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct VariablesArgs {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Execution stack/thread index from `debug threads`.
    #[arg(long)]
    pub thread: Option<String>,
    /// Frame index within the selected thread.
    #[arg(long)]
    pub frame: Option<usize>,
    /// Object expansion depth. 0 prints only local values, 1 prints direct fields.
    #[arg(long, default_value_t = 0)]
    pub depth: u32,
    /// Maximum instance fields to include per object.
    #[arg(long, default_value_t = 64)]
    pub max_fields: u32,
    /// Maximum array/list items to include per array.
    #[arg(long, default_value_t = 32)]
    pub max_array_items: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct EvalArgs {
    /// Deterministic expression path: `this`, a local name, fields, and array indexes.
    pub expression: String,
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Execution stack/thread index from `debug threads`.
    #[arg(long)]
    pub thread: Option<String>,
    /// Frame index within the selected thread.
    #[arg(long)]
    pub frame: Option<usize>,
    /// Object expansion depth. 0 prints only the result value.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
    /// Maximum instance fields to include per object.
    #[arg(long, default_value_t = 64)]
    pub max_fields: u32,
    /// Maximum array/list items to include per array.
    #[arg(long, default_value_t = 32)]
    pub max_array_items: u32,
    /// Evaluation request timeout.
    #[arg(long, default_value_t = 5000)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct InspectArgs {
    /// Deterministic expression path: `this`, a local name, fields, and array indexes.
    #[arg(conflicts_with = "handle")]
    pub expression: Option<String>,
    /// Object handle returned by `debug variables`, `debug eval`, or `debug inspect`.
    #[arg(long)]
    pub handle: Option<String>,
    /// Relative path from --handle, e.g. `.field[0]`.
    #[arg(long)]
    pub path: Option<String>,
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Execution stack/thread index from `debug threads`.
    #[arg(long)]
    pub thread: Option<String>,
    /// Frame index within the selected thread.
    #[arg(long)]
    pub frame: Option<usize>,
    /// Object expansion depth. 0 prints only the result value.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
    /// Maximum instance fields to include per object.
    #[arg(long, default_value_t = 64)]
    pub max_fields: u32,
    /// Maximum array/list items to include per array.
    #[arg(long, default_value_t = 32)]
    pub max_array_items: u32,
    /// Inspection request timeout.
    #[arg(long, default_value_t = 5000)]
    pub timeout_ms: u32,
}

#[derive(Subcommand)]
pub enum CoroutinesCmd {
    /// Snapshot coroutine-like state reachable from suspended Java/Kotlin frames.
    Snapshot(CoroutineSnapshotArgs),
    /// Print debugger threads with dispatcher hints.
    Threads(CoroutineThreadsArgs),
    /// Inspect continuation-like objects in the selected frame.
    Continuation(CoroutineContinuationArgs),
    /// Inspect a Flow/StateFlow-like expression without collecting or invoking methods.
    Flow(CoroutineFlowArgs),
}

#[derive(Args)]
pub struct CoroutineSnapshotArgs {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Maximum threads/frames/continuations to include.
    #[arg(long, default_value_t = 64)]
    pub limit: u32,
    /// Object expansion depth for spilled locals.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct CoroutineThreadsArgs {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Maximum threads to include.
    #[arg(long, default_value_t = 32)]
    pub limit: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct CoroutineContinuationArgs {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Execution stack/thread index from `debug threads`.
    #[arg(long)]
    pub thread: Option<String>,
    /// Frame index within the selected thread.
    #[arg(long)]
    pub frame: Option<usize>,
    /// Object expansion depth for spilled locals.
    #[arg(long, default_value_t = 2)]
    pub depth: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct CoroutineFlowArgs {
    /// Deterministic expression path to a Flow/StateFlow-like object.
    #[arg(long)]
    pub expr: String,
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Execution stack/thread index from `debug threads`.
    #[arg(long)]
    pub thread: Option<String>,
    /// Frame index within the selected thread.
    #[arg(long)]
    pub frame: Option<usize>,
    /// Object expansion depth.
    #[arg(long, default_value_t = 2)]
    pub depth: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct ContinueUntilArgs {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Source file path to match against the top frame.
    #[arg(long, requires = "line")]
    pub file: Option<PathBuf>,
    /// One-based source line to match against the top frame.
    #[arg(long, requires = "file")]
    pub line: Option<u32>,
    /// Deterministic JDI path expression that must evaluate to true/non-null/non-zero.
    #[arg(long)]
    pub condition: Option<String>,
    /// Stop waiting after this many milliseconds.
    #[arg(long, default_value_t = 10000)]
    pub timeout_ms: u64,
    /// Poll interval while waiting.
    #[arg(long, default_value_t = 100)]
    pub poll_ms: u64,
}

#[derive(Subcommand)]
pub enum WatchCmd {
    /// Add or replace a watch expression.
    Add {
        expression: String,
        /// Optional stable name. Defaults to the expression text.
        #[arg(long)]
        name: Option<String>,
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
    },
    /// List watches and evaluate them if a session is suspended.
    List(WatchListArgs),
    /// Remove a watch by id.
    Remove {
        /// Id of the watch to remove (from `debug watch list`).
        #[arg(long)]
        id: String,
    },
    /// Remove all watches.
    Clear,
}

#[derive(Args)]
pub struct WatchListArgs {
    /// Stable session id (preferred) or current index from `debug sessions`.
    #[arg(long)]
    pub session: Option<String>,
    /// Object expansion depth for evaluated watch values.
    #[arg(long, default_value_t = 1)]
    pub depth: u32,
    /// Maximum instance fields to include per object.
    #[arg(long, default_value_t = 64)]
    pub max_fields: u32,
    /// Maximum array/list items to include per array.
    #[arg(long, default_value_t = 32)]
    pub max_array_items: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct BreakpointUpdateArgs {
    /// Breakpoint id from `debug breakpoints`.
    #[arg(long)]
    pub id: String,
    /// Project name or absolute project path when multiple projects are open.
    #[arg(long)]
    pub project: Option<String>,
    /// Enable or disable the breakpoint.
    #[arg(long)]
    pub enabled: Option<bool>,
    /// Mark breakpoint temporary or persistent.
    #[arg(long)]
    pub temporary: Option<bool>,
    /// Breakpoint condition expression evaluated by Android Studio.
    #[arg(long, conflicts_with = "clear_condition")]
    pub condition: Option<String>,
    /// Clear any breakpoint condition.
    #[arg(long)]
    pub clear_condition: bool,
    /// Log expression without stopping when suspend policy is `none`.
    #[arg(long, conflicts_with = "clear_log_expression")]
    pub log_expression: Option<String>,
    /// Clear the log expression.
    #[arg(long)]
    pub clear_log_expression: bool,
    /// Toggle the default "breakpoint hit" log message.
    #[arg(long)]
    pub log_message: Option<bool>,
    /// Toggle stack trace logging.
    #[arg(long)]
    pub log_stack: Option<bool>,
    /// Suspend policy: all, thread, or none.
    #[arg(long, value_enum)]
    pub suspend: Option<SuspendArg>,
    /// Pass count. Use 0 to disable pass-count filtering.
    #[arg(long)]
    pub pass_count: Option<u32>,
    /// Set expressions even if Android Studio's validation rejects them.
    #[arg(long)]
    pub force: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SuspendArg {
    All,
    Thread,
    None,
}

impl SuspendArg {
    fn as_bridge(self) -> &'static str {
        match self {
            SuspendArg::All => "ALL",
            SuspendArg::Thread => "THREAD",
            SuspendArg::None => "NONE",
        }
    }
}

#[derive(Args)]
pub struct AndroidClientArgs {
    /// Project name or absolute project path when multiple projects are open.
    #[arg(long)]
    pub project: Option<String>,
    /// Filter by app package/process.
    #[arg(long)]
    pub package: Option<String>,
    /// Filter by process id.
    #[arg(long)]
    pub pid: Option<i32>,
    /// Filter by device serial.
    #[arg(long)]
    pub device: Option<String>,
}

pub async fn run(cmd: &DebuggerCmd, device: Option<&str>, studio_url: Option<&str>) -> Result<()> {
    let bridge = BridgeClient::with_device(studio_url, device)?;
    let value = match cmd {
        DebuggerCmd::Status => bridge.get(route::STATUS, &[]).await?,
        DebuggerCmd::Sessions => bridge.get(route::SESSIONS, &[]).await?,
        DebuggerCmd::Clients(filter) => {
            let pid_s = filter.pid.map(|pid| pid.to_string());
            let params = [
                (query::PROJECT, filter.project.as_deref()),
                (query::PACKAGE, filter.package.as_deref()),
                (query::PID, pid_s.as_deref()),
                (query::DEVICE, filter.device.as_deref()),
            ];
            bridge.get(route::CLIENTS, &params).await?
        }
        DebuggerCmd::Attach {
            project,
            package,
            pid,
            device,
            debugger,
            mode,
            configuration,
            dialog,
        } => {
            let pid_s = pid.map(|pid| pid.to_string());
            let dialog_s = dialog.to_string();
            let mode_s = mode.map(DebugMode::as_str);
            let params = [
                (query::PROJECT, project.as_deref()),
                (query::PACKAGE, package.as_deref()),
                (query::PID, pid_s.as_deref()),
                (query::DEVICE, device.as_deref()),
                (query::DEBUGGER, debugger.as_deref()),
                (query::MODE, mode_s),
                (query::CONFIGURATION, configuration.as_deref()),
                (query::DIALOG, Some(dialog_s.as_str())),
            ];
            bridge.get(route::ATTACH, &params).await?
        }
        DebuggerCmd::Break(BreakCmd::Line {
            file,
            line,
            project,
            disabled,
            temporary,
            condition,
            clear_condition,
            force,
        }) => {
            let canonical = canonicalize_for_bridge(file)?;
            let line_s = line.to_string();
            let enabled_s = (!*disabled).to_string();
            let temporary_s = temporary.to_string();
            let clear_condition_s = clear_condition.to_string();
            let validate_s = force.then_some("false");
            let params = [
                (query::FILE, Some(canonical.as_str())),
                (query::LINE, Some(line_s.as_str())),
                (query::PROJECT, project.as_deref()),
                (query::ENABLED, Some(enabled_s.as_str())),
                (query::TEMPORARY, Some(temporary_s.as_str())),
                (query::CONDITION, condition.as_deref()),
                (query::CLEAR_CONDITION, Some(clear_condition_s.as_str())),
                (query::VALIDATE, validate_s),
            ];
            bridge.get(route::BREAKPOINT_LINE, &params).await?
        }
        DebuggerCmd::Break(BreakCmd::Exception {
            exception,
            project,
            disabled,
            caught,
            uncaught,
        }) => {
            let enabled_s = (!*disabled).to_string();
            let caught_s = caught.to_string();
            let uncaught_s = uncaught.to_string();
            let params = [
                (query::EXCEPTION, Some(exception.as_str())),
                (query::PROJECT, project.as_deref()),
                (query::ENABLED, Some(enabled_s.as_str())),
                (query::CAUGHT, Some(caught_s.as_str())),
                (query::UNCAUGHT, Some(uncaught_s.as_str())),
            ];
            bridge.get(route::BREAKPOINT_EXCEPTION, &params).await?
        }
        DebuggerCmd::Break(BreakCmd::Method {
            class,
            method,
            project,
            disabled,
            entry,
            exit,
        }) => {
            let enabled_s = (!*disabled).to_string();
            let entry_s = entry.to_string();
            let exit_s = exit.to_string();
            let params = [
                (query::CLASS, Some(class.as_str())),
                (query::METHOD, Some(method.as_str())),
                (query::PROJECT, project.as_deref()),
                (query::ENABLED, Some(enabled_s.as_str())),
                (query::ENTRY, Some(entry_s.as_str())),
                (query::EXIT, Some(exit_s.as_str())),
            ];
            bridge.get(route::BREAKPOINT_METHOD, &params).await?
        }
        DebuggerCmd::Break(BreakCmd::Field {
            file,
            line,
            class,
            field,
            project,
            disabled,
            temporary,
            access,
            modification,
        }) => {
            let canonical = canonicalize_for_bridge(file)?;
            let line_s = line.to_string();
            let enabled_s = (!*disabled).to_string();
            let temporary_s = temporary.to_string();
            let access_s = access.to_string();
            let modification_s = modification.to_string();
            let params = [
                (query::FILE, Some(canonical.as_str())),
                (query::LINE, Some(line_s.as_str())),
                (query::CLASS, Some(class.as_str())),
                (query::FIELD, Some(field.as_str())),
                (query::PROJECT, project.as_deref()),
                (query::ENABLED, Some(enabled_s.as_str())),
                (query::TEMPORARY, Some(temporary_s.as_str())),
                (query::ACCESS, Some(access_s.as_str())),
                (query::MODIFICATION, Some(modification_s.as_str())),
            ];
            bridge.get(route::BREAKPOINT_FIELD, &params).await?
        }
        DebuggerCmd::Break(BreakCmd::Update(args)) => {
            let enabled_s = args.enabled.map(|v| v.to_string());
            let temporary_s = args.temporary.map(|v| v.to_string());
            let clear_condition_s = args.clear_condition.to_string();
            let clear_log_expression_s = args.clear_log_expression.to_string();
            let log_message_s = args.log_message.map(|v| v.to_string());
            let log_stack_s = args.log_stack.map(|v| v.to_string());
            let suspend_s = args.suspend.map(SuspendArg::as_bridge);
            let pass_count_s = args.pass_count.map(|v| v.to_string());
            let validate_s = args.force.then_some("false");
            let params = [
                (query::ID, Some(args.id.as_str())),
                (query::PROJECT, args.project.as_deref()),
                (query::ENABLED, enabled_s.as_deref()),
                (query::TEMPORARY, temporary_s.as_deref()),
                (query::CONDITION, args.condition.as_deref()),
                (query::CLEAR_CONDITION, Some(clear_condition_s.as_str())),
                (query::LOG_EXPRESSION, args.log_expression.as_deref()),
                (
                    query::CLEAR_LOG_EXPRESSION,
                    Some(clear_log_expression_s.as_str()),
                ),
                (query::LOG_MESSAGE, log_message_s.as_deref()),
                (query::LOG_STACK, log_stack_s.as_deref()),
                (query::SUSPEND, suspend_s),
                (query::PASS_COUNT, pass_count_s.as_deref()),
                (query::VALIDATE, validate_s),
            ];
            bridge.get(route::BREAKPOINT_UPDATE, &params).await?
        }
        DebuggerCmd::Break(BreakCmd::Remove { id, project }) => {
            let params = [
                (query::ID, Some(id.as_str())),
                (query::PROJECT, project.as_deref()),
            ];
            bridge.get(route::BREAKPOINT_REMOVE, &params).await?
        }
        DebuggerCmd::Logpoint(LogpointCmd::Add(args)) => {
            let canonical = canonicalize_for_bridge(&args.file)?;
            let line_s = args.line.to_string();
            let enabled_s = (!args.disabled).to_string();
            let temporary_s = args.temporary.to_string();
            let log_message_s = args.log_message.to_string();
            let log_stack_s = args.log_stack.to_string();
            let pass_count_s = args.pass_count.map(|value| value.to_string());
            let validate_s = args.force.then_some("false");
            let max_events_per_second_s = args.max_events_per_second.map(|value| value.to_string());
            let max_message_chars_s = args.max_message_chars.map(|value| value.to_string());
            let params = [
                (query::FILE, Some(canonical.as_str())),
                (query::LINE, Some(line_s.as_str())),
                (query::PROJECT, args.project.as_deref()),
                (query::ENABLED, Some(enabled_s.as_str())),
                (query::TEMPORARY, Some(temporary_s.as_str())),
                (query::LOG_EXPRESSION, args.expression.as_deref()),
                (query::LOG_MESSAGE, Some(log_message_s.as_str())),
                (query::LOG_STACK, Some(log_stack_s.as_str())),
                (query::CONDITION, args.condition.as_deref()),
                (query::PASS_COUNT, pass_count_s.as_deref()),
                (query::VALIDATE, validate_s),
                (query::OWNER, Some(args.owner.as_str())),
                (
                    query::MAX_EVENTS_PER_SECOND,
                    max_events_per_second_s.as_deref(),
                ),
                (query::MAX_MESSAGE_CHARS, max_message_chars_s.as_deref()),
            ];
            bridge.get(route::LOGPOINT_ADD, &params).await?
        }
        DebuggerCmd::Logpoint(LogpointCmd::List(args)) => {
            let params = logpoint_filter_params(&args.filters);
            bridge.get(route::LOGPOINTS, &params).await?
        }
        DebuggerCmd::Logpoint(LogpointCmd::Events(args)) => {
            let filters = LogpointEventFilters::from(&args.filters);
            let page = bridge
                .read_logpoint_events(args.after, args.limit, 0, &filters)
                .await?;
            validate_logpoint_stream(&page, args.stream_id.as_deref(), args.after)?;
            page
        }
        DebuggerCmd::Logpoint(LogpointCmd::Follow(args)) => {
            return follow_logpoint_events(&bridge, args).await;
        }
        DebuggerCmd::Logpoint(LogpointCmd::Remove(args)) => {
            let params = [
                (query::ID, Some(args.id.as_str())),
                (query::PROJECT, args.project.as_deref()),
                (query::OWNER, Some(args.owner.as_str())),
            ];
            bridge.get(route::LOGPOINT_REMOVE, &params).await?
        }
        DebuggerCmd::Logpoint(LogpointCmd::Clear(args)) => {
            let params = [
                (query::PROJECT, args.project.as_deref()),
                (query::OWNER, Some(args.owner.as_str())),
            ];
            bridge.get(route::LOGPOINT_CLEAR, &params).await?
        }
        DebuggerCmd::Breakpoints => bridge.get(route::BREAKPOINTS, &[]).await?,
        DebuggerCmd::Pause(selector) => control(&bridge, session_action::PAUSE, selector).await?,
        DebuggerCmd::Resume(selector) => control(&bridge, session_action::RESUME, selector).await?,
        DebuggerCmd::StepIn(selector) => {
            control(&bridge, session_action::STEP_INTO, selector).await?
        }
        DebuggerCmd::StepOver(selector) => {
            control(&bridge, session_action::STEP_OVER, selector).await?
        }
        DebuggerCmd::StepOut(selector) => {
            control(&bridge, session_action::STEP_OUT, selector).await?
        }
        DebuggerCmd::Stop(selector) => control(&bridge, session_action::STOP, selector).await?,
        DebuggerCmd::Stack(args) => {
            let session_s = args.session.clone();
            let limit_s = args.limit.to_string();
            let timeout_ms_s = args.timeout_ms.to_string();
            let params = [
                (query::SESSION, session_s.as_deref()),
                (query::LIMIT, Some(limit_s.as_str())),
                (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
            ];
            bridge
                .get(route::SESSION_STACK, &params)
                .await
                .unwrap_or_else(|err| read_error_json("debugger_stack", err))
        }
        DebuggerCmd::Threads(args) => {
            let session_s = args.session.clone();
            let limit_s = args.limit.to_string();
            let timeout_ms_s = args.timeout_ms.to_string();
            let params = [
                (query::SESSION, session_s.as_deref()),
                (query::LIMIT, Some(limit_s.as_str())),
                (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
            ];
            bridge
                .get(route::SESSION_THREADS, &params)
                .await
                .unwrap_or_else(|err| read_error_json("debugger_threads", err))
        }
        DebuggerCmd::Variables(args) => {
            let session_s = args.session.clone();
            let frame_s = args.frame.map(|s| s.to_string());
            let depth_s = args.depth.to_string();
            let max_fields_s = args.max_fields.to_string();
            let max_array_items_s = args.max_array_items.to_string();
            let timeout_ms_s = args.timeout_ms.to_string();
            let params = [
                (query::SESSION, session_s.as_deref()),
                (query::THREAD, args.thread.as_deref()),
                (query::FRAME, frame_s.as_deref()),
                (query::DEPTH, Some(depth_s.as_str())),
                (query::MAX_FIELDS, Some(max_fields_s.as_str())),
                (query::MAX_ARRAY_ITEMS, Some(max_array_items_s.as_str())),
                (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
            ];
            bridge
                .get(route::SESSION_VARIABLES, &params)
                .await
                .unwrap_or_else(|err| read_error_json("debugger_variables", err))
        }
        DebuggerCmd::Eval(args) => {
            let session_s = args.session.clone();
            let frame_s = args.frame.map(|s| s.to_string());
            let depth_s = args.depth.to_string();
            let max_fields_s = args.max_fields.to_string();
            let max_array_items_s = args.max_array_items.to_string();
            let timeout_ms_s = args.timeout_ms.to_string();
            let params = [
                (query::SESSION, session_s.as_deref()),
                (query::THREAD, args.thread.as_deref()),
                (query::FRAME, frame_s.as_deref()),
                (query::EXPRESSION, Some(args.expression.as_str())),
                (query::DEPTH, Some(depth_s.as_str())),
                (query::MAX_FIELDS, Some(max_fields_s.as_str())),
                (query::MAX_ARRAY_ITEMS, Some(max_array_items_s.as_str())),
                (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
            ];
            match tokio::time::timeout(
                std::time::Duration::from_millis(u64::from(args.timeout_ms)),
                bridge.get(route::SESSION_EVALUATE, &params),
            )
            .await
            {
                Ok(Ok(value)) => value,
                Ok(Err(err)) => serde_json::json!({
                    "ok": false,
                    "type": "debugger_eval",
                    "error": err.to_string(),
                    "expression": args.expression.as_str(),
                }),
                Err(_) => serde_json::json!({
                    "ok": false,
                    "type": "debugger_eval",
                    "timeout": true,
                    "timeout_ms": args.timeout_ms,
                    "expression": args.expression.as_str(),
                }),
            }
        }
        DebuggerCmd::Inspect(args) => {
            if args.expression.is_none() && args.handle.is_none() {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "debug_inspect_target_required",
                    "input",
                    "debug inspect requires an expression or --handle",
                )
                .detail(serde_json::json!({
                    "accepted": ["EXPRESSION", "--handle HANDLE"],
                }))
                .next_actions([
                    "shadowdroid commands --json --describe 'debug inspect'",
                    "shadowdroid debug variables",
                ])
                .into());
            }
            let session_s = args.session.clone();
            let frame_s = args.frame.map(|s| s.to_string());
            let depth_s = args.depth.to_string();
            let max_fields_s = args.max_fields.to_string();
            let max_array_items_s = args.max_array_items.to_string();
            let timeout_ms_s = args.timeout_ms.to_string();
            let params = [
                (query::SESSION, session_s.as_deref()),
                (query::THREAD, args.thread.as_deref()),
                (query::FRAME, frame_s.as_deref()),
                (query::EXPRESSION, args.expression.as_deref()),
                (query::HANDLE, args.handle.as_deref()),
                (query::PATH, args.path.as_deref()),
                (query::DEPTH, Some(depth_s.as_str())),
                (query::MAX_FIELDS, Some(max_fields_s.as_str())),
                (query::MAX_ARRAY_ITEMS, Some(max_array_items_s.as_str())),
                (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
            ];
            match tokio::time::timeout(
                std::time::Duration::from_millis(u64::from(args.timeout_ms)),
                bridge.get(route::SESSION_INSPECT, &params),
            )
            .await
            {
                Ok(Ok(value)) => value,
                Ok(Err(err)) => serde_json::json!({
                    "ok": false,
                    "type": "debug_inspect",
                    "error": err.to_string(),
                    "expression": args.expression.as_deref(),
                    "handle": args.handle.as_deref(),
                }),
                Err(_) => serde_json::json!({
                    "ok": false,
                    "type": "debug_inspect",
                    "timeout": true,
                    "timeout_ms": args.timeout_ms,
                    "expression": args.expression.as_deref(),
                    "handle": args.handle.as_deref(),
                }),
            }
        }
        DebuggerCmd::Coroutines(cmd) => match cmd {
            CoroutinesCmd::Snapshot(args) => {
                let session_s = args.session.clone();
                let limit_s = args.limit.to_string();
                let depth_s = args.depth.to_string();
                let timeout_ms_s = args.timeout_ms.to_string();
                let params = [
                    (query::SESSION, session_s.as_deref()),
                    (query::LIMIT, Some(limit_s.as_str())),
                    (query::DEPTH, Some(depth_s.as_str())),
                    (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
                ];
                bridge
                    .get(route::SESSION_COROUTINES, &params)
                    .await
                    .unwrap_or_else(|err| read_error_json("debug_coroutines_snapshot", err))
            }
            CoroutinesCmd::Threads(args) => {
                let session_s = args.session.clone();
                let limit_s = args.limit.to_string();
                let timeout_ms_s = args.timeout_ms.to_string();
                let params = [
                    (query::SESSION, session_s.as_deref()),
                    (query::LIMIT, Some(limit_s.as_str())),
                    (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
                ];
                bridge
                    .get(route::SESSION_COROUTINES_THREADS, &params)
                    .await
                    .unwrap_or_else(|err| read_error_json("debug_coroutines_threads", err))
            }
            CoroutinesCmd::Continuation(args) => {
                let session_s = args.session.clone();
                let frame_s = args.frame.map(|f| f.to_string());
                let depth_s = args.depth.to_string();
                let timeout_ms_s = args.timeout_ms.to_string();
                let params = [
                    (query::SESSION, session_s.as_deref()),
                    (query::THREAD, args.thread.as_deref()),
                    (query::FRAME, frame_s.as_deref()),
                    (query::DEPTH, Some(depth_s.as_str())),
                    (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
                ];
                bridge
                    .get(route::SESSION_COROUTINES_CONTINUATION, &params)
                    .await
                    .unwrap_or_else(|err| read_error_json("debug_coroutines_continuation", err))
            }
            CoroutinesCmd::Flow(args) => {
                let session_s = args.session.clone();
                let frame_s = args.frame.map(|f| f.to_string());
                let depth_s = args.depth.to_string();
                let timeout_ms_s = args.timeout_ms.to_string();
                let params = [
                    (query::SESSION, session_s.as_deref()),
                    (query::THREAD, args.thread.as_deref()),
                    (query::FRAME, frame_s.as_deref()),
                    (query::EXPRESSION, Some(args.expr.as_str())),
                    (query::DEPTH, Some(depth_s.as_str())),
                    (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
                ];
                bridge
                    .get(route::SESSION_COROUTINES_FLOW, &params)
                    .await
                    .unwrap_or_else(|err| read_error_json("debug_coroutines_flow", err))
            }
        },
        DebuggerCmd::ContinueUntil(args) => continue_until(&bridge, args).await?,
        DebuggerCmd::Watch(WatchCmd::Add {
            expression,
            name,
            project,
        }) => {
            let params = [
                (query::EXPRESSION, Some(expression.as_str())),
                (query::NAME, name.as_deref()),
                (query::PROJECT, project.as_deref()),
            ];
            bridge.get(route::WATCHES_ADD, &params).await?
        }
        DebuggerCmd::Watch(WatchCmd::List(args)) => {
            let session_s = args.session.clone();
            let depth_s = args.depth.to_string();
            let max_fields_s = args.max_fields.to_string();
            let max_array_items_s = args.max_array_items.to_string();
            let timeout_ms_s = args.timeout_ms.to_string();
            let params = [
                (query::SESSION, session_s.as_deref()),
                (query::DEPTH, Some(depth_s.as_str())),
                (query::MAX_FIELDS, Some(max_fields_s.as_str())),
                (query::MAX_ARRAY_ITEMS, Some(max_array_items_s.as_str())),
                (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
            ];
            bridge
                .get(route::WATCHES, &params)
                .await
                .unwrap_or_else(|err| read_error_json("debugger_watches", err))
        }
        DebuggerCmd::Watch(WatchCmd::Remove { id }) => {
            let params = [(query::ID, Some(id.as_str()))];
            bridge.get(route::WATCHES_REMOVE, &params).await?
        }
        DebuggerCmd::Watch(WatchCmd::Clear) => bridge.get(route::WATCHES_CLEAR, &[]).await?,
    };
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Android Studio debugger operation did not complete");
        return Err(crate::diagnostic::DiagnosticError::new(
            "debugger_operation_failed",
            "debugger",
            message,
        )
        .detail(serde_json::json!({"bridge_reply": value}))
        .next_actions([
            "inspect detail.bridge_reply and select a valid suspended session when required",
            "run `shadowdroid studio status --json` before retrying",
        ])
        .into());
    }
    emit(&value)?;
    Ok(())
}

fn logpoint_filter_params(filters: &LogpointFilterArgs) -> [(&'static str, Option<&str>); 4] {
    [
        (query::PROJECT, filters.project.as_deref()),
        (query::SESSION, filters.session.as_deref()),
        (query::ID, filters.breakpoint_id.as_deref()),
        (query::OWNER, filters.owner.as_deref()),
    ]
}

async fn follow_logpoint_events(bridge: &BridgeClient, args: &LogpointFollowArgs) -> Result<()> {
    let filters = LogpointEventFilters::from(&args.filters);
    let limit = args.limit.max(1);
    let poll_ms = args.poll_ms.clamp(50, 5_000);
    let started = std::time::Instant::now();
    let mut emitted = 0u64;
    let mut cursor = args.after.unwrap_or(0);
    let mut stream_id = args.stream_id.clone();
    let mut evicted_total = 0u64;
    let mut rate_limited_total = 0u64;

    if args.after.is_none() {
        let initial = bridge.read_logpoint_events(None, 1, 0, &filters).await?;
        stream_id = Some(required_logpoint_stream_id(&initial)?.to_string());
        evicted_total = cursor_field(&initial, "evicted_total").unwrap_or(0);
        rate_limited_total = cursor_field(&initial, "rate_limited_total").unwrap_or(0);
        cursor = initial_logpoint_follow_cursor(&initial, args.replay_existing, cursor);
        if args.replay_existing {
            // Page forward from the stream origin so every still-retained
            // event is replayed in sequence. A null-cursor read returns only a
            // newest tail page, which would silently skip older retained hits.
            debug_assert_eq!(cursor, 0);
        }
    }

    let mut stop = Box::pin(tokio::signal::ctrl_c());
    let reason = loop {
        if args.max_events.is_some_and(|maximum| emitted >= maximum) {
            break "max_events";
        }
        if args
            .duration_ms
            .is_some_and(|duration| started.elapsed() >= Duration::from_millis(duration))
        {
            break "duration";
        }

        let timeout_ms = args
            .duration_ms
            .map(|duration| {
                let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let remaining = duration.saturating_sub(elapsed);
                u32::try_from(remaining.min(u64::from(poll_ms))).unwrap_or(poll_ms)
            })
            .unwrap_or(poll_ms)
            .max(1);
        let request = bridge.read_logpoint_events(Some(cursor), limit, timeout_ms, &filters);
        let page = tokio::select! {
            result = request => result?,
            result = &mut stop => {
                result.context("waiting for ctrl-c")?;
                break "interrupt";
            }
        };

        let page_stream_id = required_logpoint_stream_id(&page)?.to_string();
        if let Some(previous) = stream_id.as_deref()
            && previous != page_stream_id
        {
            crate::events::emit(&serde_json::json!({
                "type": "warning",
                "stream": "debug_logpoint_follow",
                "stage": "logpoint_follow",
                "code": "logpoint_stream_reset",
                "msg": "Android Studio restarted its logpoint event stream; following from the new live tail",
                "retryable": true,
                "detail": {
                    "previous_stream_id": previous,
                    "stream_id": page_stream_id,
                    "previous_cursor": cursor,
                },
                "next_actions": ["shadowdroid debug logpoint events"],
                "ts": crate::events::now_ts(),
            }));
            cursor = logpoint_page_high_water(&page, 0);
            stream_id = Some(page_stream_id);
            continue;
        }
        if stream_id.is_none() {
            stream_id = Some(page_stream_id);
        }

        let latest = cursor_field(&page, "latest_cursor").unwrap_or(cursor);
        if cursor > latest {
            crate::events::emit(&serde_json::json!({
                "type": "warning",
                "stream": "debug_logpoint_follow",
                "stage": "logpoint_follow",
                "code": "logpoint_cursor_reset",
                "msg": "the requested logpoint cursor is ahead of this Studio event stream; following from its live tail",
                "retryable": true,
                "detail": {"requested_cursor": cursor, "latest_cursor": latest},
                "next_actions": ["shadowdroid debug logpoint events"],
                "ts": crate::events::now_ts(),
            }));
            cursor = latest;
            continue;
        }

        if let Some(oldest) = cursor_field(&page, "oldest_cursor")
            && cursor.saturating_add(1) < oldest
        {
            crate::events::emit(&serde_json::json!({
                "type": "warning",
                "stream": "debug_logpoint_follow",
                "stage": "logpoint_follow",
                "code": "logpoint_cursor_gap",
                "msg": "logpoint events were evicted before this follower consumed them",
                "retryable": false,
                "detail": {
                    "after": cursor,
                    "oldest_cursor": oldest,
                    "latest_cursor": latest,
                    "missed_at_least": oldest.saturating_sub(cursor.saturating_add(1)),
                    "overflowed": page.get("overflowed").and_then(Value::as_bool).unwrap_or(false),
                    "evicted_total": cursor_field(&page, "evicted_total"),
                },
                "next_actions": [
                    "increase --limit or reduce logpoint hit rate",
                    "shadowdroid debug logpoint events --limit 200",
                ],
                "ts": crate::events::now_ts(),
            }));
        }

        evicted_total = cursor_field(&page, "evicted_total").unwrap_or(evicted_total);
        rate_limited_total =
            cursor_field(&page, "rate_limited_total").unwrap_or(rate_limited_total);
        if emit_logpoint_page(&page, &mut cursor, &mut emitted, args.max_events) {
            cursor = cursor_field(&page, "next_cursor").unwrap_or(cursor);
        }
    };

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    crate::events::emit_action(
        "debug_logpoint_follow",
        &serde_json::json!({
            "status": "stopped",
            "reason": reason,
            "events_emitted": emitted,
            "cursor": cursor,
            "stream_id": stream_id,
            "elapsed_ms": elapsed_ms,
            "evicted_total": evicted_total,
            "rate_limited_total": rate_limited_total,
        }),
    );
    Ok(())
}

fn emit_logpoint_page(
    page: &Value,
    cursor: &mut u64,
    emitted: &mut u64,
    max_events: Option<u64>,
) -> bool {
    let Some(events) = page.get("events").and_then(Value::as_array) else {
        return true;
    };
    for event in events {
        if max_events.is_some_and(|maximum| *emitted >= maximum) {
            return false;
        }
        crate::events::emit(event);
        *emitted = emitted.saturating_add(1);
        if let Some(sequence) = cursor_field(event, "seq") {
            *cursor = sequence;
        }
    }
    true
}

fn cursor_field(value: &Value, field: &str) -> Option<u64> {
    value
        .get(field)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

pub(crate) fn logpoint_page_high_water(page: &Value, fallback: u64) -> u64 {
    cursor_field(page, "latest_cursor")
        .or_else(|| cursor_field(page, "next_cursor"))
        .unwrap_or(fallback)
}

fn initial_logpoint_follow_cursor(page: &Value, replay_existing: bool, fallback: u64) -> u64 {
    if replay_existing {
        0
    } else {
        logpoint_page_high_water(page, fallback)
    }
}

pub(crate) fn logpoint_event_matches_package(event: &Value, expected: Option<&str>) -> bool {
    expected.is_none_or(|expected| {
        event
            .get("package")
            .and_then(Value::as_str)
            .is_some_and(|actual| actual == expected)
    })
}

pub(crate) fn logpoint_page_metadata(page: &Value) -> Value {
    const FIELDS: [&str; 8] = [
        "stream_id",
        "oldest_cursor",
        "latest_cursor",
        "next_cursor",
        "overflowed",
        "evicted_total",
        "rate_limited_total",
        "buffer_capacity",
    ];
    let mut metadata = serde_json::Map::new();
    for field in FIELDS {
        if let Some(value) = page.get(field) {
            metadata.insert(field.to_string(), value.clone());
        }
    }
    Value::Object(metadata)
}

fn required_logpoint_stream_id(page: &Value) -> Result<&str> {
    string_field(page, "stream_id")
        .filter(|stream_id| !stream_id.trim().is_empty())
        .ok_or_else(|| {
            crate::diagnostic::DiagnosticError::new(
                "debugger_bridge_protocol",
                "debugger",
                "Android Studio omitted the logpoint event stream id",
            )
            .retryable(true)
            .detail(serde_json::json!({"bridge_reply": page}))
            .next_actions([
                "update the ShadowDroid Android Studio plugin and CLI together",
                "shadowdroid debug logpoint events",
            ])
            .into()
        })
}

fn validate_logpoint_stream(
    page: &Value,
    expected_stream_id: Option<&str>,
    after: Option<u64>,
) -> Result<()> {
    let Some(expected_stream_id) = expected_stream_id else {
        return Ok(());
    };
    let actual_stream_id = required_logpoint_stream_id(page)?;
    if actual_stream_id == expected_stream_id {
        return Ok(());
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "logpoint_stream_changed",
        "debugger",
        "Android Studio restarted its logpoint event stream; the supplied cursor cannot be used",
    )
    .retryable(true)
    .detail(serde_json::json!({
        "expected_stream_id": expected_stream_id,
        "stream_id": actual_stream_id,
        "after": after,
        "latest_cursor": cursor_field(page, "latest_cursor"),
    }))
    .next_actions([
        "read `shadowdroid debug logpoint events` without --after to obtain the current stream_id and cursor",
        "restart the consumer at the current live tail rather than reusing the old numeric cursor",
    ])
    .into())
}

async fn continue_until(bridge: &BridgeClient, args: &ContinueUntilArgs) -> Result<Value> {
    let session_s = args.session.clone();
    let canonical_file = match &args.file {
        Some(path) => Some(canonicalize_for_bridge(path)?),
        None => None,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(args.timeout_ms);
    let mut resumes = 0u64;

    loop {
        control(
            bridge,
            session_action::RESUME,
            &SessionSelector {
                session: args.session.clone(),
            },
        )
        .await?;
        resumes += 1;

        loop {
            if std::time::Instant::now() >= deadline {
                return Ok(serde_json::json!({
                    "ok": false,
                    "type": "continue_until",
                    "timeout": true,
                    "resumes": resumes,
                    "session": args.session,
                }));
            }
            tokio::time::sleep(std::time::Duration::from_millis(args.poll_ms.max(25))).await;
            let status = bridge.get(route::STATUS, &[]).await?;
            if !selected_session_suspended(&status, args.session.as_deref(), bridge.device()) {
                continue;
            }
            let stack = bridge
                .get(
                    route::SESSION_STACK,
                    &[
                        (query::SESSION, session_s.as_deref()),
                        (query::LIMIT, Some("4")),
                    ],
                )
                .await?;
            let location_matches = match (&canonical_file, args.line) {
                (Some(file), Some(line)) => stack_top_matches(&stack, file, line)?,
                _ => true,
            };
            let condition_matches = match &args.condition {
                Some(condition) => {
                    let eval = bridge
                        .get(
                            route::SESSION_EVALUATE,
                            &[
                                (query::SESSION, session_s.as_deref()),
                                (query::EXPRESSION, Some(condition.as_str())),
                                (query::DEPTH, Some("0")),
                            ],
                        )
                        .await?;
                    eval_truthy(&eval)
                }
                None => true,
            };
            if location_matches && condition_matches {
                return Ok(serde_json::json!({
                    "ok": true,
                    "type": "continue_until",
                    "matched": true,
                    "resumes": resumes,
                    "status": status,
                    "stack": stack,
                }));
            }
            break;
        }
    }
}

fn selected_session_suspended(
    status: &Value,
    selected: Option<&str>,
    device: Option<&str>,
) -> bool {
    status
        .get("sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| {
            // Mirror the plugin's selectSession precedence: explicit stable id
            // (or legacy current index), then device, then the first session.
            if let Some(selector) = selected {
                sessions
                    .iter()
                    .find(|session| session.get("id").and_then(Value::as_str) == Some(selector))
                    .or_else(|| {
                        selector
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| sessions.get(index))
                    })
            } else if let Some(dev) = device {
                sessions
                    .iter()
                    .find(|session| session_matches_device(session, dev))
            } else {
                sessions.first()
            }
        })
        .and_then(|session| session.get("suspended"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether a status `sessions[]` entry is on `device` (serial or AVD name),
/// using the `device` block the plugin now embeds in each session payload.
fn session_matches_device(session: &Value, device: &str) -> bool {
    let block = session.get("device");
    let serial = block.and_then(|d| d.get("serial")).and_then(Value::as_str);
    let avd = block.and_then(|d| d.get("avd")).and_then(Value::as_str);
    serial == Some(device) || avd == Some(device)
}

fn stack_top_matches(stack: &Value, file: &str, line: u32) -> Result<bool> {
    let Some(frame) = stack
        .get("frames")
        .and_then(Value::as_array)
        .and_then(|frames| frames.first())
    else {
        return Ok(false);
    };
    let frame_line = debugger_frame_line(frame)?;
    if frame_line != line {
        return Ok(false);
    }
    let frame_file = frame
        .get("file")
        .or_else(|| frame.get("source"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(frame_file == file || file.ends_with(frame_file))
}

fn debugger_frame_line(frame: &Value) -> Result<u32> {
    let value = frame.get("line").ok_or_else(|| {
        debugger_frame_line_error(None, "the debugger stack frame omitted `line`")
    })?;
    let raw = value.as_u64().ok_or_else(|| {
        debugger_frame_line_error(
            Some(value),
            "the debugger stack frame `line` is not a non-negative integer",
        )
    })?;
    u32::try_from(raw).map_err(|_| {
        debugger_frame_line_error(
            Some(value),
            "the debugger stack frame `line` exceeds the supported u32 range",
        )
    })
}

fn debugger_frame_line_error(value: Option<&Value>, message: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new("debugger_bridge_protocol", "debugger", message)
        .retryable(true)
        .detail(serde_json::json!({"field": "line", "value": value}))
        .next_actions([
            "refresh the suspended session and retry",
            "update the ShadowDroid Android Studio plugin if the malformed frame persists",
        ])
        .into()
}

fn eval_truthy(eval: &Value) -> bool {
    let Some(result) = eval.get("result") else {
        return false;
    };
    let value = result.get("value").and_then(Value::as_str);
    match value {
        Some("true") => true,
        Some("false") | Some("0") | Some("null") | None => false,
        Some(other) => !other.is_empty(),
    }
}

async fn control(
    bridge: &BridgeClient,
    action: &'static str,
    selector: &SessionSelector,
) -> Result<Value> {
    let session_s = selector.session.clone();
    let params = [
        (query::ACTION, Some(action)),
        (query::SESSION, session_s.as_deref()),
    ];
    bridge.get(route::SESSION_CONTROL, &params).await
}

fn emit(value: &Value) -> Result<()> {
    crate::events::emit_result(value);
    Ok(())
}

fn read_error_json(kind: &str, err: anyhow::Error) -> Value {
    serde_json::json!({
        "ok": false,
        "type": kind,
        "error": err.to_string(),
    })
}

/// Machine-readable failure code a bridge error reply may carry alongside the
/// human `error` string (e.g. `invalid_expression` from breakpoint routes).
fn bridge_error_code(reply: &Value) -> Option<&str> {
    reply.get("error_code").and_then(Value::as_str)
}

fn layout_conflict_session_id(reply: &Value) -> Option<&str> {
    reply
        .get("session_id")
        .or_else(|| reply.get("debug_session_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            ["conflicting_session", "debug_session", "session"]
                .into_iter()
                .find_map(|field| {
                    reply
                        .get(field)
                        .and_then(|session| session.get("id"))
                        .and_then(Value::as_str)
                })
        })
        .or_else(|| {
            reply
                .get("detail")
                .and_then(|detail| detail.get("session"))
                .and_then(|session| session.get("id"))
                .and_then(Value::as_str)
        })
}

pub(crate) fn layout_debugger_conflict_diagnostic(
    bridge_reply: Value,
    context: Value,
) -> crate::diagnostic::DiagnosticError {
    let message = bridge_reply
        .get("error")
        .or_else(|| bridge_reply.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or(
            "Android Studio Layout Inspector cannot inspect a process with a matching active debugger session",
        )
        .to_string();
    let mut detail = context
        .as_object()
        .cloned()
        .unwrap_or_else(|| serde_json::Map::from_iter([("context".to_string(), context)]));
    let session_id = layout_conflict_session_id(&bridge_reply).map(str::to_string);
    detail.insert("bridge_reply".into(), bridge_reply);

    let mut next_actions = vec!["shadowdroid debug sessions".to_string()];
    if let Some(session_id) = session_id {
        next_actions.push(format!(
            "shadowdroid debug stop --session {}",
            crate::events::shell_token(&session_id)
        ));
    } else {
        next_actions.push(
            "stop the debugger session matching detail.bridge_reply with `shadowdroid debug stop --session <id>`"
                .to_string(),
        );
    }
    next_actions.push(
        "retry the layout command after the matching debugger session has stopped".to_string(),
    );

    crate::diagnostic::DiagnosticError::new("layout_debugger_conflict", "layout", message)
        .retryable(true)
        .detail(Value::Object(detail))
        .next_actions(next_actions)
}

fn canonicalize_for_bridge(path: &Path) -> Result<String> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("source file not found: {}", path.display()))?;
    Ok(canonical.display().to_string())
}

pub(crate) struct BridgeClient {
    base_url: String,
    http: reqwest::Client,
    /// The target device serial, if known. Auto-appended to session-scoped
    /// routes so the global `--device` picks the matching debug session when
    /// several devices are debugged in one Studio. `None` for host-only callers.
    device: Option<String>,
}

impl BridgeClient {
    pub(crate) fn new(explicit_url: Option<&str>) -> Result<Self> {
        Self::with_device(explicit_url, None)
    }

    pub(crate) fn with_device(explicit_url: Option<&str>, device: Option<&str>) -> Result<Self> {
        Self::with_device_and_timeout(
            explicit_url,
            device,
            Duration::from_millis(DEFAULT_BRIDGE_TIMEOUT_MS),
        )
    }

    pub(crate) fn with_timeout(
        explicit_url: Option<&str>,
        request_timeout: Duration,
    ) -> Result<Self> {
        Self::with_device_and_timeout(explicit_url, None, request_timeout)
    }

    fn with_device_and_timeout(
        explicit_url: Option<&str>,
        device: Option<&str>,
        request_timeout: Duration,
    ) -> Result<Self> {
        let base_url = resolve_url(explicit_url)?;
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .connect_timeout(Duration::from_millis(2_000))
            .build()
            .context("creating debugger bridge HTTP client")?;
        Ok(Self {
            base_url,
            http,
            device: device.map(str::to_string),
        })
    }

    pub(crate) fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    pub(crate) async fn read_logpoint_events(
        &self,
        after: Option<u64>,
        limit: u32,
        timeout_ms: u32,
        filters: &LogpointEventFilters,
    ) -> Result<Value> {
        let after_s = after.map(|value| value.to_string());
        let limit_s = limit.max(1).to_string();
        let timeout_ms_s = timeout_ms.to_string();
        let params = [
            (query::AFTER, after_s.as_deref()),
            (query::LIMIT, Some(limit_s.as_str())),
            (query::TIMEOUT_MS, Some(timeout_ms_s.as_str())),
            (query::PROJECT, filters.project.as_deref()),
            (query::SESSION, filters.session.as_deref()),
            (query::ID, filters.breakpoint_id.as_deref()),
            (query::OWNER, filters.owner.as_deref()),
        ];
        self.get(route::LOGPOINT_EVENTS, &params).await
    }

    pub(crate) async fn get(&self, path: &str, params: &[(&str, Option<&str>)]) -> Result<Value> {
        let url = self.url(path, params);
        let response = self.http.get(&url).send().await.map_err(|error| {
            crate::diagnostic::DiagnosticError::new(
                "studio_bridge_unreachable",
                "debugger",
                format!(
                    "cannot reach the Android Studio debugger bridge at {}: {error}",
                    self.base_url
                ),
            )
            .retryable(true)
            .detail(serde_json::json!({"base_url": self.base_url, "route": path}))
            .next_actions([
                "run `shadowdroid studio status --json`",
                "start Android Studio with the plugin installed, or pass --studio-url, then retry",
            ])
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            crate::diagnostic::DiagnosticError::new(
                "studio_bridge_response",
                "debugger",
                format!("failed reading the Android Studio debugger bridge response: {error}"),
            )
            .retryable(true)
            .detail(serde_json::json!({"route": path, "status": status.as_u16()}))
            .next_actions(["run `shadowdroid studio status --json`, then retry"])
        })?;
        let value: Value = serde_json::from_str(&body).map_err(|error| {
            crate::diagnostic::DiagnosticError::new(
                "studio_bridge_protocol",
                "debugger",
                format!("Android Studio debugger bridge returned invalid JSON: {error}"),
            )
            .detail(serde_json::json!({
                "route": path,
                "status": status.as_u16(),
                "body_preview": body.chars().take(512).collect::<String>(),
            }))
            .next_actions([
                "run `shadowdroid studio status --json` and verify plugin/CLI versions",
                "restart Android Studio after updating the plugin, then retry",
            ])
        })?;
        if bridge_error_code(&value) == Some("layout_debugger_conflict") {
            return Err(layout_debugger_conflict_diagnostic(
                value,
                serde_json::json!({
                    "route": path,
                    "status": status.as_u16(),
                }),
            )
            .into());
        }
        if status == reqwest::StatusCode::NOT_FOUND && path.starts_with("/v1/logpoints") {
            return Err(crate::diagnostic::DiagnosticError::new(
                "studio_plugin_upgrade_required",
                "debugger",
                "the installed Android Studio plugin does not support structured logpoints",
            )
            .detail(serde_json::json!({
                "route": path,
                "status": status.as_u16(),
                "bridge_reply": value,
            }))
            .next_actions([
                "build or download the matching ShadowDroid Android Studio plugin",
                "shadowdroid studio install --plugin <shadowdroid-plugin.zip>",
                "restart Android Studio after installing the plugin, then retry",
            ])
            .into());
        }
        if !status.is_success() {
            let message = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            if bridge_error_code(&value) == Some("invalid_expression") {
                let is_logpoint = path.starts_with("/v1/logpoints");
                let subject = if is_logpoint {
                    "logpoint expression"
                } else {
                    "breakpoint expression"
                };
                let forced_hit_action = if is_logpoint {
                    "re-run with --force to set it anyway; a later evaluation failure is captured as a structured logpoint event and execution remains non-suspending"
                } else {
                    "re-run with --force to set it anyway; if it then fails at a hit, the session pauses and the error appears in `shadowdroid debug breakpoints`"
                };
                return Err(crate::diagnostic::DiagnosticError::new(
                    "debug_expression_invalid",
                    "debugger",
                    format!("Android Studio rejected the {subject}: {message}"),
                )
                .detail(serde_json::json!({
                    "route": path,
                    "status": status.as_u16(),
                    "bridge_reply": value,
                }))
                .next_actions([
                    "fix the expression for the breakpoint's language (Kotlin files evaluate Kotlin syntax, Java files Java) — detail.bridge_reply.problems lists what failed",
                    "check names with `shadowdroid debug variables` on a suspended frame",
                    forced_hit_action,
                ])
                .into());
            }
            if path.starts_with("/v1/logpoints")
                && let Some(
                    code @ ("logpoint_conflict"
                    | "logpoint_not_owned"
                    | "logpoint_owner_mismatch"
                    | "logpoint_ownership_changed"
                    | "logpoint_follow_limit"),
                ) = bridge_error_code(&value)
            {
                return Err(crate::diagnostic::DiagnosticError::new(
                    code,
                    "debugger",
                    message,
                )
                .retryable(code == "logpoint_follow_limit")
                .detail(serde_json::json!({
                    "route": path,
                    "status": status.as_u16(),
                    "bridge_reply": value,
                }))
                .next_actions([
                    "shadowdroid debug logpoint list",
                    "inspect the existing logpoint owner and choose a different source line or owner",
                ])
                .into());
            }
            return Err(crate::diagnostic::DiagnosticError::new(
                "debugger_bridge_rejected",
                "debugger",
                format!(
                    "Android Studio debugger bridge rejected the request (HTTP {status}): {message}"
                ),
            )
            .detail(serde_json::json!({
                "route": path,
                "status": status.as_u16(),
                "bridge_reply": value,
            }))
            .next_actions([
                "inspect detail.bridge_reply and select a valid project/session when required",
                "run `shadowdroid debug sessions` or `shadowdroid studio status --json`",
            ])
            .into());
        }
        Ok(value)
    }

    fn url(&self, path: &str, params: &[(&str, Option<&str>)]) -> String {
        let mut url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let mut pairs: Vec<String> = params
            .iter()
            .filter_map(|(key, value)| value.map(|v| (*key, v)))
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect();
        // Session-scoped routes default to the session on this client's device
        // when the caller didn't pin an explicit `session=` index. Harmless when
        // a session index IS given — the plugin prefers the index.
        if let Some(device) = &self.device {
            let already = params.iter().any(|(key, _)| *key == query::DEVICE);
            if route_is_session_scoped(path) && !already {
                pairs.push(format!(
                    "{}={}",
                    urlencoding::encode(query::DEVICE),
                    urlencoding::encode(device)
                ));
            }
        }
        if !pairs.is_empty() {
            url.push('?');
            url.push_str(&pairs.join("&"));
        }
        url
    }
}

/// Routes that operate on a single debug session, so the client's `--device`
/// can pick the matching one. The session-control/stack/variables/evaluate/
/// inspect/coroutines endpoints all live under `/v1/session/`; watch values and
/// logpoint list/event reads also accept device/session filtering.
fn route_is_session_scoped(path: &str) -> bool {
    path.starts_with("/v1/session/")
        || path == route::WATCHES
        || path == route::LOGPOINTS
        || path == route::LOGPOINT_EVENTS
}

fn resolve_url(explicit_url: Option<&str>) -> Result<String> {
    if let Some(url) = explicit_url
        && !url.trim().is_empty()
    {
        return Ok(url.trim().to_string());
    }
    if let Some(url) = registry_url()? {
        return Ok(url);
    }
    Ok(studio_contract::DEFAULT_URL.to_string())
}

fn registry_url() -> Result<Option<String>> {
    let path = shadowdroid_home()?.join(studio_contract::REGISTRY_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading debugger bridge registry {}", path.display()))?;
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing debugger bridge registry {}", path.display()))?;
    Ok(value
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
        .map(|url| url.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const URL: &str = "http://127.0.0.1:50576";

    #[test]
    fn route_scoping_classification() {
        assert!(route_is_session_scoped(route::SESSION_STACK));
        assert!(route_is_session_scoped(route::SESSION_CONTROL));
        assert!(route_is_session_scoped(route::SESSION_COROUTINES_FLOW));
        assert!(route_is_session_scoped(route::WATCHES));
        assert!(route_is_session_scoped(route::LOGPOINTS));
        assert!(route_is_session_scoped(route::LOGPOINT_EVENTS));
        assert!(!route_is_session_scoped(route::STATUS));
        assert!(!route_is_session_scoped(route::SESSIONS));
        assert!(!route_is_session_scoped(route::WATCHES_ADD));
        assert!(!route_is_session_scoped(route::ATTACH));
    }

    #[test]
    fn debugger_frame_lines_are_range_checked() {
        let frame = json!({"file": "Main.kt", "line": 42});
        assert_eq!(debugger_frame_line(&frame).unwrap(), 42);

        let frame = json!({"file": "Main.kt", "line": u64::from(u32::MAX) + 1});
        let error = debugger_frame_line(&frame).unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&error),
            "debugger_bridge_protocol"
        );
        assert!(error.to_string().contains("u32 range"));
    }

    #[test]
    fn session_routes_carry_the_clients_device() {
        let bridge = BridgeClient::with_device(Some(URL), Some("emulator-5556")).unwrap();
        let u = bridge.url(route::SESSION_STACK, &[(query::LIMIT, Some("4"))]);
        assert!(u.contains("limit=4"));
        assert!(
            u.contains("device=emulator-5556"),
            "session route should carry device: {u}"
        );
        assert!(
            bridge
                .url(route::WATCHES, &[])
                .contains("device=emulator-5556")
        );
        assert!(
            bridge
                .url(route::LOGPOINT_EVENTS, &[(query::AFTER, Some("17"))])
                .contains("device=emulator-5556")
        );
    }

    #[test]
    fn non_session_routes_omit_the_device() {
        let bridge = BridgeClient::with_device(Some(URL), Some("emulator-5556")).unwrap();
        assert!(!bridge.url(route::STATUS, &[]).contains("device="));
        assert!(!bridge.url(route::SESSIONS, &[]).contains("device="));
        assert!(
            !bridge
                .url(route::WATCHES_ADD, &[(query::EXPRESSION, Some("x"))])
                .contains("device=")
        );
    }

    #[test]
    fn explicit_device_param_is_not_duplicated() {
        let bridge = BridgeClient::with_device(Some(URL), Some("dev-A")).unwrap();
        let u = bridge.url(route::SESSION_CONTROL, &[(query::DEVICE, Some("dev-B"))]);
        assert_eq!(
            u.matches("device=").count(),
            1,
            "no duplicate device param: {u}"
        );
        assert!(u.contains("device=dev-B"));
    }

    #[test]
    fn no_device_means_no_append() {
        let bridge = BridgeClient::new(Some(URL)).unwrap();
        assert!(!bridge.url(route::SESSION_STACK, &[]).contains("device="));
        assert_eq!(bridge.device(), None);
    }

    #[test]
    fn suspension_selects_session_by_device() {
        let status = json!({"sessions": [
            {"id": "session_1", "index": 0, "suspended": false, "device": {"serial": "emulator-5554", "avd": "Pixel_9"}},
            {"id": "session_2", "index": 1, "suspended": true,  "device": {"serial": "emulator-5556", "avd": "Pixel_9_Pro_XL"}},
        ]});
        // by serial
        assert!(selected_session_suspended(
            &status,
            None,
            Some("emulator-5556")
        ));
        assert!(!selected_session_suspended(
            &status,
            None,
            Some("emulator-5554")
        ));
        // by avd name
        assert!(selected_session_suspended(
            &status,
            None,
            Some("Pixel_9_Pro_XL")
        ));
        // explicit stable id wins over device
        assert!(!selected_session_suspended(
            &status,
            Some("session_1"),
            Some("emulator-5556")
        ));
        // A current numeric index is still accepted by the bridge contract.
        assert!(selected_session_suspended(&status, Some("1"), None));
        // unknown device matches nothing (does not fall back to first)
        assert!(!selected_session_suspended(
            &status,
            None,
            Some("emulator-9999")
        ));
        // no index, no device -> first session
        assert!(!selected_session_suspended(&status, None, None));
    }

    #[test]
    fn bridge_error_codes_are_extracted_from_replies() {
        let reply = json!({"ok": false, "error": "invalid_expression: cannot resolve 'foo'", "error_code": "invalid_expression"});
        assert_eq!(bridge_error_code(&reply), Some("invalid_expression"));
        assert_eq!(
            bridge_error_code(&json!({"ok": false, "error": "nope"})),
            None
        );
        assert_eq!(bridge_error_code(&json!({"error_code": 7})), None);
    }

    #[test]
    fn cursor_fields_accept_json_numbers_and_numeric_strings() {
        assert_eq!(
            cursor_field(&json!({"next_cursor": 42}), "next_cursor"),
            Some(42)
        );
        assert_eq!(
            cursor_field(&json!({"next_cursor": "43"}), "next_cursor"),
            Some(43)
        );
        assert_eq!(
            cursor_field(&json!({"next_cursor": "invalid"}), "next_cursor"),
            None
        );
    }

    #[test]
    fn resumed_logpoint_cursor_is_bound_to_its_stream() {
        let page = json!({
            "stream_id": "stream-b",
            "latest_cursor": 12,
            "events": [{"seq": 12, "message": "must not leak"}],
        });
        let error = validate_logpoint_stream(&page, Some("stream-a"), Some(11)).unwrap_err();
        assert_eq!(crate::cli::error_code_of(&error), "logpoint_stream_changed");
        let diagnostic = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::diagnostic::DiagnosticError>())
            .unwrap();
        assert_eq!(diagnostic.detail["expected_stream_id"], "stream-a");
        assert_eq!(diagnostic.detail["stream_id"], "stream-b");
        assert_eq!(diagnostic.detail["after"], 11);
        assert!(diagnostic.retryable);

        validate_logpoint_stream(&page, Some("stream-b"), Some(11)).unwrap();
        assert_eq!(logpoint_page_high_water(&page, 0), 12);
    }

    #[test]
    fn scoped_logpoint_events_require_an_exact_package() {
        let event = json!({"type": "logpoint", "package": "com.example.target"});
        assert!(logpoint_event_matches_package(
            &event,
            Some("com.example.target")
        ));
        assert!(!logpoint_event_matches_package(
            &event,
            Some("com.example.other")
        ));
        assert!(!logpoint_event_matches_package(
            &json!({"type": "logpoint"}),
            Some("com.example.target")
        ));
        assert!(logpoint_event_matches_package(&event, None));
    }

    #[test]
    fn overflow_warning_metadata_cannot_leak_mixed_package_events() {
        let page = json!({
            "stream_id": "stream-a",
            "oldest_cursor": 7,
            "latest_cursor": 12,
            "next_cursor": 12,
            "overflowed": true,
            "evicted_total": 6,
            "rate_limited_total": 2,
            "buffer_capacity": 512,
            "events": [
                {
                    "type": "logpoint",
                    "package": "com.example.target",
                    "message": "target-value",
                },
                {
                    "type": "logpoint",
                    "package": "com.example.other",
                    "message": "OTHER_PACKAGE_SECRET",
                },
            ],
        });

        let metadata = logpoint_page_metadata(&page);
        assert_eq!(metadata["stream_id"], "stream-a");
        assert_eq!(metadata["overflowed"], true);
        assert_eq!(metadata["evicted_total"], 6);
        assert!(metadata.get("events").is_none());
        let rendered = serde_json::to_string(&metadata).unwrap();
        assert!(!rendered.contains("com.example.other"));
        assert!(!rendered.contains("OTHER_PACKAGE_SECRET"));
        assert!(!rendered.contains("com.example.target"));
        assert!(!rendered.contains("target-value"));
    }

    #[test]
    fn replay_existing_starts_at_origin_while_live_follow_starts_at_tail() {
        let page = json!({"latest_cursor": 120, "next_cursor": 120});
        assert_eq!(initial_logpoint_follow_cursor(&page, true, 9), 0);
        assert_eq!(initial_logpoint_follow_cursor(&page, false, 9), 120);
    }

    #[tokio::test]
    async fn one_shot_logpoint_events_reject_a_cursor_from_another_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let reply = json!({
            "ok": true,
            "stream_id": "stream-new",
            "latest_cursor": 3,
            "events": [{"type": "logpoint", "seq": 3, "message": "wrong stream"}],
        })
        .to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let command = DebuggerCmd::Logpoint(LogpointCmd::Events(LogpointEventsArgs {
            after: Some(2),
            stream_id: Some("stream-old".to_string()),
            limit: 100,
            filters: LogpointFilterArgs::default(),
        }));
        let error = run(&command, None, Some(&format!("http://{address}")))
            .await
            .unwrap_err();
        assert_eq!(crate::cli::error_code_of(&error), "logpoint_stream_changed");
    }

    #[test]
    fn layout_debugger_conflict_is_typed_and_names_the_exact_session_stop() {
        let reply = json!({
            "ok": false,
            "error_code": "layout_debugger_conflict",
            "error": "stop debugger session before attaching Layout Inspector",
            "conflicting_session": {
                "id": "session_7",
                "package": "com.example.app",
                "pid": 42
            }
        });

        let error = layout_debugger_conflict_diagnostic(
            reply.clone(),
            json!({"route": "/v1/layout/snapshot", "status": 409}),
        );
        assert_eq!(error.code, "layout_debugger_conflict");
        assert_eq!(error.stage, "layout");
        assert!(error.retryable);
        assert_eq!(error.detail["bridge_reply"], reply);
        assert_eq!(error.detail["status"], 409);
        assert!(
            error
                .next_actions
                .iter()
                .any(|action| action == "shadowdroid debug stop --session session_7")
        );
    }

    #[tokio::test]
    async fn bridge_http_conflict_reply_bypasses_generic_rejection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let reply = json!({
            "ok": false,
            "available": false,
            "type": "layout_snapshot",
            "error_code": "layout_debugger_conflict",
            "error": "stop the matching debugger session before retrying",
            "session": {"id": "session_9", "package": "com.example.app", "pid": 42}
        })
        .to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let bridge = BridgeClient::new(Some(&format!("http://{address}"))).unwrap();
        let error = bridge.get(route::LAYOUT_SNAPSHOT, &[]).await.unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&error),
            "layout_debugger_conflict"
        );
        assert_eq!(crate::cli::error_stage_of(&error), "layout");
        let diagnostic = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::diagnostic::DiagnosticError>())
            .unwrap();
        assert_eq!(diagnostic.detail["status"], 409);
        assert_eq!(
            diagnostic.detail["bridge_reply"]["session"]["id"],
            "session_9"
        );
    }

    #[tokio::test]
    async fn missing_logpoint_route_requests_a_plugin_upgrade() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let reply =
            json!({"ok": false, "error": "not_found", "path": route::LOGPOINT_EVENTS}).to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let bridge = BridgeClient::new(Some(&format!("http://{address}"))).unwrap();
        let error = bridge
            .read_logpoint_events(None, 10, 0, &LogpointEventFilters::default())
            .await
            .unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&error),
            "studio_plugin_upgrade_required"
        );
        assert_eq!(crate::cli::error_stage_of(&error), "debugger");
    }

    #[tokio::test]
    async fn logpoint_conflict_preserves_the_bridge_error_code() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let reply = json!({
            "ok": false,
            "error": "an unowned breakpoint already exists at Foo.kt:42",
            "error_code": "logpoint_conflict",
            "existing_breakpoint_id": "bp_manual"
        })
        .to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let bridge = BridgeClient::new(Some(&format!("http://{address}"))).unwrap();
        let error = bridge.get(route::LOGPOINT_ADD, &[]).await.unwrap_err();
        assert_eq!(crate::cli::error_code_of(&error), "logpoint_conflict");
        assert_eq!(crate::cli::error_stage_of(&error), "debugger");
    }

    #[tokio::test]
    async fn forced_logpoint_expression_failure_stays_non_suspending() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let reply = json!({
            "ok": false,
            "error": "cannot resolve counter",
            "error_code": "invalid_expression",
        })
        .to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let bridge = BridgeClient::new(Some(&format!("http://{address}"))).unwrap();
        let error = bridge.get(route::LOGPOINT_ADD, &[]).await.unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&error),
            "debug_expression_invalid"
        );
        let diagnostic = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::diagnostic::DiagnosticError>())
            .unwrap();
        assert!(diagnostic.message.contains("logpoint expression"));
        let force_action = diagnostic.next_actions.last().unwrap();
        assert!(force_action.contains("remains non-suspending"));
        assert!(!force_action.contains("session pauses"));
    }

    #[tokio::test]
    async fn inspect_without_expression_or_handle_is_a_typed_failure() {
        let command = DebuggerCmd::Inspect(InspectArgs {
            expression: None,
            handle: None,
            path: None,
            session: None,
            thread: None,
            frame: None,
            depth: 1,
            max_fields: 64,
            max_array_items: 32,
            timeout_ms: 100,
        });
        let error = run(&command, None, Some(URL)).await.unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&error),
            "debug_inspect_target_required"
        );
    }
}
