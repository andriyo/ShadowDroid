//! Public `video` lifecycle commands.

use super::control;
use super::daemon;
use super::paths;
use super::session::{self, ActiveState, Bundle};
use super::{DaemonArgs, RecordArgs, StartArgs};
use crate::ids::Serial;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

pub async fn start(
    serial: &Serial,
    args: &StartArgs,
    redact: bool,
    redaction: crate::redaction::PolicySpec,
) -> Result<Value> {
    let lifecycle = Serial::new(format!("video:{serial}"));
    let _guard = crate::device::installer::acquire_lifecycle_lock(&lifecycle)?;
    match control::probe_status(serial).await {
        control::StatusProbe::Running(status) => {
            return Err(already_recording(serial, status));
        }
        control::StatusProbe::Unreachable(error) => {
            if session::read_active(serial)?.as_ref().is_some_and(|state| {
                !is_terminal(&state.state)
                    || matches!(
                        inspect_host_daemon(state),
                        ProcessIdentity::Matching | ProcessIdentity::Unknown(_)
                    )
            }) {
                return Err(control_unavailable(serial, &error));
            }
        }
        control::StatusProbe::Absent => {}
    }
    reject_unresolved_session(serial)?;

    // Fail on unsupported options before claiming the destination.
    let capabilities = super::backend::probe(serial).await?;
    super::backend::validate_capture(&args.capture, &capabilities)?;
    let bundle = Bundle::create(&absolute_output(&args.out)?)?;
    let session_id = unique_id("v");
    let startup_id = unique_id("s");
    let config = DaemonArgs {
        serial: serial.to_string(),
        startup_id: startup_id.clone(),
        session_id: session_id.clone(),
        out: bundle.root.clone(),
        capture: args.capture.clone(),
        capture_redact: redact,
        redaction_json_key: redaction.json_keys,
        redaction_pattern: redaction.patterns,
    };
    let mut child = spawn(&config)?;
    let daemon_pid = child.id();
    if let Some(status) = await_ready(
        serial,
        &startup_id,
        &session_id,
        daemon_pid,
        Duration::from_secs(12),
    )
    .await
    {
        return Ok(json!({
            "session_id": session_id,
            "device": serial.as_str(),
            "running": true,
            "already_running": false,
            "backend": status.get("backend"),
            "bundle": bundle.root.display().to_string(),
            "manifest": bundle.manifest_path.display().to_string(),
            "timeline": bundle.timeline_path.display().to_string(),
            "started_at": status.get("started_at"),
            "capture": {
                "size": args.capture.size,
                "bit_rate_bps": args.capture.bit_rate,
                "display_id": args.capture.display_id,
                "bugreport": args.capture.bugreport,
                "segment_seconds": args.capture.segment_seconds,
                "split_on_rotation": !args.capture.no_split_on_rotation,
                "audio_included": false,
            },
            "contains_sensitive_data": true,
            "potentially_sensitive": true,
            "encrypted": false,
            "redaction": {
                "metadata": if redact { "marker_labels_only" } else { "not_requested" },
                "video_pixels": false,
            },
            "warnings": [
                "screenrecord captures video only; audio is not included",
                "video pixels are not redacted and may contain sensitive information",
            ],
        }));
    }
    terminate_owned_child(&mut child);
    let log = paths::log(serial)?;
    Err(crate::diagnostic::DiagnosticError::new(
        "video_start_timeout",
        "video",
        "video daemon did not become ready within 12 seconds",
    )
    .retryable(true)
    .detail(json!({
        "device": serial.as_str(),
        "session_id": session_id,
        "startup_id": startup_id,
        "daemon_pid": daemon_pid,
        "bundle": bundle.root,
        "log": log,
        "log_tail": log_tail(&log, 20),
        "timeout_ms": 12_000,
    }))
    .next_actions([
        format!(
            "tail -n 50 {}",
            crate::events::shell_token(&log.display().to_string())
        ),
        "shadowdroid video status".to_string(),
        "shadowdroid video stop".to_string(),
        "shadowdroid doctor --json".to_string(),
    ])
    .into())
}

