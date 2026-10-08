//! `shadowdroid __debugd`: one detached daemon per attached process. It owns
//! the JDWP connection and answers JSON-RPC 2.0 requests (one per
//! connection, newline-delimited) on a `0600` unix socket listed in the
//! registry (`~/.shadowdroid/debug/<serial>/<pid>.json`).
//!
//! Lifetime: until `detach`, JDWP EOF (the process died), a signal, or the
//! idle timeout with no breakpoints and nothing suspended. Every exit path
//! disposes the VM connection so the app is never left suspended; the
//! [`Teardown`] guard covers panics and early returns with a non-blocking
//! Dispose plus registry cleanup.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use serde_json::{Value as Json, json};

use super::paths;
use super::session::{RpcError, RpcResult, Session, SessionInfo};

/// First JDWP command after attach: ART loads its JDWP agent lazily, so the
/// first reply is slower than the rest (58 ms measured; budget generously).
pub const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Ordinary per-command deadline inside the daemon.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Args, Clone, Debug)]
pub struct DebugdArgs {
    /// Device serial the process runs on.
    #[arg(long)]
    pub serial: String,
    /// Process id to attach to.
    #[arg(long)]
    pub pid: u32,
    /// Package name, for status output.
    #[arg(long)]
    pub package: Option<String>,
    /// Identity the parent waits for in the registry.
    #[arg(long)]
    pub startup_id: String,
    /// Exit after this long with no breakpoints, nothing suspended, and no requests.
    #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_MS)]
    pub idle_timeout_ms: u64,
    /// JSON file of breakpoints to set right after the handshake, before the
    /// daemon reports ready (launch-time debugging).
    #[arg(long)]
    pub init: Option<std::path::PathBuf>,
    /// The process was started under `am set-debug-app -w` (no ANR timers).
    #[arg(long)]
    pub launched_under_debugger: bool,
}

/// Breakpoints a launch-time attach installs before anything else runs.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct InitialBreakpoints {
    pub lines: Vec<InitialLine>,
    pub exceptions: Vec<InitialException>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct InitialLine {
    pub target: super::resolve::SourceTarget,
    pub line: u32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct InitialException {
    pub class: String,
    #[serde(default = "yes")]
    pub caught: bool,
    #[serde(default = "yes")]
    pub uncaught: bool,
}

fn yes() -> bool {
    true
}

/// Set the initial breakpoints; each result (breakpoint or error) is kept for
/// `status` so the attach reply can show what bound.
async fn install_initial(session: &Session, init: &InitialBreakpoints) -> Vec<Json> {
    use super::breakpoints::BreakpointOptions;
    let mut results = Vec::new();
    for line in &init.lines {
        let outcome = session
            .break_line_with(line.target.clone(), line.line, BreakpointOptions::default())
            .await;
        results.push(match outcome {
            Ok(value) => value["breakpoint"].clone(),
            Err(error) => json!({
                "ok": false,
                "file": line.target.basename,
                "line": line.line,
                "error": error,
            }),
        });
    }
    for exception in &init.exceptions {
        let outcome = session
            .break_exception(
                &exception.class,
                exception.caught,
                exception.uncaught,
                BreakpointOptions::default(),
            )
            .await;
        results.push(match outcome {
            Ok(value) => value,
            Err(error) => json!({"ok": false, "exception": exception.class, "error": error}),
        });
    }
    results
}

/// A structured startup failure the parent turns into the attach error.
fn startup_failure(args: &DebugdArgs, error: &RpcError) {
    if let Ok(path) = paths::startup_error_path(&args.serial, args.pid) {
        let body = json!({
            "startup_id": args.startup_id,
            "error": error,
        });
        let _ = std::fs::write(path, body.to_string());
    }
}

pub async fn run(args: DebugdArgs) -> Result<()> {
    paths::ensure_serial_dir(&args.serial)?;
    match start(&args).await {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::error!("debugd startup failed: {} ({})", error.message, error.code);
            startup_failure(&args, &error);
            Err(anyhow::anyhow!("{}: {}", error.code, error.message))
        }
    }
}

