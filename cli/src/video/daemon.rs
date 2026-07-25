//! Detached per-device recording daemon.

use super::backend;
use super::paths;
use super::session::{
    self, ActiveState, Bundle, DeviceProcessIdentity, Gap, Manifest, Marker, Segment,
};
use super::{CaptureArgs, DaemonArgs};
use crate::ids::Serial;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct Shared {
    serial: Serial,
    startup_id: String,
    bundle: Bundle,
    manifest: Manifest,
    active: ActiveState,
    ready: bool,
    segment_started_at: Option<f64>,
    stop_reason: Option<String>,
}

impl Shared {
    fn persist(&mut self) -> Result<()> {
        self.manifest.capture.elapsed_ms = self.manifest.elapsed_ms();
        self.bundle.write_manifest(&self.manifest)?;
        session::write_active(&self.serial, &self.active)
    }

    fn status(&self) -> Value {
        let elapsed_ms = self.manifest.elapsed_ms();
        let current_segment = self.active.current_segment.and_then(|index| {
            self.manifest
                .segments
                .iter()
                .find(|item| item.index == index)
        });
        json!({
            "ok": true,
            "running": true,
            "ready": self.ready,
            "state": self.active.state,
            "device": self.serial.as_str(),
            "startup_id": self.startup_id,
            "session_id": self.manifest.session_id,
            "daemon_pid": self.active.daemon_pid,
            "started_at": self.manifest.capture.started_at,
            "backend": self.manifest.backend,
            "elapsed_ms": elapsed_ms,
            "bundle": self.bundle.root.display().to_string(),
            "manifest": self.bundle.manifest_path.display().to_string(),
            "timeline": self.bundle.timeline_path.display().to_string(),
            "current_segment": current_segment,
            "segments_complete": self.manifest.segments.iter().filter(|segment| segment.state == "complete").count(),
            "warnings": self.manifest.warnings,
            "contains_sensitive_data": true,
            "encrypted": false,
            "redaction": {
                "metadata": self.manifest.privacy.metadata_redaction,
                "video_pixels": false,
            },
        })
    }
}

pub async fn run(args: DaemonArgs) -> Result<()> {
    let serial = Serial::new(args.serial.clone());
    paths::ensure_dir()?;
    crate::redaction::configure(
        args.capture_redact,
        crate::redaction::PolicySpec {
            json_keys: args.redaction_json_key.clone(),
            patterns: args.redaction_pattern.clone(),
        },
    )?;

    let capabilities = backend::probe(&serial).await?;
    let parsed_size = backend::validate_capture(&args.capture, &capabilities)?;
    let executable = capabilities.executable.clone();
    let device = backend::device_metadata(&serial).await;
    let bundle = Bundle::open(args.out.clone());
    let remote_dir = format!(
        "/data/local/tmp/shadowdroid-video/{}",
        safe_session_component(&args.session_id)?
    );
    create_remote_session_dir(&serial, &remote_dir).await?;

    let mut manifest = Manifest::new(
        args.session_id.clone(),
        device,
        capabilities,
        &args.capture,
        parsed_size,
        args.capture_redact,
    );
    bundle.write_manifest(&manifest)?;
    bundle.append_event(&session::timeline_event(
        "video_session_start",
        manifest.capture.started_at,
        json!({
            "session_id": args.session_id,
            "device": serial.as_str(),
            "backend": "screenrecord",
            "bundle": bundle.root.display().to_string(),
        }),
    ))?;

    let active = ActiveState {
        schema_version: session::SCHEMA_VERSION,
        serial: serial.to_string(),
        startup_id: args.startup_id.clone(),
        session_id: args.session_id.clone(),
        daemon_pid: std::process::id(),
        control_port: None,
        bundle: bundle.root.clone(),
        remote_dir: remote_dir.clone(),
        state: "starting".into(),
        started_at: manifest.capture.started_at,
        current_segment: None,
        device_process: None,
        last_error: None,
    };
    manifest.state = "starting".into();
    let shared = Arc::new(Mutex::new(Shared {
        serial: serial.clone(),
        startup_id: args.startup_id.clone(),
        bundle,
        manifest,
        active,
        ready: false,
        segment_started_at: None,
        stop_reason: None,
    }));
    shared.lock().await.persist()?;

    // Bind before launching the recorder, but publish the port only after the
    // first remote process has passed exact ownership verification.
    let listener = TcpListener::bind(("127.0.0.1", 0u16))
        .await
        .context("bind video control socket")?;
    let ctl_port = listener.local_addr()?.port();
    {
        let mut state = shared.lock().await;
        state.active.control_port = Some(ctl_port);
        state.persist()?;
    }
    let cancel = CancellationToken::new();
    let (ready_tx, ready_rx) = oneshot::channel();
    let recorder_shared = shared.clone();
    let recorder_cancel = cancel.clone();
    let capture = args.capture.clone();
    let mut recorder = tokio::spawn(async move {
        recorder_loop(
            serial,
            remote_dir,
            executable,
            capture,
            recorder_shared,
            recorder_cancel,
            ready_tx,
        )
        .await
    });

    match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => {
            cancel.cancel();
            let _ = (&mut recorder).await;
            return Err(error).context("start first video segment");
        }
        Ok(Err(_)) => {
            cancel.cancel();
            let result = (&mut recorder).await;
            return match result {
                Ok(Err(error)) => Err(error).context("video recorder exited during startup"),
                Ok(Ok(())) => Err(anyhow!("video recorder exited before readiness")),
                Err(error) => Err(anyhow!("video recorder task failed: {error}")),
            };
        }
        Err(_) => {
            cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(30), &mut recorder)
                .await
                .is_err()
            {
                recorder.abort();
                let _ = (&mut recorder).await;
            }
            return Err(crate::diagnostic::DiagnosticError::new(
                "video_start_timeout",
                "video",
                "screenrecord did not become ready within 10 seconds",
            )
            .retryable(true)
            .detail(json!({
                "device": args.serial,
                "session_id": args.session_id,
                "timeout_ms": 10_000,
            }))
            .next_actions([
                "shadowdroid video status",
                "shadowdroid video stop",
                "shadowdroid doctor --json",
            ])
            .into());
        }
    }

    let pid_path = paths::pid(&Serial::new(&args.serial))?;
    let ctl_path = paths::control(&Serial::new(&args.serial))?;
    publish_marker(&pid_path, &std::process::id().to_string())?;
    if let Err(error) = publish_marker(&ctl_path, &ctl_port.to_string()) {
        let _ = std::fs::remove_file(&pid_path);
        cancel.cancel();
        let _ = (&mut recorder).await;
        return Err(error);
    }
    tracing::info!(
        "video daemon ready for {} on 127.0.0.1:{}",
        args.serial,
        ctl_port
    );

    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let recorder_finished = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let shared = shared.clone();
                let stop_tx = stop_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_client(stream, shared, stop_tx).await {
                        tracing::debug!("video control request failed: {error:#}");
                    }
                });
            }
            result = &mut recorder => break Some(result),
            _ = stop_rx.recv() => break None,
            _ = tokio::signal::ctrl_c() => {
                shared.lock().await.stop_reason = Some("interrupt".into());
                break None
            },
        }
    };
    let recorder_result = if let Some(result) = recorder_finished {
        result
    } else {
        cancel.cancel();
        (&mut recorder).await
    };
    let result = match recorder_result {
        Ok(result) => result,
        Err(error) => Err(anyhow!("video recorder task failed: {error}")),
    };
    if let Err(error) = &result {
        let mut state = shared.lock().await;
        if !is_terminal_state(&state.active.state) {
            let message = format!("{error:#}");
            if state.active.state == "recoverable" {
                state.active.last_error.get_or_insert(message.clone());
            } else if state.active.device_process.is_some() {
                state.active.state = "recoverable".into();
                state.manifest.state = "recoverable".into();
            } else {
                state.active.state = "failed".into();
                state.manifest.state = "failed".into();
                state.active.current_segment = None;
            }
            state.active.last_error = Some(message.clone());
            state.manifest.warnings.push(message);
            let _ = state.persist();
        }
    }
    remove_markers_if_owned(&pid_path, &ctl_path, std::process::id(), ctl_port);
    tracing::info!("video daemon stopped for {}", args.serial);
    result
}