pub async fn record(
    serial: &Serial,
    args: &RecordArgs,
    redact: bool,
    redaction: crate::redaction::PolicySpec,
) -> Result<()> {
    let duration = args.duration.as_deref().map(parse_duration).transpose()?;
    let start_args = StartArgs {
        out: args.out.clone(),
        capture: args.capture.clone(),
    };
    let started = start(serial, &start_args, redact, redaction).await?;
    let stop_reason = wait_for_recording(serial, duration).await;
    let mut stopped = stop_value(serial, &stop_reason).await?;
    if let Value::Object(fields) = &mut stopped {
        fields.insert("mode".into(), "foreground".into());
        fields.insert("stop_reason".into(), stop_reason.into());
        fields.insert("started".into(), started);
    }
    crate::events::emit_action("video_record", &stopped);
    Ok(())
}

pub async fn status(serial: &Serial) -> Result<()> {
    let value = match control::probe_status(serial).await {
        control::StatusProbe::Running(mut live) => {
            if let Value::Object(fields) = &mut live {
                fields.remove("ok");
                fields.insert("backend_alive".into(), true.into());
            }
            live
        }
        control::StatusProbe::Absent => inactive_status(serial, None)?,
        control::StatusProbe::Unreachable(error) => {
            let mut value = inactive_status(serial, Some(&error))?;
            if let Some(active) = session::read_active(serial)?
                && matches!(inspect_host_daemon(&active), ProcessIdentity::Matching)
                && let Value::Object(fields) = &mut value
            {
                fields.insert("running".into(), true.into());
                fields.insert("state".into(), "control_unavailable".into());
                fields.insert("backend_alive".into(), true.into());
            }
            value
        }
    };
    crate::events::emit_action("video_status", &value);
    Ok(())
}

pub async fn mark(serial: &Serial, label: &str) -> Result<()> {
    if label.trim().is_empty() || label.chars().count() > 1000 {
        return Err(crate::diagnostic::DiagnosticError::new(
            "video_invalid_marker",
            "input",
            "video marker label must contain 1 to 1000 characters",
        )
        .detail(json!({"label_length": label.chars().count()}))
        .next_actions([
            "shadowdroid video status",
            "shadowdroid commands --json --describe 'video mark'",
        ])
        .into());
    }
    let response = control::request(serial, json!({"op": "mark", "label": label}))
        .await
        .map_err(|error| {
            if session::read_active(serial)
                .ok()
                .flatten()
                .is_some_and(|state| !is_terminal(&state.state))
            {
                control_unavailable(serial, &format!("{error:#}"))
            } else {
                session_not_active(serial)
            }
        })?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(crate::diagnostic::DiagnosticError::new(
            response
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("video_mark_failed"),
            "video",
            response
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("video marker was rejected"),
        )
        .detail(response)
        .next_actions(["shadowdroid video status", "shadowdroid video stop"])
        .into());
    }
    let mut body = response;
    if let Value::Object(fields) = &mut body {
        fields.remove("ok");
    }
    crate::events::emit_action("video_mark", &body);
    Ok(())
}

pub async fn stop(serial: &Serial, reason: &str) -> Result<()> {
    let value = stop_value(serial, reason).await?;
    crate::events::emit_action("video_stop", &value);
    Ok(())
}