async fn connect(args: &DebugdArgs) -> RpcResult<tokio::net::TcpStream> {
    match super::transport::tcp_override() {
        Some(address) => super::transport::connect_tcp(&address, CONNECT_TIMEOUT)
            .await
            .map_err(|error| {
                RpcError::new("daemon_unreachable", format!("{error:#}"))
                    .detail(json!({"address": address}))
            }),
        None => super::transport::connect_adb(&args.serial, args.pid, CONNECT_TIMEOUT)
            .await
            .map_err(|error| connect_error(args, error)),
    }
}

/// Blame the stage that failed: the device transport, or the app's endpoint.
fn connect_error(args: &DebugdArgs, error: super::transport::ConnectError) -> RpcError {
    use super::transport::ConnectError;
    let detail = json!({"serial": args.serial, "pid": args.pid, "reason": error.to_string()});
    match error {
        ConnectError::Transport(_) => RpcError::new(
            "device_unavailable",
            format!("cannot reach {} through adb: {error}", args.serial),
        )
        .retryable()
        .detail(detail)
        .next(&["shadowdroid devices", "adb devices -l"]),
        ConnectError::Service(_) => RpcError::new(
            "process_not_debuggable",
            format!("adb refused jdwp:{}: {error}", args.pid),
        )
        .detail(detail)
        .next(&[
            "the app must be debuggable (debug build) or the image ro.debuggable=1",
            "shadowdroid app current",
        ]),
        ConnectError::Timeout { .. } => RpcError::new("debugger_timeout", error.to_string())
            .retryable()
            .detail(detail),
    }
}

/// Map a failed JDWP handshake. Only "OKAY, then EOF before the echo" is
/// the spike's signature of another debugger holding the process; a silent
/// endpoint or an I/O error is reported as what it is.
fn handshake_error(
    args: &DebugdArgs,
    error: super::conn::JdwpError,
    elapsed: Duration,
) -> RpcError {
    use super::conn::JdwpError;
    let elapsed_ms = elapsed.as_millis() as u64;
    match error {
        JdwpError::HandshakeClosed { read } => RpcError::new(
            "debugger_already_attached",
            format!("another debugger holds pid {}: {error}", args.pid),
        )
        .detail(json!({
            "serial": args.serial,
            "pid": args.pid,
            "handshake_bytes_read": read,
            "elapsed_ms": elapsed_ms,
        }))
        .next(&[
            "shadowdroid debug sessions --backend jdwp",
            "shadowdroid debug detach --backend jdwp",
            "detach the debugger in Android Studio, then retry",
        ]),
        JdwpError::Timeout { .. } => RpcError::new(
            "debugger_timeout",
            format!(
                "pid {} did not answer the JDWP handshake: {error}",
                args.pid
            ),
        )
        .retryable()
        .detail(json!({
            "serial": args.serial,
            "pid": args.pid,
            "command": "JDWP-Handshake",
            "elapsed_ms": elapsed_ms,
        })),
        other => RpcError::new(
            "daemon_unreachable",
            format!("JDWP handshake with pid {} failed: {other}", args.pid),
        )
        .retryable()
        .detail(json!({"serial": args.serial, "pid": args.pid, "elapsed_ms": elapsed_ms})),
    }
}