/// Recover a session whose host daemon exited after publishing an exact device
/// process identity. This is intentionally callable only from ownership-checked
/// `video stop`: it never signals a numeric PID unless start ticks, executable,
/// comm, and the unique remote output argv all still match.
pub async fn recover(serial: &Serial, active: ActiveState) -> Result<ActiveState> {
    if active.serial != serial.as_str() {
        bail!("video recovery state belongs to another device");
    }
    if is_terminal_state(&active.state) {
        return Ok(active);
    }
    let bundle = Bundle::open(active.bundle.clone());
    let manifest = session::manifest_from_bundle(&active.bundle)?;
    let shared = Arc::new(Mutex::new(Shared {
        serial: serial.clone(),
        startup_id: active.startup_id.clone(),
        bundle: bundle.clone(),
        manifest,
        active,
        ready: false,
        segment_started_at: None,
        stop_reason: None,
    }));

    hydrate_recovery_identity(serial, &shared).await?;
    let device_process = shared.lock().await.active.device_process.clone();
    if let Some(identity) = &device_process {
        if inspect_owned(serial, identity).await? {
            signal_owned(serial, identity).await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                match inspect_process_generation(serial, identity).await? {
                    DeviceProcessGeneration::Missing | DeviceProcessGeneration::Reused => break,
                    DeviceProcessGeneration::Same => {}
                    DeviceProcessGeneration::Unknown => {
                        return Err(ownership_unproven(serial, identity));
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(crate::diagnostic::DiagnosticError::new(
                        "video_finalize_failed",
                        "video",
                        "owned device screenrecord process did not finalize during recovery",
                    )
                    .retryable(true)
                    .detail(json!({
                        "device": serial.as_str(),
                        "pid": identity.pid,
                        "start_ticks": identity.start_ticks,
                        "remote_path": identity.remote_path,
                    }))
                    .next_actions([
                        "retry `shadowdroid video stop`",
                        "shadowdroid video status",
                        "inspect the exact device process before manual intervention",
                    ])
                    .into());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } else if !matches!(
            inspect_process_generation(serial, identity).await?,
            DeviceProcessGeneration::Missing | DeviceProcessGeneration::Reused
        ) {
            return Err(ownership_unproven(serial, identity));
        }
    }

    {
        let mut state = shared.lock().await;
        if let Some(identity) = &device_process {
            let stopped_at = session::now_ts();
            if let Some(segment) = state
                .manifest
                .segments
                .iter_mut()
                .find(|segment| segment.remote_path == identity.remote_path)
            {
                segment.state = "finalized_remote".into();
                segment.stopped_at = Some(stopped_at);
                segment.elapsed_ms = session::elapsed_ms(segment.started_at, stopped_at);
                segment.stop_reason = Some("recovered_stop".into());
                segment.error = None;
            }
        }
        state.active.device_process = None;
        state.active.state = "recovering".into();
        state.manifest.state = "recovering".into();
        state.persist()?;
    }

    let pending = {
        let state = shared.lock().await;
        state
            .manifest
            .segments
            .iter()
            .filter(|segment| segment.state != "complete" || !segment.remote_cleaned)
            .map(|segment| {
                (
                    segment.index,
                    segment.remote_path.clone(),
                    state.bundle.root.join(&segment.path),
                    segment.state.clone(),
                    segment.remote_cleaned,
                )
            })
            .collect::<Vec<_>>()
    };
    for (index, remote_path, local_path, segment_state, remote_cleaned) in pending {
        let identity_path = format!("{remote_path}.identity");
        if segment_state == "complete" {
            if !remote_cleaned {
                cleanup_remote_segment(serial, index, &remote_path, &identity_path, &shared)
                    .await?;
            }
            continue;
        }
        if local_path.is_file() {
            let video_info = session::inspect_mp4_video(&local_path)
                .with_context(|| format!("inspect recovered video segment {index}"))?;
            let bytes = local_path.metadata()?.len();
            let sha256 = session::sha256_file(&local_path)?;
            record_published_segment(&shared, index, bytes, sha256, video_info).await?;
            cleanup_remote_segment(serial, index, &remote_path, &identity_path, &shared).await?;
        } else {
            match remote_path_state(serial, &remote_path).await? {
                RemotePathState::File => {
                    if matches!(segment_state.as_str(), "launching" | "recording")
                        && remote_path_in_use(serial, &remote_path).await?
                    {
                        return Err(crate::diagnostic::DiagnosticError::new(
                            "video_ownership_unproven",
                            "video",
                            "a recorder still references the remote segment, but no exact owned process identity is available",
                        )
                        .detail(json!({
                            "device": serial.as_str(),
                            "segment_index": index,
                            "remote_path": remote_path,
                        }))
                        .next_actions([
                            "shadowdroid video status",
                            "shadowdroid doctor --json",
                            "inspect the device process before manual intervention",
                        ])
                        .into());
                    }
                    pull_segment(
                        serial,
                        index,
                        &remote_path,
                        &identity_path,
                        &local_path,
                        shared.clone(),
                    )
                    .await?;
                }
                RemotePathState::Missing => {
                    mark_missing_remote_segment(&shared, index).await?;
                    cleanup_remote_segment(serial, index, &remote_path, &identity_path, &shared)
                        .await?;
                }
                RemotePathState::Other => {
                    bail!("remote video segment {index} exists but is not a regular file");
                }
            }
        }
    }

    let mut state = shared.lock().await;
    let stopped_at = session::now_ts();
    state.manifest.capture.stopped_at = Some(stopped_at);
    state.manifest.capture.stop_reason = Some("recovery".into());
    state.manifest.capture.elapsed_ms =
        session::elapsed_ms(state.manifest.capture.started_at, stopped_at);
    state.active.current_segment = None;
    state.active.device_process = None;
    let completed = state
        .manifest
        .segments
        .iter()
        .filter(|segment| segment.state == "complete")
        .count();
    let playable = state
        .manifest
        .segments
        .iter()
        .filter(|segment| segment.state == "complete" && segment.playable)
        .count();
    let unresolved = state
        .manifest
        .segments
        .iter()
        .any(|segment| segment.state != "complete" || !segment.remote_cleaned);
    let final_state = if completed == 0 {
        "failed"
    } else if unresolved || playable < completed {
        "partial"
    } else {
        "completed"
    };
    state.active.state = final_state.into();
    state.manifest.state = final_state.into();
    state.active.last_error = if final_state == "completed" {
        None
    } else {
        Some("recovery completed without a fully playable set of video segments".into())
    };
    if state.manifest.segments.is_empty() {
        state
            .manifest
            .warnings
            .push("recovered session contains no video segments".into());
    }
    state.bundle.append_event(&session::timeline_event(
        "video_session_recovered",
        state.manifest.capture.started_at,
        json!({
            "session_id": state.manifest.session_id,
            "segments": state.manifest.segments.len(),
        }),
    ))?;
    state.bundle.append_event(&session::timeline_event(
        "video_session_stop",
        state.manifest.capture.started_at,
        json!({
            "session_id": state.manifest.session_id,
            "state": final_state,
            "stop_reason": "recovery",
            "segments": state.manifest.segments.len(),
        }),
    ))?;
    let timeline_path = state.bundle.timeline_path.clone();
    let timeline_bytes = timeline_path.metadata().ok().map(|meta| meta.len());
    let timeline_sha256 = session::sha256_file(&timeline_path).ok();
    if let Some(timeline) = state
        .manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == "events.jsonl")
    {
        timeline.complete = true;
        timeline.bytes = timeline_bytes;
        timeline.sha256 = timeline_sha256;
    }
    state.persist()?;
    let bundle_for_assembly = state.bundle.clone();
    if let Err(error) = session::assemble_video(&bundle_for_assembly, &mut state.manifest) {
        state
            .manifest
            .warnings
            .push(format!("aggregate video assembly failed: {error:#}"));
    }
    state.persist()?;
    let recovered = state.active.clone();
    let remote_dir = recovered.remote_dir.clone();
    drop(state);
    let cleanup = format!(
        "rmdir {} 2>/dev/null || true",
        crate::config::quote_device_shell_arg(&remote_dir)
    );
    let _ = crate::device::adb::shell_mutating(serial, cleanup).await;
    if let Some(control_port) = recovered.control_port {
        remove_markers_if_owned(
            &paths::pid(serial)?,
            &paths::control(serial)?,
            recovered.daemon_pid,
            control_port,
        );
    }
    Ok(recovered)
}

async fn serve_client(
    stream: TcpStream,
    shared: Arc<Mutex<Shared>>,
    stop_tx: mpsc::Sender<()>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let request: Value = serde_json::from_str(&line).unwrap_or_else(|_| json!({}));
    let serial = request.get("serial").and_then(Value::as_str);
    let expected_serial = shared.lock().await.serial.to_string();
    if serial != Some(expected_serial.as_str()) {
        return write_response(
            &mut write,
            &json!({
                "ok": false,
                "code": "video_control_serial_mismatch",
                "device": expected_serial,
            }),
        )
        .await;
    }
    match request.get("op").and_then(Value::as_str).unwrap_or("") {
        "status" => {
            let value = shared.lock().await.status();
            write_response(&mut write, &value).await
        }
        "mark" => {
            let raw = request
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if raw.is_empty() || raw.chars().count() > 1000 {
                return write_response(
                    &mut write,
                    &json!({
                        "ok": false,
                        "code": "video_invalid_marker",
                        "msg": "marker label must contain 1 to 1000 characters",
                    }),
                )
                .await;
            }
            let label = crate::redaction::redact_text_if_active(raw).into_owned();
            let mut state = shared.lock().await;
            let now = session::now_ts();
            let elapsed_ms = session::elapsed_ms(state.manifest.capture.started_at, now);
            let segment_elapsed_ms = state
                .segment_started_at
                .map(|started| session::elapsed_ms(started, now));
            let marker = Marker {
                label: label.clone(),
                ts: now,
                elapsed_ms,
                segment_index: state.active.current_segment,
                segment_elapsed_ms,
                timing_basis: "host_receive_time_approximate".into(),
            };
            state.manifest.markers.push(marker.clone());
            state.bundle.append_event(&session::timeline_event(
                "video_marker",
                state.manifest.capture.started_at,
                json!({
                    "session_id": state.manifest.session_id,
                    "label": label,
                    "segment_index": marker.segment_index,
                    "segment_elapsed_ms": marker.segment_elapsed_ms,
                    "timing_basis": marker.timing_basis,
                }),
            ))?;
            state.persist()?;
            write_response(
                &mut write,
                &json!({
                    "ok": true,
                    "session_id": state.manifest.session_id,
                    "label": marker.label,
                    "elapsed_ms": marker.elapsed_ms,
                    "segment_index": marker.segment_index,
                    "segment_elapsed_ms": marker.segment_elapsed_ms,
                    "timeline": state.bundle.timeline_path.display().to_string(),
                }),
            )
            .await
        }
        "stop" => {
            let reason = request
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| {
                    !reason.is_empty()
                        && reason.len() <= 64
                        && reason
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
                })
                .unwrap_or("explicit")
                .to_string();
            {
                let mut state = shared.lock().await;
                state.stop_reason = Some(reason.clone());
                if !is_terminal_state(&state.active.state) && state.active.state != "recoverable" {
                    state.active.state = "finalizing".into();
                    state.manifest.state = "finalizing".into();
                    state.persist()?;
                }
            }
            let _ = stop_tx.try_send(());
            write_response(
                &mut write,
                &json!({"ok": true, "stopping": true, "reason": reason}),
            )
            .await
        }
        _ => {
            write_response(
                &mut write,
                &json!({
                    "ok": false,
                    "code": "video_control_invalid_request",
                    "msg": "unknown video control operation",
                }),
            )
            .await
        }
    }
}