async fn stop_value(serial: &Serial, reason: &str) -> Result<Value> {
    let lifecycle = Serial::new(format!("video:{serial}"));
    let _guard = crate::device::installer::acquire_lifecycle_lock(&lifecycle)?;
    let probe = control::probe_status(serial).await;
    let active = session::read_active(serial)?;
    let terminal_settled = active.as_ref().is_some_and(|state| {
        is_terminal(&state.state)
            && matches!(
                inspect_host_daemon(state),
                ProcessIdentity::Missing | ProcessIdentity::Mismatched(_)
            )
    });
    if !matches!(&probe, control::StatusProbe::Running(_)) && (active.is_none() || terminal_settled)
    {
        let summary = active
            .as_ref()
            .and_then(|state| session::manifest_from_bundle(&state.bundle).ok())
            .map(|manifest| session::artifact_summary(&active.as_ref().unwrap().bundle, &manifest));
        return Ok(json!({
            "stopped": false,
            "already_stopped": true,
            "device": serial.as_str(),
            "last_session": summary,
            "stop_reason": reason,
        }));
    }

    if matches!(&probe, control::StatusProbe::Running(_)) {
        let response = control::request(serial, json!({"op": "stop", "reason": reason})).await?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(stop_failed(
                serial,
                active.as_ref(),
                "video daemon rejected stop",
            ));
        }
    } else if let Some(state) = &active {
        match inspect_host_daemon(state) {
            ProcessIdentity::Matching => signal_host_daemon(state)?,
            ProcessIdentity::Missing => {
                daemon::recover(serial, state.clone()).await?;
            }
            ProcessIdentity::Mismatched(command) => {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "video_ownership_unproven",
                    "video",
                    "refusing to signal a host process that does not match the recorded video daemon identity",
                )
                .detail(json!({
                    "device": serial.as_str(),
                    "expected_pid": state.daemon_pid,
                    "startup_id": state.startup_id,
                    "process_command": command,
                }))
                .next_actions([
                    "shadowdroid video status",
                    "shadowdroid doctor --json",
                    "inspect the daemon process before manual intervention",
                ])
                .into());
            }
            ProcessIdentity::Unknown(error) => {
                return Err(stop_failed(
                    serial,
                    Some(state),
                    &format!("could not verify video daemon ownership: {error}"),
                ));
            }
        }
    }

    let finalized = wait_for_finalization(serial, Duration::from_secs(180)).await?;
    let manifest = session::manifest_from_bundle(&finalized.bundle)?;
    let mut summary = session::artifact_summary(&finalized.bundle, &manifest);
    if let Value::Object(fields) = &mut summary {
        fields.insert("stopped".into(), true.into());
        fields.insert("already_stopped".into(), false.into());
        fields.insert("finalized".into(), is_terminal(&manifest.state).into());
        fields.insert("stop_reason".into(), reason.into());
        fields.insert(
            "pulled".into(),
            (!manifest.segments.is_empty()
                && manifest
                    .segments
                    .iter()
                    .all(|segment| segment.state == "complete"))
            .into(),
        );
        fields.insert(
            "device_temp_removed".into(),
            (!manifest.segments.is_empty()
                && manifest
                    .segments
                    .iter()
                    .all(|segment| segment.remote_cleaned))
            .into(),
        );
    }
    Ok(summary)
}

async fn wait_for_recording(serial: &Serial, duration: Option<Duration>) -> String {
    let started = tokio::time::Instant::now();
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return "interrupt".into(),
            _ = interval.tick() => {
                if duration.is_some_and(|duration| started.elapsed() >= duration) {
                    return "duration".into();
                }
                match control::probe_status(serial).await {
                    control::StatusProbe::Running(_) => {}
                    control::StatusProbe::Absent => return "backend_exit".into(),
                    control::StatusProbe::Unreachable(_) => return "control_error".into(),
                }
            }
        }
    }
}

