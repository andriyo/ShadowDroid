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
            .map_err(|error| {
                RpcError::new(
                    "process_not_debuggable",
                    format!("adb refused jdwp:{}: {error:#}", args.pid),
                )
                .detail(json!({"serial": args.serial, "pid": args.pid}))
                .next(&[
                    "the app must be debuggable (debug build) or the image ro.debuggable=1",
                    "shadowdroid app current",
                ])
            }),
    }
}

async fn start(args: &DebugdArgs) -> RpcResult<()> {
    let stream = connect(args).await?;
    let (conn, incoming) = super::conn::Connection::start(stream, CONNECT_TIMEOUT)
        .await
        .map_err(|error| match error {
            // adb answers OKAY, then the app's adbconnection closes the
            // stream before echoing the handshake: another debugger (Studio,
            // or a second debugd) holds the process.
            super::conn::JdwpError::Handshake(message) => RpcError::new(
                "debugger_already_attached",
                format!("another debugger holds pid {}: {message}", args.pid),
            )
            .detail(json!({"serial": args.serial, "pid": args.pid}))
            .next(&[
                "shadowdroid debug sessions --backend jdwp",
                "shadowdroid debug detach --backend jdwp",
                "detach the debugger in Android Studio, then retry",
            ]),
            other => other.into(),
        })?;
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
    };
    let session = Session::new(jdwp, info);
    let events = tokio::spawn(session.clone().run_events(incoming));

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

fn str_param<'a>(params: &'a Json, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Json::as_str)
}

fn u64_param(params: &Json, name: &str, default: u64) -> u64 {
    params.get(name).and_then(Json::as_u64).unwrap_or(default)
}

fn render_options(params: &Json, default_depth: u64) -> super::inspect::RenderOptions {
    super::inspect::RenderOptions {
        depth: u64_param(params, "depth", default_depth).min(8) as u32,
        max_fields: u64_param(params, "max_fields", 64).clamp(1, 512) as u32,
        max_array_items: u64_param(params, "max_array_items", 32).min(512) as u32,
    }
}

pub async fn dispatch(session: &Arc<Session>, method: &str, params: &Json) -> RpcResult<Json> {
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
        })),
        "detach" => {
            session.release_handles().await;
            session.dispose().await?;
            Ok(json!({"detached": true, "session_id": session.info.session_id}))
        }
        "break_line" => {
            let target: super::resolve::SourceTarget =
                serde_json::from_value(params.get("target").cloned().unwrap_or(Json::Null))
                    .map_err(|error| RpcError::new("invalid_request", error.to_string()))?;
            let line = u64_param(params, "line", 0) as u32;
            Ok(json!({"breakpoint": session.break_line(target, line).await?}))
        }
        "break_exception" => {
            let class = str_param(params, "class")
                .ok_or_else(|| RpcError::new("invalid_request", "missing class"))?;
            let caught = params.get("caught").and_then(Json::as_bool).unwrap_or(true);
            let uncaught = params
                .get("uncaught")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            Ok(json!({"breakpoint": session.break_exception(class, caught, uncaught).await?}))
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
                .eval(expression, thread, frame, render_options(params, 1))
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
                )
                .await
        }
        other => Err(RpcError::new(
            "invalid_request",
            format!("unknown debugd method: {other}"),
        )),
    }
}
