//! Client side of the debug daemon: spawn, readiness, registry lookup, and
//! one JSON-RPC call per CLI verb over the unix socket.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value as Json, json};

use super::paths::{self, RegistryEntry};
use super::session::RpcError;

/// Why a call failed: the daemon answered with an error, or it could not be
/// reached at all (stale registry, crashed daemon).
#[derive(Debug)]
pub enum CallError {
    Rpc(RpcError),
    Unreachable(String),
}

#[cfg(unix)]
pub async fn call(
    entry: &RegistryEntry,
    method: &str,
    params: Json,
    timeout: Duration,
) -> Result<Json, CallError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(&entry.socket)
            .await
            .map_err(|error| {
                CallError::Unreachable(format!("connect {}: {error}", entry.socket.display()))
            })?;
        let (read, mut write) = stream.into_split();
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|error| CallError::Unreachable(error.to_string()))?;
        let mut lines = BufReader::new(read).lines();
        let line = lines
            .next_line()
            .await
            .map_err(|error| CallError::Unreachable(error.to_string()))?
            .ok_or_else(|| {
                CallError::Unreachable("daemon closed the connection without replying".into())
            })?;
        let response: Json = serde_json::from_str(&line).map_err(|error| {
            CallError::Unreachable(format!("unparseable daemon reply: {error}"))
        })?;
        if let Some(result) = response.get("result") {
            return Ok(result.clone());
        }
        let data = response
            .pointer("/error/data")
            .cloned()
            .unwrap_or(Json::Null);
        Err(CallError::Rpc(serde_json::from_value(data).unwrap_or_else(
            |_| {
                RpcError::new(
                    "jdwp_error",
                    response
                        .pointer("/error/message")
                        .and_then(Json::as_str)
                        .unwrap_or("debug daemon request failed"),
                )
            },
        )))
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(outcome) => outcome,
        Err(_) => Err(CallError::Rpc(
            RpcError::new(
                "debugger_timeout",
                format!(
                    "debug daemon did not answer {method} within {} ms",
                    timeout.as_millis()
                ),
            )
            .retryable()
            .detail(json!({"command": method, "timeout_ms": timeout.as_millis() as u64})),
        )),
    }
}

#[cfg(not(unix))]
pub async fn call(_: &RegistryEntry, _: &str, _: Json, _: Duration) -> Result<Json, CallError> {
    Err(CallError::Rpc(RpcError::new(
        "unsupported_backend",
        "the JDWP backend needs a unix host",
    )))
}

/// Start `__debugd` detached, logging to the per-pid log file.
pub fn spawn(
    serial: &str,
    pid: u32,
    package: Option<&str>,
    startup_id: &str,
    init: Option<&Path>,
    launched_under_debugger: bool,
) -> Result<std::process::Child> {
    paths::ensure_serial_dir(serial)?;
    let exe = std::env::current_exe().context("resolve current exe")?;
    let log = paths::log_path(serial, pid)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600));
    }
    let log_file2 = log_file.try_clone()?;
    let _ = std::fs::remove_file(paths::startup_error_path(serial, pid)?);
    let mut command = std::process::Command::new(exe);
    command
        .arg("__debugd")
        .arg("--serial")
        .arg(serial)
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--startup-id")
        .arg(startup_id);
    if let Some(package) = package {
        command.arg("--package").arg(package);
    }
    if let Some(init) = init {
        command.arg("--init").arg(init);
    }
    if launched_under_debugger {
        command.arg("--launched-under-debugger");
    }
    if let Some(idle) = crate::hostenv::nonempty_env("SHADOWDROID_DEBUGD_IDLE_TIMEOUT_MS") {
        command.arg("--idle-timeout-ms").arg(idle);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().context("spawn debug daemon")
}

/// Write the launch-time breakpoints for `__debugd --init` (owner-only).
pub fn write_init(
    serial: &str,
    pid: u32,
    init: &super::daemon::InitialBreakpoints,
) -> Result<std::path::PathBuf> {
    let dir = paths::ensure_serial_dir(serial)?;
    let path = dir.join(format!("{pid}.init.json"));
    std::fs::write(&path, serde_json::to_vec(init)?)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

pub enum Ready {
    Up(Json),
    Failed(RpcError),
    TimedOut,
}

/// Wait for the exact daemon `startup_id` to publish its registry and answer
/// `status`, or report its structured startup failure.
pub async fn await_ready(
    serial: &str,
    pid: u32,
    startup_id: &str,
    child: &mut std::process::Child,
    timeout: Duration,
) -> Ready {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(path) = paths::registry_path(serial, pid)
            && let Some(entry) = paths::read_entry(&path)
            && entry.startup_id == startup_id
            && let Ok(status) = call(&entry, "status", json!({}), Duration::from_millis(2000)).await
        {
            return Ready::Up(status);
        }
        if let Ok(Some(_exit)) = child.try_wait() {
            return Ready::Failed(read_startup_error(serial, pid, startup_id).unwrap_or_else(
                || {
                    RpcError::new(
                        "daemon_unreachable",
                        "the debug daemon exited during startup",
                    )
                },
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ready::TimedOut;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn read_startup_error(serial: &str, pid: u32, startup_id: &str) -> Option<RpcError> {
    let path = paths::startup_error_path(serial, pid).ok()?;
    let body: Json = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if body.get("startup_id").and_then(Json::as_str) != Some(startup_id) {
        return None;
    }
    serde_json::from_value(body.get("error")?.clone()).ok()
}

/// Last non-empty lines of a daemon log, ANSI stripped.
pub fn log_tail(path: &Path, lines: usize) -> Option<String> {
    crate::net::daemon::log_tail(path, lines)
}

/// Remove a registry entry whose daemon no longer answers.
pub fn prune(entry: &RegistryEntry) {
    paths::remove_if_owned(&entry.serial, entry.pid, &entry.startup_id);
}
