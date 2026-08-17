//! Control plane: line-delimited JSON over the daemon's loopback-TCP control
//! socket. The chosen port lives in `~/.shadowdroid/net/<serial>.ctl` (TCP
//! rather than a Unix domain socket so `net` builds + runs on Windows too).
//!
//! Why a socket (not the existing `watch` stdin model): a *held* intercepted
//! flow must survive across the agent's discrete one-shot `net` commands —
//! observe an `http_intercept` event, reason, then `net resume` as a *separate*
//! process. That shared state lives in the daemon; the verbs are clients here.
//!
//! Protocol: the client sends one JSON request line `{"op": "...", ...}`. The
//! daemon replies with one JSON line (most ops) or a stream of event lines
//! (`watch`) until the client disconnects.

use crate::ids::Serial;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{broadcast, mpsc};

use crate::events::Event;
use crate::net::flow::FlowRecord;
use crate::net::proxy::{
    ActiveReplay, HoldDecision, InterceptCfg, ReleaseHeldResult, SharedState, TerminalHold,
};
use crate::net::rule::compile_rule;
use crate::net::{Matcher, Mutation, RuleSpec, flow, paths, store};

/// In-daemon state the control handlers read/mutate.
pub struct DaemonState {
    pub serial: Serial,
    pub port: u16,
    /// Host-side listener target for `adb reverse tcp:<port> tcp:<host_port>`.
    /// Exposed by status so a repeated `net start` can repair wiring after a
    /// device reboot without restarting the daemon or discarding its rules.
    pub host_port: u16,
    /// Identity and process expected by the parent startup attempt. Both are
    /// exposed by status so readiness cannot be satisfied by stale metadata or
    /// a previous daemon that happens to use the same serial and ports.
    pub startup_id: String,
    pub pid: u32,
    pub started: f64,
    pub capture_session_id: String,
    pub checkpoint_count: AtomicU64,
    /// SHA-256 of the CA cert the daemon signs with, so a repeated `net start`
    /// resolving a *different* CA (e.g. switching projects on one device) can
    /// warn that the live daemon is still using the old one.
    pub ca_fingerprint: String,
    pub flow_count: AtomicU64,
    /// Live event fan-out to `watch` subscribers. `Arc` so the broadcast value
    /// is cheaply `Clone` (the `Event` tree itself isn't `Clone`).
    pub events: broadcast::Sender<Arc<Event>>,
}

fn public_rule(id: &str, spec: &RuleSpec) -> Value {
    let action = crate::net::rule::action_summary(&spec.action);
    let value = json!({
        "id": id,
        "kind": spec.action_kind(),
        "phase": spec.phase(),
        "match_on": spec.match_on,
        "matcher": spec.matcher,
        "action": action,
    });
    value
}

fn capture_redaction_status(policy: Option<&crate::redaction::Policy>) -> Value {
    match policy {
        Some(policy) => json!({
            "enabled": true,
            "policy": policy.label(),
            "version": crate::redaction::POLICY_VERSION,
            "fingerprint": policy.fingerprint(),
            "custom_json_keys": policy.spec().json_keys.len(),
            "custom_patterns": policy.spec().patterns.len(),
        }),
        None => json!({"enabled": false}),
    }
}

fn replay_status(slot: &std::sync::RwLock<Option<Arc<ActiveReplay>>>) -> Value {
    match slot.read().unwrap().clone() {
        Some(active) => json!({
            "active": true,
            "generation": active.generation,
            "count": active.set.len(),
            "active_set_sha256": active.set.active_set_sha256(),
            "loaded_at": active.loaded_at,
        }),
        None => json!({"active": false}),
    }
}

fn publish_replay_set(
    slot: &std::sync::RwLock<Option<Arc<ActiveReplay>>>,
    set: crate::net::replay::ReplaySet,
) -> Result<Value> {
    let count = set.len();
    let active_set_sha256 = set.active_set_sha256().to_string();
    let loaded_at = crate::events::now_ts();
    let mut guard = slot.write().unwrap();
    let generation = guard
        .as_ref()
        .map(|active| active.generation)
        .unwrap_or_default()
        .checked_add(1)
        .context("replay generation overflow")?;
    *guard = Some(Arc::new(ActiveReplay {
        set,
        generation,
        loaded_at,
    }));
    Ok(json!({
        "count": count,
        "generation": generation,
        "active_set_sha256": active_set_sha256,
        "loaded_at": loaded_at,
    }))
}

fn replace_replay_from_wire(
    slot: &std::sync::RwLock<Option<Arc<ActiveReplay>>>,
    payload: Value,
) -> Result<Value> {
    // Decode, validate response bytes/headers, and bind the fingerprint before
    // acquiring the write lock. Publication is one pointer replacement.
    let set = crate::net::replay::ReplaySet::from_wire_value(payload)?;
    publish_replay_set(slot, set)
}

fn request_u32_field(req: &Value, field: &str, default: u32) -> Result<u32> {
    let Some(value) = req.get(field).filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    let Some(raw) = value.as_u64() else {
        return Err(invalid_numeric_request(
            field,
            value,
            "a non-negative integer no greater than 4294967295",
        ));
    };
    u32::try_from(raw).map_err(|_| {
        invalid_numeric_request(
            field,
            value,
            "a non-negative integer no greater than 4294967295",
        )
    })
}

fn request_status_field(req: &Value, field: &str, default: Option<u16>) -> Result<Option<u16>> {
    let Some(value) = req.get(field).filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    let Some(raw) = value.as_u64() else {
        return Err(invalid_numeric_request(
            field,
            value,
            "a final HTTP status integer from 200 to 599",
        ));
    };
    let status = u16::try_from(raw).map_err(|_| {
        invalid_numeric_request(field, value, "a final HTTP status integer from 200 to 599")
    })?;
    if !(200..=599).contains(&status) {
        return Err(invalid_numeric_request(
            field,
            value,
            "a final HTTP status integer from 200 to 599",
        ));
    }
    Ok(Some(status))
}

fn invalid_numeric_request(field: &str, value: &Value, expected: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "net_control_invalid_request",
        "net",
        format!("invalid control request field `{field}`; expected {expected}"),
    )
    .detail(json!({"field": field, "value": value, "expected": expected}))
    .into()
}

