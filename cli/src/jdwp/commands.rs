//! `debug <verb> --backend jdwp`: the CLI side of the standalone debugger.
//! Each verb resolves a daemon from the registry (attach spawns one) and
//! makes one JSON-RPC call; the reply is emitted with `"backend": "jdwp"`.
//! Verbs the backend does not serve yet fail with `unsupported_by_backend`
//! instead of silently falling back to Studio.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value as Json, json};

use super::control::{self, CallError, Ready};
use super::paths::{self, RegistryEntry};
use super::resolve::{self, LocateError};
use super::session::RpcError;
use super::transport;
use crate::cmd::debugger::{BreakCmd, DebugMode, DebuggerCmd};
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
            attach(serial, package.as_deref(), *pid).await?
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
            ..
        }) => {
            if *disabled || *temporary || condition.is_some() || *clear_condition {
                return Err(unsupported("break line --disabled/--temporary/--condition"));
            }
            let target = match resolve::locate(file, *line, ctx.project_root) {
                Ok(target) => target,
                Err(LocateError::Ambiguous(candidates)) => {
                    return Err(DiagnosticError::new(
                        "breakpoint_unresolved",
                        "debugger",
                        format!("{} matches several project files", file.display()),
                    )
                    .detail(json!({"backend": "jdwp", "candidates": candidates}))
                    .next_actions(["pass a longer path suffix or an absolute path in --file"])
                    .into());
                }
                Err(LocateError::NotASourceFile(name)) => {
                    return Err(DiagnosticError::new(
                        "breakpoint_unresolved",
                        "debugger",
                        format!("{name} is not a .kt or .java source file"),
                    )
                    .detail(json!({"backend": "jdwp"}))
                    .into());
                }
            };
            let entry = select(ctx.serial, None)?;
            rpc(
                &entry,
                "break_line",
                json!({"target": target, "line": line}),
                DEFAULT_CALL_TIMEOUT,
            )
            .await?
        }
        DebuggerCmd::Break(BreakCmd::Exception {
            exception,
            disabled,
            caught,
            uncaught,
            ..
        }) => {
            if *disabled {
                return Err(unsupported("break exception --disabled"));
            }
            let entry = select(ctx.serial, None)?;
            rpc(
                &entry,
                "break_exception",
                json!({"class": exception, "caught": caught, "uncaught": uncaught}),
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
        DebuggerCmd::Break(BreakCmd::Method { .. }) => return Err(unsupported("break method")),
        DebuggerCmd::Break(BreakCmd::Field { .. }) => return Err(unsupported("break field")),
        DebuggerCmd::Break(BreakCmd::Update(_)) => return Err(unsupported("break update")),
        DebuggerCmd::Breakpoints => {
            let entry = select(ctx.serial, None)?;
            rpc(&entry, "breakpoints", json!({}), DEFAULT_CALL_TIMEOUT).await?
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
                }),
                timeout_for(u64::from(args.timeout_ms)),
            )
            .await?
        }
        DebuggerCmd::Clients(_) => return Err(unsupported("clients")),
        DebuggerCmd::Logpoint(_) => return Err(unsupported("logpoint")),
        DebuggerCmd::Coroutines(_) => return Err(unsupported("coroutines")),
        DebuggerCmd::ContinueUntil(_) => return Err(unsupported("continue-until")),
        DebuggerCmd::Watch(_) => return Err(unsupported("watch")),
    };
    emit(value);
    Ok(())
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

async fn attach(serial: &str, package: Option<&str>, pid: Option<i32>) -> Result<Json> {
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
                return Ok(json!({
                    "action": "attach",
                    "already_attached": true,
                    "session": status.get("session"),
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
    let mut child = control::spawn(serial, pid, package, &startup_id)?;
    match control::await_ready(serial, pid, &startup_id, &mut child, ATTACH_READY_TIMEOUT).await {
        Ready::Up(status) => Ok(json!({
            "action": "attach",
            "already_attached": false,
            "session": status.get("session"),
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
