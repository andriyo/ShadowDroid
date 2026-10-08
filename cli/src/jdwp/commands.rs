//! `debug <verb> --backend jdwp`: the CLI side of the standalone debugger.
//! Each verb resolves a daemon from the registry (attach spawns one) and
//! makes one JSON-RPC call; the reply is emitted with `"backend": "jdwp"`.
//! Verbs the backend does not serve yet fail with `unsupported_by_backend`
//! instead of silently falling back to Studio.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value as Json, json};

use super::breakpoints::{BreakpointOptions, BreakpointUpdate, SuspendKind};
pub use super::control::CallError;
use super::control::{self, Ready};
use super::daemon::{InitialBreakpoints, InitialException, InitialLine};
pub use super::paths::RegistryEntry;
use super::paths::{self};
use super::resolve::{self, LocateError, SourceTarget};
use super::session::RpcError;
use super::transport;
use crate::cmd::debugger::{
    BreakCmd, DebugMode, DebuggerCmd, LaunchArgs, LogpointCmd, LogpointEventFilters,
    LogpointReader, SuspendArg, follow_logpoint_events, validate_logpoint_stream,
};
use crate::diagnostic::DiagnosticError;

/// Minimum API level: OpenJDK libjdwp under ART (design §4.3).
const MIN_API_LEVEL: u32 = 28;
/// Headroom over a verb's own `--timeout-ms` for the socket round trip.
const CALL_HEADROOM: Duration = Duration::from_secs(5);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const ATTACH_READY_TIMEOUT: Duration = Duration::from_secs(25);

fn diagnostic(error: RpcError) -> anyhow::Error {
    let mut detail = error.detail;
    if !detail.is_object() {
        detail = json!({"value": detail});
    }
    detail["backend"] = json!("jdwp");
    DiagnosticError::new(error.code, "debugger", error.message)
        .retryable(error.retryable)
        .detail(detail)
        .next_actions(error.next_actions)
        .into()
}

fn unsupported(verb: &str) -> anyhow::Error {
    DiagnosticError::new(
        "unsupported_by_backend",
        "debugger",
        format!("`debug {verb}` is not available on the jdwp backend yet"),
    )
    .detail(json!({"backend": "jdwp", "verb": verb}))
    .next_actions([format!("shadowdroid debug {verb} --backend studio")])
    .into()
}

fn emit(mut value: Json) {
    if let Json::Object(map) = &mut value {
        map.insert("ok".into(), json!(true));
        map.insert("backend".into(), json!("jdwp"));
    }
    crate::events::emit_result(&value);
}

pub struct JdwpContext<'a> {
    /// Explicit or resolved device serial; `None` lets session lookup span
    /// every device's registry.
    pub serial: Option<&'a str>,
    pub project_root: Option<&'a Path>,
}

