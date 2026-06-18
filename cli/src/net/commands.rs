//! Host-side handlers for the `net` verbs. Most are thin clients that talk to
//! the daemon over the control socket ([crate::net::control]); `check`/`trust`
//! run host-only logic. `cli::dispatch_net` routes the parsed clap command here.

use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::Path;

use crate::device::adb;
use crate::events;
use crate::net::{control, daemon, paths, store, DaemonConfig, Matcher, Mutation, RuleSpec};

/// Emit a `{"type":"action","cmd":<cmd>, …}` line (the existing CLI envelope).
fn emit(cmd: &str, body: serde_json::Value) {
    let mut m = serde_json::Map::new();
    m.insert("type".into(), json!("action"));
    m.insert("cmd".into(), json!(cmd));
    if let serde_json::Value::Object(b) = body {
        for (k, v) in b {
            m.insert(k, v);
        }
    }
    println!("{}", serde_json::to_string(&serde_json::Value::Object(m)).unwrap());
}

// ── lifecycle ─────────────────────────────────────────────────

pub async fn start(
    serial: &str,
    port: u16,
    apps: Vec<String>,
    foreground: bool,
    anticache: bool,
    anticomp: bool,
) -> Result<()> {
    let cfg = DaemonConfig {
        serial: serial.to_string(),
        port,
        app_filters: apps.clone(),
        anticache,
        anticomp,
    };

    if foreground {
        setup_wiring(serial, port).await?;
        emit(
            "net_start",
            json!({"device": serial, "port": port, "mode": "foreground"}),
        );
        let r = daemon::run(cfg).await;
        let _ = teardown_wiring(serial, port).await;
        return r;
    }

    if control::is_running(serial).await {
        bail!("net proxy already running for {serial}. Run `net stop` first.");
    }
    daemon::spawn(&cfg)?;
    if !daemon::await_ready(serial, 5000).await {
        bail!(
            "net daemon did not come up (see {})",
            paths::daemon_log_path(serial)?.display()
        );
    }
    setup_wiring(serial, port).await?;
    emit(
        "net_start",
        json!({
            "device": serial,
            "port": port,
            "proxy": format!("localhost:{port}"),
            "apps": apps,
            "anticache": anticache,
            "anticomp": anticomp,
            "ca": paths::ca_cert_path()?.display().to_string(),
            "hint": "next: `net check <pkg>` to confirm trust; `watch` streams HTTP events alongside screen/crash events",
        }),
    );
    Ok(())
}

pub async fn stop(serial: &str, revoke_ca: bool) -> Result<()> {
    // Learn the port from a live daemon (falls back to default for teardown).
    let port = control::request(serial, json!({"op": "status"}))
        .await
        .ok()
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .map(|p| p as u16);

    let stopped = match control::request(serial, json!({"op": "stop"})).await {
        Ok(_) => true,
        Err(_) => {
            // Socket unreachable — kill a possibly-wedged daemon by pid.
            if let Some(pid) = control::daemon_pid(serial) {
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status();
            }
            false
        }
    };

    let p = port.unwrap_or(crate::net::DEFAULT_PROXY_PORT);
    let _ = teardown_wiring(serial, p).await;

    let ca_removed = if revoke_ca {
        crate::net::trust::remove(serial).await.unwrap_or(false)
    } else {
        false
    };

    emit(
        "net_stop",
        json!({
            "device": serial,
            "stopped": stopped,
            "http_proxy_cleared": true,
            "revoke_ca": revoke_ca,
            "ca_removed": ca_removed,
        }),
    );
    Ok(())
}

pub async fn status(serial: &str) -> Result<()> {
    let running = control::is_running(serial).await;
    let daemon = if running {
        control::request(serial, json!({"op": "status"})).await.ok()
    } else {
        None
    };
    let port = daemon
        .as_ref()
        .and_then(|d| d.get("port").and_then(|p| p.as_u64()))
        .map(|p| p as u16);

    let http_proxy = adb::shell(serial, "settings get global http_proxy")
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "null");

    let pointed = match (&http_proxy, port) {
        (Some(hp), Some(p)) => hp == &format!("localhost:{p}") || hp.ends_with(&format!(":{p}")),
        _ => false,
    };

    emit(
        "net_status",
        json!({
            "device": serial,
            "running": running,
            "daemon": daemon,
            "http_proxy": http_proxy,
            "pointed_at_proxy": pointed,
            "ca_generated": paths::ca_cert_path().map(|p| p.exists()).unwrap_or(false),
        }),
    );
    Ok(())
}

/// Point the device at the host proxy: `adb reverse` so device-localhost tunnels
/// to the host, then set the system `http_proxy` to that localhost port.
async fn setup_wiring(serial: &str, port: u16) -> Result<()> {
    adb::reverse(serial, port, port).await?;
    adb::shell(serial, format!("settings put global http_proxy localhost:{port}")).await?;
    Ok(())
}

/// Undo [setup_wiring]. Best-effort: clearing a dangling proxy is exactly what
/// `doctor --fix` also does, so failures here aren't fatal.
async fn teardown_wiring(serial: &str, port: u16) -> Result<()> {
    let _ = adb::shell(serial, "settings put global http_proxy :0").await;
    let _ = adb::reverse_remove(serial, port).await;
    Ok(())
}

