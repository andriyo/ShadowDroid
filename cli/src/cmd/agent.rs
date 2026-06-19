//! Device-side control client for the in-app ShadowDroid agent (the AAR).
//!
//! The agent (core `shadowdroid-agent` AAR) opens a newline-framed line-JSON
//! control channel on a loopback port and logs a readiness marker. This client
//! discovers that port from logcat, sets up `adb forward`, and exchanges one
//! command line for one JSON response — backing the `aar capture` / `intercept`
//! / `resume` / `drop` / `status` verbs.
//!
//! In-app capture is **above TLS**: when the OkHttp companion interceptor is
//! wired into the host app, this reaches the decrypted request/response of
//! cert-pinned / Cronet / QUIC stacks the host MITM proxy can't. Captured flows
//! share the host [FlowRecord] shape, so they feed `net export fixtures` and the
//! `net` session store unchanged.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::device::adb;
use crate::net::export;
use crate::net::flow::FlowRecord;
use crate::net::store;

/// Host-side `adb forward` port used to reach the agent's loopback channel.
const HOST_PORT: u16 = 8129;
/// Readiness marker logged once by [ShadowDroidAgent.start]; carries `port=N`.
const MARKER: &str = "shadowdroid-agent-ready";

// ── low-level control channel ─────────────────────────────────────────────

/// Parse the agent's control port from a logcat dump — the `port=N` field of
/// the most recent readiness marker. `Some(-1)` means the channel failed to
/// bind; `None` means no marker was seen.
fn parse_agent_port(logcat: &str) -> Option<i32> {
    logcat
        .lines()
        .rev()
        .filter(|l| l.contains(MARKER))
        .find_map(|l| {
            l.split_whitespace().find_map(|tok| {
                tok.strip_prefix("port=")
                    .and_then(|n| n.parse::<i32>().ok())
            })
        })
}

/// Find the agent's control port from the most recent readiness marker.
pub async fn discover_port(serial: &str) -> Result<u16> {
    let logcat = adb::shell(serial, "logcat -d -s ShadowDroidAgent:I").await?;
    match parse_agent_port(&logcat) {
        Some(p) if p > 0 => Ok(p as u16),
        Some(_) => bail!("the in-app agent reported port=-1 (control channel failed to bind)"),
        None => bail!(
            "ShadowDroid agent not seen in logcat. Wire the AAR (`shadowdroid aar install`) \
             and launch the app, then confirm with `adb logcat -s ShadowDroidAgent`."
        ),
    }
}

/// Send one command line; return the parsed single-line JSON response.
pub async fn send(serial: &str, command: String) -> Result<Value> {
    let agent_port = discover_port(serial).await?;
    adb::forward(serial, HOST_PORT, agent_port).await?;
    let exchanged = tokio::task::spawn_blocking(move || exchange(HOST_PORT, &command))
        .await
        .context("agent exchange task panicked")?;
    // Best-effort cleanup; the result is what matters.
    let _ = adb::forward_remove(serial, HOST_PORT).await;
    exchanged
}

fn exchange(host_port: u16, command: &str) -> Result<Value> {
    let mut stream = TcpStream::connect(("127.0.0.1", host_port)).with_context(|| {
        format!("connect to in-app agent on 127.0.0.1:{host_port} (adb forward)")
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(40)))?;
    stream.write_all(command.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).context("read agent response")?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        bail!("empty response from in-app agent");
    }
    serde_json::from_str(trimmed).with_context(|| format!("parse agent response: {trimmed}"))
}

// ── verb handlers ─────────────────────────────────────────────────────────

/// `aar status` — agent info, armed matcher, held flows, capture count.
pub async fn status(serial: &str, json: bool) -> Result<()> {
    let resp = send(serial, "status".into()).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        let captured = resp.get("captured").and_then(Value::as_i64).unwrap_or(0);
        let intercept = resp.get("intercept");
        let armed = intercept
            .and_then(|i| i.get("armed"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let held = intercept
            .and_then(|i| i.get("held"))
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        println!(
            "agent: package={}",
            resp.get("package").and_then(Value::as_str).unwrap_or("?")
        );
        println!("  captured flows (buffered): {captured}");
        println!("  intercept armed:           {armed}");
        println!("  held flows:                {held}");
        if let Some(arr) = intercept
            .and_then(|i| i.get("held"))
            .and_then(Value::as_array)
        {
            for h in arr {
                println!(
                    "    {} {} {}{}  op={}",
                    h.get("id").and_then(Value::as_str).unwrap_or("?"),
                    h.get("method").and_then(Value::as_str).unwrap_or("?"),
                    h.get("host").and_then(Value::as_str).unwrap_or(""),
                    h.get("path").and_then(Value::as_str).unwrap_or(""),
                    h.get("operationName")
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                );
            }
        }
    }
    Ok(())
}

/// `aar capture` — drain buffered flows; optionally export or persist them.
pub async fn capture(
    serial: &str,
    clear: bool,
    out: Option<&PathBuf>,
    fixtures: Option<&PathBuf>,
    store_flows: bool,
    json: bool,
) -> Result<()> {
    let cmd = if clear { "capture --clear" } else { "capture" };
    let resp = send(serial, cmd.into()).await?;
    let flows_json = resp
        .get("flows")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    let flows: Vec<FlowRecord> =
        serde_json::from_value(flows_json).context("parse captured flows from agent")?;

    let mut outputs = serde_json::Map::new();
    if store_flows {
        for f in &flows {
            store::append(serial, f)?;
        }
        let path = crate::net::paths::session_log_path(serial)?;
        outputs.insert("stored".into(), Value::String(path.display().to_string()));
    }
    if let Some(path) = out {
        write_jsonl(path, &flows)?;
        outputs.insert("jsonl".into(), Value::String(path.display().to_string()));
    }
    if let Some(dir) = fixtures {
        let report = export::write_fixtures(&flows, dir)?;
        outputs.insert("fixtures".into(), report);
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "count": flows.len(),
                "outputs": Value::Object(outputs),
                "flows": flows,
            }))?
        );
    } else {
        println!("captured {} flow(s) from the in-app agent", flows.len());
        for f in &flows {
            let op = export::graphql_operation_name(&f.req_body)
                .map(|o| format!(" op={o}"))
                .unwrap_or_default();
            println!(
                "  {} {} {}{}{} -> {}{}",
                f.id,
                f.method,
                f.host,
                f.path,
                op,
                f.status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".into()),
                if f.modified { " (modified)" } else { "" },
            );
        }
        for (k, v) in &outputs {
            println!("  {k}: {v}");
        }
        if clear {
            println!("  (buffer cleared)");
        }
    }
    Ok(())
}

