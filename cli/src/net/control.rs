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
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{broadcast, mpsc};

use crate::events::Event;
use crate::net::flow::FlowRecord;
use crate::net::proxy::{HoldDecision, InterceptCfg, ReleaseHeldResult, SharedState, TerminalHold};
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
    let phase = match spec.kind.as_str() {
        "block" | "delay" | "map-local" | "map-remote" | "respond" | "set-request-header" => {
            "request"
        }
        "set-status" | "set-response-header" | "replace" => "response",
        "ws-drop" | "ws-set-text" => "websocket",
        _ => "unknown",
    };
    let mut matcher = serde_json::to_value(&spec.matcher).unwrap_or_else(|_| json!({}));
    if let Value::Object(fields) = &mut matcher {
        fields.retain(|_, value| !value.is_null());
        if let Some(operation_name) = &spec.operation_name {
            fields.insert("graphql_operation".into(), json!(operation_name));
        }
    }
    let mut value = json!({
        "id": id,
        "kind": spec.kind,
        "phase": phase,
        "matcher": matcher,
    });
    let Value::Object(fields) = &mut value else {
        return value;
    };
    if let Some(content_type) = &spec.content_type {
        fields.insert("content_type".into(), json!(content_type));
    }
    if let Some(ws_dir) = &spec.ws_dir {
        fields.insert("ws_dir".into(), json!(ws_dir));
    }
    if let Some(ws_opcode) = &spec.ws_opcode {
        fields.insert("ws_opcode".into(), json!(ws_opcode));
    }
    if !spec.args.is_empty() {
        fields.insert("args".into(), json!(spec.args));
    }
    if let Some(response) = &spec.response {
        let content_type = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value);
        fields.insert(
            "response".into(),
            json!({
                "status": response.status,
                "headers": response.headers,
                "content_type": content_type,
                "body_bytes": response.body.len(),
                "upstream_bypassed": true,
            }),
        );
    }
    value
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
                        json!({
                            "id": h.meta.id,
                            "capture_session_id": h.meta.capture_session_id,
                            "phase": h.phase,
                            "state": "held",
                            "held_at": h.held_at,
                            "expires_at": h.expires_at,
                            "client_connected": h.tx.as_ref().is_some_and(|tx| !tx.is_closed()),
                            "method": h.meta.method,
                            "host": h.meta.host,
                            "path": h.meta.path,
                            "status": h.meta.status,
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
            let spec: Option<RuleSpec> = req
                .get("spec")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok());
            match spec {
                None => {
                    write_json(
                        &mut wr,
                        &json!({"ok": false, "error": "missing/invalid rule spec"}),
                    )
                    .await?
                }
                Some(spec) => match validate_rule(&spec) {
                    Err(e) => write_json(&mut wr, &json!({"ok": false, "error": e})).await?,
                    Ok(()) => {
                        let id = next_rule_id();
                        let mut reply = public_rule(&id, &spec);
                        shared.rules.write().unwrap().push((id.clone(), spec));
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
                .map(|(id, spec)| public_rule(id, spec))
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
        "replay" => {
            let flows: Vec<FlowRecord> = req
                .get("flows")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let count = flows.len();
            *shared.replay.write().unwrap() = if flows.is_empty() { None } else { Some(flows) };
            write_json(&mut wr, &json!({"ok": true, "count": count})).await?;
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

fn validate_request_rule_filters(spec: &RuleSpec) -> Result<(), String> {
    if spec.content_type.is_some() {
        return Err(format!(
            "request-phase rule `{}` cannot match response content-type",
            spec.kind
        ));
    }
    if spec.matcher.status.is_some() {
        return Err(format!(
            "request-phase rule `{}` cannot match response status",
            spec.kind
        ));
    }
    Ok(())
}

/// WebSocket frame rules match only on host + `ws_dir` + `ws_opcode`; the HTTP
/// matchers (path/method/status/content-type) don't apply and would silently do
/// nothing, so reject them rather than mislead. Also validate the WS selectors.
fn validate_ws_rule_filters(spec: &RuleSpec) -> Result<(), String> {
    if spec.matcher.path.is_some() {
        return Err(format!(
            "WebSocket rule `{}` cannot match `--path` (WS rules match on host + --dir + --opcode)",
            spec.kind
        ));
    }
    if spec.matcher.method.is_some() {
        return Err(format!(
            "WebSocket rule `{}` cannot match `--method` (a WebSocket handshake is always GET)",
            spec.kind
        ));
    }
    if spec.matcher.status.is_some() {
        return Err(format!(
            "WebSocket rule `{}` cannot match `--status`",
            spec.kind
        ));
    }
    if spec.content_type.is_some() {
        return Err(format!(
            "WebSocket rule `{}` cannot match `--content-type`",
            spec.kind
        ));
    }
    if let Some(dir) = spec.ws_dir.as_deref()
        && !matches!(dir, "c2s" | "s2c")
    {
        return Err(format!(
            "invalid WebSocket direction {dir:?}; expected `c2s` or `s2c`"
        ));
    }
    if let Some(opcode) = spec.ws_opcode.as_deref()
        && !matches!(opcode, "text" | "binary" | "ping" | "pong" | "close")
    {
        return Err(format!(
            "invalid WebSocket opcode {opcode:?}; expected text|binary|ping|pong|close"
        ));
    }
    Ok(())
}

fn validate_map_remote_target(target: &str) -> Result<(), String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("map-remote target must not be empty".into());
    }
    let candidate = if target.contains("://") {
        target.to_string()
    } else {
        format!("http://{target}")
    };
    let parsed = reqwest::Url::parse(&candidate)
        .map_err(|error| format!("invalid map-remote target {target:?}: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(format!(
            "map-remote target must contain an http(s) host: {target:?}"
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("map-remote target must not embed credentials".into());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("map-remote target must not contain a query or fragment".into());
    }
    Ok(())
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

/// Validate a rule completely before storing it. Rule application runs on the
/// proxy hot path and must not turn bad user input into a silent no-op (or an
/// invalid HTTP response) after the control call already reported success.
fn validate_rule(spec: &RuleSpec) -> Result<(), String> {
    let n = spec.args.len();
    let exact = |k: usize| {
        if n == k {
            Ok(())
        } else {
            Err(format!(
                "rule `{}` needs exactly {k} arg(s), got {n}",
                spec.kind
            ))
        }
    };
    let status = |value: &str| {
        let parsed = value
            .parse::<u16>()
            .map_err(|_| format!("invalid final HTTP status {value:?}; expected 200..=599"))?;
        validate_final_status(parsed)
    };
    if let Some(status) = spec.matcher.status
        && !(100..=599).contains(&status)
    {
        return Err(format!(
            "invalid status matcher {status}; expected 100..=599"
        ));
    }
    if spec.kind != "respond" && (spec.operation_name.is_some() || spec.response.is_some()) {
        return Err(format!(
            "rule `{}` cannot use a GraphQL operation or synthetic response; use kind `respond`",
            spec.kind
        ));
    }
    match spec.kind.as_str() {
        "respond" => {
            validate_request_rule_filters(spec)?;
            exact(0)?;
            if spec
                .operation_name
                .as_deref()
                .is_some_and(|operation| operation.trim().is_empty())
            {
                return Err("respond rule GraphQL operation name must not be empty".into());
            }
            let response = spec
                .response
                .as_ref()
                .ok_or_else(|| "respond rule is missing its synthetic response".to_string())?;
            validate_final_status(response.status)?;
            if response.body.len() > 8 * 1024 * 1024 {
                return Err(format!(
                    "respond rule body is {} bytes; maximum is 8388608",
                    response.body.len()
                ));
            }
            for (name, value) in &response.headers {
                validate_header(name, value)?;
                if is_managed_response_header(name) {
                    return Err(format!(
                        "response framing header {name:?} is managed by the proxy and cannot be set by a rule"
                    ));
                }
            }
            Ok(())
        }
        "block" => {
            validate_request_rule_filters(spec)?;
            match spec.args.as_slice() {
                [] => Ok(()),
                [value] => status(value),
                _ => Err(format!("rule `block` needs zero or one arg, got {n}")),
            }
        }
        "delay" => {
            validate_request_rule_filters(spec)?;
            exact(1)?;
            spec.args[0].parse::<u32>().map(|_| ()).map_err(|_| {
                format!(
                    "invalid delay {:?}; expected milliseconds as a u32",
                    spec.args[0]
                )
            })
        }
        "map-local" => {
            validate_request_rule_filters(spec)?;
            exact(1)?;
            let path = std::path::Path::new(&spec.args[0]);
            let metadata = std::fs::metadata(path).map_err(|error| {
                format!("cannot read map-local file {}: {error}", path.display())
            })?;
            if !metadata.is_file() {
                return Err(format!(
                    "map-local path is not a regular file: {}",
                    path.display()
                ));
            }
            std::fs::File::open(path)
                .map(|_| ())
                .map_err(|error| format!("cannot open map-local file {}: {error}", path.display()))
        }
        "map-remote" => {
            validate_request_rule_filters(spec)?;
            exact(1)?;
            validate_map_remote_target(&spec.args[0])
        }
        "set-status" => {
            exact(1)?;
            status(&spec.args[0])
        }
        "set-request-header" | "set-response-header" => {
            exact(2)?;
            validate_header(&spec.args[0], &spec.args[1])?;
            if spec.kind == "set-request-header" {
                validate_request_rule_filters(spec)?;
                if is_managed_request_header(&spec.args[0]) {
                    return Err(format!(
                        "request header {:?} is managed by the proxy and cannot be set by a rule",
                        spec.args[0]
                    ));
                }
            }
            if spec.kind == "set-response-header" && is_managed_response_header(&spec.args[0]) {
                return Err(format!(
                    "response framing header {:?} is managed by the proxy and cannot be set by a rule",
                    spec.args[0]
                ));
            }
            Ok(())
        }
        "replace" => {
            exact(2)?;
            regex::Regex::new(&spec.args[0])
                .map(|_| ())
                .map_err(|error| format!("invalid replacement regex {:?}: {error}", spec.args[0]))
        }
        // WebSocket frame rules — matched by host + optional dir/opcode, applied
        // per-frame in the live tap (only for sessions without permessage-deflate
        // context takeover; see `net inject` for the always-safe path).
        "ws-drop" => {
            validate_ws_rule_filters(spec)?;
            exact(0)
        }
        "ws-set-text" => {
            validate_ws_rule_filters(spec)?;
            exact(1)
        }
        other => Err(format!(
            "unknown rule kind {other:?} (block|delay|map-local|map-remote|respond|set-status|set-request-header|set-response-header|replace|ws-drop|ws-set-text)"
        )),
    }
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
    use crate::net::{Matcher, RuleSpec, SyntheticResponseSpec};

    fn spec(kind: &str, args: &[&str]) -> RuleSpec {
        RuleSpec {
            kind: kind.into(),
            matcher: Matcher::default(),
            content_type: None,
            operation_name: None,
            response: None,
            ws_dir: None,
            ws_opcode: None,
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn validate_rule_knows_request_and_response_header_kinds() {
        // Both header kinds need name + value.
        assert!(validate_rule(&spec("set-request-header", &["x-debug", "1"])).is_ok());
        assert!(
            validate_rule(&spec("set-response-header", &["cache-control", "no-store"])).is_ok()
        );
        assert!(validate_rule(&spec("set-request-header", &["x-debug"])).is_err());

        // The old umbrella `set-header` is gone — it now reads as unknown so a
        // stale rule fails loudly instead of silently applying to the wrong phase.
        assert!(validate_rule(&spec("set-header", &["a", "b"])).is_err());
    }

    #[test]
    fn respond_rule_is_atomic_validated_and_publicly_summarized() {
        let spec = RuleSpec {
            kind: "respond".into(),
            matcher: Matcher {
                host: Some("api.example.com".into()),
                method: Some("POST".into()),
                ..Default::default()
            },
            content_type: None,
            operation_name: Some("currentSession".into()),
            response: Some(SyntheticResponseSpec {
                status: 401,
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"error":"unauthorized"}"#.to_vec(),
            }),
            ws_dir: None,
            ws_opcode: None,
            args: vec![],
        };
        assert!(validate_rule(&spec).is_ok());

        let public = public_rule("r12", &spec);
        assert_eq!(public["phase"], "request");
        assert_eq!(public["matcher"]["graphql_operation"], "currentSession");
        assert_eq!(public["response"]["status"], 401);
        assert_eq!(public["response"]["content_type"], "application/json");
        assert_eq!(public["response"]["upstream_bypassed"], true);
        assert_eq!(public["response"]["body_bytes"], 24);
        assert!(public["response"].get("body").is_none());

        let mut invalid = spec;
        invalid.response.as_mut().unwrap().headers = vec![("content-length".into(), "1".into())];
        assert!(validate_rule(&invalid).is_err());
    }

    #[test]
    fn validate_rule_rejects_values_that_would_be_silent_noops() {
        for invalid in [
            spec("delay", &["forever"]),
            spec("set-status", &["199"]),
            spec("set-status", &["700"]),
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

        let mut request_filtered_by_response = spec("delay", &["1"]);
        request_filtered_by_response.content_type = Some("application/json".into());
        assert!(validate_rule(&request_filtered_by_response).is_err());
        request_filtered_by_response.content_type = None;
        request_filtered_by_response.matcher.status = Some(200);
        assert!(validate_rule(&request_filtered_by_response).is_err());

        let mut response_with_status = spec("replace", &["old", "new"]);
        response_with_status.matcher.status = Some(200);
        assert!(validate_rule(&response_with_status).is_ok());
        response_with_status.matcher.status = Some(99);
        assert!(validate_rule(&response_with_status).is_err());
    }

    #[test]
    fn ws_rules_reject_http_matchers_and_bad_selectors() {
        // A well-formed ws rule with valid selectors is accepted…
        let mut ok = spec("ws-set-text", &["{\"forced\":true}"]);
        ok.matcher.host = Some("chat.app".into());
        ok.ws_dir = Some("s2c".into());
        ok.ws_opcode = Some("text".into());
        assert!(validate_rule(&ok).is_ok());
        assert!(validate_rule(&spec("ws-drop", &[])).is_ok());

        // …and public_rule surfaces the WS selectors, not HTTP fields.
        let public = public_rule("r1", &ok);
        assert_eq!(public["phase"], "websocket");
        assert_eq!(public["ws_dir"], "s2c");
        assert_eq!(public["ws_opcode"], "text");
        assert!(public["matcher"].get("path").is_none());
        assert!(public["matcher"].get("method").is_none());

        // HTTP matchers don't apply to WS frame rules — reject, don't ignore.
        let mut with_path = spec("ws-drop", &[]);
        with_path.matcher.path = Some("/ws".into());
        assert!(validate_rule(&with_path).is_err());
        let mut with_method = spec("ws-drop", &[]);
        with_method.matcher.method = Some("GET".into());
        assert!(validate_rule(&with_method).is_err());
        let mut with_status = spec("ws-drop", &[]);
        with_status.matcher.status = Some(101);
        assert!(validate_rule(&with_status).is_err());
        let mut with_ct = spec("ws-drop", &[]);
        with_ct.content_type = Some("application/json".into());
        assert!(validate_rule(&with_ct).is_err());

        // Bad direction / opcode selectors are rejected.
        let mut bad_dir = spec("ws-drop", &[]);
        bad_dir.ws_dir = Some("up".into());
        assert!(validate_rule(&bad_dir).is_err());
        let mut bad_opcode = spec("ws-drop", &[]);
        bad_opcode.ws_opcode = Some("frame".into());
        assert!(validate_rule(&bad_opcode).is_err());

        // Arg arity still enforced alongside the WS filters.
        assert!(validate_rule(&spec("ws-set-text", &[])).is_err());
        assert!(validate_rule(&spec("ws-drop", &["x"])).is_err());
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