// ── observe (task 8) ──────────────────────────────────────────

pub async fn log(serial: &str, matcher: Matcher, limit: usize) -> Result<()> {
    let flows = store::read_filtered(serial, &matcher, limit)?;
    for f in &flows {
        events::emit(&f.http_event());
    }
    emit("net_log", json!({"count": flows.len(), "limit": limit}));
    Ok(())
}

pub async fn show(serial: &str, id: &str, body: bool, har: bool) -> Result<()> {
    if har {
        // Single-flow HAR export lives in `net export har <id>`.
        emit("net_show", json!({"id": id, "hint": "use `net export har <id>` for HAR"}));
    }
    // Completed flows live in the session log; a *held* (in-flight) flow lives
    // only in the daemon — try the store first, then ask the daemon.
    if let Some(flow) = store::find_by_id(serial, id)? {
        println!("{}", serde_json::to_string(&flow.detail(body))?);
        return Ok(());
    }
    if control::is_running(serial).await {
        if let Ok(reply) = control::request(serial, json!({"op": "show", "id": id})).await {
            if let Some(flow) = reply.get("flow").filter(|v| !v.is_null()) {
                println!("{}", serde_json::to_string(flow)?);
                return Ok(());
            }
        }
    }
    bail!("no flow `{id}` in the session log or held set (try `net log`)");
}

// ── not yet implemented (later tasks) ─────────────────────────

pub async fn check(serial: &str, package: &str) -> Result<()> {
    crate::net::check::run(serial, package).await
}

pub async fn trust(serial: &str, system: bool, ui: bool) -> Result<()> {
    crate::net::trust::run(serial, system, ui).await
}

pub async fn export(serial: &str, format: &str, id: Option<String>) -> Result<()> {
    let flows = match &id {
        Some(id) => store::find_by_id(serial, id)?
            .map(|f| vec![f])
            .unwrap_or_default(),
        None => store::read_all(serial)?,
    };
    if flows.is_empty() {
        bail!("no flows to export (try `net log`)");
    }
    match format {
        "curl" => {
            for f in &flows {
                println!("{}", crate::net::export::curl_command(f));
            }
        }
        "har" => println!(
            "{}",
            serde_json::to_string_pretty(&crate::net::export::to_har(&flows))?
        ),
        other => bail!("unknown export format {other:?} (curl|har)"),
    }
    Ok(())
}

pub async fn intercept(
    serial: &str,
    matcher: Matcher,
    at: String,
    hold_ms: u32,
    on_timeout: String,
) -> Result<()> {
    let reply = control::request(
        serial,
        json!({"op": "intercept", "matcher": matcher, "at": at, "hold_ms": hold_ms, "on_timeout": on_timeout}),
    )
    .await?;
    emit("net_intercept", reply);
    Ok(())
}

pub async fn resume(serial: &str, id: &str, mutation: Mutation) -> Result<()> {
    let reply =
        control::request(serial, json!({"op": "resume", "id": id, "mutation": mutation})).await?;
    emit("net_resume", reply);
    Ok(())
}

pub async fn drop_flow(serial: &str, id: &str, status: Option<u16>) -> Result<()> {
    let reply = control::request(serial, json!({"op": "drop", "id": id, "status": status})).await?;
    emit("net_drop", reply);
    Ok(())
}

pub async fn respond(
    serial: &str,
    id: &str,
    status: u16,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
) -> Result<()> {
    let reply = control::request(
        serial,
        json!({"op": "respond", "id": id, "status": status, "body": body.unwrap_or_default(), "headers": headers}),
    )
    .await?;
    emit("net_respond", reply);
    Ok(())
}

pub async fn rule_add(serial: &str, spec: RuleSpec) -> Result<()> {
    let reply = control::request(serial, json!({"op": "rule_add", "spec": spec})).await?;
    emit("net_rule_add", reply);
    Ok(())
}

pub async fn rule_list(serial: &str) -> Result<()> {
    let reply = control::request(serial, json!({"op": "rule_list"})).await?;
    emit("net_rule_list", reply);
    Ok(())
}

pub async fn rule_rm(serial: &str, id: &str) -> Result<()> {
    let reply = control::request(serial, json!({"op": "rule_rm", "id": id})).await?;
    emit("net_rule_rm", reply);
    Ok(())
}

pub async fn rule_clear(serial: &str) -> Result<()> {
    let reply = control::request(serial, json!({"op": "rule_clear"})).await?;
    emit("net_rule_clear", reply);
    Ok(())
}

pub async fn rules_apply(serial: &str, file: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
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
        let reply = control::request(serial, json!({"op": "rule_add", "spec": spec})).await?;
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

pub async fn replay(serial: &str, from: &Path, host: Option<String>) -> Result<()> {
    let text =
        std::fs::read_to_string(from).with_context(|| format!("read {}", from.display()))?;
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
    let reply = control::request(serial, json!({"op": "replay", "flows": flows})).await?;
    emit(
        "net_replay",
        json!({"loaded": count, "from": from.display().to_string(), "daemon": reply}),
    );
    Ok(())
}