async fn write_response(write: &mut tokio::net::tcp::OwnedWriteHalf, value: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await?;
    Ok(())
}

async fn recorder_loop(
    serial: Serial,
    remote_dir: String,
    executable: String,
    capture: CaptureArgs,
    shared: Arc<Mutex<Shared>>,
    cancel: CancellationToken,
    ready_tx: oneshot::Sender<Result<()>>,
) -> Result<()> {
    let mut ready_tx = Some(ready_tx);
    let (rotation_tx, mut rotation_rx) = mpsc::channel::<u8>(4);
    let rotation_cancel = cancel.child_token();
    let rotation_task = if capture.no_split_on_rotation {
        None
    } else {
        Some(tokio::spawn(watch_rotation(
            serial.clone(),
            rotation_tx,
            rotation_cancel.clone(),
        )))
    };
    let mut pull_tasks: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    let mut transient_pull_errors = Vec::new();
    let mut previous_end: Option<(u32, f64)> = None;
    let mut index = 0u32;
    let mut terminal_error: Option<anyhow::Error> = None;
    let mut early_exit_warning: Option<String> = None;
    let mut launch_uncertain = false;

    while !cancel.is_cancelled() {
        index += 1;
        let orientation = backend::orientation(&serial).await;
        let remote_path = format!("{remote_dir}/segment-{index:06}.mp4");
        let identity_path = format!("{remote_path}.identity");
        let local_path = {
            let mut state = shared.lock().await;
            let started_at = session::now_ts();
            if let Some((after_segment, gap_started)) = previous_end.take() {
                state.manifest.gaps.push(Gap {
                    after_segment: Some(after_segment),
                    started_at: gap_started,
                    ended_at: Some(started_at),
                    duration_ms: Some(session::elapsed_ms(gap_started, started_at)),
                    reason: "segment_rollover".into(),
                    timing_basis: "host_observed_approximate".into(),
                });
            }
            let local = state.bundle.segment_path(index);
            state.manifest.segments.push(Segment {
                index,
                path: format!("segments/{index:06}.mp4"),
                remote_path: remote_path.clone(),
                state: "launching".into(),
                started_at,
                stopped_at: None,
                elapsed_ms: 0,
                bytes: None,
                sha256: None,
                remote_cleaned: false,
                sample_count: None,
                media_duration_ms: None,
                playable: false,
                codec: None,
                width: None,
                height: None,
                timescale: None,
                sample_entry_sha256: None,
                timing_basis: "host_observed_approximate".into(),
                orientation,
                stop_reason: None,
                error: None,
            });
            state.active.current_segment = Some(index);
            state.active.device_process = None;
            state.segment_started_at = Some(started_at);
            state.bundle.append_event(&session::timeline_event(
                "video_segment_start",
                state.manifest.capture.started_at,
                json!({
                    "session_id": state.manifest.session_id,
                    "segment_index": index,
                    "path": format!("segments/{index:06}.mp4"),
                    "orientation": orientation,
                }),
            ))?;
            state.persist()?;
            local
        };

        let command = screenrecord_command(&capture, &executable, &remote_path, &identity_path);
        let backend_started = tokio::time::Instant::now();
        let mut backend_task = tokio::spawn(crate::device::adb::shell_long_running(
            serial.to_string(),
            command,
        ));
        let identity = match await_device_identity(
            &serial,
            &identity_path,
            &remote_path,
            &executable,
            index,
            &shared,
            &mut backend_task,
        )
        .await
        {
            Ok(identity) => identity,
            Err(error) => {
                let backend_finished = backend_task.is_finished();
                fail_segment(&shared, index, &error).await;
                if backend_finished {
                    clear_device_process_for_segment(index, &shared).await;
                    let cleanup_result = match remote_path_state(&serial, &remote_path).await {
                        Ok(RemotePathState::File) => {
                            mark_finalized_remote(&shared, index, "failed_launch").await;
                            pull_segment(
                                &serial,
                                index,
                                &remote_path,
                                &identity_path,
                                &local_path,
                                shared.clone(),
                            )
                            .await
                        }
                        Ok(RemotePathState::Missing) => {
                            cleanup_remote_segment(
                                &serial,
                                index,
                                &remote_path,
                                &identity_path,
                                &shared,
                            )
                            .await
                        }
                        Ok(RemotePathState::Other) => {
                            Err(anyhow!("failed launch left a non-file remote artifact"))
                        }
                        Err(cleanup_error) => Err(cleanup_error),
                    };
                    if let Err(cleanup_error) = cleanup_result {
                        let mut state = shared.lock().await;
                        state.manifest.warnings.push(format!(
                            "failed to clean up video segment {index} after launch error: {cleanup_error:#}"
                        ));
                        let _ = state.persist();
                    }
                } else if shared.lock().await.active.device_process.is_none() {
                    launch_uncertain = true;
                }
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(anyhow!("{error:#}")));
                }
                terminal_error = Some(error);
                break;
            }
        };
        {
            let mut state = shared.lock().await;
            let segment = segment_mut(&mut state.manifest, index)?;
            segment.state = "recording".into();
            state.active.device_process = Some(identity.clone());
            state.active.state = "recording".into();
            state.manifest.state = "recording".into();
            state.ready = true;
            state.persist()?;
        }
        if let Some(tx) = ready_tx.take() {
            let _ = tx.send(Ok(()));
        }

        enum End {
            Stop,
            Rotation(u8),
            Backend(Result<Result<crate::device::adb::LongShellOutput>, tokio::task::JoinError>),
            Timeout,
        }
        let safety = tokio::time::sleep(Duration::from_secs(capture.segment_seconds as u64 + 20));
        tokio::pin!(safety);
        let end = loop {
            let selected = tokio::select! {
                _ = cancel.cancelled() => End::Stop,
                Some(rotation) = rotation_rx.recv() => End::Rotation(rotation),
                result = &mut backend_task => End::Backend(result),
                _ = &mut safety => End::Timeout,
            };
            if matches!(&selected, End::Rotation(value) if Some(*value) == orientation) {
                continue;
            }
            break selected;
        };

        let (stop_reason, continue_recording, backend_output, early_exit) = match end {
            End::Backend(result) => {
                let elapsed = backend_started.elapsed();
                let expected = Duration::from_secs(capture.segment_seconds as u64);
                let tolerance = Duration::from_secs(2).min(expected / 3);
                let early = elapsed.saturating_add(tolerance) < expected;
                (
                    if early {
                        "backend_exit".to_string()
                    } else {
                        "segment_limit".to_string()
                    },
                    !early && !cancel.is_cancelled(),
                    flatten_backend_result(result),
                    early.then_some((elapsed, expected)),
                )
            }
            End::Rotation(value) => {
                {
                    let state = shared.lock().await;
                    state.bundle.append_event(&session::timeline_event(
                        "video_orientation_change",
                        state.manifest.capture.started_at,
                        json!({
                            "session_id": state.manifest.session_id,
                            "segment_index": index,
                            "orientation": value,
                        }),
                    ))?;
                }
                (
                    "rotation".into(),
                    !cancel.is_cancelled(),
                    stop_backend(&serial, &identity, &mut backend_task).await,
                    None,
                )
            }
            End::Stop => {
                let reason = shared
                    .lock()
                    .await
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "stop".into());
                (
                    reason,
                    false,
                    stop_backend(&serial, &identity, &mut backend_task).await,
                    None,
                )
            }
            End::Timeout => (
                "safety_timeout".into(),
                !cancel.is_cancelled(),
                stop_backend(&serial, &identity, &mut backend_task).await,
                None,
            ),
        };
        match backend_output {
            Ok(output) if output.status.is_none_or(|status| status == 0) => {
                if let Some((elapsed, expected)) = early_exit {
                    early_exit_warning = Some(format!(
                        "screenrecord exited successfully after {} ms, before the {} ms segment limit; recording stopped to avoid an empty rollover loop",
                        elapsed.as_millis(),
                        expected.as_millis()
                    ));
                }
            }
            Ok(output) => {
                let error = anyhow!(
                    "screenrecord exited with status {:?}: {}{}",
                    output.status,
                    output.stderr.trim(),
                    if output.stdout.trim().is_empty() {
                        String::new()
                    } else {
                        format!("; stdout: {}", output.stdout.trim())
                    }
                );
                fail_segment(&shared, index, &error).await;
                clear_device_process_after_backend_exit(&identity, &shared).await;
                terminal_error = Some(error);
                break;
            }
            Err(error) => {
                fail_segment(&shared, index, &error).await;
                terminal_error = Some(error);
                break;
            }
        }

        let stopped_at = session::now_ts();
        {
            let mut state = shared.lock().await;
            let segment = segment_mut(&mut state.manifest, index)?;
            segment.state = "finalized_remote".into();
            segment.stopped_at = Some(stopped_at);
            segment.elapsed_ms = session::elapsed_ms(segment.started_at, stopped_at);
            segment.stop_reason = Some(stop_reason);
            state.active.device_process = None;
            state.persist()?;
        }
        previous_end = Some((index, stopped_at));
        let mut task_index = 0;
        while task_index < pull_tasks.len() {
            if pull_tasks[task_index].is_finished() {
                let task = pull_tasks.swap_remove(task_index);
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => transient_pull_errors.push(error.to_string()),
                    Err(error) => {
                        transient_pull_errors.push(format!("segment pull task failed: {error}"))
                    }
                }
            } else {
                task_index += 1;
            }
        }
        if pull_tasks.len() < 2 {
            let pull_shared = shared.clone();
            let pull_serial = serial.clone();
            let pull_remote = remote_path.clone();
            let pull_identity = identity_path.clone();
            pull_tasks.push(tokio::spawn(async move {
                pull_segment(
                    &pull_serial,
                    index,
                    &pull_remote,
                    &pull_identity,
                    &local_path,
                    pull_shared,
                )
                .await
            }));
        } else {
            let mut state = shared.lock().await;
            state.manifest.warnings.push(format!(
                "segment {index} pull deferred until stop because two transfers are already active"
            ));
            state.persist()?;
        }
        if !continue_recording {
            break;
        }
    }

    rotation_cancel.cancel();
    if let Some(task) = rotation_task {
        task.abort();
    }
    let mut pull_errors = Vec::new();
    for task in pull_tasks {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => transient_pull_errors.push(error.to_string()),
            Err(error) => transient_pull_errors.push(format!("segment pull task failed: {error}")),
        }
    }
    let deferred = {
        let state = shared.lock().await;
        state
            .manifest
            .segments
            .iter()
            .filter(|segment| {
                segment.state == "finalized_remote"
                    || (segment.state == "complete" && !segment.remote_cleaned)
            })
            .map(|segment| {
                (
                    segment.index,
                    segment.remote_path.clone(),
                    state.bundle.root.join(&segment.path),
                    segment.state.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    for (deferred_index, remote_path, local_path, segment_state) in deferred {
        let identity_path = format!("{remote_path}.identity");
        let result = if segment_state == "complete" {
            cleanup_remote_segment(
                &serial,
                deferred_index,
                &remote_path,
                &identity_path,
                &shared,
            )
            .await
        } else if local_path.is_file() {
            async {
                let video_info = session::inspect_mp4_video(&local_path)?;
                let bytes = local_path.metadata()?.len();
                let sha256 = session::sha256_file(&local_path)?;
                record_published_segment(&shared, deferred_index, bytes, sha256, video_info)
                    .await?;
                cleanup_remote_segment(
                    &serial,
                    deferred_index,
                    &remote_path,
                    &identity_path,
                    &shared,
                )
                .await
            }
            .await
        } else {
            pull_segment(
                &serial,
                deferred_index,
                &remote_path,
                &identity_path,
                &local_path,
                shared.clone(),
            )
            .await
        };
        if let Err(error) = result {
            pull_errors.push(error.to_string());
        }
    }
    let mut state = shared.lock().await;
    if !transient_pull_errors.is_empty() {
        state.manifest.warnings.push(format!(
            "{} background segment pull attempt(s) required finalization retry",
            transient_pull_errors.len()
        ));
    }
    if launch_uncertain || state.active.device_process.is_some() {
        let message = terminal_error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "screenrecord remains active after daemon finalization".into());
        state.manifest.state = "recoverable".into();
        state.active.state = "recoverable".into();
        state.active.last_error = Some(message.clone());
        state.manifest.warnings.extend(pull_errors);
        state.manifest.warnings.push(format!(
            "{message}; exact device process identity and remote artifacts were preserved"
        ));
        state.persist()?;
        let detail = json!({
            "device": state.serial.as_str(),
            "session_id": state.manifest.session_id,
            "bundle": state.bundle.root,
            "device_process": state.active.device_process,
        });
        drop(state);
        return Err(crate::diagnostic::DiagnosticError::new(
            "video_finalize_failed",
            "video",
            "screenrecord is still active after the finalization deadline",
        )
        .retryable(true)
        .detail(detail)
        .next_actions([
            "retry `shadowdroid video stop` to recover the owned device process",
            "shadowdroid video status",
            "shadowdroid doctor --json",
        ])
        .into());
    }
    let stopped_at = session::now_ts();
    state.manifest.capture.stopped_at = Some(stopped_at);
    let stop_reason = state.stop_reason.clone().unwrap_or_else(|| {
        if early_exit_warning.is_some() {
            "backend_exit".into()
        } else if terminal_error.is_some() {
            "backend_error".into()
        } else {
            "stop".into()
        }
    });
    state.manifest.capture.stop_reason = Some(stop_reason.clone());
    state.manifest.capture.elapsed_ms =
        session::elapsed_ms(state.manifest.capture.started_at, stopped_at);
    state.active.current_segment = None;
    state.segment_started_at = None;
    let has_unplayable_segment = state
        .manifest
        .segments
        .iter()
        .any(|segment| segment.state == "complete" && !segment.playable);
    if let Some(warning) = &early_exit_warning {
        state.manifest.warnings.push(warning.clone());
    }
    if let Some(error) = terminal_error {
        state.manifest.state = "failed".into();
        state.active.state = "failed".into();
        state.active.last_error = Some(error.to_string());
        state.manifest.warnings.push(error.to_string());
    } else if pull_errors.is_empty() && !has_unplayable_segment && early_exit_warning.is_none() {
        state.manifest.state = "completed".into();
        state.active.state = "completed".into();
    } else {
        state.manifest.state = "partial".into();
        state.active.state = "partial".into();
        state.active.last_error = Some(if let Some(warning) = early_exit_warning {
            warning
        } else if pull_errors.is_empty() {
            "one or more segments contain no playable video duration".into()
        } else {
            pull_errors.join("; ")
        });
        state.manifest.warnings.extend(pull_errors);
    }
    let final_state = state.manifest.state.clone();
    state.bundle.append_event(&session::timeline_event(
        "video_session_stop",
        state.manifest.capture.started_at,
        json!({
            "session_id": state.manifest.session_id,
            "state": final_state,
            "stop_reason": stop_reason,
            "segments": state.manifest.segments.iter().filter(|segment| segment.state == "complete").count(),
        }),
    ))?;
    let timeline_path = state.bundle.timeline_path.clone();
    let timeline_bytes = timeline_path.metadata().ok().map(|meta| meta.len());
    let timeline_sha256 = session::sha256_file(&timeline_path).ok();
    if let Some(timeline) = state
        .manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.path == "events.jsonl")
    {
        timeline.complete = true;
        timeline.bytes = timeline_bytes;
        timeline.sha256 = timeline_sha256;
    }
    state.persist()?;
    let bundle = state.bundle.clone();
    if let Err(error) = session::assemble_video(&bundle, &mut state.manifest) {
        state
            .manifest
            .warnings
            .push(format!("aggregate video assembly failed: {error:#}"));
    }
    state.persist()?;
    let cleanup_serial = state.serial.clone();
    let cleanup_remote_dir = state.active.remote_dir.clone();
    drop(state);
    let cleanup = format!(
        "rmdir {} 2>/dev/null || true",
        crate::config::quote_device_shell_arg(&cleanup_remote_dir)
    );
    let _ = crate::device::adb::shell_mutating(&cleanup_serial, cleanup).await;
    Ok(())
}

async fn clear_device_process_after_backend_exit(
    identity: &DeviceProcessIdentity,
    shared: &Arc<Mutex<Shared>>,
) {
    let mut state = shared.lock().await;
    if state.active.device_process.as_ref().is_some_and(|current| {
        current.pid == identity.pid
            && current.start_ticks == identity.start_ticks
            && current.remote_path == identity.remote_path
    }) {
        state.active.device_process = None;
        let _ = state.persist();
    }
}

async fn clear_device_process_for_segment(index: u32, shared: &Arc<Mutex<Shared>>) {
    let mut state = shared.lock().await;
    if state.active.current_segment == Some(index) {
        state.active.device_process = None;
        let _ = state.persist();
    }
}

async fn mark_finalized_remote(shared: &Arc<Mutex<Shared>>, index: u32, reason: &str) {
    let mut state = shared.lock().await;
    let stopped_at = session::now_ts();
    if let Ok(segment) = segment_mut(&mut state.manifest, index) {
        segment.state = "finalized_remote".into();
        segment.stopped_at = Some(stopped_at);
        segment.elapsed_ms = session::elapsed_ms(segment.started_at, stopped_at);
        segment.stop_reason = Some(reason.into());
    }
    let _ = state.persist();
}

async fn hydrate_recovery_identity(serial: &Serial, shared: &Arc<Mutex<Shared>>) -> Result<()> {
    let candidate = {
        let state = shared.lock().await;
        if state.active.device_process.is_some() {
            return Ok(());
        }
        let segment = state
            .manifest
            .segments
            .iter()
            .rev()
            .find(|segment| segment.state != "complete" || !segment.remote_cleaned);
        segment.map(|segment| {
            (
                segment.index,
                segment.remote_path.clone(),
                state.manifest.backend.capabilities.executable.clone(),
            )
        })
    };
    let Some((index, remote_path, executable)) = candidate else {
        return Ok(());
    };
    if executable.is_empty() {
        return Ok(());
    }
    let identity_path = format!("{remote_path}.identity");
    let output = crate::device::adb::shell(
        serial,
        format!(
            "cat {} 2>/dev/null",
            crate::config::quote_device_shell_arg(&identity_path)
        ),
    )
    .await
    .unwrap_or_default();
    let fields = output.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 {
        return Ok(());
    }
    let (Ok(pid), Ok(start_ticks)) = (fields[0].parse::<u32>(), fields[1].parse::<u64>()) else {
        return Ok(());
    };
    if pid == 0 {
        return Ok(());
    }
    let mut state = shared.lock().await;
    if state.active.device_process.is_none()
        && state
            .active
            .current_segment
            .is_none_or(|current| current == index)
    {
        state.active.current_segment = Some(index);
        state.active.device_process = Some(DeviceProcessIdentity {
            pid,
            start_ticks,
            executable,
            remote_path,
        });
        state.persist()?;
    }
    Ok(())
}

async fn pull_segment(
    serial: &Serial,
    index: u32,
    remote_path: &str,
    identity_path: &str,
    local_path: &Path,
    shared: Arc<Mutex<Shared>>,
) -> Result<()> {
    let staging_path = local_path.with_file_name(format!(".{index:06}.pulling.mp4"));
    if std::fs::symlink_metadata(local_path).is_ok() {
        bail!("refusing to replace an existing local video segment {index}");
    }
    let reuse_staging = match std::fs::symlink_metadata(&staging_path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            if session::validate_mp4(&staging_path).is_ok() {
                paths::protect_file(&staging_path)?;
                true
            } else {
                std::fs::remove_file(&staging_path)
                    .with_context(|| format!("remove invalid staged video segment {index}"))?;
                false
            }
        }
        Ok(_) => bail!("staged video segment {index} is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspect staged video segment"),
    };
    if !reuse_staging {
        let remote_size = remote_file_size(serial, remote_path).await?;
        if remote_size < 24 {
            bail!("remote segment {index} is only {remote_size} bytes");
        }
        let extra = remote_size / (1024 * 1024);
        let timeout = Duration::from_secs(120u64.max(30 + extra));
        crate::device::adb::pull_to_path_with_timeout(serial, remote_path, &staging_path, timeout)
            .await
            .with_context(|| format!("pull video segment {index}"))?;
        paths::protect_file(&staging_path)?;
    }
    if let Err(error) = session::validate_mp4(&staging_path) {
        let _ = std::fs::remove_file(&staging_path);
        return Err(error).with_context(|| format!("validate video segment {index}"));
    }
    let video_info = match session::inspect_mp4_video(&staging_path) {
        Ok(info) => info,
        Err(error) => {
            let _ = std::fs::remove_file(&staging_path);
            return Err(error).with_context(|| format!("inspect video segment {index}"));
        }
    };
    let bytes = staging_path.metadata()?.len();
    let sha256 = match session::sha256_file(&staging_path) {
        Ok(sha256) => sha256,
        Err(error) => {
            let _ = std::fs::remove_file(&staging_path);
            return Err(error);
        }
    };
    std::fs::rename(&staging_path, local_path)
        .with_context(|| format!("publish video segment {index}"))?;
    paths::protect_file(local_path)?;
    record_published_segment(&shared, index, bytes, sha256, video_info).await?;
    cleanup_remote_segment(serial, index, remote_path, identity_path, &shared).await?;
    Ok(())
}