async fn start(args: &DebugdArgs) -> RpcResult<()> {
    let stream = connect(args).await?;
    let handshake_started = std::time::Instant::now();
    let (conn, incoming) = super::conn::Connection::start(stream, CONNECT_TIMEOUT)
        .await
        .map_err(|error| handshake_error(args, error, handshake_started.elapsed()))?;
    let jdwp = super::vm::Jdwp::new(conn, COMMAND_TIMEOUT);
    let sizes = jdwp.id_sizes(FIRST_REQUEST_TIMEOUT).await?;
    let version = jdwp.version().await?;
    let capabilities = jdwp.capabilities_new().await.unwrap_or_default();
    let capability_map: serde_json::Map<String, Json> = super::protocol::CAPABILITY_NAMES
        .iter()
        .zip(capabilities.iter())
        .map(|(name, value)| ((*name).to_string(), json!(value)))
        .collect();
    let info = SessionInfo {
        session_id: paths::session_id(&args.serial, args.pid),
        serial: args.serial.clone(),
        pid: args.pid,
        package: args.package.clone(),
        attached_at: crate::events::now_ts(),
        vm: json!({
            "description": version.description,
            "jdwp_version": format!("{}.{}", version.jdwp_major, version.jdwp_minor),
            "vm_version": version.vm_version,
            "vm_name": version.vm_name,
            "id_sizes": {
                "field": sizes.field,
                "method": sizes.method,
                "object": sizes.object,
                "reference_type": sizes.reference_type,
                "frame": sizes.frame,
            },
        }),
        capabilities: Json::Object(capability_map),
        launched_under_debugger: args.launched_under_debugger,
    };
    let session = Session::new(jdwp, info);
    let events = tokio::spawn(session.clone().run_events(incoming));
    if let Some(path) = &args.init {
        let init: InitialBreakpoints = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .ok_or_else(|| {
                RpcError::new(
                    "invalid_request",
                    format!("unreadable initial breakpoints {}", path.display()),
                )
            })?;
        // `Debug.waitForDebugger` releases the app ~1.3 s after the last
        // debugger command; keep it parked until every request is set.
        let pinger = {
            let jdwp = session.jdwp.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = jdwp.version().await;
                }
            })
        };
        let installed = install_initial(&session, &init).await;
        pinger.abort();
        session.set_initial_breakpoints(installed);
        let _ = std::fs::remove_file(path);
    }

    let socket = paths::socket_path(&args.serial, args.pid)
        .map_err(|error| RpcError::new("daemon_unreachable", format!("{error:#}")))?;
    let guard = Teardown {
        session: session.clone(),
        serial: args.serial.clone(),
        pid: args.pid,
        startup_id: args.startup_id.clone(),
        armed: true,
    };
    let result = serve(args, &session, &socket).await;
    guard.finish().await;
    events.abort();
    result
}

#[cfg(unix)]
async fn serve(
    args: &DebugdArgs,
    session: &Arc<Session>,
    socket: &std::path::Path,
) -> RpcResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(socket);
    let listener = tokio::net::UnixListener::bind(socket).map_err(|error| {
        RpcError::new(
            "daemon_unreachable",
            format!("bind {}: {error}", socket.display()),
        )
    })?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        RpcError::new(
            "daemon_unreachable",
            format!("chmod {}: {error}", socket.display()),
        )
    })?;
    let entry = paths::RegistryEntry {
        session_id: session.info.session_id.clone(),
        serial: args.serial.clone(),
        pid: args.pid,
        package: args.package.clone(),
        daemon_pid: std::process::id(),
        socket: socket.to_path_buf(),
        startup_id: args.startup_id.clone(),
        attached_at: session.info.attached_at,
        log: paths::log_path(&args.serial, args.pid).unwrap_or_default(),
    };
    paths::write_entry(&entry)
        .map_err(|error| RpcError::new("daemon_unreachable", format!("{error:#}")))?;
    if let Ok(path) = paths::startup_error_path(&args.serial, args.pid) {
        let _ = std::fs::remove_file(path);
    }
    tracing::info!(
        "debugd attached to pid {} on {} ({}), socket {}",
        args.pid,
        args.serial,
        session.info.vm["vm_name"].as_str().unwrap_or("?"),
        socket.display()
    );

    let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);
    let mut changes = session.subscribe();
    let idle_timeout = Duration::from_millis(args.idle_timeout_ms.max(1000));
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|error| RpcError::new("daemon_unreachable", error.to_string()))?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let session = session.clone();
                    let stop_tx = stop_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = serve_client(stream, session, stop_tx).await {
                            tracing::debug!("control client: {error}");
                        }
                    });
                }
            }
            _ = stop_rx.recv() => {
                tracing::info!("debugd detaching on request");
                break;
            }
            _ = changes.changed() => {
                if let Some(reason) = session.closed_reason() {
                    tracing::info!("debugd exiting: {reason}");
                    break;
                }
            }
            _ = tick.tick() => {
                session.rearm_due().await;
                session.expire_slow_watches().await;
                if session.begin_anr_check() {
                    spawn_anr_probe(session.clone());
                }
                if let Some(reason) = session.closed_reason() {
                    tracing::info!("debugd exiting: {reason}");
                    break;
                }
                if session.is_quiescent() && session.idle_for() >= idle_timeout {
                    tracing::info!("debugd idle for {} ms; detaching", idle_timeout.as_millis());
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    if session.closed_reason().is_some() {
        // Let in-flight long-polls (`wait_stop`) report the exit.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Ok(())
}

#[cfg(not(unix))]
async fn serve(_: &DebugdArgs, _: &Arc<Session>, _: &std::path::Path) -> RpcResult<()> {
    Err(RpcError::new(
        "unsupported_backend",
        "the JDWP backend's control socket needs a unix host",
    ))
}

/// Disposes the VM and removes the registry on every exit path.
struct Teardown {
    session: Arc<Session>,
    serial: String,
    pid: u32,
    startup_id: String,
    armed: bool,
}

impl Teardown {
    async fn finish(mut self) {
        self.session.release_handles().await;
        if let Err(error) = self.session.dispose().await {
            tracing::warn!("dispose: {}", error.message);
        }
        paths::remove_if_owned(&self.serial, self.pid, &self.startup_id);
        self.armed = false;
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if self.armed {
            self.session.dispose_nowait();
            paths::remove_if_owned(&self.serial, self.pid, &self.startup_id);
        }
    }
}

#[cfg(unix)]
async fn serve_client(
    stream: tokio::net::UnixStream,
    session: Arc<Session>,
    stop_tx: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Json = serde_json::from_str(&line).unwrap_or(Json::Null);
    let id = request.get("id").cloned().unwrap_or(Json::Null);
    let method = request
        .get("method")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    session.touch();
    let outcome = dispatch(&session, &method, &params).await;
    let response = match &outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": error.message, "data": error},
        }),
    };
    write.write_all(format!("{response}\n").as_bytes()).await?;
    write.flush().await?;
    if method == "detach" && outcome.is_ok() {
        let _ = stop_tx.send(()).await;
    }
    Ok(())
}

