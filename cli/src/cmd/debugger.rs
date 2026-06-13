//! Host-side Android Studio debugger bridge commands.
//!
//! These commands talk to the ShadowDroid Android Studio plugin over its local
//! loopback HTTP bridge. They do not need the on-device ShadowDroid server.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cmd::studio_contract::{self, query, route, session_action};

const DEFAULT_BRIDGE_TIMEOUT_MS: u64 = 10_000;

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

#[derive(Args)]
pub struct SessionSelector {
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
}

#[derive(Args)]
pub struct StackArgs {
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
    /// Maximum number of frames per stack.
    #[arg(long, default_value_t = 64)]
    pub limit: u32,
    /// Debugger manager request timeout.
    #[arg(long, default_value_t = 2500)]
    pub timeout_ms: u32,
}

#[derive(Args)]
pub struct VariablesArgs {
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
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
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
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
pub struct ContinueUntilArgs {
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
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
        #[arg(long)]
        id: String,
    },
    /// Remove all watches.
    Clear,
}

#[derive(Args)]
pub struct WatchListArgs {
    /// Debug session index from `debug sessions`.
    #[arg(long)]
    pub session: Option<usize>,
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

pub async fn run(cmd: &DebuggerCmd, studio_url: Option<&str>) -> Result<()> {
    let bridge = BridgeClient::new(studio_url)?;
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
            configuration,
            dialog,
        } => {
            let pid_s = pid.map(|pid| pid.to_string());
            let dialog_s = dialog.to_string();
            let params = [
                (query::PROJECT, project.as_deref()),
                (query::PACKAGE, package.as_deref()),
                (query::PID, pid_s.as_deref()),
                (query::DEVICE, device.as_deref()),
                (query::DEBUGGER, debugger.as_deref()),
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
        }) => {
            let canonical = canonicalize_for_bridge(file)?;
            let line_s = line.to_string();
            let enabled_s = (!*disabled).to_string();
            let temporary_s = temporary.to_string();
            let clear_condition_s = clear_condition.to_string();
            let params = [
                (query::FILE, Some(canonical.as_str())),
                (query::LINE, Some(line_s.as_str())),
                (query::PROJECT, project.as_deref()),
                (query::ENABLED, Some(enabled_s.as_str())),
                (query::TEMPORARY, Some(temporary_s.as_str())),
                (query::CONDITION, condition.as_deref()),
                (query::CLEAR_CONDITION, Some(clear_condition_s.as_str())),
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
            let session_s = args.session.map(|s| s.to_string());
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
            let session_s = args.session.map(|s| s.to_string());
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
            let session_s = args.session.map(|s| s.to_string());
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
            let session_s = args.session.map(|s| s.to_string());
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
                std::time::Duration::from_millis(args.timeout_ms as u64),
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
            let session_s = args.session.map(|s| s.to_string());
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
    emit(&value)?;
    Ok(())
}

async fn continue_until(bridge: &BridgeClient, args: &ContinueUntilArgs) -> Result<Value> {
    let session_s = args.session.map(|s| s.to_string());
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
                session: args.session,
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
            if !selected_session_suspended(&status, args.session) {
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
                (Some(file), Some(line)) => stack_top_matches(&stack, file, line),
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

fn selected_session_suspended(status: &Value, selected: Option<usize>) -> bool {
    status
        .get("sessions")
        .and_then(Value::as_array)
        .and_then(|sessions| {
            selected
                .and_then(|index| sessions.get(index))
                .or_else(|| sessions.first())
        })
        .and_then(|session| session.get("suspended"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn stack_top_matches(stack: &Value, file: &str, line: u32) -> bool {
    let Some(frame) = stack
        .get("frames")
        .and_then(Value::as_array)
        .and_then(|frames| frames.first())
    else {
        return false;
    };
    let frame_line = frame.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
    if frame_line != line {
        return false;
    }
    let frame_file = frame
        .get("file")
        .or_else(|| frame.get("source"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    frame_file == file || file.ends_with(frame_file)
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
    let session_s = selector.session.map(|s| s.to_string());
    let params = [
        (query::ACTION, Some(action)),
        (query::SESSION, session_s.as_deref()),
    ];
    bridge.get(route::SESSION_CONTROL, &params).await
}

fn emit(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn read_error_json(kind: &str, err: anyhow::Error) -> Value {
    serde_json::json!({
        "ok": false,
        "type": kind,
        "error": err.to_string(),
    })
}

fn canonicalize_for_bridge(path: &Path) -> Result<String> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("source file not found: {}", path.display()))?;
    Ok(canonical.display().to_string())
}

pub(crate) struct BridgeClient {
    base_url: String,
    http: reqwest::Client,
}

impl BridgeClient {
    pub(crate) fn new(explicit_url: Option<&str>) -> Result<Self> {
        let base_url = resolve_url(explicit_url)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(DEFAULT_BRIDGE_TIMEOUT_MS))
            .connect_timeout(Duration::from_millis(2_000))
            .build()
            .context("creating debugger bridge HTTP client")?;
        Ok(Self { base_url, http })
    }

    pub(crate) async fn get(&self, path: &str, params: &[(&str, Option<&str>)]) -> Result<Value> {
        let url = self.url(path, params);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "connecting to Android Studio debugger bridge at {}. Install/start the ShadowDroid Android Studio plugin or pass --studio-url.",
                    self.base_url
                )
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("reading debugger bridge response")?;
        let value: Value = serde_json::from_str(&body)
            .with_context(|| format!("debugger bridge returned non-JSON response: {body}"))?;
        if !status.is_success() {
            let message = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            bail!("debugger bridge request failed (HTTP {status}): {message}");
        }
        Ok(value)
    }

    fn url(&self, path: &str, params: &[(&str, Option<&str>)]) -> String {
        let mut url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let query = params
            .iter()
            .filter_map(|(key, value)| value.map(|v| (*key, v)))
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        url
    }
}

fn resolve_url(explicit_url: Option<&str>) -> Result<String> {
    if let Some(url) = explicit_url {
        if !url.trim().is_empty() {
            return Ok(url.trim().to_string());
        }
    }
    if let Some(url) = registry_url()? {
        return Ok(url);
    }
    Ok(studio_contract::DEFAULT_URL.to_string())
}

fn registry_url() -> Result<Option<String>> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    let path = PathBuf::from(home).join(studio_contract::REGISTRY_PATH);
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