async fn wait_for_finalization(serial: &Serial, timeout: Duration) -> Result<ActiveState> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(state) = session::read_active(serial)? {
            let control_alive = match control::probe_status(serial).await {
                control::StatusProbe::Running(_) => true,
                control::StatusProbe::Absent => false,
                control::StatusProbe::Unreachable(_) => {
                    matches!(inspect_host_daemon(&state), ProcessIdentity::Matching)
                }
            };
            if is_terminal(&state.state) && !control_alive {
                return Ok(state);
            }
            if !control_alive && matches!(inspect_host_daemon(&state), ProcessIdentity::Missing) {
                return daemon::recover(serial, state).await;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let active = session::read_active(serial)?;
            return Err(stop_failed(
                serial,
                active.as_ref(),
                "video did not finalize within 180 seconds",
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn await_ready(
    serial: &Serial,
    startup_id: &str,
    session_id: &str,
    daemon_pid: u32,
    timeout: Duration,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let control::StatusProbe::Running(status) = control::probe_status(serial).await
            && status.get("startup_id").and_then(Value::as_str) == Some(startup_id)
            && status.get("session_id").and_then(Value::as_str) == Some(session_id)
            && status.get("daemon_pid").and_then(Value::as_u64) == Some(daemon_pid as u64)
            && status.get("ready").and_then(Value::as_bool) == Some(true)
        {
            return Some(status);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
}

fn spawn(config: &DaemonArgs) -> Result<std::process::Child> {
    paths::ensure_dir()?;
    let executable = std::env::current_exe().context("resolve current executable")?;
    let log_path = paths::log(&Serial::new(&config.serial))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    paths::protect_file(&log_path)?;
    let log_copy = log.try_clone()?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("video")
        .arg("daemon")
        .arg("--serial")
        .arg(&config.serial)
        .arg("--startup-id")
        .arg(&config.startup_id)
        .arg("--session-id")
        .arg(&config.session_id)
        .arg("--out")
        .arg(&config.out)
        .arg("--backend")
        .arg(config.capture.backend.as_str())
        .arg("--segment-seconds")
        .arg(config.capture.segment_seconds.to_string());
    if let Some(size) = &config.capture.size {
        command.arg("--size").arg(size);
    }
    if let Some(bit_rate) = config.capture.bit_rate {
        command.arg("--bit-rate").arg(bit_rate.to_string());
    }
    if let Some(display_id) = config.capture.display_id {
        command.arg("--display-id").arg(display_id.to_string());
    }
    if config.capture.bugreport {
        command.arg("--bugreport");
    }
    if config.capture.no_split_on_rotation {
        command.arg("--no-split-on-rotation");
    }
    if config.capture_redact {
        command.arg("--capture-redact");
        for key in &config.redaction_json_key {
            command.arg("--redaction-json-key").arg(key);
        }
        for pattern in &config.redaction_pattern {
            command.arg("--redaction-pattern").arg(pattern);
        }
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_copy));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().context("spawn video daemon process")
}

fn absolute_output(path: &Path) -> Result<std::path::PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn unique_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{prefix}{nanos:x}{:x}{count:x}", std::process::id())
}

fn parse_duration(value: &str) -> Result<Duration> {
    let trimmed = value.trim();
    let seconds = if let Some(number) = trimmed.strip_suffix("ms") {
        let millis: f64 = number.parse().map_err(|_| invalid_duration(value))?;
        millis / 1000.0
    } else {
        crate::cmd::log::parse_duration_secs(trimmed).map_err(|_| invalid_duration(value))?
    };
    if !seconds.is_finite() || seconds < 1.0 {
        return Err(invalid_duration(value));
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| invalid_duration(value))
}

fn invalid_duration(value: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "video_invalid_duration",
        "input",
        "video duration must be at least one second and fit the platform timer",
    )
    .detail(json!({"duration": value}))
    .next_actions([
        "rerun with a duration such as `--duration 30s`",
        "shadowdroid commands --json --describe 'video record'",
    ])
    .into()
}

fn reject_unresolved_session(serial: &Serial) -> Result<()> {
    let Some(active) = session::read_active(serial)? else {
        return Ok(());
    };
    if is_terminal(&active.state)
        && matches!(
            inspect_host_daemon(&active),
            ProcessIdentity::Missing | ProcessIdentity::Mismatched(_)
        )
    {
        return Ok(());
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "video_already_recording",
        "video",
        "this device already has an unfinished video session",
    )
    .detail(json!({
        "device": serial.as_str(),
        "session_id": active.session_id,
        "state": active.state,
        "bundle": active.bundle,
        "daemon_pid": active.daemon_pid,
        "device_process": active.device_process,
    }))
    .next_actions([
        "shadowdroid video status",
        "shadowdroid video mark 'before stop'",
        "shadowdroid video stop",
    ])
    .into())
}

fn already_recording(serial: &Serial, status: Value) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "video_already_recording",
        "video",
        "a video recording is already active on this device",
    )
    .detail(json!({"device": serial.as_str(), "status": status}))
    .next_actions([
        "shadowdroid video status",
        "shadowdroid video mark 'checkpoint'",
        "shadowdroid video stop",
    ])
    .into()
}