fn target_param(params: &Json) -> RpcResult<super::resolve::SourceTarget> {
    serde_json::from_value(params.get("target").cloned().unwrap_or(Json::Null))
        .map_err(|error| RpcError::new("invalid_request", format!("target: {error}")))
}

fn options_param(params: &Json) -> RpcResult<super::breakpoints::BreakpointOptions> {
    serde_json::from_value(params.get("options").cloned().unwrap_or(json!({})))
        .map_err(|error| RpcError::new("invalid_request", format!("options: {error}")))
}

fn str_param<'a>(params: &'a Json, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Json::as_str)
}

fn u64_param(params: &Json, name: &str, default: u64) -> u64 {
    params.get(name).and_then(Json::as_u64).unwrap_or(default)
}

fn render_options(params: &Json, default_depth: u64) -> super::inspect::RenderOptions {
    super::inspect::RenderOptions {
        max_message_chars: u64_param(
            params,
            "max_message_chars",
            u64::from(super::inspect::DEFAULT_TO_STRING_CHARS),
        )
        .clamp(16, 65_536) as u32,
        ..super::inspect::RenderOptions::new(
            u64_param(params, "depth", default_depth).min(8) as u32,
            u64_param(params, "max_fields", 64).clamp(1, 512) as u32,
            u64_param(params, "max_array_items", 32).min(512) as u32,
        )
    }
}

/// `invoke: true` → the caller's `--timeout-ms` bounds each invoke.
fn invoke_param(params: &Json) -> Option<Duration> {
    params
        .get("invoke")
        .and_then(Json::as_bool)
        .unwrap_or(false)
        .then(|| super::inspect::read_timeout(u64_param(params, "timeout_ms", 5_000)))
}

