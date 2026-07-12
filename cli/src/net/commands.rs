//! Host-side handlers for the `net` verbs. Most are thin clients that talk to
//! the daemon over the control socket ([crate::net::control]); `check`/`trust`
//! run host-only logic. `cli::dispatch_net` routes the parsed clap command here.

use crate::ids::Serial;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::device::{adb, installer};
use crate::events;
use crate::net::{DaemonConfig, Matcher, Mutation, RuleSpec, control, daemon, paths, store};

/// Emit a `{"type":"action","cmd":<cmd>, …}` line — thin adapter over the shared
/// [`crate::events::emit_action`].
fn emit(cmd: &str, body: serde_json::Value) {
    crate::events::emit_action(cmd, &body);
}

fn checked_control_reply(op: &str, reply: serde_json::Value) -> Result<serde_json::Value> {
    match reply.get("ok").and_then(serde_json::Value::as_bool) {
        Some(true) => Ok(reply),
        Some(false) => {
            let message = reply
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("network daemon rejected the operation");
            Err(crate::diagnostic::DiagnosticError::new(
                "net_daemon_rejected",
                "net",
                format!("net daemon rejected `{op}`: {message}"),
            )
            .detail(json!({"operation": op, "reply": reply}))
            .next_actions([
                "inspect detail.reply and correct the flow id, rule, or daemon state",
                "run `shadowdroid net status` before retrying",
            ])
            .into())
        }
        None => Err(crate::diagnostic::DiagnosticError::new(
            "net_daemon_protocol",
            "net",
            format!("net daemon returned a malformed reply for `{op}`"),
        )
        .retryable(true)
        .detail(json!({"operation": op, "reply": reply}))
        .next_actions([
            "run `shadowdroid net stop`, then `shadowdroid net start`",
            "retry the original command",
        ])
        .into()),
    }
}

fn daemon_port_field(status: &serde_json::Value, field: &str) -> Result<Option<u16>> {
    let Some(value) = status.get(field) else {
        return Ok(None);
    };
    let Some(raw) = value.as_u64() else {
        return Err(invalid_daemon_port(field, value, status));
    };
    match u16::try_from(raw) {
        Ok(port) if port != 0 => Ok(Some(port)),
        _ => Err(invalid_daemon_port(field, value, status)),
    }
}

fn invalid_daemon_port(
    field: &str,
    value: &serde_json::Value,
    status: &serde_json::Value,
) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "net_daemon_protocol",
        "net",
        format!("net daemon returned invalid `{field}`; expected an integer from 1 to 65535"),
    )
    .retryable(true)
    .detail(json!({"field": field, "value": value, "daemon_status": status}))
    .next_actions([
        "run `shadowdroid net stop`, then `shadowdroid net start`",
        "retry the original command",
    ])
    .into()
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum DaemonProcessIdentity {
    Matching(String),
    Missing,
    Mismatched(String),
    Unknown(String),
}

impl DaemonProcessIdentity {
    fn state(&self) -> &'static str {
        match self {
            Self::Matching(_) => "matching",
            Self::Missing => "missing",
            Self::Mismatched(_) => "mismatched",
            Self::Unknown(_) => "unknown",
        }
    }

    fn command(&self) -> Option<&str> {
        match self {
            Self::Matching(command) | Self::Mismatched(command) => Some(command),
            Self::Missing | Self::Unknown(_) => None,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Unknown(error) => Some(error),
            _ => None,
        }
    }
}

fn daemon_command_matches(command: &str, serial: &Serial, startup_id: &str) -> bool {
    if startup_id.is_empty() {
        return false;
    }
    let tokens = command
        .split_whitespace()
        .map(|token| token.trim_matches(['\'', '"']))
        .collect::<Vec<_>>();
    tokens.iter().any(|token| token.contains("shadowdroid"))
        && tokens.windows(2).any(|pair| pair == ["net", "daemon"])
        && tokens
            .windows(2)
            .any(|pair| pair[0] == "--serial" && pair[1] == serial.as_str())
        && tokens
            .windows(2)
            .any(|pair| pair[0] == "--startup-id" && pair[1] == startup_id)
}

fn classify_daemon_command(
    command: String,
    serial: &Serial,
    startup_id: &str,
) -> DaemonProcessIdentity {
    if daemon_command_matches(&command, serial, startup_id) {
        DaemonProcessIdentity::Matching(command)
    } else {
        DaemonProcessIdentity::Mismatched(command)
    }
}

/// Inspect the command line immediately before using a pidfile as authority.
/// PID values can be stale and reused, so a numeric match alone is never enough
/// to signal a process.
fn inspect_daemon_process(pid: u32, serial: &Serial, startup_id: &str) -> DaemonProcessIdentity {
    if pid == 0 || startup_id.is_empty() {
        return DaemonProcessIdentity::Unknown("incomplete daemon identity".into());
    }
    #[cfg(unix)]
    {
        let output = match std::process::Command::new("ps")
            .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
            .output()
        {
            Ok(output) => output,
            Err(error) => return DaemonProcessIdentity::Unknown(error.to_string()),
        };
        if !output.status.success() {
            return if output.status.code() == Some(1) {
                DaemonProcessIdentity::Missing
            } else {
                DaemonProcessIdentity::Unknown(
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                )
            };
        }
        let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if command.is_empty() {
            DaemonProcessIdentity::Missing
        } else {
            classify_daemon_command(command, serial, startup_id)
        }
    }
    #[cfg(windows)]
    {
        let script = format!(
            "$p = Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}'; \
             if ($null -eq $p) {{ exit 3 }}; [Console]::Out.Write($p.CommandLine)"
        );
        let output = match std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
        {
            Ok(output) => output,
            Err(error) => return DaemonProcessIdentity::Unknown(error.to_string()),
        };
        if output.status.code() == Some(3) {
            return DaemonProcessIdentity::Missing;
        }
        if !output.status.success() {
            return DaemonProcessIdentity::Unknown(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            );
        }
        let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if command.is_empty() {
            DaemonProcessIdentity::Missing
        } else {
            classify_daemon_command(command, serial, startup_id)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, serial, startup_id);
        DaemonProcessIdentity::Unknown("process inspection is unsupported on this platform".into())
    }
}

/// Send a termination signal only after [`inspect_daemon_process`] proves the
/// pid still belongs to the expected daemon. The caller must subsequently
/// verify that the process exited; command success alone is not sufficient.
fn send_termination_signal(pid: u32) -> Result<bool> {
    if pid == 0 {
        bail!("refusing to signal pid 0")
    }
    #[cfg(unix)]
    let status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("run kill")?;
    #[cfg(windows)]
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()
        .context("run taskkill")?;
    #[cfg(not(any(unix, windows)))]
    bail!("forced daemon termination is unsupported on this platform");
    #[cfg(any(unix, windows))]
    Ok(status.success())
}

fn daemon_start_timeout(serial: &Serial, daemon_pid: u32, startup_id: &str) -> Result<()> {
    let log = paths::daemon_log_path(serial)?;
    let log_tail = daemon::log_tail(&log, 10);
    Err(crate::diagnostic::DiagnosticError::new(
        "net_daemon_start_timeout",
        "net",
        "net daemon did not become ready within 5 seconds",
    )
    .retryable(true)
    .detail(json!({
        "device": serial.as_str(),
        "daemon_pid": daemon_pid,
        "startup_id": startup_id,
        "timeout_ms": 5_000,
        "log": log.display().to_string(),
        "log_tail": log_tail,
    }))
    .next_actions([
        format!(
            "tail -n 50 {}",
            crate::events::shell_token(&log.display().to_string())
        ),
        "shadowdroid net start".to_string(),
        "shadowdroid net status".to_string(),
        "shadowdroid doctor --json".to_string(),
    ])
    .into())
}

fn lifecycle_busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::diagnostic::DiagnosticError>()
        .is_some_and(|diagnostic| diagnostic.code == "device_lifecycle_busy")
}