fn inactive_status(serial: &Serial, control_error: Option<&str>) -> Result<Value> {
    if let Some(active) = session::read_active(serial)? {
        let terminal = is_terminal(&active.state);
        let mut warnings = if terminal {
            Vec::new()
        } else {
            vec!["the video daemon is unavailable, but recoverable session state remains".into()]
        };
        if let Some(error) = control_error {
            warnings.push(format!("video control endpoint is unavailable: {error}"));
        }
        Ok(json!({
            "running": false,
            "state": if terminal { Value::Null } else { Value::String("recoverable".into()) },
            "last_state": active.state,
            "session_id": active.session_id,
            "device": serial.as_str(),
            "backend_alive": false,
            "bundle": active.bundle,
            "manifest": active.bundle.join("manifest.json"),
            "timeline": active.bundle.join("events.jsonl"),
            "last_error": active.last_error,
            "control_error": control_error,
            "warnings": warnings,
        }))
    } else {
        Ok(json!({
            "running": false,
            "state": null,
            "session_id": null,
            "device": serial.as_str(),
            "backend_alive": false,
            "control_error": control_error,
            "warnings": control_error
                .map(|error| vec![format!("stale or unreadable video control marker: {error}")])
                .unwrap_or_default(),
        }))
    }
}

fn control_unavailable(serial: &Serial, error: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "video_control_unavailable",
        "video",
        "video daemon control endpoint exists but is not responding correctly",
    )
    .retryable(true)
    .detail(json!({"device": serial.as_str(), "error": error}))
    .next_actions([
        "shadowdroid video status",
        "shadowdroid video stop",
        "shadowdroid doctor --json",
    ])
    .into()
}

fn session_not_active(serial: &Serial) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "video_session_not_active",
        "video",
        "there is no active video recording to mark",
    )
    .detail(json!({"device": serial.as_str()}))
    .next_actions([
        "shadowdroid video status",
        "shadowdroid video start --out shadowdroid-video",
    ])
    .into()
}

fn stop_failed(serial: &Serial, active: Option<&ActiveState>, message: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new("video_stop_failed", "video", message)
        .retryable(true)
        .detail(json!({
            "device": serial.as_str(),
            "session": active,
        }))
        .next_actions([
            "retry `shadowdroid video stop`",
            "shadowdroid video status",
            "shadowdroid doctor --json",
        ])
        .into()
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "completed" | "partial" | "failed" | "interrupted")
}

#[derive(Debug)]
enum ProcessIdentity {
    Matching,
    Missing,
    Mismatched(String),
    Unknown(String),
}