/// Run one `debug` verb on the jdwp backend.
pub async fn run(cmd: &DebuggerCmd, ctx: JdwpContext<'_>) -> Result<()> {
    let value = match cmd {
        DebuggerCmd::Status => status(ctx.serial).await?,
        DebuggerCmd::Sessions => sessions(ctx.serial).await?,
        DebuggerCmd::Attach {
            package,
            pid,
            mode,
            dialog,
            launch,
            ..
        } => {
            if *dialog {
                return Err(unsupported("attach --dialog"));
            }
            if matches!(mode, Some(DebugMode::Native | DebugMode::Mixed)) {
                return Err(unsupported("attach --mode native|mixed"));
            }
            let serial = ctx.serial.ok_or_else(|| {
                DiagnosticError::new(
                    "device_unavailable",
                    "debugger",
                    "jdwp attach needs a device; pass -d <serial>",
                )
            })?;
            let request = AttachRequest {
                serial,
                package: package.as_deref(),
                pid: *pid,
                init: initial_breakpoints(launch, ctx.project_root)?,
                wait_for_launch: launch.wait_for_launch,
                launch_timeout: Duration::from_millis(launch.launch_timeout_ms),
            };
            let host = super::launch::AdbHost {
                serial: serial.to_string(),
            };
            let activity = launch.launch_activity.clone();
            attach_with(request, |package| async move {
                super::launch::launcher_launch(&host, &package, activity.as_deref()).await
            })
            .await?
        }
        DebuggerCmd::Detach(selector) | DebuggerCmd::Stop(selector) => {
            let entry = select(ctx.serial, selector.session.as_deref())?;
            let result = rpc(&entry, "detach", json!({}), DEFAULT_CALL_TIMEOUT).await?;
            json!({"action": "detach", "result": result, "session_id": entry.session_id})
        }
        DebuggerCmd::Break(BreakCmd::Line {
            file,
            line,
            disabled,
            temporary,
            condition,
            clear_condition,
            force,
            invoke,
            variant,
            ..
        }) => {
            let target = locate_target(file, *line, ctx.project_root)?;
            let entry = select(ctx.serial, None)?;
            let options = BreakpointOptions {
                enabled: !*disabled,
                temporary: *temporary,
                condition: condition.clone(),
                force: *force,
                invoke: *invoke,
                variant: variant.unwrap_or_default(),
                ..Default::default()
            };
            let mut value = rpc(
                &entry,
                "break_line",
                json!({"target": target, "line": line, "options": options}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?;
            if *clear_condition && value["created"] == false {
                let id = value
                    .pointer("/breakpoint/id")
                    .cloned()
                    .unwrap_or(Json::Null);
                value = rpc(
                    &entry,
                    "break_update",
                    json!({"id": id, "update": {"clear_condition": true}}),
                    DEFAULT_CALL_TIMEOUT,
                )
                .await?;
                value["created"] = json!(false);
            }
            if target.path.is_none() {
                value["warning"] = json!(not_found_locally(&target));
            }
            value
        }
        DebuggerCmd::Break(BreakCmd::Exception {
            exception,
            disabled,
            caught,
            uncaught,
            ..
        }) => {
            let entry = select(ctx.serial, None)?;
            let options = BreakpointOptions {
                enabled: !*disabled,
                ..Default::default()
            };
            rpc(
                &entry,
                "break_exception",
                json!({"class": exception, "caught": caught, "uncaught": uncaught, "options": options}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        DebuggerCmd::Break(BreakCmd::Update(args)) => {
            let entry = select(ctx.serial, None)?;
            let update = BreakpointUpdate {
                enabled: args.enabled,
                temporary: args.temporary,
                condition: args.condition.clone(),
                clear_condition: args.clear_condition,
                log_expression: args.log_expression.clone(),
                clear_log_expression: args.clear_log_expression,
                log_message: args.log_message,
                log_stack: args.log_stack,
                suspend: args.suspend.map(|suspend| match suspend {
                    SuspendArg::All => SuspendKind::All,
                    SuspendArg::Thread => SuspendKind::Thread,
                    SuspendArg::None => SuspendKind::None,
                }),
                pass_count: args.pass_count,
                force: args.force,
                invoke: args.invoke.then_some(true),
            };
            rpc(
                &entry,
                "break_update",
                json!({"id": args.id, "update": update}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        DebuggerCmd::Break(BreakCmd::Remove { id, .. }) => {
            let entry = select(ctx.serial, None)?;
            rpc(
                &entry,
                "break_remove",
                json!({"id": id}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        DebuggerCmd::Break(BreakCmd::Method {
            class,
            method,
            disabled,
            entry,
            exit,
            ..
        }) => {
            let entry_point = select(ctx.serial, None)?;
            let options = BreakpointOptions {
                enabled: !*disabled,
                ..Default::default()
            };
            rpc(
                &entry_point,
                "break_method",
                json!({
                    "class": class,
                    "method": method,
                    "entry": entry,
                    "exit": exit,
                    "options": options,
                }),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        DebuggerCmd::Break(BreakCmd::Field {
            class,
            field,
            disabled,
            temporary,
            access,
            modification,
            accept_slowdown,
            duration_ms,
            ..
        }) => {
            let entry = select(ctx.serial, None)?;
            let options = BreakpointOptions {
                enabled: !*disabled,
                temporary: *temporary,
                ..Default::default()
            };
            rpc(
                &entry,
                "break_field",
                json!({
                    "class": class,
                    "field": field,
                    "access": access,
                    "modification": modification,
                    "accept_slowdown": accept_slowdown,
                    "duration_ms": duration_ms,
                    "options": options,
                }),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        DebuggerCmd::Breakpoints => {
            let entry = select(ctx.serial, None)?;
            rpc(&entry, "breakpoints", json!({}), DEFAULT_CALL_TIMEOUT).await?
        }
        DebuggerCmd::Logpoint(cmd) => return logpoint(cmd, &ctx).await,
        DebuggerCmd::ContinueUntil(args) => {
            let (Some(file), Some(line)) = (&args.file, args.line) else {
                return Err(DiagnosticError::new(
                    "unsupported_by_backend",
                    "debugger",
                    "continue-until on the jdwp backend needs --file and --line (a condition alone has no place to stop)",
                )
                .detail(json!({"backend": "jdwp"}))
                .next_actions(["add --file <File.kt> --line <n>"])
                .into());
            };
            let target = locate_target(file, line, ctx.project_root)?;
            let entry = select(ctx.serial, args.session.as_deref())?;
            rpc(
                &entry,
                "continue_until",
                json!({
                    "target": target,
                    "line": line,
                    "condition": args.condition,
                    "timeout_ms": args.timeout_ms,
                }),
                timeout_for(args.timeout_ms),
            )
            .await?
        }
        DebuggerCmd::Pause(selector) => {
            simple(ctx.serial, selector.session.as_deref(), "pause", json!({})).await?
        }
        DebuggerCmd::Resume(selector) => {
            simple(ctx.serial, selector.session.as_deref(), "resume", json!({})).await?
        }
        DebuggerCmd::StepIn(selector) => {
            simple(
                ctx.serial,
                selector.session.as_deref(),
                "step",
                json!({"depth": "into"}),
            )
            .await?
        }
        DebuggerCmd::StepOver(selector) => {
            simple(
                ctx.serial,
                selector.session.as_deref(),
                "step",
                json!({"depth": "over"}),
            )
            .await?
        }
        DebuggerCmd::StepOut(selector) => {
            simple(
                ctx.serial,
                selector.session.as_deref(),
                "step",
                json!({"depth": "out"}),
            )
            .await?
        }
        DebuggerCmd::Stack(args) => {
            let entry = select(ctx.serial, args.session.as_deref())?;
            rpc(
                &entry,
                "stack",
                json!({"limit": args.limit}),
                timeout_for(u64::from(args.timeout_ms)),
            )
            .await?
        }
        DebuggerCmd::Threads(args) => {
            let entry = select(ctx.serial, args.session.as_deref())?;
            rpc(
                &entry,
                "threads",
                json!({"limit": args.limit}),
                timeout_for(u64::from(args.timeout_ms)),
            )
            .await?
        }
        DebuggerCmd::Variables(args) => {
            let entry = select(ctx.serial, args.session.as_deref())?;
            rpc(
                &entry,
                "variables",
                json!({
                    "thread": args.thread,
                    "frame": args.frame,
                    "depth": args.depth,
                    "max_fields": args.max_fields,
                    "max_array_items": args.max_array_items,
                }),
                timeout_for(u64::from(args.timeout_ms)),
            )
            .await?
        }
        DebuggerCmd::Eval(args) => {
            let entry = select(ctx.serial, args.session.as_deref())?;
            rpc(
                &entry,
                "eval",
                json!({
                    "expression": args.expression,
                    "thread": args.thread,
                    "frame": args.frame,
                    "depth": args.depth,
                    "max_fields": args.max_fields,
                    "max_array_items": args.max_array_items,
                    "invoke": args.invoke,
                    "timeout_ms": args.timeout_ms,
                    "max_message_chars": args.max_message_chars,
                }),
                timeout_for(u64::from(args.timeout_ms)),
            )
            .await?
        }
        DebuggerCmd::Inspect(args) => {
            let entry = select(ctx.serial, args.session.as_deref())?;
            rpc(
                &entry,
                "inspect",
                json!({
                    "expression": args.expression,
                    "handle": args.handle,
                    "path": args.path,
                    "thread": args.thread,
                    "frame": args.frame,
                    "depth": args.depth,
                    "max_fields": args.max_fields,
                    "max_array_items": args.max_array_items,
                    "invoke": args.invoke,
                    "timeout_ms": args.timeout_ms,
                    "max_message_chars": args.max_message_chars,
                }),
                timeout_for(u64::from(args.timeout_ms)),
            )
            .await?
        }
        DebuggerCmd::Clients(_) => return Err(unsupported("clients")),
        DebuggerCmd::Coroutines(_) => return Err(unsupported("coroutines")),
        DebuggerCmd::Watch(_) => return Err(unsupported("watch")),
    };
    emit(value);
    Ok(())
}

fn not_found_locally(target: &SourceTarget) -> String {
    // A typo binds nothing, ever: say the file was not found locally and that
    // binding is by file name only.
    format!(
        "{} was not found under the project root; binding by file name only, with no package filter or line check",
        target.basename
    )
}

/// Resolve `--file`/`--line` to a source target, with the structured
/// `breakpoint_unresolved` errors.
pub fn locate_target(file: &Path, line: u32, project_root: Option<&Path>) -> Result<SourceTarget> {
    match resolve::locate(file, line, project_root) {
        Ok(target) => Ok(target),
        Err(LocateError::Ambiguous(candidates)) => Err(DiagnosticError::new(
            "breakpoint_unresolved",
            "debugger",
            format!("{} matches several project files", file.display()),
        )
        .detail(json!({"backend": "jdwp", "candidates": candidates}))
        .next_actions(["pass a longer path suffix or an absolute path in --file"])
        .into()),
        Err(LocateError::NoCodeAtLine { path, line, reason }) => Err(DiagnosticError::new(
            "breakpoint_unresolved",
            "debugger",
            format!("{path}:{line} has no code ({reason})"),
        )
        .detail(json!({"backend": "jdwp", "file": path, "line": line, "reason": reason}))
        .next_actions(["pick a line with an executable statement"])
        .into()),
        Err(LocateError::NotASourceFile(name)) => Err(DiagnosticError::new(
            "breakpoint_unresolved",
            "debugger",
            format!("{name} is not a .kt or .java source file"),
        )
        .detail(json!({"backend": "jdwp"}))
        .into()),
    }
}

/// `File.kt:31` → (`File.kt`, 31).
pub fn parse_break_spec(spec: &str) -> Result<(PathBuf, u32)> {
    let parsed = spec
        .rsplit_once(':')
        .and_then(|(file, line)| Some((file.trim(), line.trim().parse::<u32>().ok()?)))
        .filter(|(file, line)| !file.is_empty() && *line > 0);
    match parsed {
        Some((file, line)) => Ok((PathBuf::from(file), line)),
        None => Err(DiagnosticError::new(
            "invalid_arguments",
            "debugger",
            format!("--break expects FILE:LINE, got `{spec}`"),
        )
        .next_actions(["--break MainActivity.kt:61"])
        .into()),
    }
}

/// `--break` / `--break-exception` → the daemon's initial breakpoints.
pub fn initial_breakpoints_from(
    breaks: &[String],
    exceptions: &[String],
    project_root: Option<&Path>,
) -> Result<InitialBreakpoints> {
    let mut init = InitialBreakpoints::default();
    for spec in breaks {
        let (file, line) = parse_break_spec(spec)?;
        init.lines.push(InitialLine {
            target: locate_target(&file, line, project_root)?,
            line,
        });
    }
    for class in exceptions {
        init.exceptions.push(InitialException {
            class: class.clone(),
            caught: true,
            uncaught: true,
        });
    }
    Ok(init)
}

fn initial_breakpoints(
    launch: &LaunchArgs,
    project_root: Option<&Path>,
) -> Result<InitialBreakpoints> {
    initial_breakpoints_from(&launch.break_at, &launch.break_exception, project_root)
}

/// Everything `debug attach --backend jdwp` (and `debug auto`) needs.
pub struct AttachRequest<'a> {
    pub serial: &'a str,
    pub package: Option<&'a str>,
    pub pid: Option<i32>,
    pub init: InitialBreakpoints,
    pub wait_for_launch: bool,
    pub launch_timeout: Duration,
}

/// Attach, optionally starting the app under `am set-debug-app -w` first
/// (`launch` starts it). Initial breakpoints are installed by the daemon
/// before it reports ready.
pub async fn attach_with<L, LFut>(request: AttachRequest<'_>, launch: L) -> Result<Json>
where
    L: FnOnce(String) -> LFut,
    LFut: std::future::Future<Output = Result<Json>>,
{
    let tcp = transport::tcp_override();
    if !request.wait_for_launch {
        return attach(
            request.serial,
            request.package,
            request.pid,
            request.init,
            false,
        )
        .await;
    }
    let package = request.package.ok_or_else(|| {
        DiagnosticError::new(
            "invalid_arguments",
            "debugger",
            "--wait-for-launch needs --package (or a configured app)",
        )
        .next_actions(["shadowdroid debug attach --backend jdwp --wait-for-launch --package <pkg>"])
    })?;
    if tcp.is_some() {
        // Test/forward mode: there is no device to launch on.
        let mut value = attach(
            request.serial,
            Some(package),
            request.pid,
            request.init,
            true,
        )
        .await?;
        value["launch"] = json!({
            "wait_for_launch": true,
            "steps": [{"step": "launch", "skipped": true, "reason": "SHADOWDROID_JDWP_TCP"}],
        });
        return Ok(value);
    }
    check_api_level(request.serial).await?;
    let host = super::launch::AdbHost {
        serial: request.serial.to_string(),
    };
    let mut steps = Vec::new();
    let init = request.init;
    let serial = request.serial;
    let outcome = super::launch::launch_for_debug(
        &host,
        package,
        request.launch_timeout,
        Duration::from_millis(100),
        &mut steps,
        || launch(package.to_string()),
        |pid| attach(serial, Some(package), Some(pid as i32), init, true),
        super::launch::interrupted(),
    )
    .await;
    match outcome {
        Ok(mut value) => {
            // Surface launch warnings (several launcher activities, ...).
            let warning = steps
                .iter()
                .find_map(|step| step.pointer("/result/warning").cloned());
            value["launch"] = json!({"wait_for_launch": true, "steps": steps, "warning": warning});
            Ok(value)
        }
        Err(error) => match error.downcast::<DiagnosticError>() {
            Ok(mut diagnostic) => {
                if diagnostic.detail.is_object() {
                    diagnostic.detail["launch_steps"] = json!(steps);
                }
                Err(diagnostic.into())
            }
            Err(error) => {
                Err(
                    DiagnosticError::new("debug_launch_failed", "debugger", format!("{error:#}"))
                        .detail(json!({"backend": "jdwp", "launch_steps": steps}))
                        .next_actions([
                            "shadowdroid app current",
                            "shadowdroid debug status --backend jdwp",
                        ])
                        .into(),
                )
            }
        },
    }
}

// ── logpoints ───────────────────────────────────────────────────────────

struct DaemonLogpoints(RegistryEntry);

impl LogpointReader for DaemonLogpoints {
    async fn read_page(
        &self,
        after: Option<u64>,
        limit: u32,
        timeout_ms: u32,
        filters: &LogpointEventFilters,
    ) -> Result<Json> {
        rpc(
            &self.0,
            "logpoint_events",
            json!({
                "after": after,
                "limit": limit,
                "timeout_ms": timeout_ms,
                "id": filters.breakpoint_id,
                "owner": filters.owner,
                "session": filters.session,
            }),
            Duration::from_millis(u64::from(timeout_ms)) + CALL_HEADROOM,
        )
        .await
    }
}

async fn logpoint(cmd: &LogpointCmd, ctx: &JdwpContext<'_>) -> Result<()> {
    let value = match cmd {
        LogpointCmd::Add(args) => {
            let target = locate_target(&args.file, args.line, ctx.project_root)?;
            let entry = select(ctx.serial, None)?;
            let options = BreakpointOptions {
                enabled: !args.disabled,
                temporary: args.temporary,
                condition: args.condition.clone(),
                force: args.force,
                suspend: SuspendKind::None,
                pass_count: args.pass_count.filter(|n| *n > 0),
                log_expression: args.expression.clone(),
                log_message: args.log_message,
                log_stack: args.log_stack,
                owner: Some(args.owner.clone()),
                max_events_per_second: args.max_events_per_second,
                max_message_chars: args.max_message_chars,
                invoke: args.invoke,
                variant: args.variant.unwrap_or_default(),
            };
            let mut value = rpc(
                &entry,
                "logpoint_add",
                json!({"target": target, "line": args.line, "options": options}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?;
            if target.path.is_none() && value["warning"].is_null() {
                value["warning"] = json!(not_found_locally(&target));
            }
            value
        }
        LogpointCmd::List(args) => {
            let entry = select(
                ctx.serial,
                args.filters.session.as_deref().filter(|s| is_jdwp_id(s)),
            )?;
            rpc(
                &entry,
                "logpoints",
                json!({"id": args.filters.breakpoint_id, "owner": args.filters.owner}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        LogpointCmd::Events(args) => {
            let entry = select(
                ctx.serial,
                args.filters.session.as_deref().filter(|s| is_jdwp_id(s)),
            )?;
            let filters = LogpointEventFilters::from(&args.filters);
            let page = DaemonLogpoints(entry)
                .read_page(args.after, args.limit, 0, &filters)
                .await?;
            validate_logpoint_stream(&page, args.stream_id.as_deref(), args.after)?;
            page
        }
        LogpointCmd::Follow(args) => {
            let entry = select(
                ctx.serial,
                args.filters.session.as_deref().filter(|s| is_jdwp_id(s)),
            )?;
            return follow_logpoint_events(&DaemonLogpoints(entry), args).await;
        }
        LogpointCmd::Remove(args) => {
            let entry = select(ctx.serial, None)?;
            rpc(
                &entry,
                "logpoint_remove",
                json!({"id": args.id, "owner": args.owner}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        LogpointCmd::Clear(args) => {
            let entry = select(ctx.serial, None)?;
            rpc(
                &entry,
                "logpoint_clear",
                json!({"owner": args.owner}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
    };
    emit(value);
    Ok(())
}

fn is_jdwp_id(session: &str) -> bool {
    session.starts_with("jdwp:") || session.parse::<u32>().is_ok()
}

// ── library surface for the composed `debug` workflows ──────────────────

/// The registered session for `serial`, if exactly one is attached.
pub fn session_for(serial: &str, session: Option<&str>) -> Result<RegistryEntry> {
    select(Some(serial), session)
}

/// One daemon call with jdwp diagnostics.
pub async fn call(
    entry: &RegistryEntry,
    method: &str,
    params: Json,
    timeout: Duration,
) -> Result<Json> {
    rpc(entry, method, params, timeout).await
}

/// One daemon call that keeps "unreachable" distinct (the process ended).
pub async fn call_raw(
    entry: &RegistryEntry,
    method: &str,
    params: Json,
    timeout: Duration,
) -> std::result::Result<Json, CallError> {
    control::call(entry, method, params, timeout).await
}

/// The `debugger` section of `debug snapshot --backend jdwp`, in the Studio
/// section's shape (`status.sessions[]`, `breakpoints`, `stack`,
/// `variables`, `logpoint_events`).
pub async fn debugger_snapshot(serial: &str, depth: u32) -> Json {
    let entry = match select(Some(serial), None) {
        Ok(entry) => entry,
        Err(error) => {
            return json!({
                "available": false,
                "ok": false,
                "backend": "jdwp",
                "type": "jdwp_debugger_unavailable",
                "error": error.to_string(),
                "next_command": "shadowdroid debug attach --backend jdwp --package <pkg>",
            });
        }
    };
    let timeout = Duration::from_secs(10);
    let status = match rpc(&entry, "status", json!({}), timeout).await {
        Ok(status) => status,
        Err(error) => {
            return json!({
                "available": false,
                "ok": false,
                "backend": "jdwp",
                "type": "jdwp_debugger_unavailable",
                "error": error.to_string(),
            });
        }
    };
    let or_error = |result: Result<Json>| {
        result.unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()}))
    };
    let breakpoints = or_error(rpc(&entry, "breakpoints", json!({}), timeout).await);
    let stack = or_error(rpc(&entry, "stack", json!({"limit": 24}), timeout).await);
    let variables = or_error(
        rpc(
            &entry,
            "variables",
            json!({"depth": depth, "max_fields": 48, "max_array_items": 24}),
            timeout,
        )
        .await,
    );
    let logpoint_events =
        or_error(rpc(&entry, "logpoint_events", json!({"limit": 50}), timeout).await);
    json!({
        "available": true,
        "backend": "jdwp",
        "status": {
            "ok": true,
            "backend": "jdwp",
            "sessions": [status.get("session").cloned().unwrap_or(Json::Null)],
            "vm": status.pointer("/details/vm"),
        },
        "breakpoints": breakpoints,
        "stack": stack,
        "variables": variables,
        "logpoint_events": logpoint_events,
    })
}

/// Studio cannot see a process our daemon holds (its client list keeps
/// `debugger_attached=false`, spike Q9), and its own attach then fails
/// asynchronously after the bridge already answered ok. Refuse a Studio
/// attach to a pid or package a live jdwp session holds, up front.
pub async fn ensure_not_held_by_jdwp(
    device: Option<&str>,
    package: Option<&str>,
    pid: Option<i32>,
) -> Result<()> {
    for entry in paths::entries(device) {
        let same = match pid.filter(|pid| *pid > 0) {
            Some(pid) => entry.pid == pid as u32,
            None => package.is_some() && entry.package.as_deref() == package,
        };
        if !same {
            continue;
        }
        if let Err(CallError::Unreachable(_)) =
            control::call(&entry, "status", json!({}), Duration::from_secs(3)).await
        {
            control::prune(&entry);
            continue;
        }
        return Err(DiagnosticError::new(
            "debugger_already_attached",
            "debugger",
            format!(
                "the jdwp debugger ({}) holds pid {}; a process accepts one debugger at a time",
                entry.session_id, entry.pid
            ),
        )
        .detail(json!({
            "backend": "studio",
            "holder": "jdwp",
            "session_id": entry.session_id,
            "serial": entry.serial,
            "pid": entry.pid,
        }))
        .next_actions([format!(
            "shadowdroid -d {} debug detach --backend jdwp --session {}",
            entry.serial, entry.session_id
        )])
        .into());
    }
    Ok(())
}

fn timeout_for(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms).max(DEFAULT_CALL_TIMEOUT) + CALL_HEADROOM
}

async fn simple(
    serial: Option<&str>,
    session: Option<&str>,
    method: &str,
    params: Json,
) -> Result<Json> {
    let entry = select(serial, session)?;
    rpc(&entry, method, params, DEFAULT_CALL_TIMEOUT).await
}

async fn rpc(entry: &RegistryEntry, method: &str, params: Json, timeout: Duration) -> Result<Json> {
    match control::call(entry, method, params, timeout).await {
        Ok(value) => Ok(value),
        Err(CallError::Rpc(error)) => Err(diagnostic(error)),
        Err(CallError::Unreachable(reason)) => {
            control::prune(entry);
            Err(DiagnosticError::new(
                "daemon_unreachable",
                "debugger",
                format!(
                    "the debug daemon for {} is not running: {reason}",
                    entry.session_id
                ),
            )
            .retryable(true)
            .detail(json!({
                "backend": "jdwp",
                "session_id": entry.session_id,
                "log_tail": control::log_tail(&entry.log, 8),
            }))
            .next_actions([format!(
                "shadowdroid -d {} debug attach --backend jdwp --pid {}",
                entry.serial, entry.pid
            )])
            .into())
        }
    }
}

/// Pick the daemon a verb talks to: `--session` (id, pid, or index) or the
/// only registered one for the device filter.
fn select(serial: Option<&str>, session: Option<&str>) -> Result<RegistryEntry> {
    let entries = paths::entries(serial);
    if let Some(wanted) = session.filter(|s| !s.trim().is_empty()) {
        let found = entries.iter().enumerate().find(|(index, entry)| {
            entry.session_id == wanted
                || entry.pid.to_string() == wanted
                || index.to_string() == wanted
        });
        return found.map(|(_, entry)| entry.clone()).ok_or_else(|| {
            DiagnosticError::new(
                "debugger_session_not_found",
                "debugger",
                format!("no jdwp debug session matches {wanted}"),
            )
            .detail(json!({
                "backend": "jdwp",
                "sessions": entries.iter().map(|e| &e.session_id).collect::<Vec<_>>(),
            }))
            .next_actions(["shadowdroid debug sessions --backend jdwp"])
            .into()
        });
    }
    match entries.len() {
        1 => Ok(entries.into_iter().next().expect("one entry")),
        0 => Err(DiagnosticError::new(
            "debugger_session_not_found",
            "debugger",
            "no jdwp debug session is attached",
        )
        .detail(json!({"backend": "jdwp", "serial": serial}))
        .next_actions(["shadowdroid debug attach --backend jdwp --package <pkg>"])
        .into()),
        _ => Err(DiagnosticError::new(
            "debugger_session_ambiguous",
            "debugger",
            "several jdwp debug sessions are attached; pass --session or -d",
        )
        .detail(json!({
            "backend": "jdwp",
            "sessions": entries.iter().map(|e| &e.session_id).collect::<Vec<_>>(),
        }))
        .next_actions(["shadowdroid debug sessions --backend jdwp"])
        .into()),
    }
}

/// Probe every registered daemon; unreachable ones are pruned.
async fn live_sessions(serial: Option<&str>) -> Vec<Json> {
    let mut sessions = Vec::new();
    for (index, entry) in paths::entries(serial).into_iter().enumerate() {
        match control::call(&entry, "status", json!({}), Duration::from_secs(3)).await {
            Ok(status) => {
                let mut session = status.get("session").cloned().unwrap_or(Json::Null);
                session["index"] = json!(index);
                session["daemon_pid"] = json!(entry.daemon_pid);
                sessions.push(session);
            }
            Err(CallError::Unreachable(_)) => control::prune(&entry),
            Err(CallError::Rpc(error)) => sessions.push(json!({
                "id": entry.session_id,
                "backend": "jdwp",
                "index": index,
                "error": error.message,
            })),
        }
    }
    sessions
}

async fn sessions(serial: Option<&str>) -> Result<Json> {
    Ok(json!({"sessions": live_sessions(serial).await}))
}

async fn status(serial: Option<&str>) -> Result<Json> {
    let daemons = live_sessions(serial).await;
    Ok(json!({
        "backends": {
            "jdwp": {"available": cfg!(unix), "daemons": daemons},
            "studio": {"checked": false, "hint": "shadowdroid debug status --backend studio"},
        },
        "sessions": daemons,
    }))
}

async fn attach(
    serial: &str,
    package: Option<&str>,
    pid: Option<i32>,
    init: InitialBreakpoints,
    launched_under_debugger: bool,
) -> Result<Json> {
    let tcp = transport::tcp_override();
    if tcp.is_none() {
        check_api_level(serial).await?;
    }
    let pid = match (pid, tcp.is_some()) {
        (Some(pid), _) if pid > 0 => {
            let pid = pid as u32;
            if tcp.is_none() {
                let pids = transport::debuggable_pids(serial, Duration::from_secs(3))
                    .await
                    .map_err(adb_error)?;
                if !pids.contains(&pid) {
                    return Err(not_debuggable(serial, &format!("pid {pid}"), &pids));
                }
            }
            pid
        }
        (_, true) => 1,
        (_, false) => {
            let package = package.ok_or_else(|| {
                DiagnosticError::new(
                    "invalid_arguments",
                    "debugger",
                    "jdwp attach needs --package or --pid",
                )
                .next_actions(["shadowdroid debug attach --backend jdwp --package <pkg>"])
            })?;
            resolve_package_pid(serial, package).await?
        }
    };

    let registry = paths::registry_path(serial, pid)?;
    if let Some(entry) = paths::read_entry(&registry) {
        match control::call(&entry, "status", json!({}), Duration::from_secs(3)).await {
            Ok(status) => {
                // Already attached: set any requested breakpoints now.
                let mut installed = Vec::new();
                for line in &init.lines {
                    installed.push(
                        rpc(
                            &entry,
                            "break_line",
                            json!({"target": line.target, "line": line.line}),
                            DEFAULT_CALL_TIMEOUT,
                        )
                        .await
                        .map(|value| value["breakpoint"].clone())
                        .unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()})),
                    );
                }
                for exception in &init.exceptions {
                    installed.push(
                        rpc(
                            &entry,
                            "break_exception",
                            json!({"class": exception.class}),
                            DEFAULT_CALL_TIMEOUT,
                        )
                        .await
                        .map(|value| value["breakpoint"].clone())
                        .unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()})),
                    );
                }
                return Ok(json!({
                    "action": "attach",
                    "already_attached": true,
                    "session": status.get("session"),
                    "breakpoints": installed,
                }));
            }
            Err(_) => control::prune(&entry),
        }
    }

    let startup_id = format!(
        "{}-{}",
        std::process::id(),
        (crate::events::now_ts() * 1000.0) as u64
    );
    let init_path = if init.lines.is_empty() && init.exceptions.is_empty() {
        None
    } else {
        Some(control::write_init(serial, pid, &init)?)
    };
    let mut child = control::spawn(
        serial,
        pid,
        package,
        &startup_id,
        init_path.as_deref(),
        launched_under_debugger,
    )?;
    match control::await_ready(serial, pid, &startup_id, &mut child, ATTACH_READY_TIMEOUT).await {
        Ready::Up(status) => Ok(json!({
            "action": "attach",
            "already_attached": false,
            "session": status.get("session"),
            "breakpoints": status.get("initial_breakpoints"),
            "vm": status.pointer("/details/vm"),
            "capabilities": status.pointer("/details/capabilities"),
        })),
        Ready::Failed(error) => {
            let _ = child.wait();
            Err(diagnostic(error))
        }
        Ready::TimedOut => {
            let _ = child.kill();
            let _ = child.wait();
            let log = paths::log_path(serial, pid)?;
            Err(DiagnosticError::new(
                "debugger_timeout",
                "debugger",
                format!(
                    "the debug daemon did not become ready within {} ms",
                    ATTACH_READY_TIMEOUT.as_millis()
                ),
            )
            .retryable(true)
            .detail(json!({
                "backend": "jdwp",
                "command": "attach",
                "log_tail": control::log_tail(&log, 8),
            }))
            .into())
        }
    }
}

fn adb_error(error: anyhow::Error) -> anyhow::Error {
    DiagnosticError::new("device_unavailable", "debugger", format!("{error:#}"))
        .retryable(true)
        .detail(json!({"backend": "jdwp"}))
        .into()
}

fn not_debuggable(serial: &str, what: &str, pids: &[u32]) -> anyhow::Error {
    DiagnosticError::new(
        "process_not_debuggable",
        "debugger",
        format!("{what} on {serial} exposes no JDWP endpoint"),
    )
    .detail(json!({"backend": "jdwp", "serial": serial, "debuggable_pids": pids}))
    .next_actions([
        "install a debuggable build (android:debuggable=true) or use a userdebug/emulator image",
        "shadowdroid app current",
    ])
    .into()
}

async fn check_api_level(serial: &str) -> Result<()> {
    let sdk = transport::shell_line(serial, "getprop ro.build.version.sdk")
        .await
        .map_err(adb_error)?;
    match sdk.parse::<u32>() {
        Ok(level) if level < MIN_API_LEVEL => Err(DiagnosticError::new(
            "unsupported_api_level",
            "debugger",
            format!("the jdwp backend needs API {MIN_API_LEVEL}+; {serial} runs API {level}"),
        )
        .detail(json!({"backend": "jdwp", "api_level": level, "minimum": MIN_API_LEVEL}))
        .next_actions(["shadowdroid debug attach --backend studio"])
        .into()),
        _ => Ok(()),
    }
}

async fn resolve_package_pid(serial: &str, package: &str) -> Result<u32> {
    let pids = transport::debuggable_pids(serial, Duration::from_secs(3))
        .await
        .map_err(adb_error)?;
    let names = transport::process_names(serial, &pids)
        .await
        .map_err(adb_error)?;
    let matches = transport::pick_package_pid(&names, package);
    match matches.as_slice() {
        [(pid, _)] => Ok(*pid),
        [] => {
            let running = transport::shell_line(serial, &format!(
                "pidof {}",
                crate::config::quote_device_shell_arg(package)
            ))
            .await
            .unwrap_or_default();
            if running.trim().is_empty() {
                Err(DiagnosticError::new(
                    "app_not_running",
                    "debugger",
                    format!("{package} is not running on {serial}"),
                )
                .detail(json!({"backend": "jdwp", "package": package}))
                .next_actions([format!("shadowdroid app start {package}")])
                .into())
            } else {
                Err(not_debuggable(serial, package, &pids))
            }
        }
        many => Err(DiagnosticError::new(
            "debug_target_ambiguous",
            "debugger",
            format!("{package} has several debuggable processes; pass --pid"),
        )
        .detail(json!({
            "backend": "jdwp",
            "candidates": many.iter().map(|(pid, name)| json!({"pid": pid, "name": name})).collect::<Vec<_>>(),
        }))
        .into()),
    }
}