async fn record_published_segment(
    shared: &Arc<Mutex<Shared>>,
    index: u32,
    bytes: u64,
    sha256: String,
    video_info: session::VideoTrackInfo,
) -> Result<()> {
    let mut state = shared.lock().await;
    let started_at = state.manifest.capture.started_at;
    let session_id = state.manifest.session_id.clone();
    let segment_path = {
        let segment = segment_mut(&mut state.manifest, index)?;
        segment.state = "complete".into();
        segment.bytes = Some(bytes);
        segment.sha256 = Some(sha256.clone());
        segment.sample_count = Some(video_info.sample_count);
        segment.media_duration_ms = Some(video_info.duration_ms);
        segment.playable = video_info.playable();
        segment.codec = Some(video_info.codec.clone());
        segment.width = Some(video_info.width);
        segment.height = Some(video_info.height);
        segment.timescale = Some(video_info.timescale);
        segment.sample_entry_sha256 = Some(video_info.sample_entry_sha256.clone());
        segment.path.clone()
    };
    if !video_info.playable() {
        state.manifest.warnings.push(format!(
            "segment {index} contains {} video sample(s) and {} ms of media; it is preserved but excluded from video.mp4",
            video_info.sample_count, video_info.duration_ms
        ));
    }
    state.bundle.append_event(&session::timeline_event(
        "video_segment_complete",
        started_at,
        json!({
            "session_id": session_id,
            "segment_index": index,
            "path": segment_path,
            "bytes": bytes,
            "sha256": sha256,
            "sample_count": video_info.sample_count,
            "media_duration_ms": video_info.duration_ms,
            "playable": video_info.playable(),
            "codec": video_info.codec,
            "width": video_info.width,
            "height": video_info.height,
            "timescale": video_info.timescale,
            "sample_entry_sha256": video_info.sample_entry_sha256,
        }),
    ))?;
    state.persist()
}