fn inspect_host_daemon(state: &ActiveState) -> ProcessIdentity {
    if state.daemon_pid == 0 || state.startup_id.is_empty() {
        return ProcessIdentity::Unknown("incomplete daemon identity".into());
    }
    #[cfg(unix)]
    {
        let output = match std::process::Command::new("ps")
            .args(["-ww", "-p", &state.daemon_pid.to_string(), "-o", "command="])
            .output()
        {
            Ok(output) => output,
            Err(error) => return ProcessIdentity::Unknown(error.to_string()),
        };
        if !output.status.success() {
            return if output.status.code() == Some(1) {
                ProcessIdentity::Missing
            } else {
                ProcessIdentity::Unknown(String::from_utf8_lossy(&output.stderr).trim().to_string())
            };
        }
        let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if command.is_empty() {
            return ProcessIdentity::Missing;
        }
        if daemon_command_matches(&command, state) {
            ProcessIdentity::Matching
        } else {
            ProcessIdentity::Mismatched(command)
        }
    }
    #[cfg(windows)]
    {
        let script = format!(
            "$p = Get-CimInstance Win32_Process -Filter 'ProcessId = {}'; \
             if ($null -eq $p) {{ exit 3 }}; [Console]::Out.Write($p.CommandLine)",
            state.daemon_pid
        );
        let output = match std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
        {
            Ok(output) => output,
            Err(error) => return ProcessIdentity::Unknown(error.to_string()),
        };
        if output.status.code() == Some(3) {
            return ProcessIdentity::Missing;
        }
        if !output.status.success() {
            return ProcessIdentity::Unknown(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            );
        }
        let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if daemon_command_matches(&command, state) {
            ProcessIdentity::Matching
        } else {
            ProcessIdentity::Mismatched(command)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        ProcessIdentity::Unknown("process inspection is unsupported on this platform".into())
    }
}

fn daemon_command_matches(command: &str, state: &ActiveState) -> bool {
    let tokens = command
        .split_whitespace()
        .map(|token| token.trim_matches(['\'', '"']))
        .collect::<Vec<_>>();
    tokens.iter().any(|token| token.contains("shadowdroid"))
        && tokens.windows(2).any(|pair| pair == ["video", "daemon"])
        && tokens
            .windows(2)
            .any(|pair| pair[0] == "--serial" && pair[1] == state.serial)
        && tokens
            .windows(2)
            .any(|pair| pair[0] == "--startup-id" && pair[1] == state.startup_id)
        && tokens
            .windows(2)
            .any(|pair| pair[0] == "--session-id" && pair[1] == state.session_id)
}

fn signal_host_daemon(state: &ActiveState) -> Result<()> {
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .args(["-INT", &state.daemon_pid.to_string()])
            .status()
            .context("signal video daemon")?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("operating system rejected video daemon signal"))
        }
    }
    #[cfg(windows)]
    {
        Err(crate::diagnostic::DiagnosticError::new(
            "video_control_unavailable",
            "video",
            "the Windows video daemon control endpoint is unavailable; refusing a hard process kill that could orphan the device recorder",
        )
        .retryable(true)
        .detail(json!({
            "device": state.serial,
            "daemon_pid": state.daemon_pid,
        }))
        .next_actions([
            "retry `shadowdroid video stop`",
            "shadowdroid video status",
            "wait for the current segment limit, then retry recovery",
        ])
        .into())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(anyhow!(
            "video daemon signalling is unsupported on this platform"
        ))
    }
}

fn terminate_owned_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    for _ in 0..150 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn log_tail(path: &Path, count: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(count);
    Some(lines[start..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_accepts_human_units() {
        assert!(parse_duration("500ms").is_err());
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        for invalid in ["", "0", "-1s", "forever", "1e300s"] {
            assert!(parse_duration(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn daemon_identity_requires_every_owned_token() {
        let state = ActiveState {
            schema_version: 1,
            serial: "emulator-5554".into(),
            startup_id: "s1".into(),
            session_id: "v1".into(),
            daemon_pid: 42,
            control_port: Some(12345),
            bundle: "/tmp/video".into(),
            remote_dir: "/data/local/tmp/video/v1".into(),
            state: "recording".into(),
            started_at: 0.0,
            current_segment: Some(1),
            device_process: None,
            last_error: None,
        };
        assert!(daemon_command_matches(
            "shadowdroid video daemon --serial emulator-5554 --startup-id s1 --session-id v1",
            &state
        ));
        assert!(!daemon_command_matches(
            "shadowdroid video daemon --serial other --startup-id s1 --session-id v1",
            &state
        ));
    }
}