async fn request_daemon_stop(serial: &Serial) -> bool {
    control::request(serial, json!({"op": "stop"}))
        .await
        .ok()
        .and_then(|reply| reply.get("ok").and_then(serde_json::Value::as_bool))
        == Some(true)
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct DaemonIdentity {
    startup_id: Option<String>,
    pid: Option<u32>,
}

impl DaemonIdentity {
    fn from_status(status: Option<&serde_json::Value>, pid_file: Option<u32>) -> Self {
        let status = status.filter(|value| {
            value.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
                && value.get("running").and_then(serde_json::Value::as_bool) == Some(true)
        });
        let startup_id = status
            .and_then(|value| value.get("startup_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let status_pid = status
            .and_then(|value| value.get("pid"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|pid| *pid != 0);
        Self {
            startup_id,
            pid: status_pid.or(pid_file),
        }
    }

    fn is_empty(&self) -> bool {
        self.startup_id.is_none() && self.pid.is_none()
    }

    fn force_identity(&self) -> Option<(&str, u32)> {
        Some((
            self.startup_id
                .as_deref()
                .filter(|value| !value.is_empty())?,
            self.pid.filter(|pid| *pid != 0)?,
        ))
    }
}

fn daemon_status_matches_identity(status: &serde_json::Value, expected: &DaemonIdentity) -> bool {
    if expected.is_empty()
        || status.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
        || status.get("running").and_then(serde_json::Value::as_bool) != Some(true)
    {
        return false;
    }
    if let Some(startup_id) = &expected.startup_id
        && status.get("startup_id").and_then(serde_json::Value::as_str) != Some(startup_id.as_str())
    {
        return false;
    }
    if let Some(pid) = expected.pid
        && status.get("pid").and_then(serde_json::Value::as_u64) != Some(u64::from(pid))
    {
        return false;
    }
    true
}

fn require_daemon_serial(status: &serde_json::Value, serial: &Serial) -> Result<()> {
    if status.get("serial").and_then(serde_json::Value::as_str) == Some(serial.as_str()) {
        return Ok(());
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "net_daemon_identity_mismatch",
        "net",
        "the control endpoint belongs to a different device daemon",
    )
    .detail(json!({
        "expected_serial": serial.as_str(),
        "daemon_serial": status.get("serial"),
        "daemon_status": status,
    }))
    .next_actions([
        "do not mutate this daemon; remove stale pre-0.12 net marker files manually after inspecting them",
        "run `shadowdroid net status --json` for the intended device",
    ])
    .into())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OwnedDaemonMarkers {
    ctl: bool,
    pid: bool,
}

impl OwnedDaemonMarkers {
    fn any(self) -> bool {
        self.ctl || self.pid
    }
}

async fn owned_daemon_markers(
    serial: &Serial,
    expected: &DaemonIdentity,
) -> Result<OwnedDaemonMarkers> {
    if expected.is_empty() {
        return Ok(OwnedDaemonMarkers::default());
    }
    let ctl_path = paths::ctl_path(serial)?;
    let ctl_exists = ctl_path.exists();
    let pid_owned = expected
        .pid
        .is_some_and(|pid| control::daemon_pid(serial) == Some(pid));
    let status = if ctl_exists {
        tokio::time::timeout(
            Duration::from_millis(100),
            control::request(serial, json!({"op": "status"})),
        )
        .await
        .ok()
        .and_then(std::result::Result::ok)
    } else {
        None
    };
    let ctl_owned = ctl_exists
        && (status.as_ref().is_some_and(|status| {
            require_daemon_serial(status, serial).is_ok()
                && daemon_status_matches_identity(status, expected)
        }) || (status.is_none() && pid_owned));
    Ok(OwnedDaemonMarkers {
        ctl: ctl_owned,
        pid: pid_owned,
    })
}

async fn await_owned_daemon_markers_gone(
    serial: &Serial,
    expected: &DaemonIdentity,
    timeout: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !owned_daemon_markers(serial, expected).await?.any() {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn remove_owned_marker(path: PathBuf) -> Result<()> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

async fn remove_owned_daemon_markers(serial: &Serial, expected: &DaemonIdentity) -> Result<()> {
    let owned = owned_daemon_markers(serial, expected).await?;
    if owned.ctl {
        remove_owned_marker(paths::ctl_path(serial)?)?;
    }
    if owned.pid {
        remove_owned_marker(paths::pid_path(serial)?)?;
    }
    Ok(())
}

fn daemon_termination_error(
    serial: &Serial,
    expected: &DaemonIdentity,
    inspection: Option<&DaemonProcessIdentity>,
    message: impl Into<String>,
) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "net_daemon_stop_failed",
        "net",
        message,
    )
    .retryable(true)
    .detail(json!({
        "device": serial.as_str(),
        "expected_pid": expected.pid,
        "expected_startup_id": expected.startup_id,
        "process_state": inspection.map(DaemonProcessIdentity::state),
        "process_command": inspection.and_then(DaemonProcessIdentity::command),
        "inspection_error": inspection.and_then(DaemonProcessIdentity::error),
    }))
    .next_actions([
        "retry `shadowdroid net stop`",
        "inspect `shadowdroid net status --json` and the daemon log before removing marker files manually",
    ])
    .into()
}

/// Wait for normal daemon teardown while the transition lock is held. A stuck
/// daemon is signalled only after its command line proves the exact pid,
/// serial, and startup identity still belong together. Marker cleanup happens
/// only after confirmed process exit; a stale/reused pid is never signalled.
async fn complete_daemon_teardown(
    serial: &Serial,
    expected: &DaemonIdentity,
    graceful_requested: bool,
) -> Result<()> {
    if expected.is_empty() {
        return Ok(());
    }

    let markers_gone =
        await_owned_daemon_markers_gone(serial, expected, Duration::from_secs(2)).await?;
    if markers_gone && graceful_requested {
        return Ok(());
    }
    let Some((startup_id, pid)) = expected.force_identity() else {
        if markers_gone {
            return Ok(());
        }
        return Err(daemon_termination_error(
            serial,
            expected,
            None,
            "the network daemon did not stop and no complete owned identity is available for safe termination",
        ));
    };
    let inspection = inspect_daemon_process(pid, serial, startup_id);
    match &inspection {
        DaemonProcessIdentity::Missing => {
            return remove_owned_daemon_markers(serial, expected).await;
        }
        DaemonProcessIdentity::Matching(_) => {}
        DaemonProcessIdentity::Mismatched(_) | DaemonProcessIdentity::Unknown(_) => {
            return Err(daemon_termination_error(
                serial,
                expected,
                Some(&inspection),
                "refusing to terminate a process whose command line does not prove it is the expected network daemon",
            ));
        }
    }

    if !send_termination_signal(pid)? {
        let after_signal = inspect_daemon_process(pid, serial, startup_id);
        if after_signal == DaemonProcessIdentity::Missing {
            return remove_owned_daemon_markers(serial, expected).await;
        }
        return Err(daemon_termination_error(
            serial,
            expected,
            Some(&after_signal),
            "the operating system rejected network-daemon termination",
        ));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let inspection = inspect_daemon_process(pid, serial, startup_id);
        match &inspection {
            DaemonProcessIdentity::Missing => {
                return remove_owned_daemon_markers(serial, expected).await;
            }
            DaemonProcessIdentity::Matching(_) => {}
            DaemonProcessIdentity::Mismatched(_) | DaemonProcessIdentity::Unknown(_) => {
                return Err(daemon_termination_error(
                    serial,
                    expected,
                    Some(&inspection),
                    "network-daemon exit could not be confirmed after termination",
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(daemon_termination_error(
                serial,
                expected,
                Some(&inspection),
                "the network daemon remained alive after termination",
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_child_exit(child: &mut std::process::Child, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .context("inspect net daemon child")?
            .is_some()
        {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stop a daemon that this process just spawned. The live Child handle is
/// stronger authority than any marker or PID lookup and also lets us reap the
/// process on every startup-failure path.
async fn terminate_spawned_daemon(
    serial: &Serial,
    expected: &DaemonIdentity,
    child: &mut std::process::Child,
    graceful_requested: bool,
) -> Result<()> {
    if graceful_requested && wait_for_child_exit(child, Duration::from_secs(2)).await? {
        return remove_owned_daemon_markers(serial, expected).await;
    }
    child.kill().context("terminate failed net daemon child")?;
    if !wait_for_child_exit(child, Duration::from_secs(2)).await? {
        return Err(daemon_termination_error(
            serial,
            expected,
            None,
            "the freshly spawned network daemon did not exit after termination",
        ));
    }
    remove_owned_daemon_markers(serial, expected).await
}

// ── lifecycle ─────────────────────────────────────────────────

/// Options for [`start`] — grouped into a struct so the knob set can grow
/// (anticache/anticomp/verify-upstream/redact/…) without a widening arg list.
pub struct StartOpts {
    pub port: u16,
    pub apps: Vec<String>,
    pub foreground: bool,
    pub anticache: bool,
    pub anticomp: bool,
    pub verify_upstream: bool,
    pub redact: bool,
    /// Resolved signing-CA cert + key (from [`crate::net::ca::resolve_ca`], made
    /// to exist by `ensure_ca`) that the daemon loads and `net start` reports.
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
}

const NETWORK_STATE_SCHEMA: u32 = 2;
const RAW_IP_CANARY: &str = "8.8.8.8";

fn net_lifecycle_serial(serial: &Serial) -> Serial {
    Serial::new(format!("net:{serial}"))
}

/// The device-side state ShadowDroid owns for one proxy session. Persist this
/// before wiring so `net stop` can recover after either process crashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceNetworkState {
    schema_version: u32,
    serial: String,
    /// Empty only for state written before startup ownership was introduced.
    #[serde(default)]
    startup_id: String,
    /// `None` means the setting did not exist (`settings get` returned null).
    prior_http_proxy: Option<String>,
    /// Host endpoint previously mapped from `tcp:<device_port>`, if any.
    #[serde(default)]
    prior_reverse_host_port: Option<u16>,
    device_port: u16,
    host_port: u16,
    captured_at: f64,
}

#[derive(Debug, Clone, Serialize)]
struct PingCheck {
    host: String,
    resolved: bool,
    reachable: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ConnectivityCheck {
    raw_ip: PingCheck,
    dns: PingCheck,
    connectivity_restored: bool,
}

pub async fn start(serial: &Serial, opts: StartOpts) -> Result<()> {
    let StartOpts {
        port,
        apps,
        foreground,
        anticache,
        anticomp,
        verify_upstream,
        redact,
        ca_cert,
        ca_key,
    } = opts;
    // Network transitions use their own lifecycle namespace so start/stop for
    // one device cannot race each other, without blocking unrelated UI-server
    // lifecycle work on that device.
    let lifecycle_serial = net_lifecycle_serial(serial);
    let lifecycle_guard = installer::acquire_lifecycle_lock(&lifecycle_serial)?;

    // A live daemon may outlive the device-side reverse/proxy settings (most
    // commonly after a device reboot). Reuse its actual ports and repair the
    // wiring idempotently instead of forcing stop/start and losing rules.
    if control::is_running(serial).await {
        let daemon_status = checked_control_reply(
            "status",
            control::request(serial, json!({"op": "status"})).await?,
        )?;
        require_daemon_serial(&daemon_status, serial)?;
        let daemon_port = daemon_port_field(&daemon_status, "port")?.unwrap_or(port);
        let Some(host_port) = daemon_port_field(&daemon_status, "host_port")? else {
            return Err(crate::diagnostic::DiagnosticError::new(
                "net_daemon_metadata_missing",
                "net",
                "the running net daemon predates automatic rewiring metadata",
            )
            .retryable(true)
            .detail(json!({
                "device": serial.as_str(),
                "daemon_status": daemon_status,
                "missing": "host_port",
            }))
            .next_actions([
                "shadowdroid net stop".to_string(),
                "shadowdroid net start".to_string(),
                "shadowdroid net status".to_string(),
            ])
            .into());
        };

        // If the live daemon signs with a different CA than we just resolved
        // (e.g. `net start` from a different project on the same device), warn —
        // reuse keeps the daemon's original CA; a restart is needed to switch.
        let ca_mismatch = {
            let running = daemon_status
                .get("ca_fingerprint")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let resolved = crate::net::ca::fingerprint_of(&ca_cert).unwrap_or_default();
            (!running.is_empty() && !resolved.is_empty() && running != resolved).then(|| {
                format!(
                    "the running proxy still signs with a different CA ({running:.12}…); it keeps \
                     that CA until you `net stop` then `net start`. Resolved CA for this directory: \
                     {} ({resolved:.12}…).",
                    ca_cert.display()
                )
            })
        };

        let daemon_startup_id = daemon_status
            .get("startup_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let state_existed = load_device_network_state(serial)?.is_some();
        if !state_existed {
            // Compatibility path for a daemon started by an older CLI. If the
            // device still points at that daemon, its true prior value is no
            // longer knowable; absence is the safest teardown default.
            let current = read_http_proxy(serial).await?;
            let prior = if proxy_points_at(&current, daemon_port) {
                None
            } else {
                current
            };
            save_device_network_state(&DeviceNetworkState {
                schema_version: NETWORK_STATE_SCHEMA,
                serial: serial.to_string(),
                startup_id: daemon_startup_id.clone(),
                prior_http_proxy: prior,
                prior_reverse_host_port: match reverse_host_port(serial, daemon_port).await? {
                    Some(existing) if existing == host_port => None,
                    other => other,
                },
                device_port: daemon_port,
                host_port,
                captured_at: events::now_ts(),
            })?;
        }
        let current_reverse = reverse_host_port(serial, daemon_port).await?;
        let saved_prior = load_device_network_state(serial)?
            .filter(|state| state.startup_id == daemon_startup_id)
            .and_then(|state| state.prior_reverse_host_port);
        let expected_reverse = if current_reverse == Some(host_port)
            || current_reverse.is_none()
            || current_reverse == saved_prior
        {
            current_reverse
        } else {
            bail!(
                "refusing to rewire adb reverse tcp:{daemon_port}: current host {current_reverse:?} is not owned by the running ShadowDroid session"
            );
        };
        setup_wiring(serial, daemon_port, host_port, expected_reverse).await?;
        emit(
            "net_start",
            json!({
                "device": serial,
                "port": daemon_port,
                "host_port": host_port,
                "already_running": true,
                "rewired": true,
                "rules_preserved": true,
                "state_recovered": !state_existed,
                "ca_warning": ca_mismatch,
            }),
        );
        return Ok(());
    }

    // A dead daemon can leave its state file and wiring behind. Restore it
    // before starting a genuinely new session so the new snapshot is truthful.
    if let Some(stale) = load_device_network_state(serial)? {
        let outcome = restore_network_state(serial, &stale).await?;
        for warning in outcome.warnings {
            tracing::warn!(warning);
        }
        remove_device_network_state(serial)?;
    }

    // Host loopback port is per-serial so concurrent daemons for different
    // devices don't fight over one port; the device-facing `port` stays stable.
    let startup_id = crate::net::new_startup_id();
    let host_allocation = crate::device::portmap::reserve_loopback_port()?;
    let host_port = host_allocation.port();
    let device_state = DeviceNetworkState {
        schema_version: NETWORK_STATE_SCHEMA,
        serial: serial.to_string(),
        startup_id: startup_id.clone(),
        prior_http_proxy: read_http_proxy(serial).await?,
        prior_reverse_host_port: reverse_host_port(serial, port).await?,
        device_port: port,
        host_port,
        captured_at: events::now_ts(),
    };
    save_device_network_state(&device_state)?;
    let expected_reverse_host_port = device_state.prior_reverse_host_port;
    let cfg = DaemonConfig {
        serial: serial.clone(),
        startup_id: startup_id.clone(),
        ca_cert: ca_cert.clone(),
        ca_key: ca_key.clone(),
        port,
        host_port,
        app_filters: apps.clone(),
        anticache,
        anticomp,
        verify_upstream,
        redact,
    };

    if foreground {
        let daemon_pid = std::process::id();
        let mut daemon_task = tokio::spawn(daemon::run(cfg));
        if !daemon::await_ready(serial, &startup_id, daemon_pid, 5000).await {
            daemon_task.abort();
            let _ = daemon_task.await;
            if control::daemon_pid(serial) == Some(daemon_pid) {
                let _ = std::fs::remove_file(paths::pid_path(serial)?);
                let _ = std::fs::remove_file(paths::ctl_path(serial)?);
            }
            remove_device_network_state_if_owned(serial, &startup_id)?;
            return daemon_start_timeout(serial, daemon_pid, &startup_id);
        }
        drop(host_allocation);
        if let Err(err) = setup_wiring(serial, port, host_port, expected_reverse_host_port).await {
            if !request_daemon_stop(serial).await {
                daemon_task.abort();
            }
            if tokio::time::timeout(Duration::from_secs(2), &mut daemon_task)
                .await
                .is_err()
            {
                daemon_task.abort();
                let _ = daemon_task.await;
            }
            if control::daemon_pid(serial) == Some(daemon_pid) {
                remove_owned_marker(paths::pid_path(serial)?)?;
                remove_owned_marker(paths::ctl_path(serial)?)?;
            }
            restore_and_consume_network_state_if_owned(serial, &startup_id).await?;
            return Err(err);
        }
        emit(
            "net_start",
            json!({
                "device": serial,
                "port": port,
                "host_port": host_port,
                "startup_id": startup_id,
                "mode": "foreground",
            }),
        );
        // The transition is complete; holding a cross-process lock for the
        // daemon's whole foreground lifetime would make `net stop` impossible.
        drop(lifecycle_guard);
        let daemon_result = daemon_task
            .await
            .context("join foreground net daemon task")?;

        // A concurrent stop (or a new start after an unexpected daemon exit)
        // owns teardown while it holds the lock. Do not fight it or restore a
        // snapshot that a newer startup has replaced.
        let cleanup = match installer::acquire_lifecycle_lock(&lifecycle_serial) {
            Ok(_cleanup_guard) => {
                restore_and_consume_network_state_if_owned(serial, &startup_id).await?;
                Ok(())
            }
            Err(error) if lifecycle_busy(&error) => Ok(()),
            Err(error) => Err(error),
        };
        daemon_result?;
        return cleanup;
    }

    let mut daemon_child = match daemon::spawn(&cfg) {
        Ok(child) => child,
        Err(err) => {
            remove_device_network_state_if_owned(serial, &startup_id)?;
            return Err(err);
        }
    };
    let daemon_pid = daemon_child.id();
    let expected_daemon = DaemonIdentity {
        startup_id: Some(startup_id.clone()),
        pid: Some(daemon_pid),
    };
    if !daemon::await_ready(serial, &startup_id, daemon_pid, 5000).await {
        terminate_spawned_daemon(serial, &expected_daemon, &mut daemon_child, false).await?;
        remove_device_network_state_if_owned(serial, &startup_id)?;
        return daemon_start_timeout(serial, daemon_pid, &startup_id);
    }
    drop(host_allocation);
    if let Err(err) = setup_wiring(serial, port, host_port, expected_reverse_host_port).await {
        let graceful_requested = request_daemon_stop(serial).await;
        terminate_spawned_daemon(
            serial,
            &expected_daemon,
            &mut daemon_child,
            graceful_requested,
        )
        .await?;
        restore_and_consume_network_state_if_owned(serial, &startup_id).await?;
        return Err(err);
    }
    emit(
        "net_start",
        json!({
            "device": serial,
            "port": port,
            "host_port": host_port,
            "startup_id": startup_id,
            "proxy": format!("localhost:{port}"),
            "apps": apps,
            "anticache": anticache,
            "anticomp": anticomp,
            "verify_upstream": verify_upstream,
            "redact": redact,
            "ca": ca_cert.display().to_string(),
            "note": "net check verifies trust for a package; watch streams HTTP events alongside screen/crash events",
        }),
    );
    Ok(())
}

pub async fn stop(
    serial: &Serial,
    revoke_ca: bool,
    canary_host: &str,
    ca_cert: &Path,
) -> Result<()> {
    let lifecycle_serial = net_lifecycle_serial(serial);
    let _lifecycle_guard = installer::acquire_lifecycle_lock(&lifecycle_serial)?;
    let state = load_device_network_state(serial)?;
    let daemon_status = control::request(serial, json!({"op": "status"})).await.ok();
    if let Some(status) = &daemon_status {
        require_daemon_serial(status, serial)?;
    }
    let pid = control::daemon_pid(serial);
    let mut expected_daemon = DaemonIdentity::from_status(daemon_status.as_ref(), pid);
    if expected_daemon.startup_id.is_none() {
        expected_daemon.startup_id = state
            .as_ref()
            .map(|state| state.startup_id.clone())
            .filter(|startup_id| !startup_id.is_empty());
    }
    let initial_http_proxy = read_http_proxy(serial).await?;
    let already_stopped = daemon_status.is_none() && pid.is_none() && state.is_none();

    let daemon_evidence = daemon_status.is_some() || pid.is_some();
    let stop_requested = if daemon_status.is_some() {
        request_daemon_stop(serial).await
    } else {
        false
    };
    if daemon_evidence {
        if !stop_requested && expected_daemon.force_identity().is_none() {
            return Err(daemon_termination_error(
                serial,
                &expected_daemon,
                None,
                "the network daemon is unreachable and its pid/startup ownership cannot be proven safely",
            ));
        }
        complete_daemon_teardown(serial, &expected_daemon, stop_requested).await?;
    }
    let stopped = daemon_evidence;

    let mut warnings = Vec::<String>::new();
    let (http_proxy_restored, adb_reverse_restored, adb_reverse_removed, prior_http_proxy) =
        if let Some(state) = &state {
            let outcome = restore_network_state(serial, state).await?;
            remove_device_network_state(serial)?;
            warnings.extend(outcome.warnings);
            (
                outcome.http_proxy_restored,
                outcome.adb_reverse_restored,
                outcome.adb_reverse_removed,
                state.prior_http_proxy.clone(),
            )
        } else if let Some(status) = &daemon_status {
            // Compatibility cleanup for a daemon with no persisted snapshot. Only
            // remove wiring when both current fields still prove ownership; the
            // prior values are unknowable, so never touch an arbitrary localhost
            // proxy or a reverse mapping owned by another tool.
            let port = daemon_port_field(status, "port")?;
            let host_port = daemon_port_field(status, "host_port")?;
            let mut proxy_removed = false;
            let mut reverse_removed = false;
            if let (Some(port), Some(host_port)) = (port, host_port) {
                let current_proxy = read_http_proxy(serial).await?;
                let current_reverse = reverse_host_port(serial, port).await?;
                if proxy_points_at(&current_proxy, port) && current_reverse == Some(host_port) {
                    restore_http_proxy(serial, &None).await?;
                    // A stale reverse is harmless; a localhost proxy whose reverse
                    // has already disappeared breaks all device networking. Clear
                    // the proxy first so a partial failure remains recoverable.
                    adb::reverse_replace(serial, port, Some(host_port), None).await?;
                    proxy_removed = true;
                    reverse_removed = true;
                    warnings.push(
                    "no pre-proxy state was available; removed only wiring still proven to belong to this daemon"
                        .into(),
                );
                } else {
                    warnings.push(format!(
                    "preserved unowned device wiring (http_proxy {current_proxy:?}, reverse host {current_reverse:?}) because no pre-proxy snapshot exists"
                ));
                }
            } else {
                warnings.push(
                "preserved device wiring because the daemon did not expose enough metadata to prove ownership"
                    .into(),
            );
            }
            (proxy_removed, false, reverse_removed, None)
        } else {
            // Idempotent already-stopped path: do not overwrite a proxy setting
            // owned by the user or another tool.
            (false, false, false, initial_http_proxy)
        };

    let connectivity = connectivity_check(serial, canary_host).await;
    let connectivity_restored = connectivity.connectivity_restored;
    let raw_ip_check = serde_json::to_value(&connectivity.raw_ip)?;
    let dns_check = serde_json::to_value(&connectivity.dns)?;
    if !connectivity.connectivity_restored {
        warnings.push(format!(
            "device connectivity is degraded after proxy teardown (raw IP reachable: {}, DNS resolved: {}); run `shadowdroid doctor --fix` or repair the device network before continuing",
            connectivity.raw_ip.reachable, connectivity.dns.resolved
        ));
    }

    let ca_removed = if revoke_ca {
        crate::net::trust::remove(serial, ca_cert)
            .await
            .unwrap_or(false)
    } else {
        false
    };

    emit(
        "net_stop",
        json!({
            "device": serial,
            "stopped": stopped,
            "already_stopped": already_stopped,
            "http_proxy_restored": http_proxy_restored,
            "prior_http_proxy": prior_http_proxy,
            "current_http_proxy": read_http_proxy(serial).await?,
            "adb_reverse_restored": adb_reverse_restored,
            "adb_reverse_removed": adb_reverse_removed,
            "prior_reverse_host_port": state.as_ref().and_then(|state| state.prior_reverse_host_port),
            "raw_ip_check": raw_ip_check,
            "dns_check": dns_check,
            "connectivity_restored": connectivity_restored,
            "connectivity": connectivity,
            "revoke_ca": revoke_ca,
            "ca_removed": ca_removed,
            "warnings": warnings,
        }),
    );
    Ok(())
}

pub async fn status(serial: &Serial, ca_cert: Option<&Path>) -> Result<()> {
    let running = control::is_running(serial).await;
    let daemon = if running {
        control::request(serial, json!({"op": "status"})).await.ok()
    } else {
        None
    };
    let port = daemon
        .as_ref()
        .map_or(Ok(None), |status| daemon_port_field(status, "port"))?;
    let host_port = daemon
        .as_ref()
        .map_or(Ok(None), |status| daemon_port_field(status, "host_port"))?;

    let http_proxy = adb::shell(serial, "settings get global http_proxy")
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "null");

    let http_proxy_matches = port.is_some_and(|port| proxy_points_at(&http_proxy, port));
    let (adb_reverse_matches, adb_reverse_mappings, adb_reverse_error) = match (port, host_port) {
        (Some(device_port), Some(host_port)) => match adb::reverse_list(serial).await {
            Ok(mappings) => {
                let matches = reverse_mapping_matches(&mappings, device_port, host_port);
                let mappings = mappings
                    .into_iter()
                    .map(|mapping| json!({"device": mapping.device, "host": mapping.host}))
                    .collect::<Vec<_>>();
                (matches, mappings, None)
            }
            Err(err) => (false, Vec::new(), Some(err.to_string())),
        },
        _ => (false, Vec::new(), None),
    };
    let pointed = http_proxy_matches && adb_reverse_matches;

    emit(
        "net_status",
        json!({
            "device": serial,
            "running": running,
            "daemon": daemon,
            "http_proxy": http_proxy,
            "http_proxy_matches": http_proxy_matches,
            "adb_reverse_matches": adb_reverse_matches,
            "adb_reverse_mappings": adb_reverse_mappings,
            "adb_reverse_error": adb_reverse_error,
            "pointed_at_proxy": pointed,
            // The CA `net start` would use here (resolved), falling back to the
            // global CA when resolution isn't possible.
            "ca": ca_cert.map(|p| p.display().to_string()),
            "ca_generated": ca_cert
                .map(|p| p.exists())
                .unwrap_or_else(|| paths::ca_cert_path().map(|p| p.exists()).unwrap_or(false)),
        }),
    );
    Ok(())
}

fn reverse_mapping_matches(
    mappings: &[adb::ReverseMapping],
    device_port: u16,
    host_port: u16,
) -> bool {
    let expected_device = format!("tcp:{device_port}");
    let expected_host = format!("tcp:{host_port}");
    mappings
        .iter()
        .any(|mapping| mapping.device == expected_device && mapping.host == expected_host)
}

async fn reverse_host_port(serial: &Serial, device_port: u16) -> Result<Option<u16>> {
    let device = format!("tcp:{device_port}");
    let mapping = adb::reverse_list(serial)
        .await?
        .into_iter()
        .find(|mapping| mapping.device == device);
    let Some(mapping) = mapping else {
        return Ok(None);
    };
    let Some(host) = mapping.host.strip_prefix("tcp:") else {
        bail!(
            "cannot safely replace existing adb reverse {} -> {}; remove or relocate it first",
            mapping.device,
            mapping.host
        );
    };
    let port = host
        .parse::<u16>()
        .with_context(|| format!("parse existing adb reverse host endpoint {}", mapping.host))?;
    if port == 0 {
        bail!("existing adb reverse host port cannot be zero")
    }
    Ok(Some(port))
}

/// Point the device at the host proxy: `adb reverse` so the device's
/// `localhost:<port>` tunnels to the host's `localhost:<host_port>` (where the
/// daemon binds), then set the system `http_proxy` to the device-facing port.
/// `port` and `host_port` differ so concurrent devices share the device-side
/// port but each own a distinct host port.
async fn setup_wiring(
    serial: &Serial,
    port: u16,
    host_port: u16,
    expected_reverse_host_port: Option<u16>,
) -> Result<()> {
    adb::reverse_replace(serial, port, expected_reverse_host_port, Some(host_port)).await?;
    adb::shell_mutating(
        serial,
        format!("settings put global http_proxy localhost:{port}"),
    )
    .await?;
    Ok(())
}

async fn read_http_proxy(serial: &Serial) -> Result<Option<String>> {
    let raw = adb::shell(serial, "settings get global http_proxy").await?;
    Ok(parse_http_proxy(&raw))
}

fn parse_http_proxy(raw: &str) -> Option<String> {
    let value = raw.trim();
    (!value.is_empty() && value != "null").then(|| value.to_string())
}

fn proxy_points_at(value: &Option<String>, port: u16) -> bool {
    value.as_deref().is_some_and(|proxy| {
        proxy == format!("localhost:{port}")
            || proxy == format!("127.0.0.1:{port}")
            || proxy == format!("[::1]:{port}")
    })
}

fn device_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn restore_http_proxy(serial: &Serial, prior: &Option<String>) -> Result<()> {
    let command = match prior {
        Some(value) => format!(
            "settings put global http_proxy {}",
            device_shell_quote(value)
        ),
        None => "settings delete global http_proxy".to_string(),
    };
    adb::shell_mutating(serial, command).await?;
    Ok(())
}

#[derive(Debug, Default)]
struct RestoreNetworkOutcome {
    http_proxy_restored: bool,
    adb_reverse_restored: bool,
    adb_reverse_removed: bool,
    warnings: Vec<String>,
}

async fn restore_network_state(
    serial: &Serial,
    state: &DeviceNetworkState,
) -> Result<RestoreNetworkOutcome> {
    let mut outcome = RestoreNetworkOutcome::default();

    let current_reverse = reverse_host_port(serial, state.device_port).await?;
    let owns_reverse = current_reverse == Some(state.host_port);
    let current_proxy = read_http_proxy(serial).await?;
    let expected_proxy = format!("localhost:{}", state.device_port);
    if current_proxy == state.prior_http_proxy {
        // Already restored by a prior partial attempt (or independently).
    } else if current_proxy.as_deref() == Some(expected_proxy.as_str())
        && (owns_reverse || current_reverse.is_none())
    {
        // Restore the global proxy first. If the second ADB operation fails,
        // the remaining reverse is inert and a retry can safely finish it;
        // doing this in the opposite order can strand a live localhost proxy.
        // A missing reverse (common after reboot) also makes this exact proxy
        // endpoint provably dead, so recover it even though the pair is partial.
        restore_http_proxy(serial, &state.prior_http_proxy).await?;
        outcome.http_proxy_restored = true;
    } else {
        outcome.warnings.push(format!(
            "preserved http_proxy {:?} because the exact ShadowDroid wiring pair is no longer owned (expected proxy {expected_proxy:?}, reverse host tcp:{})",
            current_proxy, state.host_port
        ));
    }

    if current_reverse == state.prior_reverse_host_port {
        // Already restored/removed by a prior attempt.
    } else if owns_reverse {
        match state.prior_reverse_host_port {
            Some(previous) => {
                adb::reverse_replace(
                    serial,
                    state.device_port,
                    Some(state.host_port),
                    Some(previous),
                )
                .await?;
                outcome.adb_reverse_restored = true;
            }
            None => {
                adb::reverse_replace(serial, state.device_port, Some(state.host_port), None)
                    .await?;
                outcome.adb_reverse_removed = true;
            }
        }
    } else {
        outcome.warnings.push(format!(
            "preserved adb reverse tcp:{} because its current host {:?} no longer matches ShadowDroid's tcp:{} ownership",
            state.device_port, current_reverse, state.host_port
        ));
    }

    Ok(outcome)
}

fn network_state_owned_by(state: &DeviceNetworkState, startup_id: &str) -> bool {
    !startup_id.is_empty() && state.startup_id == startup_id
}

/// Restore a foreground/background startup snapshot only if it is still the
/// persisted snapshot for that exact startup. The caller holds the net
/// lifecycle lock, so the ownership check remains stable across the ADB awaits.
async fn restore_and_consume_network_state_if_owned(
    serial: &Serial,
    startup_id: &str,
) -> Result<bool> {
    let Some(state) = load_device_network_state(serial)? else {
        return Ok(false);
    };
    if !network_state_owned_by(&state, startup_id) {
        return Ok(false);
    }
    let outcome = restore_network_state(serial, &state).await?;
    for warning in outcome.warnings {
        tracing::warn!(warning);
    }
    remove_device_network_state(serial)?;
    Ok(true)
}

fn remove_device_network_state_if_owned(serial: &Serial, startup_id: &str) -> Result<bool> {
    let Some(state) = load_device_network_state(serial)? else {
        return Ok(false);
    };
    if !network_state_owned_by(&state, startup_id) {
        return Ok(false);
    }
    remove_device_network_state(serial)?;
    Ok(true)
}

fn load_device_network_state(serial: &Serial) -> Result<Option<DeviceNetworkState>> {
    let path = paths::device_state_path(serial)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let state: DeviceNetworkState =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if !matches!(state.schema_version, 1 | NETWORK_STATE_SCHEMA) || state.serial != serial.as_str()
    {
        bail!("invalid device network state in {}", path.display());
    }
    Ok(Some(state))
}

fn save_device_network_state(state: &DeviceNetworkState) -> Result<()> {
    paths::ensure_net_dir()?;
    let serial = Serial::from(state.serial.clone());
    let path = paths::device_state_path(&serial)?;
    let tmp = path.with_extension("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("install {}", path.display()))?;
    Ok(())
}

fn remove_device_network_state(serial: &Serial) -> Result<()> {
    let path = paths::device_state_path(serial)?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

async fn connectivity_check(serial: &Serial, canary_host: &str) -> ConnectivityCheck {
    let raw_ip = ping_check(serial, RAW_IP_CANARY).await;
    let dns = ping_check(serial, canary_host).await;
    ConnectivityCheck {
        connectivity_restored: raw_ip.reachable && dns.resolved,
        raw_ip,
        dns,
    }
}

async fn ping_check(serial: &Serial, host: &str) -> PingCheck {
    const EXIT_MARKER: &str = "__shadowdroid_ping_exit__:";
    let command = format!(
        "ping -c 1 -W 2 {} 2>&1; echo {EXIT_MARKER}$?",
        device_shell_quote(host)
    );
    let output = adb::shell(serial, command).await.unwrap_or_default();
    parse_ping_check(host, &output)
}

fn parse_ping_check(host: &str, output: &str) -> PingCheck {
    const EXIT_MARKER: &str = "__shadowdroid_ping_exit__:";
    let exit = output
        .lines()
        .find_map(|line| line.trim().strip_prefix(EXIT_MARKER))
        .and_then(|value| value.parse::<i32>().ok());
    let lower = output.to_lowercase();
    let name_error = [
        "unknown host",
        "bad address",
        "name or service not known",
        "temporary failure in name resolution",
        "ping: not found",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    PingCheck {
        host: host.to_string(),
        resolved: exit.is_some() && !name_error,
        reachable: exit == Some(0),
    }
}

// ── observe (task 8) ──────────────────────────────────────────

pub async fn log(serial: &Serial, matcher: Matcher, limit: usize) -> Result<()> {
    // One bounded pass interleaves completed flows with TLS-handshake failures,
    // so a "why is nothing captured?" moment shows the rejected host inline.
    let mut items = store::read_recent_timeline(serial, &matcher, limit)?;
    for v in &mut items {
        attach_recalled_tls_actions(serial, v);
        events::emit(v);
    }
    let ids = items
        .iter()
        .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    emit(
        "net_log",
        json!({"count": items.len(), "limit": limit, "ids": ids}),
    );
    Ok(())
}

fn attach_recalled_tls_actions(serial: &Serial, event: &mut serde_json::Value) {
    if event.get("type").and_then(serde_json::Value::as_str) == Some("tls_error")
        && event
            .get("next_actions")
            .and_then(serde_json::Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        event["next_actions"] = json!(crate::net::tls_error_next_actions(serial));
    }
}

pub async fn show(
    serial: &Serial,
    id: &str,
    body: bool,
    har: bool,
    body_file: Option<&Path>,
) -> Result<()> {
    // Completed flows live in the session log; a *held* (in-flight) flow lives
    // only in the daemon — try the store first, then ask the daemon.
    if let Some(flow) = store::find_by_id(serial, id)? {
        if har {
            events::emit_result(&json!({
                "format": "har",
                "id": id,
                "held": false,
                "har": crate::net::export::to_har(&[flow]),
            }));
            return Ok(());
        }
        if let Some(path) = body_file {
            return write_body_file(
                id,
                flow.resp_body.as_deref(),
                flow.resp_truncated,
                path,
                false,
            );
        }
        let mut detail = flow.detail(body);
        detail["held"] = json!(false);
        events::emit_result(&detail);
        return Ok(());
    }
    if control::is_running(serial).await {
        // Ask the daemon with bodies so `--body-file` works on a held flow too.
        if let Ok(reply) = control::request(serial, json!({"op": "show", "id": id})).await
            && let Some(flow_value) = reply.get("flow").filter(|v| !v.is_null())
        {
            let flow: crate::net::flow::FlowRecord = serde_json::from_value(flow_value.clone())
                .map_err(|error| {
                    crate::diagnostic::DiagnosticError::new(
                        "net_daemon_protocol",
                        "net",
                        format!("network daemon returned an invalid held flow: {error}"),
                    )
                    .retryable(true)
                    .detail(json!({"id": id, "reply": reply}))
                    .next_actions([
                        "run `shadowdroid net status` to verify the daemon version",
                        "restart the proxy, then retry `shadowdroid net show`",
                    ])
                })?;
            if har {
                events::emit_result(&json!({
                    "format": "har",
                    "id": id,
                    "held": true,
                    "har": crate::net::export::to_har(&[flow]),
                }));
                return Ok(());
            }
            if let Some(path) = body_file {
                return write_body_file(
                    id,
                    flow.resp_body.as_deref(),
                    flow.resp_truncated,
                    path,
                    true,
                );
            }
            let mut detail = flow.detail(body);
            detail["held"] = json!(true);
            events::emit_result(&detail);
            return Ok(());
        }
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "net_flow_not_found",
        "net",
        format!("no flow `{id}` exists in the session log or held set"),
    )
    .detail(json!({"id": id}))
    .next_actions([
        "run `shadowdroid net log` and choose an emitted flow id",
        "if the request is still in flight, run `shadowdroid net status` and retry",
    ])
    .into())
}

/// Write a flow's captured response body to `path` and emit a summary instead of
/// inlining a large body in the JSON. The body is whatever was stored (up to
/// [`crate::net::flow::BODY_CAP`]); `truncated` is surfaced so the caller knows
/// if the response exceeded the capture cap.
fn write_body_file(
    id: &str,
    resp_body: Option<&str>,
    truncated: bool,
    path: &Path,
    held: bool,
) -> Result<()> {
    let Some(b) = resp_body else {
        bail!(
            "flow `{id}` has no captured response body (binary, empty, or non-textual content-type)"
        );
    };
    std::fs::write(path, b).with_context(|| format!("writing {}", path.display()))?;
    emit(
        "net_show",
        json!({
            "id": id,
            "saved_body": path.display().to_string(),
            "bytes": b.len(),
            "truncated": truncated,
            "held": held,
        }),
    );
    Ok(())
}

// ── not yet implemented (later tasks) ─────────────────────────

pub async fn check(
    serial: &Serial,
    package: &str,
    tctx: &crate::net::trust::TrustContext,
) -> Result<()> {
    crate::net::check::run(serial, package, tctx).await
}

pub async fn trust(
    serial: &Serial,
    auto: bool,
    system: bool,
    ui: bool,
    tctx: &crate::net::trust::TrustContext,
) -> Result<()> {
    crate::net::trust::run(serial, auto, system, ui, tctx).await
}

// ── CA management (`net ca`) ──────────────────────────────────

/// Best-effort "is a proxy daemon live for this serial?" — used only to decide
/// whether to tell the user to restart it. `net ca` may run with no device
/// attached (empty sentinel serial), in which case there's nothing to check.
async fn proxy_running(serial: &Serial) -> bool {
    !serial.as_str().is_empty() && control::is_running(serial).await
}

/// `net ca import [--project|--global]` — install a user-provided CA as the
/// proxy's signing CA in the resolved scope.
pub async fn ca_import(
    serial: &Serial,
    dir: &Path,
    origin: &str,
    cert: &Path,
    key: Option<&Path>,
) -> Result<()> {
    // Project CA material must never be published until its ignore rules are
    // durable. Propagate failures so a read-only/broken project cannot receive
    // an unignored private key.
    let gitignore_added = if origin == "project" {
        crate::config::ensure_shadowdroid_gitignore(dir)?
    } else {
        Vec::new()
    };
    let (info, warnings) = crate::net::ca::import_into(dir, cert, key)?;
    // A new CA invalidates any cached "trusted" verdict for this device.
    crate::net::trust::clear_trust_cache(serial);

    // The device still trusts the *old* CA (or none), and a live daemon holds the
    // old CA in memory — spell out both so leaves actually validate.
    let mut next =
        vec!["run `shadowdroid net trust` so the device trusts the imported CA".to_string()];
    if proxy_running(serial).await {
        next.push(
            "restart the proxy (`net stop` then `net start`) — the running daemon still holds \
             the previous CA"
                .to_string(),
        );
    }

    emit(
        "net_ca_import",
        json!({
            "imported": true,
            "scope": origin,
            "dir": dir.display().to_string(),
            "ca": info,
            "warnings": warnings,
            "gitignore_added": gitignore_added,
            "backup": "the previous CA (if any) was saved alongside as ca.crt.bak / ca.key.bak",
            "next": next,
        }),
    );
    Ok(())
}

/// `net ca info [--project|--global]` — describe the CA in the resolved scope.
pub async fn ca_info(dir: &Path, origin: &str) -> Result<()> {
    let info = crate::net::ca::info_in(dir)?;
    let mut value = serde_json::to_value(&info)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("scope".into(), json!(origin));
    }
    emit("net_ca_info", value);
    Ok(())
}

/// `net ca reset [--project|--global]` — regenerate a fresh ShadowDroid CA in the
/// resolved scope (the current one is backed up). Also how a project CA is first
/// minted.
pub async fn ca_reset(serial: &Serial, dir: &Path, origin: &str) -> Result<()> {
    let gitignore_added = if origin == "project" {
        crate::config::ensure_shadowdroid_gitignore(dir)?
    } else {
        Vec::new()
    };
    let info = crate::net::ca::reset_in(dir)?;
    crate::net::trust::clear_trust_cache(serial);
    let mut next = vec!["re-run `shadowdroid net trust` to install the regenerated CA".to_string()];
    if proxy_running(serial).await {
        next.push(
            "restart the proxy (`net stop` then `net start`) so it uses the new CA".to_string(),
        );
    }
    emit(
        "net_ca_reset",
        json!({
            "reset": true,
            "scope": origin,
            "dir": dir.display().to_string(),
            "ca": info,
            "gitignore_added": gitignore_added,
            "backup": "the previous CA was saved alongside as ca.crt.bak / ca.key.bak",
            "next": next,
        }),
    );
    Ok(())
}

pub async fn export(
    serial: &Serial,
    format: &str,
    id: Option<String>,
    out: Option<PathBuf>,
) -> Result<()> {
    let flows = match &id {
        Some(id) => store::find_by_id(serial, id)?
            .map(|f| vec![f])
            .unwrap_or_default(),
        None => store::read_all(serial)?,
    };
    if flows.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "net_export_empty",
            "net",
            match id.as_deref() {
                Some(id) => format!("no completed flow `{id}` exists to export"),
                None => "no completed flows exist to export".to_string(),
            },
        )
        .detail(json!({"flow_id": id, "format": format}))
        .next_actions([
            "run `shadowdroid net log` and choose a completed flow id",
            "generate the request again while the proxy is running, then retry the export",
        ])
        .into());
    }
    match format {
        "curl" => {
            let out = out.unwrap_or_else(|| PathBuf::from("shadowdroid-network.curl.sh"));
            let mut script = String::from("#!/bin/sh\nset -eu\n\n");
            script.push_str(
                &flows
                    .iter()
                    .map(crate::net::export::curl_command)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            );
            script.push('\n');
            let bytes = crate::cmd::artifact::write_bytes(&out, script.as_bytes())?;
            let token = crate::events::shell_token(&out.display().to_string());
            emit(
                "net_export",
                json!({
                    "format": "curl",
                    "artifact": out.display().to_string(),
                    "bytes": bytes,
                    "count": flows.len(),
                    "replay": {
                        "command": format!("sh {token}"),
                        "requires_confirmation": true,
                        "side_effect": "replays captured requests and may repeat authenticated writes",
                    },
                    "next_actions": [
                        format!("cat {token}"),
                        "shadowdroid net log"
                    ],
                }),
            );
        }
        "har" => {
            let out = out.unwrap_or_else(|| PathBuf::from("shadowdroid-network.har"));
            let bytes =
                crate::cmd::artifact::write_json(&out, &crate::net::export::to_har(&flows))?;
            let token = crate::events::shell_token(&out.display().to_string());
            emit(
                "net_export",
                json!({
                    "format": "har",
                    "artifact": out.display().to_string(),
                    "bytes": bytes,
                    "count": flows.len(),
                    "next_actions": [
                        format!("jq '.log.entries | length' {token}"),
                        "shadowdroid net log"
                    ],
                }),
            );
        }
        "fixtures" => {
            let out = out.unwrap_or_else(|| PathBuf::from("shadowdroid-fixtures"));
            let summary = crate::net::export::write_fixtures(&flows, &out)?;
            events::emit_result(&summary);
        }
        other => bail!("unknown export format {other:?} (curl|har|fixtures)"),
    }
    Ok(())
}

pub async fn intercept(
    serial: &Serial,
    matcher: Matcher,
    at: String,
    hold_ms: u32,
    on_timeout: String,
) -> Result<()> {
    let reply = checked_control_reply("intercept", control::request(
        serial,
        json!({"op": "intercept", "matcher": matcher, "at": at, "hold_ms": hold_ms, "on_timeout": on_timeout}),
    )
    .await?)?;
    emit("net_intercept", reply);
    Ok(())
}

pub async fn resume(serial: &Serial, id: &str, mutation: Mutation) -> Result<()> {
    let reply = checked_control_reply(
        "resume",
        control::request(
            serial,
            json!({"op": "resume", "id": id, "mutation": mutation}),
        )
        .await?,
    )?;
    emit("net_resume", reply);
    Ok(())
}

pub async fn drop_flow(serial: &Serial, id: &str, status: Option<u16>) -> Result<()> {
    let reply = checked_control_reply(
        "drop",
        control::request(serial, json!({"op": "drop", "id": id, "status": status})).await?,
    )?;
    emit("net_drop", reply);
    Ok(())
}

pub async fn respond(
    serial: &Serial,
    id: &str,
    status: u16,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
) -> Result<()> {
    let reply = checked_control_reply("respond", control::request(
        serial,
        json!({"op": "respond", "id": id, "status": status, "body": body.unwrap_or_default(), "headers": headers}),
    )
    .await?)?;
    emit("net_respond", reply);
    Ok(())
}

pub async fn rule_add(serial: &Serial, spec: RuleSpec) -> Result<()> {
    let warning = map_remote_path_warning(&spec);
    let mut reply = checked_control_reply(
        "rule_add",
        control::request(serial, json!({"op": "rule_add", "spec": spec})).await?,
    )?;
    if let Some(w) = warning
        && let Some(obj) = reply.as_object_mut()
    {
        obj.insert("warning".into(), json!(w));
    }
    emit("net_rule_add", reply);
    Ok(())
}

pub async fn override_local(serial: &Serial, url_glob: &str, file: &Path) -> Result<()> {
    if !file.is_file() {
        bail!(
            "override file does not exist or is not a file: {}",
            file.display()
        );
    }
    let matcher = matcher_from_url_glob(url_glob);
    let spec = RuleSpec {
        kind: "map-local".into(),
        matcher,
        content_type: None,
        args: vec![file.display().to_string()],
    };
    let reply = checked_control_reply(
        "rule_add",
        control::request(serial, json!({"op": "rule_add", "spec": spec})).await?,
    )?;
    emit(
        "net_override",
        json!({
            "url": url_glob,
            "file": file.display().to_string(),
            "rule": reply,
            "hint": "equivalent to `net rule add map-local <file> --host <host> --path <path>`",
        }),
    );
    Ok(())
}

fn matcher_from_url_glob(url_glob: &str) -> Matcher {
    let mut raw = url_glob
        .trim()
        .trim_start_matches('*')
        .trim_end_matches('*')
        .to_string();
    if let Some((_, rest)) = raw.split_once("://") {
        raw = rest.to_string();
    }
    let (host, path) = match raw.split_once('/') {
        Some((host, path)) => (
            host.trim_matches('*'),
            format!("/{}", path.trim_matches('*')),
        ),
        None => (raw.trim_matches('*'), String::new()),
    };
    Matcher {
        host: (!host.is_empty()).then(|| host.to_string()),
        path: (!path.is_empty() && path != "/").then_some(path),
        ..Default::default()
    }
}

/// `map-remote` rewrites scheme+host only and keeps the original request path. If
/// the replacement URL carries its own path, that path is *prepended* to every
/// matched request path (→ duplicated segments like `/api/v2/api/v2/...`), which
/// is almost never intended. Warn so callers pass host+port only.
fn map_remote_path_warning(spec: &RuleSpec) -> Option<String> {
    if spec.kind != "map-remote" {
        return None;
    }
    let repl = spec.args.first()?;
    let after_scheme = repl.split_once("://").map(|(_, r)| r).unwrap_or(repl);
    let (_authority, path) = after_scheme.split_once('/')?;
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(format!(
        "map-remote replaces scheme+host only and keeps the original request path; the `/{path}` in `{repl}` will be prepended to every matched request (duplicated path segments). Pass host+port only unless you intend that."
    ))
}

pub async fn rule_list(serial: &Serial) -> Result<()> {
    let reply = checked_control_reply(
        "rule_list",
        control::request(serial, json!({"op": "rule_list"})).await?,
    )?;
    emit("net_rule_list", reply);
    Ok(())
}

pub async fn rule_rm(serial: &Serial, id: &str) -> Result<()> {
    let reply = checked_control_reply(
        "rule_rm",
        control::request(serial, json!({"op": "rule_rm", "id": id})).await?,
    )?;
    emit("net_rule_rm", reply);
    Ok(())
}

pub async fn rule_clear(serial: &Serial) -> Result<()> {
    let reply = checked_control_reply(
        "rule_clear",
        control::request(serial, json!({"op": "rule_clear"})).await?,
    )?;
    emit("net_rule_clear", reply);
    Ok(())
}

pub async fn rules_apply(serial: &Serial, file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    // Accept a JSON array of rule specs, or one spec per line.
    let specs: Vec<RuleSpec> = if text.trim_start().starts_with('[') {
        serde_json::from_str(&text).context("parse rules JSON array")?
    } else {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()
            .context("parse rules JSONL")?
    };
    let mut ids = Vec::new();
    for spec in &specs {
        let reply = checked_control_reply(
            "rule_add",
            control::request(serial, json!({"op": "rule_add", "spec": spec})).await?,
        )?;
        if let Some(id) = reply.get("id").and_then(|v| v.as_str()) {
            ids.push(id.to_string());
        }
    }
    emit(
        "net_rules_apply",
        json!({"applied": ids.len(), "ids": ids, "from": file.display().to_string()}),
    );
    Ok(())
}

pub async fn replay(serial: &Serial, from: &Path, host: Option<String>) -> Result<()> {
    let text = std::fs::read_to_string(from).with_context(|| format!("read {}", from.display()))?;
    let mut flows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if let Some(h) = &host {
        flows.retain(|f| {
            f.get("host")
                .and_then(|v| v.as_str())
                .map(|x| x.contains(h.as_str()))
                .unwrap_or(false)
        });
    }
    let count = flows.len();
    let reply = checked_control_reply(
        "replay",
        control::request(serial, json!({"op": "replay", "flows": flows})).await?,
    )?;
    emit(
        "net_replay",
        json!({"loaded": count, "from": from.display().to_string(), "daemon": reply}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Matcher;

    #[tokio::test]
    async fn project_ca_reset_does_not_publish_before_gitignore_preflight() {
        let dir = tempfile::tempdir().unwrap();
        // Invalid UTF-8 makes the existing ignore file unreadable through the
        // text-preserving updater. Reset must surface that error before it
        // creates either CA file.
        std::fs::write(dir.path().join(".gitignore"), [0xff]).unwrap();

        let error = ca_reset(&Serial::new(""), dir.path(), "project")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(".gitignore"), "{error}");
        assert!(!dir.path().join(paths::CA_CERT_FILE).exists());
        assert!(!dir.path().join(paths::CA_KEY_FILE).exists());
    }

    fn network_state(startup_id: &str) -> DeviceNetworkState {
        DeviceNetworkState {
            schema_version: NETWORK_STATE_SCHEMA,
            serial: "emulator-5554".into(),
            startup_id: startup_id.into(),
            prior_http_proxy: Some("proxy.example:8888".into()),
            prior_reverse_host_port: Some(40000),
            device_port: 8080,
            host_port: 49152,
            captured_at: 1.0,
        }
    }

    #[test]
    fn net_lifecycle_lock_uses_a_separate_device_namespace() {
        let serial = Serial::new("emulator-5554");
        assert_eq!(net_lifecycle_serial(&serial).as_str(), "net:emulator-5554");
    }

    #[test]
    fn legacy_network_state_defaults_to_unowned() {
        let legacy: DeviceNetworkState = serde_json::from_value(json!({
            "schema_version": NETWORK_STATE_SCHEMA,
            "serial": "emulator-5554",
            "prior_http_proxy": null,
            "device_port": 8080,
            "host_port": 49152,
            "captured_at": 1.0,
        }))
        .unwrap();
        assert!(legacy.startup_id.is_empty());
        assert!(!network_state_owned_by(&legacy, "new-startup"));

        let owned = network_state("startup-a");
        assert!(network_state_owned_by(&owned, "startup-a"));
        assert!(!network_state_owned_by(&owned, "startup-b"));
        assert!(!network_state_owned_by(&owned, ""));
    }

    #[test]
    fn daemon_identity_requires_exact_status_ownership() {
        let status = json!({"ok": true, "running": true, "startup_id": "startup-a", "pid": 42});
        let identity = DaemonIdentity::from_status(Some(&status), Some(7));
        assert_eq!(
            identity,
            DaemonIdentity {
                startup_id: Some("startup-a".into()),
                pid: Some(42),
            }
        );
        assert!(daemon_status_matches_identity(&status, &identity));
        assert!(!daemon_status_matches_identity(
            &json!({"ok": true, "running": true, "startup_id": "startup-b", "pid": 42}),
            &identity
        ));
        assert!(!daemon_status_matches_identity(
            &json!({"ok": true, "running": true, "startup_id": "startup-a", "pid": 43}),
            &identity
        ));
        assert!(!daemon_status_matches_identity(
            &json!({"ok": false, "running": true, "startup_id": "startup-a", "pid": 42}),
            &identity
        ));

        let overflowing = json!({
            "ok": true,
            "running": true,
            "startup_id": "startup-a",
            "pid": u64::from(u32::MAX) + 1,
        });
        assert_eq!(
            DaemonIdentity::from_status(Some(&overflowing), Some(7)).pid,
            Some(7)
        );
    }

    #[test]
    fn force_termination_requires_the_exact_daemon_command_identity() {
        let serial = Serial::new("emulator-5554");
        let command = "/opt/bin/shadowdroid net daemon --serial emulator-5554 \
                       --startup-id startup-a --port 8080";
        assert!(daemon_command_matches(command, &serial, "startup-a"));
        assert!(!daemon_command_matches(command, &serial, "startup-b"));
        assert!(!daemon_command_matches(
            command,
            &Serial::new("emulator-5556"),
            "startup-a"
        ));
        assert!(!daemon_command_matches(
            "/opt/bin/shadowdroid net daemon --serial emulator-5556 \
             --startup-id startup-a --ca-cert /tmp/emulator-5554/ca.crt",
            &serial,
            "startup-a"
        ));
        assert!(!daemon_command_matches(
            "unrelated --serial emulator-5554 --startup-id startup-a",
            &serial,
            "startup-a"
        ));
        assert_eq!(
            DaemonIdentity {
                startup_id: Some("startup-a".into()),
                pid: Some(42),
            }
            .force_identity(),
            Some(("startup-a", 42))
        );
        assert!(
            DaemonIdentity {
                startup_id: None,
                pid: Some(42),
            }
            .force_identity()
            .is_none()
        );
    }

    #[test]
    fn rejected_control_reply_is_a_nonzero_typed_failure() {
        let err =
            checked_control_reply("resume", json!({"ok": false, "error": "no such held flow"}))
                .unwrap_err();
        assert_eq!(crate::cli::error_code_of(&err), "net_daemon_rejected");
        assert!(err.to_string().contains("no such held flow"));
    }

    #[test]
    fn daemon_ports_reject_wrapping_protocol_values() {
        let status = json!({"ok": true, "port": 8080});
        assert_eq!(daemon_port_field(&status, "port").unwrap(), Some(8080));
        assert_eq!(daemon_port_field(&status, "host_port").unwrap(), None);

        for value in [json!(0), json!(u64::from(u16::MAX) + 1), json!("8080")] {
            let status = json!({"ok": true, "port": value});
            let error = daemon_port_field(&status, "port").unwrap_err();
            assert_eq!(crate::cli::error_code_of(&error), "net_daemon_protocol");
            assert_eq!(
                error
                    .downcast_ref::<crate::diagnostic::DiagnosticError>()
                    .unwrap()
                    .detail["field"],
                "port"
            );
        }
    }

    #[test]
    fn recalled_legacy_tls_errors_gain_device_scoped_actions() {
        let serial = Serial::new("emulator-5554; unsafe");
        let mut event = json!({
            "type": "tls_error",
            "ts": 2.0,
            "host": "api.example.com",
            "reason": "rejected"
        });
        attach_recalled_tls_actions(&serial, &mut event);
        let actions = event["next_actions"].as_array().unwrap();
        assert!(!actions.is_empty());
        assert!(actions.iter().all(|action| {
            action
                .as_str()
                .unwrap()
                .starts_with("shadowdroid -d 'emulator-5554; unsafe' net ")
        }));
    }

    fn spec(kind: &str, arg: &str) -> RuleSpec {
        RuleSpec {
            kind: kind.into(),
            matcher: Matcher::default(),
            content_type: None,
            args: vec![arg.into()],
        }
    }

    #[test]
    fn map_remote_warns_only_when_replacement_has_a_path() {
        // host+port only (with or without scheme, trailing slash) — no warning.
        assert!(map_remote_path_warning(&spec("map-remote", "localhost:8080")).is_none());
        assert!(map_remote_path_warning(&spec("map-remote", "https://localhost:8080")).is_none());
        assert!(map_remote_path_warning(&spec("map-remote", "https://localhost:8080/")).is_none());

        // A path in the replacement is the foot-gun — warn and name the path.
        let w = map_remote_path_warning(&spec(
            "map-remote",
            "http://localhost:8080/device-ips/screens/v2",
        ))
        .expect("path should trigger a warning");
        assert!(w.contains("/device-ips/screens/v2"));

        // Other rule kinds never warn.
        assert!(map_remote_path_warning(&spec("set-request-header", "x-debug")).is_none());
    }

    #[test]
    fn url_glob_to_matcher_extracts_host_and_path() {
        let m = matcher_from_url_glob("https://api.example.com/v1/dict*");
        assert_eq!(m.host.as_deref(), Some("api.example.com"));
        assert_eq!(m.path.as_deref(), Some("/v1/dict"));

        let m = matcher_from_url_glob("*.example.com");
        assert_eq!(m.host.as_deref(), Some(".example.com"));
        assert_eq!(m.path, None);
    }

    #[test]
    fn proxy_state_preserves_absent_disabled_and_custom_values() {
        assert_eq!(parse_http_proxy("null\n"), None);
        assert_eq!(parse_http_proxy("  \n"), None);
        assert_eq!(parse_http_proxy(":0\n").as_deref(), Some(":0"));
        assert_eq!(
            parse_http_proxy("proxy.example:3128\n").as_deref(),
            Some("proxy.example:3128")
        );
        assert!(proxy_points_at(&Some("localhost:8080".into()), 8080));
        assert!(!proxy_points_at(&Some("proxy.example:8080".into()), 8080));
    }

    #[test]
    fn reverse_status_requires_both_expected_endpoints() {
        let mappings = vec![adb::ReverseMapping {
            device: "tcp:8080".into(),
            host: "tcp:43127".into(),
        }];
        assert!(reverse_mapping_matches(&mappings, 8080, 43127));
        assert!(!reverse_mapping_matches(&mappings, 8081, 43127));
        assert!(!reverse_mapping_matches(&mappings, 8080, 43128));
        assert!(!reverse_mapping_matches(&[], 8080, 43127));
    }

    #[test]
    fn ping_result_separates_dns_resolution_from_reachability() {
        let ok = parse_ping_check(
            "example.com",
            "64 bytes from 93.184.216.34\n__shadowdroid_ping_exit__:0\n",
        );
        assert!(ok.resolved);
        assert!(ok.reachable);

        let blocked_icmp = parse_ping_check(
            "example.com",
            "PING example.com (93.184.216.34)\n__shadowdroid_ping_exit__:1\n",
        );
        assert!(blocked_icmp.resolved);
        assert!(!blocked_icmp.reachable);

        let dns_failure = parse_ping_check(
            "example.com",
            "ping: unknown host example.com\n__shadowdroid_ping_exit__:2\n",
        );
        assert!(!dns_failure.resolved);
        assert!(!dns_failure.reachable);
    }
}