async fn cleanup_remote_segment(
    serial: &Serial,
    index: u32,
    remote_path: &str,
    identity_path: &str,
    shared: &Arc<Mutex<Shared>>,
) -> Result<()> {
    let cleanup = format!(
        "rm -f {} {} && echo __shadowdroid_video_removed__",
        crate::config::quote_device_shell_arg(remote_path),
        crate::config::quote_device_shell_arg(identity_path)
    );
    let output = crate::device::adb::shell_mutating(serial, cleanup).await?;
    if !output.contains("__shadowdroid_video_removed__") {
        bail!("device did not confirm cleanup for video segment {index}");
    }
    let mut state = shared.lock().await;
    segment_mut(&mut state.manifest, index)?.remote_cleaned = true;
    state.persist()
}

async fn remote_file_size(serial: &Serial, path: &str) -> Result<u64> {
    let output = crate::device::adb::shell(
        serial,
        format!(
            "stat -c %s {} 2>/dev/null",
            crate::config::quote_device_shell_arg(path)
        ),
    )
    .await?;
    output
        .trim()
        .parse()
        .with_context(|| format!("read remote video size for {path}"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemotePathState {
    File,
    Missing,
    Other,
}

async fn remote_path_state(serial: &Serial, path: &str) -> Result<RemotePathState> {
    let path = crate::config::quote_device_shell_arg(path);
    let output = crate::device::adb::shell(
        serial,
        format!(
            "if [ -f {path} ]; then echo __shadowdroid_video_file__; \
             elif [ -e {path} ]; then echo __shadowdroid_video_other__; \
             else echo __shadowdroid_video_missing__; fi"
        ),
    )
    .await?;
    Ok(match output.trim() {
        "__shadowdroid_video_file__" => RemotePathState::File,
        "__shadowdroid_video_missing__" => RemotePathState::Missing,
        _ => RemotePathState::Other,
    })
}

async fn remote_path_in_use(serial: &Serial, path: &str) -> Result<bool> {
    let path = crate::config::quote_device_shell_arg(path);
    let output = crate::device::adb::shell(
        serial,
        format!(
            "for proc in /proc/[0-9]*; do \
               [ -r \"$proc/cmdline\" ] || continue; \
               if tr '\\000' '\\n' < \"$proc/cmdline\" 2>/dev/null | grep -F -x {path} >/dev/null; then \
                 echo __shadowdroid_video_in_use__; break; \
               fi; \
             done"
        ),
    )
    .await?;
    Ok(output.contains("__shadowdroid_video_in_use__"))
}

async fn mark_missing_remote_segment(shared: &Arc<Mutex<Shared>>, index: u32) -> Result<()> {
    let mut state = shared.lock().await;
    let segment = segment_mut(&mut state.manifest, index)?;
    let stopped_at = segment.stopped_at.unwrap_or_else(session::now_ts);
    segment.state = "failed".into();
    segment.stopped_at = Some(stopped_at);
    segment.elapsed_ms = session::elapsed_ms(segment.started_at, stopped_at);
    segment.error = Some("remote segment was not created before recorder exit".into());
    state.persist()
}

async fn await_device_identity(
    serial: &Serial,
    identity_path: &str,
    remote_path: &str,
    executable: &str,
    index: u32,
    shared: &Arc<Mutex<Shared>>,
    backend_task: &mut tokio::task::JoinHandle<Result<crate::device::adb::LongShellOutput>>,
) -> Result<DeviceProcessIdentity> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if backend_task.is_finished() {
            let result = backend_task
                .await
                .map_err(|error| anyhow!("screenrecord worker failed: {error}"))??;
            bail!(
                "screenrecord exited before readiness (status {:?}): {}",
                result.status,
                result.stderr.trim()
            );
        }
        let output = crate::device::adb::shell(
            serial,
            format!(
                "cat {} 2>/dev/null",
                crate::config::quote_device_shell_arg(identity_path)
            ),
        )
        .await
        .unwrap_or_default();
        let fields = output.split_whitespace().collect::<Vec<_>>();
        if fields.len() == 2
            && let (Ok(pid), Ok(start_ticks)) = (fields[0].parse::<u32>(), fields[1].parse::<u64>())
            && pid != 0
        {
            let identity = DeviceProcessIdentity {
                pid,
                start_ticks,
                executable: executable.into(),
                remote_path: remote_path.into(),
            };
            persist_device_process_candidate(shared, index, &identity).await?;
            if inspect_owned(serial, &identity).await? {
                return Ok(identity);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(crate::diagnostic::DiagnosticError::new(
                "video_start_timeout",
                "video",
                "screenrecord process identity could not be verified",
            )
            .retryable(true)
            .detail(json!({
                "device": serial.as_str(),
                "remote_path": remote_path,
                "timeout_ms": 8_000,
            }))
            .next_actions([
                "shadowdroid video status",
                "shadowdroid video stop",
                "shadowdroid doctor --json",
            ])
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn persist_device_process_candidate(
    shared: &Arc<Mutex<Shared>>,
    index: u32,
    identity: &DeviceProcessIdentity,
) -> Result<()> {
    let mut state = shared.lock().await;
    if state.active.current_segment == Some(index)
        && state.active.device_process.as_ref().is_none_or(|current| {
            current.pid != identity.pid
                || current.start_ticks != identity.start_ticks
                || current.remote_path != identity.remote_path
        })
    {
        state.active.device_process = Some(identity.clone());
        state.persist()?;
    }
    Ok(())
}

async fn wait_backend(
    task: &mut tokio::task::JoinHandle<Result<crate::device::adb::LongShellOutput>>,
) -> Result<crate::device::adb::LongShellOutput> {
    match tokio::time::timeout(Duration::from_secs(15), task).await {
        Ok(result) => flatten_backend_result(result),
        Err(_) => Err(anyhow!("screenrecord did not finalize within 15 seconds")),
    }
}

async fn stop_backend(
    serial: &Serial,
    identity: &DeviceProcessIdentity,
    task: &mut tokio::task::JoinHandle<Result<crate::device::adb::LongShellOutput>>,
) -> Result<crate::device::adb::LongShellOutput> {
    if !task.is_finished()
        && let Err(error) = signal_owned(serial, identity).await
        && !task.is_finished()
    {
        return Err(error);
    }
    wait_backend(task).await
}

fn flatten_backend_result(
    result: Result<Result<crate::device::adb::LongShellOutput>, tokio::task::JoinError>,
) -> Result<crate::device::adb::LongShellOutput> {
    result.map_err(|error| anyhow!("screenrecord worker failed: {error}"))?
}

async fn inspect_owned(serial: &Serial, identity: &DeviceProcessIdentity) -> Result<bool> {
    let output = crate::device::adb::shell(serial, ownership_command(identity, false)).await?;
    Ok(output
        .lines()
        .any(|line| line.trim() == "__shadowdroid_video_owned__"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeviceProcessGeneration {
    Same,
    Missing,
    Reused,
    Unknown,
}

async fn inspect_process_generation(
    serial: &Serial,
    identity: &DeviceProcessIdentity,
) -> Result<DeviceProcessGeneration> {
    let output = crate::device::adb::shell(
        serial,
        format!(
            "pid={}; \
             if [ ! -e /proc/$pid ]; then echo __shadowdroid_video_missing__; \
             elif [ ! -r /proc/$pid/stat ]; then echo __shadowdroid_video_unknown__; \
             else ticks=$(awk '{{print $22}}' /proc/$pid/stat 2>/dev/null); \
               if [ -z \"$ticks\" ]; then echo __shadowdroid_video_unknown__; \
               elif [ \"$ticks\" = \"{}\" ]; then echo __shadowdroid_video_same__; \
               else echo __shadowdroid_video_reused__; fi; fi",
            identity.pid, identity.start_ticks
        ),
    )
    .await?;
    Ok(match output.trim() {
        "__shadowdroid_video_same__" => DeviceProcessGeneration::Same,
        "__shadowdroid_video_missing__" => DeviceProcessGeneration::Missing,
        "__shadowdroid_video_reused__" => DeviceProcessGeneration::Reused,
        _ => DeviceProcessGeneration::Unknown,
    })
}

async fn signal_owned(serial: &Serial, identity: &DeviceProcessIdentity) -> Result<()> {
    let output =
        crate::device::adb::shell_mutating(serial, ownership_command(identity, true)).await?;
    if output
        .lines()
        .any(|line| line.trim() == "__shadowdroid_video_signalled__")
    {
        return Ok(());
    }
    Err(ownership_unproven(serial, identity))
}

fn ownership_unproven(serial: &Serial, identity: &DeviceProcessIdentity) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "video_ownership_unproven",
        "video",
        "refusing to signal a recorder whose exact device process identity could not be proven",
    )
    .detail(json!({
        "device": serial.as_str(),
        "pid": identity.pid,
        "start_ticks": identity.start_ticks,
        "remote_path": identity.remote_path,
    }))
    .next_actions([
        "shadowdroid video status",
        "shadowdroid doctor --json",
        "inspect the device process before manual intervention",
    ])
    .into()
}

fn ownership_command(identity: &DeviceProcessIdentity, signal: bool) -> String {
    let path = crate::config::quote_device_shell_arg(&identity.remote_path);
    let success = if signal {
        format!(
            "kill -2 {} 2>/dev/null && echo __shadowdroid_video_signalled__",
            identity.pid
        )
    } else {
        "echo __shadowdroid_video_owned__".into()
    };
    format!(
        "pid={pid}; \
         [ -r /proc/$pid/stat ] && \
         [ \"$(awk '{{print $22}}' /proc/$pid/stat 2>/dev/null)\" = \"{ticks}\" ] && \
         [ \"$(cat /proc/$pid/comm 2>/dev/null)\" = \"screenrecord\" ] && \
         [ \"$(readlink /proc/$pid/exe 2>/dev/null)\" = {executable} ] && \
         tr '\\000' '\\n' < /proc/$pid/cmdline 2>/dev/null | grep -F -x {path} >/dev/null && \
         {success}",
        pid = identity.pid,
        ticks = identity.start_ticks,
        executable = crate::config::quote_device_shell_arg(&identity.executable),
    )
}

fn screenrecord_command(
    capture: &CaptureArgs,
    executable: &str,
    remote_path: &str,
    identity_path: &str,
) -> String {
    let mut arguments = vec![
        "--time-limit".to_string(),
        capture.segment_seconds.to_string(),
    ];
    if let Some(size) = &capture.size {
        arguments.push("--size".into());
        arguments.push(size.clone());
    }
    if let Some(bit_rate) = capture.bit_rate {
        arguments.push("--bit-rate".into());
        arguments.push(bit_rate.to_string());
    }
    if let Some(display_id) = capture.display_id {
        arguments.push("--display-id".into());
        arguments.push(display_id.to_string());
    }
    if capture.bugreport {
        arguments.push("--bugreport".into());
    }
    arguments.push(crate::config::quote_device_shell_arg(remote_path));
    format!(
        "umask 077; ticks=$(awk '{{print $22}}' /proc/$$/stat) || exit 70; \
         printf '%s %s\\n' \"$$\" \"$ticks\" > {} || exit 71; \
         exec {} {}",
        crate::config::quote_device_shell_arg(identity_path),
        crate::config::quote_device_shell_arg(executable),
        arguments.join(" ")
    )
}

async fn watch_rotation(serial: Serial, tx: mpsc::Sender<u8>, cancel: CancellationToken) {
    let mut stable = backend::orientation(&serial).await;
    let mut candidate = None;
    let mut observations = 0u8;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(750)) => {}
        }
        let observed = backend::orientation(&serial).await;
        if stable.is_none() && observed.is_some() {
            stable = observed;
            candidate = None;
            observations = 0;
            continue;
        }
        if observed == stable || observed.is_none() {
            candidate = None;
            observations = 0;
            continue;
        }
        if observed == candidate {
            observations += 1;
        } else {
            candidate = observed;
            observations = 1;
        }
        if observations >= 2 {
            let Some(value) = observed else { continue };
            stable = observed;
            candidate = None;
            observations = 0;
            if tx.send(value).await.is_err() {
                break;
            }
        }
    }
}

async fn fail_segment(shared: &Arc<Mutex<Shared>>, index: u32, error: &anyhow::Error) {
    let mut state = shared.lock().await;
    if let Ok(segment) = segment_mut(&mut state.manifest, index) {
        segment.state = "failed".into();
        segment.error = Some(format!("{error:#}"));
        segment.stopped_at = Some(session::now_ts());
    }
    state.active.last_error = Some(format!("{error:#}"));
    let _ = state.persist();
}

fn segment_mut(manifest: &mut Manifest, index: u32) -> Result<&mut Segment> {
    manifest
        .segments
        .iter_mut()
        .find(|segment| segment.index == index)
        .ok_or_else(|| anyhow!("video segment {index} is missing from the manifest"))
}

async fn create_remote_session_dir(serial: &Serial, remote_dir: &str) -> Result<()> {
    let root = "/data/local/tmp/shadowdroid-video";
    let command = format!(
        "umask 077; mkdir -p {root} || exit 1; \
         if [ -e {session} ]; then echo __shadowdroid_video_exists__; \
         elif mkdir {session}; then echo __shadowdroid_video_created__; fi",
        session = crate::config::quote_device_shell_arg(remote_dir),
    );
    let output = crate::device::adb::shell_mutating(serial, command).await?;
    if output.contains("__shadowdroid_video_created__") {
        Ok(())
    } else {
        bail!("remote video session path already exists: {remote_dir}")
    }
}

fn safe_session_component(value: &str) -> Result<&str> {
    if !value.is_empty()
        && value.len() <= 80
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        Ok(value)
    } else {
        bail!("invalid internal video session id")
    }
}

fn publish_marker(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("marker has no parent: {}", path.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish {}", path.display()))?;
    paths::protect_file(path)
}

fn remove_markers_if_owned(pid_path: &Path, ctl_path: &Path, pid: u32, control_port: u16) {
    let owns_pid = std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        == Some(pid);
    let owns_control = std::fs::read_to_string(ctl_path)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        == Some(control_port);
    if owns_control {
        let _ = std::fs::remove_file(ctl_path);
    }
    if owns_pid {
        let _ = std::fs::remove_file(pid_path);
    }
}

fn is_terminal_state(state: &str) -> bool {
    matches!(state, "completed" | "partial" | "failed" | "interrupted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recovery_is_a_noop_for_terminal_sessions() {
        let serial = Serial::new("emulator-5554");
        let active = ActiveState {
            schema_version: 1,
            serial: serial.as_str().into(),
            startup_id: "s1".into(),
            session_id: "v1".into(),
            daemon_pid: 42,
            control_port: None,
            bundle: "/nonexistent/terminal-video-session".into(),
            remote_dir: "/data/local/tmp/video/v1".into(),
            state: "completed".into(),
            started_at: 0.0,
            current_segment: None,
            device_process: None,
            last_error: None,
        };

        let recovered = recover(&serial, active).await.unwrap();

        assert_eq!(recovered.state, "completed");
        assert_eq!(recovered.session_id, "v1");
    }
}