/// Serve one control connection. `shared` lets future ops mutate proxy knobs;
/// `stop_tx` lets the `stop` op shut the daemon down.
pub async fn serve_client(
    stream: TcpStream,
    state: Arc<DaemonState>,
    shared: Arc<SharedState>,
    stop_tx: mpsc::Sender<()>,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let req: Value = serde_json::from_str(&line).unwrap_or_else(|_| json!({}));
    if req
        .get("serial")
        .and_then(Value::as_str)
        .is_some_and(|serial| serial != state.serial.as_str())
    {
        write_json(
            &mut wr,
            &json!({
                "ok": false,
                "error": "control request serial does not match this daemon",
                "serial": state.serial,
            }),
        )
        .await?;
        return Ok(());
    }
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");

    match op {
        "status" => {
            let intercepting = shared.intercept.read().unwrap().is_some();
            crate::net::proxy::prune_inactive_holds(&shared.held, &shared.terminal_holds);
            let mut held_flows: Vec<Value> = {
                let held = shared.held.lock().unwrap();
                held.values()
                    .map(|h| {
                        let mut meta = h.meta.clone();
                        if let Some(policy) = &shared.redaction {
                            policy.redact_flow_record(&mut meta);
                        }
                        json!({
                            "id": meta.id,
                            "capture_session_id": meta.capture_session_id,
                            "phase": h.phase,
                            "state": "held",
                            "held_at": h.held_at,
                            "expires_at": h.expires_at,
                            "client_connected": h.tx.as_ref().is_some_and(|tx| !tx.is_closed()),
                            "method": meta.method,
                            "host": meta.host,
                            "path": meta.path,
                            "host_redacted": meta.host_redacted,
                            "path_redacted": meta.path_redacted,
                            "status": meta.status,
                        })
                    })
                    .collect()
            };
            held_flows.sort_by(|left, right| {
                left.get("held_at")
                    .and_then(Value::as_f64)
                    .unwrap_or_default()
                    .total_cmp(
                        &right
                            .get("held_at")
                            .and_then(Value::as_f64)
                            .unwrap_or_default(),
                    )
                    .then_with(|| {
                        left.get("id")
                            .and_then(Value::as_str)
                            .cmp(&right.get("id").and_then(Value::as_str))
                    })
            });
            let ws_intercepting = shared.ws_intercept.read().unwrap().is_some();
            let ws_held: Vec<Value> = {
                let held = shared.ws_held.lock().unwrap();
                held.iter()
                    .map(|(id, frame)| {
                        json!({
                            "id": id,
                            "host": frame.host,
                            "dir": frame.dir,
                            "opcode": frame.opcode,
                            "state": "held",
                        })
                    })
                    .collect()
            };
            write_json(
                &mut wr,
                &json!({
                    "ok": true,
                    "running": true,
                    "serial": state.serial,
                    "port": state.port,
                    "host_port": state.host_port,
                    "startup_id": state.startup_id,
                    "pid": state.pid,
                    "started": state.started,
                    "capture_session_id": state.capture_session_id,
                    "ca_fingerprint": state.ca_fingerprint,
                    "capabilities": {
                        "replay_bundle_format": crate::net::replay::REPLAY_FORMAT_VERSION,
                        "replay_atomic_replace": true,
                    },
                    "capture_redaction": capture_redaction_status(shared.redaction.as_ref()),
                    "replay": replay_status(&shared.replay),
                    "flows": state.flow_count.load(Ordering::Relaxed),
                    "dropped_flows": shared.dropped_flows.load(Ordering::Relaxed),
                    "persistence_errors": shared.persistence_errors.load(Ordering::Relaxed),
                    "held": held_flows.len(),
                    "held_bytes": shared.held_bytes.load(Ordering::Relaxed),
                    "rejected_holds": shared.rejected_holds.load(Ordering::Relaxed),
                    "held_flows": held_flows,
                    "intercepting": intercepting,
                    "ws_intercepting": ws_intercepting,
                    "ws_held": ws_held,
                }),
            )
            .await?;
        }
        "intercept" => {
            let matcher: Matcher = req
                .get("matcher")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let at = req.get("at").and_then(Value::as_str).unwrap_or("response");
            let cfg = InterceptCfg {
                matcher,
                at_request: at == "request" || at == "both",
                at_response: at == "response" || at == "both",
                hold_ms: request_u32_field(&req, "hold_ms", 30_000)?,
                on_timeout_drop: req.get("on_timeout").and_then(Value::as_str) == Some("drop"),
            };
            *shared.intercept.write().unwrap() = Some(cfg);
            write_json(
                &mut wr,
                &json!({"ok": true, "intercepting": true, "at": at}),
            )
            .await?;
        }
        "ws_intercept" => {
            if req.get("clear").and_then(Value::as_bool) == Some(true) {
                *shared.ws_intercept.write().unwrap() = None;
                write_json(&mut wr, &json!({"ok": true, "intercepting": false})).await?;
            } else {
                let cfg = crate::net::ws::WsInterceptCfg {
                    host: req.get("host").and_then(Value::as_str).map(str::to_string),
                    dir: match req.get("dir").and_then(Value::as_str) {
                        Some("c2s") => Some(crate::net::ws::Direction::ClientToServer),
                        Some("s2c") => Some(crate::net::ws::Direction::ServerToClient),
                        _ => None,
                    },
                    opcode: req
                        .get("opcode")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    hold_ms: request_u32_field(&req, "hold_ms", 30_000)?,
                    on_timeout_drop: req.get("on_timeout").and_then(Value::as_str) == Some("drop"),
                };
                *shared.ws_intercept.write().unwrap() = Some(cfg);
                write_json(
                    &mut wr,
                    &json!({"ok": true, "intercepting": true, "protocol": "websocket"}),
                )
                .await?;
            }
        }
        "resume" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            // A WebSocket held frame? Resume with an optional edited payload.
            if let Some(reply) = ws_release(
                &shared,
                id,
                {
                    let payload = req
                        .get("payload_b64")
                        .and_then(Value::as_str)
                        .and_then(crate::net::ws::b64_decode);
                    match payload {
                        Some(bytes) => crate::net::ws::WsHoldDecision::Modify(bytes),
                        None => crate::net::ws::WsHoldDecision::Forward,
                    }
                },
                "resume",
            ) {
                write_json(&mut wr, &reply).await?;
                return Ok(());
            }
            let mutation: Mutation = req
                .get("mutation")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let decision = HoldDecision::Resume(mutation);
            match validate_held_decision(&shared, id, &decision) {
                Ok(()) => {
                    let released = release(&shared, id, "resume", decision);
                    write_json(&mut wr, &released_reply(&shared, id, released)).await?;
                }
                Err(error) => {
                    write_json(&mut wr, &json!({"ok": false, "error": error})).await?;
                }
            }
        }
        "drop" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            if let Some(reply) =
                ws_release(&shared, id, crate::net::ws::WsHoldDecision::Drop, "drop")
            {
                write_json(&mut wr, &reply).await?;
                return Ok(());
            }
            let status = request_status_field(&req, "status", None)?;
            let decision = HoldDecision::Drop(status);
            match validate_held_decision(&shared, id, &decision) {
                Ok(()) => {
                    let released = release(&shared, id, "drop", decision);
                    write_json(&mut wr, &released_reply(&shared, id, released)).await?;
                }
                Err(error) => {
                    write_json(&mut wr, &json!({"ok": false, "error": error})).await?;
                }
            }
        }
        "respond" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let status = request_status_field(&req, "status", Some(200))?.unwrap_or(200);
            let body: Vec<u8> = req
                .get("body")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let headers: Vec<(String, String)> = req
                .get("headers")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let decision = HoldDecision::Respond {
                status,
                body,
                headers,
            };
            match validate_held_decision(&shared, id, &decision) {
                Ok(()) => {
                    let released = release(&shared, id, "respond", decision);
                    write_json(&mut wr, &released_reply(&shared, id, released)).await?;
                }
                Err(error) => {
                    write_json(&mut wr, &json!({"ok": false, "error": error})).await?;
                }
            }
        }
        "ws_inject" => {
            let session = req.get("session").and_then(Value::as_str).unwrap_or("");
            let dir = req.get("dir").and_then(Value::as_str).unwrap_or("s2c");
            let opcode_label = req.get("opcode").and_then(Value::as_str).unwrap_or("text");
            let payload = req
                .get("payload_b64")
                .and_then(Value::as_str)
                .and_then(crate::net::ws::b64_decode)
                .unwrap_or_default();
            let direction = if dir == "c2s" {
                crate::net::ws::Direction::ClientToServer
            } else {
                crate::net::ws::Direction::ServerToClient
            };
            let reply = match crate::net::ws::Opcode::from_label(opcode_label) {
                None => json!({"ok": false, "error": format!("invalid opcode `{opcode_label}`")}),
                Some(opcode) => {
                    let control = shared.ws_control.lock().unwrap();
                    match control.get(session) {
                        None => json!({
                            "ok": false,
                            "error": format!("no live WebSocket session `{session}` (it may have closed)"),
                        }),
                        Some(handle) => {
                            let host = handle.host.clone();
                            let permessage_deflate = handle.permessage_deflate;
                            match handle.inject(
                                direction,
                                crate::net::ws::InjectedFrame {
                                    opcode,
                                    payload: payload.clone(),
                                },
                            ) {
                                // The frame is enqueued and spliced at the next
                                // frame boundary (a data frame waits for a message
                                // boundary) — `queued`, not confirmed on-wire.
                                Ok(()) => json!({
                                    "ok": true,
                                    "queued": true,
                                    "session": session,
                                    "host": host,
                                    "dir": dir,
                                    "opcode": opcode_label,
                                    "bytes": payload.len(),
                                    "permessage_deflate": permessage_deflate,
                                }),
                                Err(error) => json!({"ok": false, "error": error}),
                            }
                        }
                    }
                }
            };
            write_json(&mut wr, &reply).await?;
        }
        "show" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            crate::net::proxy::prune_inactive_holds(&shared.held, &shared.terminal_holds);
            let (flow, lifecycle) = {
                let held = shared.held.lock().unwrap();
                (
                    held.get(id).map(|h| h.meta.clone()),
                    held.get(id)
                        .map(|h| serde_json::to_value(h.lifecycle()).unwrap_or_default()),
                )
            };
            let terminal = if flow.is_none() {
                shared.terminal_holds.lock().unwrap().get(id)
            } else {
                None
            };
            write_json(
                &mut wr,
                &json!({
                    "ok": flow.is_some(),
                    "flow": flow,
                    "lifecycle": lifecycle.or_else(|| terminal.as_ref().and_then(|value| serde_json::to_value(value).ok())),
                    "terminal_state": terminal.as_ref().map(failure_terminal_state),
                }),
            )
            .await?;
        }
        "checkpoint" => {
            let sequence = flow::last_sequence();
            let checkpoint = format!(
                "cp{:x}",
                state.checkpoint_count.fetch_add(1, Ordering::Relaxed) + 1
            );
            let record = store::CheckpointRecord {
                kind: "capture_checkpoint".into(),
                checkpoint: checkpoint.clone(),
                capture_session_id: state.capture_session_id.clone(),
                created_at: crate::events::now_ts(),
                last_flow_id: (sequence > 0).then(|| format!("f{sequence:x}")),
                last_flow_sequence: sequence,
            };
            match store::append_checkpoint(&state.serial, &record) {
                Ok(()) => {
                    write_json(
                        &mut wr,
                        &json!({
                            "ok": true,
                            "checkpoint": record.checkpoint,
                            "capture_session_id": record.capture_session_id,
                            "created_at": record.created_at,
                            "last_flow_id": record.last_flow_id,
                            "last_flow_sequence": record.last_flow_sequence,
                        }),
                    )
                    .await?
                }
                Err(error) => {
                    write_json(
                        &mut wr,
                        &json!({"ok": false, "error": format!("persist checkpoint: {error}")}),
                    )
                    .await?
                }
            }
        }
        "log_clear" => {
            let sequence = flow::last_sequence();
            let record = store::ClearRecord {
                kind: "capture_clear".into(),
                capture_session_id: state.capture_session_id.clone(),
                cleared_at: crate::events::now_ts(),
                after_flow_id: (sequence > 0).then(|| format!("f{sequence:x}")),
                after_flow_sequence: sequence,
            };
            match store::append_clear(&state.serial, &record) {
                Ok(()) => {
                    write_json(
                        &mut wr,
                        &json!({
                            "ok": true,
                            "capture_session_id": record.capture_session_id,
                            "cleared_at": record.cleared_at,
                            "after_flow_id": record.after_flow_id,
                            "after_flow_sequence": record.after_flow_sequence,
                            "scope": "queryable_history",
                            "active_proxy_preserved": true,
                            "rules_preserved": shared.rules.read().unwrap().len(),
                        }),
                    )
                    .await?
                }
                Err(error) => {
                    write_json(
                        &mut wr,
                        &json!({"ok": false, "error": format!("persist clear boundary: {error}")}),
                    )
                    .await?
                }
            }
        }
        "rule_add" => {
            let spec = req
                .get("spec")
                .cloned()
                .ok_or_else(|| "missing rule spec".to_string())
                .and_then(|value| {
                    serde_json::from_value::<RuleSpec>(value).map_err(|e| e.to_string())
                });
            match spec {
                Err(error) => {
                    write_json(
                        &mut wr,
                        &json!({"ok": false, "error": format!("invalid rule spec: {error}")}),
                    )
                    .await?
                }
                Ok(spec) => match compile_rule(spec) {
                    Err(error) => {
                        write_json(&mut wr, &json!({"ok": false, "error": error})).await?
                    }
                    Ok(rule) => {
                        let id = next_rule_id();
                        let mut reply = public_rule(&id, &rule.spec);
                        shared.rules.write().unwrap().push((id.clone(), rule));
                        if let Value::Object(fields) = &mut reply {
                            fields.insert("ok".into(), json!(true));
                        }
                        write_json(&mut wr, &reply).await?;
                    }
                },
            }
        }
        "rule_list" => {
            let rules: Vec<Value> = shared
                .rules
                .read()
                .unwrap()
                .iter()
                .map(|(id, rule)| public_rule(id, &rule.spec))
                .collect();
            write_json(&mut wr, &json!({"ok": true, "rules": rules})).await?;
        }
        "rule_rm" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let removed = {
                let mut rules = shared.rules.write().unwrap();
                let before = rules.len();
                rules.retain(|(rid, _)| rid != id);
                rules.len() < before
            };
            write_json(
                &mut wr,
                &json!({"ok": removed, "id": id, "removed": removed}),
            )
            .await?;
        }
        "rule_clear" => {
            let n = {
                let mut rules = shared.rules.write().unwrap();
                let n = rules.len();
                rules.clear();
                n
            };
            write_json(&mut wr, &json!({"ok": true, "cleared": n})).await?;
        }
        "replay_replace_v1" => {
            let expected_startup_id = req
                .get("expected_startup_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if expected_startup_id != state.startup_id {
                write_json(
                    &mut wr,
                    &json!({
                        "ok": false,
                        "code": "replay_daemon_identity_changed",
                        "error": "net daemon identity changed after replay capability preflight",
                        "expected_startup_id": expected_startup_id,
                        "actual_startup_id": state.startup_id,
                    }),
                )
                .await?;
            } else {
                let payload = req.get("payload").cloned().unwrap_or(Value::Null);
                match replace_replay_from_wire(&shared.replay, payload) {
                    Ok(summary) => {
                        let mut reply = summary;
                        reply["ok"] = Value::Bool(true);
                        write_json(&mut wr, &reply).await?;
                    }
                    Err(error) => {
                        write_json(
                            &mut wr,
                            &json!({
                                "ok": false,
                                "code": "replay_payload_invalid",
                                "error": format!("invalid replay payload: {error:#}"),
                            }),
                        )
                        .await?;
                    }
                }
            }
        }
        // Compatibility for pre-bundle CLIs. It is strict and atomic: invalid
        // or empty legacy input never clears an already active replay set.
        "replay" => {
            let candidate = (|| -> Result<crate::net::replay::ReplaySet> {
                let value = req.get("flows").cloned().context("missing legacy flows")?;
                let flows: Vec<FlowRecord> =
                    serde_json::from_value(value).context("decode legacy replay flows")?;
                let sources = flows
                    .iter()
                    .map(crate::net::replay::ReplaySource::from_flow_or_default_port)
                    .collect::<Result<Vec<_>>>()?;
                crate::net::replay::build_bundle(&sources)?.into_replay_set(None)
            })();
            match candidate.and_then(|set| publish_replay_set(&shared.replay, set)) {
                Ok(summary) => {
                    let mut reply = summary;
                    reply["ok"] = Value::Bool(true);
                    reply["legacy"] = Value::Bool(true);
                    write_json(&mut wr, &reply).await?;
                }
                Err(error) => {
                    write_json(
                        &mut wr,
                        &json!({
                            "ok": false,
                            "code": "legacy_replay_invalid",
                            "error": format!("invalid legacy replay input: {error:#}"),
                        }),
                    )
                    .await?;
                }
            }
        }
        "watch" => {
            let matcher: Matcher = req
                .get("matcher")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let mut rx = state.events.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if event_matches(&ev, &matcher)
                            && write_json(&mut wr, &serde_json::to_value(ev.as_ref())?)
                                .await
                                .is_err()
                        {
                            break; // client went away
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        if write_json(&mut wr, &json!({
                            "type": "warning",
                            "stage": "net_watch",
                            "code": "events_lagged",
                            "dropped": dropped,
                            "msg": "the watcher could not keep up; some live network events were skipped",
                            "next_actions": ["use `shadowdroid net log` to recover persisted completed flows", "reduce downstream processing per event"]
                        })).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        "stop" => {
            write_json(&mut wr, &json!({"ok": true, "stopping": true})).await?;
            let _ = stop_tx.send(()).await;
        }
        other => {
            write_json(
                &mut wr,
                &json!({"ok": false, "error": format!("unknown op {other:?}")}),
            )
            .await?;
        }
    }
    Ok(())
}