/// One ANR probe (read-only system_server dumps), bounded; a probe that
/// fails or times out records "nothing seen" so the next one can run.
#[cfg(unix)]
fn spawn_anr_probe(session: Arc<Session>) {
    tokio::spawn(async move {
        let command = super::anr::probe_command(session.info.package.as_deref());
        let text = tokio::time::timeout(
            Duration::from_secs(5),
            super::transport::shell_line(&session.info.serial, &command),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
        session.record_anr(super::anr::parse_probe(
            &text,
            session.info.pid,
            session.info.package.as_deref(),
        ));
    });
}

pub async fn dispatch(session: &Arc<Session>, method: &str, params: &Json) -> RpcResult<Json> {
    let mut reply = dispatch_method(session, method, params).await?;
    if matches!(method, "status" | "stack" | "variables") {
        session.annotate_anr(&mut reply);
    }
    Ok(reply)
}

async fn dispatch_method(session: &Arc<Session>, method: &str, params: &Json) -> RpcResult<Json> {
    let thread = str_param(params, "thread");
    let frame = params
        .get("frame")
        .and_then(Json::as_u64)
        .map(|f| f as usize);
    match method {
        "status" => Ok(json!({
            "running": true,
            "daemon_pid": std::process::id(),
            "session": session.status().await,
            "details": session.details(),
            "initial_breakpoints": session.initial_breakpoints(),
        })),
        "export_breakpoints" => Ok(json!(session.export_initial_breakpoints())),
        "detach" => {
            session.release_handles().await;
            session.dispose().await?;
            Ok(json!({"detached": true, "session_id": session.info.session_id}))
        }
        "break_line" => {
            let target = target_param(params)?;
            let line = u64_param(params, "line", 0) as u32;
            session
                .break_line_with(target, line, options_param(params)?)
                .await
        }
        "break_exception" => {
            let class = str_param(params, "class")
                .ok_or_else(|| RpcError::new("invalid_request", "missing class"))?;
            let caught = params.get("caught").and_then(Json::as_bool).unwrap_or(true);
            let uncaught = params
                .get("uncaught")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            let breakpoint = session
                .break_exception(class, caught, uncaught, options_param(params)?)
                .await?;
            Ok(json!({"breakpoint": breakpoint, "created": true}))
        }
        "break_method" => {
            let class = str_param(params, "class")
                .ok_or_else(|| RpcError::new("invalid_request", "missing class"))?;
            let method = str_param(params, "method")
                .ok_or_else(|| RpcError::new("invalid_request", "missing method"))?;
            let entry = params.get("entry").and_then(Json::as_bool).unwrap_or(true);
            let exit = params.get("exit").and_then(Json::as_bool).unwrap_or(false);
            let breakpoint = session
                .break_method(class, method, entry, exit, options_param(params)?)
                .await?;
            Ok(json!({"breakpoint": breakpoint, "created": true}))
        }
        "break_field" => {
            let class = str_param(params, "class")
                .ok_or_else(|| RpcError::new("invalid_request", "missing class"))?;
            let field = str_param(params, "field")
                .ok_or_else(|| RpcError::new("invalid_request", "missing field"))?;
            let access = params
                .get("access")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            let modification = params
                .get("modification")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            let accept = params
                .get("accept_slowdown")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            let duration = Duration::from_millis(u64_param(
                params,
                "duration_ms",
                super::members::DEFAULT_WATCH_DURATION.as_millis() as u64,
            ));
            let breakpoint = session
                .break_field(
                    class,
                    field,
                    access,
                    modification,
                    accept,
                    duration,
                    options_param(params)?,
                )
                .await?;
            Ok(json!({"breakpoint": breakpoint, "created": true}))
        }
        "break_update" => {
            let id = str_param(params, "id")
                .ok_or_else(|| RpcError::new("invalid_request", "missing id"))?;
            let update: super::breakpoints::BreakpointUpdate =
                serde_json::from_value(params.get("update").cloned().unwrap_or(json!({})))
                    .map_err(|error| RpcError::new("invalid_request", error.to_string()))?;
            Ok(json!({"breakpoint": session.update_breakpoint(id, update).await?}))
        }
        "logpoint_add" => {
            let target = target_param(params)?;
            let line = u64_param(params, "line", 0) as u32;
            session
                .logpoint_add(target, line, options_param(params)?)
                .await
        }
        "logpoints" => Ok(session.logpoints(str_param(params, "id"), str_param(params, "owner"))),
        "logpoint_events" => {
            let filter = super::logpoints::Filter {
                breakpoint_id: str_param(params, "id").map(str::to_string),
                owner: str_param(params, "owner").map(str::to_string),
                session: str_param(params, "session")
                    .filter(|s| *s != session.info.session_id)
                    .map(str::to_string),
            };
            let after = params.get("after").and_then(Json::as_u64);
            let limit = u64_param(params, "limit", 100) as usize;
            let timeout = Duration::from_millis(u64_param(params, "timeout_ms", 0));
            Ok(session
                .logpoint_events(after, limit, &filter, timeout)
                .await)
        }
        "logpoint_remove" => {
            let id = str_param(params, "id")
                .ok_or_else(|| RpcError::new("invalid_request", "missing id"))?;
            let owner = str_param(params, "owner").unwrap_or("shadowdroid");
            session.logpoint_remove(id, owner).await
        }
        "logpoint_clear" => {
            let owner = str_param(params, "owner").unwrap_or("shadowdroid");
            session.logpoint_clear(owner).await
        }
        "continue_until" => {
            let target = target_param(params)?;
            let line = u64_param(params, "line", 0) as u32;
            let timeout = Duration::from_millis(u64_param(params, "timeout_ms", 10_000));
            session
                .continue_until(
                    target,
                    line,
                    str_param(params, "condition").map(str::to_string),
                    timeout,
                )
                .await
        }
        "watch_add" => {
            let expression = str_param(params, "expression")
                .ok_or_else(|| RpcError::new("invalid_expression", "missing expression"))?;
            session.watch_add(
                expression,
                str_param(params, "name"),
                params
                    .get("invoke")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            )
        }
        "watch_remove" => {
            let id = str_param(params, "id")
                .ok_or_else(|| RpcError::new("invalid_request", "missing id"))?;
            Ok(session.watch_remove(id))
        }
        "watch_clear" => Ok(session.watch_clear()),
        "watch_list" => session.watch_list(render_options(params, 1)).await,
        "coroutines_snapshot" => {
            session
                .coroutine_snapshot(
                    u64_param(params, "limit", 64).clamp(1, 256) as u32,
                    render_options(params, 1),
                    params
                        .get("invoke")
                        .and_then(Json::as_bool)
                        .unwrap_or(false),
                )
                .await
        }
        "coroutines_threads" => {
            session
                .coroutine_threads(u64_param(params, "limit", 32).clamp(1, 128) as u32)
                .await
        }
        "coroutines_continuation" => {
            session
                .coroutine_continuation(thread, frame, render_options(params, 2))
                .await
        }
        "coroutines_flow" => {
            let expression = str_param(params, "expression")
                .ok_or_else(|| RpcError::new("invalid_expression", "missing expression"))?;
            session
                .coroutine_flow(expression, thread, frame, render_options(params, 2))
                .await
        }
        "wait_stop" => {
            let timeout = Duration::from_millis(u64_param(params, "timeout_ms", 1_000).min(30_000));
            Ok(session
                .wait_stop(params.get("after_epoch").and_then(Json::as_u64), timeout)
                .await)
        }
        "break_remove" => {
            let id = str_param(params, "id")
                .ok_or_else(|| RpcError::new("invalid_request", "missing id"))?;
            session.remove_breakpoint(id).await
        }
        "breakpoints" => Ok(json!({"breakpoints": session.breakpoints()})),
        "pause" => Ok(json!({"action": "pause", "session": session.pause().await?})),
        "resume" => Ok(json!({"action": "resume", "session": session.resume().await?})),
        "step" => {
            let depth = str_param(params, "depth").unwrap_or("over");
            let thread = match thread {
                Some(selector) => Some(session.select_thread(Some(selector)).await?.0),
                None => None,
            };
            let timeout = super::inspect::read_timeout(u64_param(params, "timeout_ms", 10_000));
            Ok(json!({
                "action": format!("step_{depth}"),
                "session": session.step(depth, thread, timeout).await?,
            }))
        }
        "stack" => {
            session
                .stack(thread, u64_param(params, "limit", 64).clamp(1, 512) as u32)
                .await
        }
        "threads" => {
            session
                .threads(u64_param(params, "limit", 32).clamp(1, 512) as u32)
                .await
        }
        "variables" => {
            session
                .variables(thread, frame, render_options(params, 0))
                .await
        }
        "eval" => {
            let expression = str_param(params, "expression")
                .ok_or_else(|| RpcError::new("invalid_expression", "missing expression"))?;
            session
                .eval(
                    expression,
                    thread,
                    frame,
                    render_options(params, 1),
                    invoke_param(params),
                )
                .await
        }
        "inspect" => {
            session
                .inspect(
                    str_param(params, "expression"),
                    str_param(params, "handle"),
                    str_param(params, "path"),
                    thread,
                    frame,
                    render_options(params, 1),
                    invoke_param(params),
                )
                .await
        }
        other => Err(RpcError::new(
            "invalid_request",
            format!("unknown debugd method: {other}"),
        )),
    }
}
