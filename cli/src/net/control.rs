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

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};

use crate::events::Event;
use crate::net::flow::FlowRecord;
use crate::net::proxy::{HoldDecision, InterceptCfg, SharedState};
use crate::net::{paths, Matcher, Mutation, RuleSpec};

/// In-daemon state the control handlers read/mutate.
pub struct DaemonState {
    pub port: u16,
    pub started: f64,
    pub flow_count: AtomicU64,
    /// Live event fan-out to `watch` subscribers. `Arc` so the broadcast value
    /// is cheaply `Clone` (the `Event` tree itself isn't `Clone`).
    pub events: broadcast::Sender<Arc<Event>>,
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
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");

    match op {
        "status" => {
            let intercepting = shared.intercept.read().unwrap().is_some();
            let held_flows: Vec<Value> = {
                let held = shared.held.lock().unwrap();
                held.values()
                    .map(|h| {
                        json!({
                            "id": h.meta.id,
                            "method": h.meta.method,
                            "host": h.meta.host,
                            "path": h.meta.path,
                            "status": h.meta.status,
                        })
                    })
                    .collect()
            };
            write_json(
                &mut wr,
                &json!({
                    "ok": true,
                    "running": true,
                    "port": state.port,
                    "started": state.started,
                    "flows": state.flow_count.load(Ordering::Relaxed),
                    "held": held_flows.len(),
                    "held_flows": held_flows,
                    "intercepting": intercepting,
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
                hold_ms: req.get("hold_ms").and_then(Value::as_u64).unwrap_or(30000) as u32,
                on_timeout_drop: req.get("on_timeout").and_then(Value::as_str) == Some("drop"),
            };
            *shared.intercept.write().unwrap() = Some(cfg);
            write_json(&mut wr, &json!({"ok": true, "intercepting": true, "at": at})).await?;
        }
        "resume" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let mutation: Mutation = req
                .get("mutation")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let released = release(&shared, id, HoldDecision::Resume(mutation));
            write_json(&mut wr, &released_reply(id, released)).await?;
        }
        "drop" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let status = req.get("status").and_then(Value::as_u64).map(|n| n as u16);
            let released = release(&shared, id, HoldDecision::Drop(status));
            write_json(&mut wr, &released_reply(id, released)).await?;
        }
        "respond" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let status = req.get("status").and_then(Value::as_u64).unwrap_or(200) as u16;
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
            let released = release(
                &shared,
                id,
                HoldDecision::Respond {
                    status,
                    body,
                    headers,
                },
            );
            write_json(&mut wr, &released_reply(id, released)).await?;
        }
        "show" => {
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let flow = shared
                .held
                .lock()
                .unwrap()
                .get(id)
                .map(|h| h.meta.detail(true));
            write_json(&mut wr, &json!({"ok": flow.is_some(), "flow": flow})).await?;
        }
        "rule_add" => {
            let spec: Option<RuleSpec> = req
                .get("spec")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok());
            match spec {
                None => {
                    write_json(&mut wr, &json!({"ok": false, "error": "missing/invalid rule spec"}))
                        .await?
                }
                Some(spec) => match validate_rule(&spec) {
                    Err(e) => write_json(&mut wr, &json!({"ok": false, "error": e})).await?,
                    Ok(()) => {
                        let id = next_rule_id();
                        shared.rules.write().unwrap().push((id.clone(), spec));
                        write_json(&mut wr, &json!({"ok": true, "id": id})).await?;
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
                .map(|(id, spec)| {
                    let mut v = serde_json::to_value(spec).unwrap_or_default();
                    if let Value::Object(m) = &mut v {
                        m.insert("id".into(), json!(id));
                    }
                    v
                })
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
            write_json(&mut wr, &json!({"ok": removed, "id": id, "removed": removed})).await?;
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
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
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
        // Only HTTP events flow over network stream clients.
        _ => false,
    }
}

fn next_rule_id() -> String {
    static C: AtomicU64 = AtomicU64::new(1);
    format!("r{}", C.fetch_add(1, Ordering::Relaxed))
}

/// Validate a rule's kind + positional-arg count before storing it.
fn validate_rule(spec: &RuleSpec) -> Result<(), String> {
    let n = spec.args.len();
    let need = |k: usize| {
        if n >= k {
            Ok(())
        } else {
            Err(format!("rule `{}` needs {k} arg(s), got {n}", spec.kind))
        }
    };
    match spec.kind.as_str() {
        "block" => Ok(()),
        "delay" | "map-local" | "map-remote" | "set-status" => need(1),
        "set-header" | "replace" => need(2),
        other => Err(format!(
            "unknown rule kind {other:?} (block|delay|map-local|map-remote|set-status|set-header|replace)"
        )),
    }
}

/// Hand a held flow its decision (fires the proxy's oneshot). Returns whether a
/// held flow with that id was present + reachable.
fn release(shared: &SharedState, id: &str, decision: HoldDecision) -> bool {
    if let Some(held) = shared.held.lock().unwrap().remove(id) {
        held.tx.send(decision).is_ok()
    } else {
        false
    }
}

fn released_reply(id: &str, released: bool) -> Value {
    if released {
        json!({"ok": true, "id": id, "released": true})
    } else {
        json!({"ok": false, "id": id, "error": "no such held flow (already released, timed out, or wrong id)"})
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

/// Is a daemon for `serial` reachable on its control socket?
pub async fn is_running(serial: &str) -> bool {
    match read_ctl_port(serial) {
        Some(port) => TcpStream::connect(("127.0.0.1", port)).await.is_ok(),
        None => false,
    }
}

/// The daemon's loopback control port from its `.ctl` file, if present.
fn read_ctl_port(serial: &str) -> Option<u16> {
    let path = paths::ctl_path(serial).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The daemon pid from its pidfile, if present + parseable.
pub fn daemon_pid(serial: &str) -> Option<u32> {
    let path = paths::pid_path(serial).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

async fn connect(serial: &str) -> Result<TcpStream> {
    let port = read_ctl_port(serial).ok_or_else(|| {
        anyhow!("no net proxy daemon for {serial}. Is `shadowdroid net start` running?")
    })?;
    TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| {
        anyhow!("cannot reach the net proxy daemon on 127.0.0.1:{port}: {e}. Is `net start` running?")
    })
}

/// Send one request, read one JSON response line.
pub async fn request(serial: &str, req: Value) -> Result<Value> {
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
pub async fn request_stream(serial: &str, req: Value) -> Result<()> {
    let stream = connect(serial).await?;
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

async fn write_request(wr: &mut OwnedWriteHalf, req: &Value) -> Result<()> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    wr.write_all(line.as_bytes()).await?;
    wr.flush().await?;
    Ok(())
}