fn event_matches(ev: &Event, m: &Matcher) -> bool {
    match ev {
        Event::Http {
            host,
            path,
            method,
            status,
            ..
        }
        | Event::HttpIntercept {
            host,
            path,
            method,
            status,
            ..
        } => {
            let sub = |hay: &str, n: &Option<String>| {
                n.as_deref()
                    .map(|x| hay.to_lowercase().contains(&x.to_lowercase()))
                    .unwrap_or(true)
            };
            sub(host, &m.host)
                && sub(path, &m.path)
                && sub(method, &m.method)
                && m.status.map(|s| *status == Some(s)).unwrap_or(true)
        }
        // A handshake failure only carries a host — apply just the host filter
        // (path/method/status don't apply to a connection that never spoke HTTP).
        Event::TlsError { host, .. } => m
            .host
            .as_deref()
            .map(|x| host.to_lowercase().contains(&x.to_lowercase()))
            .unwrap_or(true),
        // A WebSocket upgrade carries host + path; method/status are HTTP-only
        // filters, so their presence excludes WS from that stream.
        Event::WsOpen { host, path, .. } => {
            let sub = |hay: &str, n: &Option<String>| {
                n.as_deref()
                    .map(|x| hay.to_lowercase().contains(&x.to_lowercase()))
                    .unwrap_or(true)
            };
            m.method.is_none() && m.status.is_none() && sub(host, &m.host) && sub(path, &m.path)
        }
        // Messages/closes/intercepts carry only host; a path/method/status
        // filter excludes them.
        Event::WsMsg { host, .. }
        | Event::WsClose { host, .. }
        | Event::WsIntercept { host, .. } => {
            m.method.is_none()
                && m.status.is_none()
                && m.path.is_none()
                && m.host
                    .as_deref()
                    .map(|x| host.to_lowercase().contains(&x.to_lowercase()))
                    .unwrap_or(true)
        }
        // Non-network events never flow over network stream clients.
        _ => false,
    }
}