/// `aar intercept` — arm in-app interception for matching flows.
pub async fn intercept(
    serial: &str,
    host: Option<&str>,
    path: Option<&str>,
    method: Option<&str>,
    operation: Option<&str>,
    hold_ms: Option<u64>,
    json: bool,
) -> Result<()> {
    let mut spec = serde_json::Map::new();
    if let Some(v) = host {
        spec.insert("host".into(), Value::String(v.into()));
    }
    if let Some(v) = path {
        spec.insert("path".into(), Value::String(v.into()));
    }
    if let Some(v) = method {
        spec.insert("method".into(), Value::String(v.into()));
    }
    if let Some(v) = operation {
        spec.insert("operationName".into(), Value::String(v.into()));
    }
    if let Some(v) = hold_ms {
        spec.insert("holdMs".into(), Value::Number(v.into()));
    }
    let resp = send(serial, format!("intercept {}", Value::Object(spec))).await?;
    print_simple(&resp, json, "armed in-app interception");
    Ok(())
}

/// `aar intercept --clear` — disarm.
pub async fn intercept_clear(serial: &str, json: bool) -> Result<()> {
    let resp = send(serial, "intercept-clear".into()).await?;
    print_simple(&resp, json, "disarmed in-app interception");
    Ok(())
}

/// `aar resume <id>` — release a held flow, optionally mutating the response.
pub async fn resume(
    serial: &str,
    id: &str,
    set_status: Option<u16>,
    body: Option<String>,
    content_type: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut action = serde_json::Map::new();
    if let Some(s) = set_status {
        action.insert("status".into(), Value::Number(s.into()));
    }
    if let Some(b) = body {
        action.insert("body".into(), Value::String(b));
    }
    if let Some(ct) = content_type {
        action.insert("contentType".into(), Value::String(ct.into()));
    }
    let cmd = if action.is_empty() {
        format!("resume {id}")
    } else {
        format!("resume {id} {}", Value::Object(action))
    };
    let resp = send(serial, cmd).await?;
    print_simple(&resp, json, "resumed");
    Ok(())
}

/// `aar drop <id>` — fail a held flow (the app sees a connection error).
pub async fn drop_flow(serial: &str, id: &str, json: bool) -> Result<()> {
    let resp = send(serial, format!("drop {id}")).await?;
    print_simple(&resp, json, "dropped");
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────

fn write_jsonl(path: &PathBuf, flows: &[FlowRecord]) -> Result<()> {
    use std::io::Write as _;
    let mut file =
        std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    for f in flows {
        writeln!(file, "{}", serde_json::to_string(f)?)?;
    }
    Ok(())
}

fn print_simple(resp: &Value, json: bool, action: &str) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(resp).unwrap_or_else(|_| resp.to_string())
        );
    } else {
        let ok = resp.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if ok {
            println!("✓ {action}");
        } else {
            println!(
                "✗ {action} failed: {}",
                resp.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_port_reads_marker() {
        let logcat = "06-19 10:50:00.1  1234  1234 I ShadowDroidAgent: \
            shadowdroid-agent-ready version=0.3.1 package=com.example.app \
            sdk=34 pid=1234 port=8099";
        assert_eq!(parse_agent_port(logcat), Some(8099));
    }

    #[test]
    fn parse_agent_port_takes_most_recent_marker() {
        let logcat = "I ShadowDroidAgent: shadowdroid-agent-ready port=8099\n\
                      I ShadowDroidAgent: shadowdroid-agent-ready port=8100";
        assert_eq!(parse_agent_port(logcat), Some(8100));
    }

    #[test]
    fn parse_agent_port_flags_bind_failure() {
        let logcat = "I ShadowDroidAgent: shadowdroid-agent-ready port=-1";
        assert_eq!(parse_agent_port(logcat), Some(-1));
    }

    #[test]
    fn parse_agent_port_none_without_marker() {
        assert_eq!(parse_agent_port("nothing here"), None);
    }
}