fn next_rule_id() -> String {
    static C: AtomicU64 = AtomicU64::new(1);
    format!("r{}", C.fetch_add(1, Ordering::Relaxed))
}

fn validate_final_status(status: u16) -> Result<(), String> {
    if (200..=599).contains(&status) {
        Ok(())
    } else {
        Err(format!(
            "invalid final HTTP status {status}; expected 200..=599"
        ))
    }
}

fn validate_header(name: &str, value: &str) -> Result<(), String> {
    name.parse::<http::header::HeaderName>()
        .map_err(|_| format!("invalid HTTP header name {name:?}"))?;
    value
        .parse::<http::header::HeaderValue>()
        .map_err(|_| format!("invalid HTTP header value for {name:?}"))?;
    Ok(())
}

fn is_managed_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-encoding"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn is_managed_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn validate_held_decision(
    shared: &SharedState,
    id: &str,
    decision: &HoldDecision,
) -> Result<(), String> {
    let response_phase = shared
        .held
        .lock()
        .unwrap()
        .get(id)
        .map(|held| held.meta.status.is_some());
    let Some(response_phase) = response_phase else {
        // Preserve the existing idempotent "already released" reply rather
        // than reporting an input error for a flow that no longer exists.
        return Ok(());
    };
    match decision {
        HoldDecision::Drop(status) => {
            if let Some(status) = status {
                validate_final_status(*status)?;
            }
        }
        HoldDecision::Respond {
            status, headers, ..
        } => {
            validate_final_status(*status)?;
            for (name, value) in headers {
                validate_header(name, value)?;
                if is_managed_response_header(name) {
                    return Err(format!("response header {name:?} is managed by the proxy"));
                }
            }
        }
        HoldDecision::Resume(mutation) => {
            if let Some(status) = mutation.set_status {
                if !response_phase {
                    return Err("set-status is only valid for a response-phase hold".into());
                }
                validate_final_status(status)?;
            }
            if mutation.set_url.is_some() && response_phase {
                return Err("set-url is only valid for a request-phase hold".into());
            }
            if let Some(url) = &mutation.set_url {
                let parsed = reqwest::Url::parse(url)
                    .map_err(|error| format!("invalid replacement URL {url:?}: {error}"))?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
                    return Err(format!(
                        "replacement URL must be an absolute http(s) URL: {url:?}"
                    ));
                }
            }
            if let Some((pattern, _)) = &mutation.replace {
                regex::Regex::new(pattern)
                    .map_err(|error| format!("invalid replacement regex {pattern:?}: {error}"))?;
            }
            for name in &mutation.remove_headers {
                name.parse::<http::header::HeaderName>()
                    .map_err(|_| format!("invalid HTTP header name {name:?}"))?;
                if (response_phase && is_managed_response_header(name))
                    || (!response_phase && is_managed_request_header(name))
                {
                    return Err(format!(
                        "header {name:?} is managed by the proxy in this interception phase"
                    ));
                }
            }
            for (name, value) in &mutation.set_headers {
                validate_header(name, value)?;
                if response_phase && is_managed_response_header(name) {
                    return Err(format!("response header {name:?} is managed by the proxy"));
                }
                if !response_phase && is_managed_request_header(name) {
                    return Err(format!("request header {name:?} is managed by the proxy"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn validate_rule(spec: &RuleSpec) -> Result<(), String> {
    compile_rule(spec.clone()).map(|_| ())
}

/// Hand a held flow its decision (fires the proxy's oneshot). Shares the atomic
/// claim and bounded terminal history with the deadline/cancellation paths.
fn release(
    shared: &SharedState,
    id: &str,
    action: &str,
    decision: HoldDecision,
) -> ReleaseHeldResult {
    crate::net::proxy::release_held(&shared.held, &shared.terminal_holds, id, action, decision)
}

/// Deliver a decision to a held WebSocket frame. Returns `None` if `id` isn't a
/// currently-held WS frame (so the caller falls back to the HTTP-flow path).
fn ws_release(
    shared: &SharedState,
    id: &str,
    decision: crate::net::ws::WsHoldDecision,
    action: &str,
) -> Option<Value> {
    let (tx, host, dir, opcode) = {
        let mut held = shared.ws_held.lock().unwrap();
        let entry = held.get_mut(id)?;
        (
            entry.tx.take(),
            entry.host.clone(),
            entry.dir.clone(),
            entry.opcode.clone(),
        )
    };
    let delivered = tx.is_some_and(|tx| tx.send(decision).is_ok());
    Some(json!({
        "ok": delivered,
        "id": id,
        "action": action,
        "host": host,
        "dir": dir,
        "opcode": opcode,
        "delivered": delivered,
        "protocol": "websocket",
    }))
}

fn failure_terminal_state(terminal: &TerminalHold) -> &'static str {
    if terminal.state == "released" {
        "already_released"
    } else {
        terminal.state
    }
}

fn terminal_failure_reply(id: &str, terminal: &TerminalHold) -> Value {
    let terminal_state = failure_terminal_state(terminal);
    json!({
        "ok": false,
        "id": id,
        "released": false,
        "terminal_state": terminal_state,
        "phase": terminal.phase,
        "held_at": terminal.held_at,
        "expires_at": terminal.expires_at,
        "terminal_at": terminal.terminal_at,
        "action": terminal.action,
        "error": format!("held flow `{id}` is no longer actionable: {terminal_state}"),
    })
}

fn missing_held_reply(id: &str, terminal: Option<&TerminalHold>) -> Value {
    if let Some(terminal) = terminal {
        terminal_failure_reply(id, terminal)
    } else {
        json!({
            "ok": false,
            "id": id,
            "released": false,
            "terminal_state": "unknown_id",
            "observed_at": crate::events::now_ts(),
            "error": format!("held flow `{id}` is unknown to this proxy session"),
        })
    }
}

fn released_reply(shared: &SharedState, id: &str, released: ReleaseHeldResult) -> Value {
    match released {
        ReleaseHeldResult::Released(terminal) => json!({
            "ok": true,
            "id": id,
            "released": true,
            "state": "released",
            "phase": terminal.phase,
            "held_at": terminal.held_at,
            "expires_at": terminal.expires_at,
            "terminal_at": terminal.terminal_at,
            "action": terminal.action,
        }),
        ReleaseHeldResult::ClientCanceled(terminal) => terminal_failure_reply(id, &terminal),
        ReleaseHeldResult::DeadlineExpired(terminal) => terminal_failure_reply(id, &terminal),
        ReleaseHeldResult::Missing => {
            let terminal = shared.terminal_holds.lock().unwrap().get(id);
            missing_held_reply(id, terminal.as_ref())
        }
    }
}

async fn write_json(wr: &mut OwnedWriteHalf, v: &Value) -> Result<()> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;
    wr.flush().await?;
    Ok(())
}

// ── client side (used by the `net` verbs) ─────────────────────

/// Is the exact daemon for `serial` alive and speaking the scoped control
/// protocol? A bare TCP connect is insufficient because a stale `.ctl` port
/// may have been reused by an unrelated local listener.
pub async fn is_running(serial: &Serial) -> bool {
    let Ok(status) = request(serial, json!({"op": "status"})).await else {
        return false;
    };
    status_matches_live_daemon(serial, &status, daemon_pid(serial))
}

fn status_matches_live_daemon(serial: &Serial, status: &Value, marker_pid: Option<u32>) -> bool {
    status.get("ok").and_then(Value::as_bool) == Some(true)
        && status.get("running").and_then(Value::as_bool) == Some(true)
        && status.get("serial").and_then(Value::as_str) == Some(serial.as_str())
        && status
            .get("startup_id")
            .and_then(Value::as_str)
            .is_some_and(|startup_id| !startup_id.is_empty())
        && status
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .is_some_and(|pid| Some(pid) == marker_pid)
}

/// The daemon's loopback control port from its `.ctl` file, if present.
fn read_ctl_port(serial: &Serial) -> Option<u16> {
    let path = paths::ctl_path(serial).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The daemon pid from its pidfile, if present + parseable.
pub fn daemon_pid(serial: &Serial) -> Option<u32> {
    let path = paths::pid_path(serial).ok()?;
    parse_daemon_pid(&std::fs::read_to_string(path).ok()?)
}

fn parse_daemon_pid(value: &str) -> Option<u32> {
    value.trim().parse().ok().filter(|pid| *pid != 0)
}

async fn connect(serial: &Serial) -> Result<TcpStream> {
    let port = read_ctl_port(serial).ok_or_else(|| {
        anyhow!("no net proxy daemon for {serial}. Is `shadowdroid net start` running?")
    })?;
    TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| {
        anyhow!(
            "cannot reach the net proxy daemon on 127.0.0.1:{port}: {e}. Is `net start` running?"
        )
    })
}

/// Send one request, read one JSON response line.
pub async fn request(serial: &Serial, req: Value) -> Result<Value> {
    let req = scoped_request(serial, req);
    tokio::time::timeout(std::time::Duration::from_secs(5), request_once(serial, req))
        .await
        .map_err(|_| {
            crate::diagnostic::DiagnosticError::new(
            "net_control_timeout",
            "net",
            "network daemon did not reply within 5 seconds",
        )
        .retryable(true)
        .detail(json!({"serial": serial.as_str(), "timeout_ms": 5000}))
        .next_actions([
            "run `shadowdroid net status` to check the daemon",
            "if it remains unresponsive, run `shadowdroid net stop`, then `shadowdroid net start`",
        ])
        })?
}

async fn request_once(serial: &Serial, req: Value) -> Result<Value> {
    let stream = connect(serial).await?;
    let (rd, mut wr) = stream.into_split();
    write_request(&mut wr, &req).await?;
    let mut lines = BufReader::new(rd).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("daemon closed the connection without replying"))?;
    Ok(serde_json::from_str(&line)?)
}

/// Send a streaming request (`watch`) and print each response line to stdout
/// until EOF or Ctrl-C.
pub async fn request_stream(serial: &Serial, req: Value) -> Result<()> {
    let req = scoped_request(serial, req);
    let stream = tokio::time::timeout(std::time::Duration::from_secs(5), connect(serial))
        .await
        .map_err(|_| {
            crate::diagnostic::DiagnosticError::new(
                "net_control_timeout",
                "net",
                "network daemon connection timed out after 5 seconds",
            )
            .retryable(true)
            .next_actions([
                "run `shadowdroid net status` to check the daemon",
                "restart the network session if it remains unresponsive",
            ])
        })??;
    let (rd, mut wr) = stream.into_split();
    write_request(&mut wr, &req).await?;
    let mut lines = BufReader::new(rd).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line? {
                Some(l) => println!("{l}"),
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

fn scoped_request(serial: &Serial, mut req: Value) -> Value {
    if let Value::Object(object) = &mut req {
        object.insert("serial".into(), Value::String(serial.to_string()));
    }
    req
}

async fn write_request(wr: &mut OwnedWriteHalf, req: &Value) -> Result<()> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;
    wr.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{
        Matcher, RuleAction, RuleMatchOn, RuleMatcher, RuleSpec, RuleTerminal, RuleTransform,
        SyntheticResponseSpec,
    };

    fn spec(kind: &str, args: &[&str]) -> RuleSpec {
        legacy_spec(kind, Matcher::default(), None, None, None, None, args).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn legacy_spec(
        kind: &str,
        matcher: Matcher,
        content_type: Option<&str>,
        operation_name: Option<&str>,
        dir: Option<&str>,
        opcode: Option<&str>,
        args: &[&str],
    ) -> Result<RuleSpec, String> {
        RuleSpec::from_legacy_parts(
            kind.into(),
            matcher,
            content_type.map(str::to_string),
            operation_name.map(str::to_string),
            None,
            dir.map(str::to_string),
            opcode.map(str::to_string),
            args.iter().map(|s| s.to_string()).collect(),
        )
    }

    #[test]
    fn validate_rule_knows_request_and_response_header_kinds() {
        // Both header kinds need name + value.
        assert!(validate_rule(&spec("set-request-header", &["x-debug", "1"])).is_ok());
        assert!(
            validate_rule(&spec("set-response-header", &["cache-control", "no-store"])).is_ok()
        );
        assert!(
            legacy_spec(
                "set-request-header",
                Matcher::default(),
                None,
                None,
                None,
                None,
                &["x-debug"]
            )
            .is_err()
        );

        // The old umbrella `set-header` is gone — it now reads as unknown so a
        // stale rule fails loudly instead of silently applying to the wrong phase.
        assert!(
            legacy_spec(
                "set-header",
                Matcher::default(),
                None,
                None,
                None,
                None,
                &["a", "b"]
            )
            .is_err()
        );
    }

    #[test]
    fn capture_redaction_status_is_explicit_without_exposing_custom_patterns() {
        assert_eq!(capture_redaction_status(None), json!({"enabled": false}));

        let policy = crate::redaction::Policy::new(crate::redaction::PolicySpec {
            json_keys: vec!["customerCode".into()],
            patterns: vec!["SENTINEL-PRIVATE-PATTERN".into()],
        })
        .unwrap();
        let status = capture_redaction_status(Some(&policy));
        assert_eq!(status["enabled"], true);
        assert_eq!(status["version"], crate::redaction::POLICY_VERSION);
        assert_eq!(status["custom_json_keys"], 1);
        assert_eq!(status["custom_patterns"], 1);
        assert_eq!(status["fingerprint"], policy.fingerprint());
        assert!(!status.to_string().contains("SENTINEL-PRIVATE-PATTERN"));
    }

    fn replay_set(path: &str, response: &str) -> crate::net::replay::ReplaySet {
        let flow = FlowRecord {
            id: format!("fixture-{path}"),
            method: "GET".into(),
            scheme: "https".into(),
            host: "api.example.com".into(),
            port: Some(443),
            path: path.into(),
            status: Some(200),
            resp_headers: vec![("content-type".into(), "text/plain".into())],
            resp_type: Some("text/plain".into()),
            resp_len: response.len() as u64,
            resp_body: Some(response.into()),
            ..Default::default()
        };
        crate::net::replay::build_bundle(&[
            crate::net::replay::ReplaySource::from_flow(&flow).unwrap()
        ])
        .unwrap()
        .into_replay_set(None)
        .unwrap()
    }

    #[test]
    fn replay_replacement_validates_before_one_atomic_generation_swap() {
        let slot = std::sync::RwLock::new(None);
        let first = replay_set("/a", "first");
        let first_fingerprint = first.active_set_sha256().to_string();
        let first_summary = publish_replay_set(&slot, first).unwrap();
        assert_eq!(first_summary["generation"], 1);
        let first_arc = slot.read().unwrap().clone().unwrap();

        let second = replay_set("/b", "second");
        let mut corrupt = serde_json::to_value(second.to_payload()).unwrap();
        corrupt["entries"][0]["response"]["body"] = json!("tampered");
        assert!(replace_replay_from_wire(&slot, corrupt).is_err());
        let after_rejection = slot.read().unwrap().clone().unwrap();
        assert!(Arc::ptr_eq(&first_arc, &after_rejection));
        assert_eq!(after_rejection.generation, 1);
        assert_eq!(after_rejection.set.active_set_sha256(), first_fingerprint);

        let second_fingerprint = second.active_set_sha256().to_string();
        let second_summary =
            replace_replay_from_wire(&slot, serde_json::to_value(second.to_payload()).unwrap())
                .unwrap();
        assert_eq!(second_summary["generation"], 2);
        assert_eq!(second_summary["active_set_sha256"], second_fingerprint);
        let status = replay_status(&slot);
        assert_eq!(status["generation"], 2);
        assert_eq!(status["count"], 1);
        assert_eq!(status["active_set_sha256"], second_fingerprint);
    }

    #[test]
    fn respond_rule_is_atomic_validated_and_publicly_summarized() {
        let response = SyntheticResponseSpec {
            status: 401,
            headers: vec![("content-type".into(), "application/json".into())],
            body: br#"{"error":"unauthorized"}"#.to_vec(),
        };
        let spec = RuleSpec::from_legacy_parts(
            "respond".into(),
            Matcher {
                host: Some("api.example.com".into()),
                method: Some("POST".into()),
                ..Default::default()
            },
            None,
            Some("currentSession".into()),
            Some(response),
            None,
            None,
            vec![],
        )
        .unwrap();
        assert!(validate_rule(&spec).is_ok());

        let public = public_rule("r12", &spec);
        assert_eq!(public["phase"], "request");
        assert_eq!(public["kind"], "respond");
        assert_eq!(public["match_on"], "original");
        assert_eq!(public["action"]["category"], "terminal");
        assert_eq!(public["action"]["terminal"]["type"], "respond");
        assert_eq!(public["action"]["terminal"]["response"]["status"], 401);
        assert_eq!(public["action"]["terminal"]["response"]["body_bytes"], 24);
        assert!(
            public["action"]["terminal"]["response"]
                .get("body")
                .is_none()
        );

        let mut invalid = spec;
        let RuleAction::Terminal {
            terminal: RuleTerminal::Respond { response },
        } = &mut invalid.action
        else {
            panic!("expected respond action")
        };
        response.headers = vec![("content-length".into(), "1".into())];
        assert!(validate_rule(&invalid).is_err());
    }

    #[test]
    fn validate_rule_rejects_values_that_would_be_silent_noops() {
        for (kind, args) in [
            ("delay", &["forever"][..]),
            ("set-status", &["199"][..]),
            ("set-status", &["700"][..]),
        ] {
            let decoded = legacy_spec(kind, Matcher::default(), None, None, None, None, args);
            assert!(decoded.is_err() || validate_rule(&decoded.unwrap()).is_err());
        }
        for invalid in [
            spec("set-request-header", &["bad header", "value"]),
            spec("set-request-header", &["Host", "example.test"]),
            spec("set-request-header", &["Transfer-Encoding", "chunked"]),
            spec("set-response-header", &["x-test", "line\nfeed"]),
            spec("set-response-header", &["content-length", "1"]),
            spec("replace", &["(", "replacement"]),
        ] {
            assert!(validate_rule(&invalid).is_err(), "accepted {invalid:?}");
        }

        for target in ["", "not a host", "ftp://example.test", "example.test?q=1"] {
            assert!(validate_rule(&spec("map-remote", &[target])).is_err());
        }
        assert!(validate_rule(&spec("map-remote", &["localhost:8080/api"])).is_ok());

        let request_filtered_by_response = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::ContentType {
                contains: "application/json".into(),
            },
            action: RuleAction::Delay { milliseconds: 1 },
        };
        assert!(validate_rule(&request_filtered_by_response).is_err());

        let request_filtered_by_response = RuleSpec {
            matcher: RuleMatcher::Status { equals: 200 },
            ..request_filtered_by_response
        };
        assert!(validate_rule(&request_filtered_by_response).is_err());

        let response_with_status = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::Status { equals: 200 },
            action: RuleAction::Transform {
                transform: RuleTransform::ReplaceBody {
                    pattern: "old".into(),
                    replacement: "new".into(),
                },
            },
        };
        assert!(validate_rule(&response_with_status).is_ok());
        let response_with_status = RuleSpec {
            matcher: RuleMatcher::Status { equals: 99 },
            ..response_with_status
        };
        assert!(validate_rule(&response_with_status).is_err());
    }

    #[test]
    fn ws_rules_reject_http_matchers_and_bad_selectors() {
        // A well-formed ws rule with valid selectors is accepted…
        let ok = legacy_spec(
            "ws-set-text",
            Matcher {
                host: Some("chat.app".into()),
                ..Default::default()
            },
            None,
            None,
            Some("s2c"),
            Some("text"),
            &["{\"forced\":true}"],
        )
        .unwrap();
        assert!(validate_rule(&ok).is_ok());
        assert!(validate_rule(&spec("ws-drop", &[])).is_ok());

        // …and public_rule surfaces the WS selectors, not HTTP fields.
        let public = public_rule("r1", &ok);
        assert_eq!(public["phase"], "websocket");
        assert!(public["matcher"].to_string().contains("s2c"));
        assert!(public["matcher"].to_string().contains("text"));

        // HTTP matchers don't apply to WS frame rules — reject, don't ignore.
        let with_path = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::Path {
                contains: "/ws".into(),
            },
            action: RuleAction::Terminal {
                terminal: RuleTerminal::DropWebsocket,
            },
        };
        assert!(validate_rule(&with_path).is_err());
        let with_method = RuleSpec {
            matcher: RuleMatcher::Method {
                equals: "GET".into(),
            },
            ..with_path.clone()
        };
        assert!(validate_rule(&with_method).is_err());
        let with_status = RuleSpec {
            matcher: RuleMatcher::Status { equals: 101 },
            ..with_path.clone()
        };
        assert!(validate_rule(&with_status).is_err());
        let with_ct = RuleSpec {
            matcher: RuleMatcher::ContentType {
                contains: "application/json".into(),
            },
            ..with_path
        };
        assert!(validate_rule(&with_ct).is_err());

        // Bad direction / opcode selectors are rejected.
        assert!(
            legacy_spec(
                "ws-drop",
                Matcher::default(),
                None,
                None,
                Some("up"),
                None,
                &[]
            )
            .is_err()
        );
        assert!(
            legacy_spec(
                "ws-drop",
                Matcher::default(),
                None,
                None,
                None,
                Some("frame"),
                &[]
            )
            .is_err()
        );

        // Arg arity still enforced alongside the WS filters.
        assert!(
            legacy_spec(
                "ws-set-text",
                Matcher::default(),
                None,
                None,
                None,
                None,
                &[]
            )
            .is_err()
        );
        assert!(
            legacy_spec(
                "ws-drop",
                Matcher::default(),
                None,
                None,
                None,
                None,
                &["x"]
            )
            .is_err()
        );
    }

    #[test]
    fn validate_rule_checks_map_local_is_readable_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_rule(&spec("map-local", &[dir.path().to_str().unwrap()])).is_err());
        let file = dir.path().join("body.json");
        std::fs::write(&file, b"{}").unwrap();
        assert!(validate_rule(&spec("map-local", &[file.to_str().unwrap()])).is_ok());
    }

    #[test]
    fn numeric_control_fields_are_checked_before_narrowing() {
        assert_eq!(
            request_u32_field(&json!({}), "hold_ms", 30_000).unwrap(),
            30_000
        );
        assert_eq!(
            request_u32_field(&json!({"hold_ms": u32::MAX}), "hold_ms", 0).unwrap(),
            u32::MAX
        );
        let error = request_u32_field(&json!({"hold_ms": u64::from(u32::MAX) + 1}), "hold_ms", 0)
            .unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&error),
            "net_control_invalid_request"
        );
    }

    #[test]
    fn daemon_pid_rejects_zero_and_malformed_markers() {
        assert_eq!(parse_daemon_pid("42\n"), Some(42));
        assert_eq!(parse_daemon_pid("0"), None);
        assert_eq!(parse_daemon_pid("-1"), None);
        assert_eq!(parse_daemon_pid("not-a-pid"), None);
    }

    #[test]
    fn daemon_liveness_requires_scoped_status_and_marker_identity() {
        let serial = Serial::from("emulator-5554");
        let valid = json!({
            "ok": true,
            "running": true,
            "serial": serial.as_str(),
            "startup_id": "start-1",
            "pid": 42,
        });
        assert!(status_matches_live_daemon(&serial, &valid, Some(42)));
        assert!(!status_matches_live_daemon(&serial, &valid, Some(43)));
        let mut wrong = valid.clone();
        wrong["serial"] = json!("other");
        assert!(!status_matches_live_daemon(&serial, &wrong, Some(42)));
    }

    #[test]
    fn control_status_fields_require_real_http_status_codes() {
        assert_eq!(
            request_status_field(&json!({}), "status", Some(200)).unwrap(),
            Some(200)
        );
        assert_eq!(
            request_status_field(&json!({"status": 599}), "status", None).unwrap(),
            Some(599)
        );
        for value in [
            json!(199),
            json!(600),
            json!(u64::from(u16::MAX) + 1),
            json!("200"),
        ] {
            let error =
                request_status_field(&json!({"status": value}), "status", None).unwrap_err();
            assert_eq!(
                crate::cli::error_code_of(&error),
                "net_control_invalid_request"
            );
        }
    }

    #[test]
    fn terminal_hold_failures_name_the_exact_state_and_timestamps() {
        let released = TerminalHold {
            id: "f19".into(),
            phase: "response".into(),
            state: "released",
            held_at: 10.0,
            expires_at: 20.0,
            terminal_at: 11.0,
            action: Some("resume".into()),
        };
        let reply = terminal_failure_reply("f19", &released);
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["terminal_state"], "already_released");
        assert_eq!(reply["phase"], "response");
        assert_eq!(reply["held_at"], 10.0);
        assert_eq!(reply["expires_at"], 20.0);
        assert_eq!(reply["terminal_at"], 11.0);

        let mut canceled = released;
        canceled.state = "client_canceled";
        canceled.action = None;
        assert_eq!(
            terminal_failure_reply("f19", &canceled)["terminal_state"],
            "client_canceled"
        );

        canceled.state = "deadline_expired";
        assert_eq!(
            terminal_failure_reply("f19", &canceled)["terminal_state"],
            "deadline_expired"
        );

        let unknown = missing_held_reply("never-seen", None);
        assert_eq!(unknown["terminal_state"], "unknown_id");
        assert!(unknown["observed_at"].as_f64().is_some());
    }

    #[test]
    fn tls_error_events_reach_watch_and_respect_host_filter() {
        let ev = Event::TlsError {
            ts: 1.0,
            capture_session_id: "n-test".into(),
            host: "appconfigs.disney-plus.net".into(),
            reason: "rejected".into(),
            host_redacted: false,
            reason_redacted: false,
            redaction_policy: None,
            redaction_policy_version: None,
            next_actions: vec!["shadowdroid net check --fresh".into()],
        };
        // Relayed to watch (previously the catch-all dropped everything non-HTTP).
        assert!(event_matches(&ev, &Matcher::default()));
        // Host filter applies (case-insensitive substring); path/method/status don't.
        assert!(event_matches(
            &ev,
            &Matcher {
                host: Some("DISNEY".into()),
                ..Default::default()
            }
        ));
        assert!(!event_matches(
            &ev,
            &Matcher {
                host: Some("example.com".into()),
                ..Default::default()
            }
        ));
    }
}
