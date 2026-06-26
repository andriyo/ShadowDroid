//! Host-side handlers for the `net` verbs. Most are thin clients that talk to
//! the daemon over the control socket ([crate::net::control]); `check`/`trust`
//! run host-only logic. `cli::dispatch_net` routes the parsed clap command here.

use crate::ids::Serial;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::device::adb;
use crate::events;
use crate::net::{control, daemon, paths, store, DaemonConfig, Matcher, Mutation, RuleSpec};

/// Emit a `{"type":"action","cmd":<cmd>, …}` line — thin adapter over the shared
/// [`crate::events::emit_action`].
fn emit(cmd: &str, body: serde_json::Value) {
    crate::events::emit_action(cmd, &body);
}

/// Best-effort terminate a wedged daemon by pid when the control socket is
/// unreachable. Portable: `kill` on Unix, `taskkill` on Windows (the control
/// socket is TCP precisely so `net` works on Windows, so the fallback must too).
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
}

// ── lifecycle ─────────────────────────────────────────────────

pub async fn start(
    serial: &Serial,
    port: u16,
    apps: Vec<String>,
    foreground: bool,
    anticache: bool,
    anticomp: bool,
) -> Result<()> {
    let cfg = DaemonConfig {
        serial: serial.clone(),
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
        let log = paths::daemon_log_path(serial)?;
        let reason = match daemon::log_tail(&log, 10) {
            Some(t) => format!("last log lines:\n{t}"),
            None => format!("no output in {}", log.display()),
        };
        bail!(
            "net daemon did not come up within 5s ({}). {}",
            log.display(),
            reason
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

pub async fn stop(serial: &Serial, revoke_ca: bool) -> Result<()> {
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
                kill_pid(pid);
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

pub async fn status(serial: &Serial) -> Result<()> {
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
async fn setup_wiring(serial: &Serial, port: u16) -> Result<()> {
    adb::reverse(serial, port, port).await?;
    adb::shell(
        serial,
        format!("settings put global http_proxy localhost:{port}"),
    )
    .await?;
    Ok(())
}

/// Undo [setup_wiring]. Best-effort: clearing a dangling proxy is exactly what
/// `doctor --fix` also does, so failures here aren't fatal.
async fn teardown_wiring(serial: &Serial, port: u16) -> Result<()> {
    let _ = adb::shell(serial, "settings put global http_proxy :0").await;
    let _ = adb::reverse_remove(serial, port).await;
    Ok(())
}

// ── observe (task 8) ──────────────────────────────────────────

pub async fn log(serial: &Serial, matcher: Matcher, limit: usize) -> Result<()> {
    let flows = store::read_filtered(serial, &matcher, limit)?;
    for f in &flows {
        events::emit(&f.http_event());
    }
    emit("net_log", json!({"count": flows.len(), "limit": limit}));
    Ok(())
}

pub async fn show(
    serial: &Serial,
    id: &str,
    body: bool,
    har: bool,
    body_file: Option<&Path>,
) -> Result<()> {
    if har {
        // Single-flow HAR export lives in `net export har <id>`.
        emit(
            "net_show",
            json!({"id": id, "hint": "use `net export har <id>` for HAR"}),
        );
    }
    // Completed flows live in the session log; a *held* (in-flight) flow lives
    // only in the daemon — try the store first, then ask the daemon.
    if let Some(flow) = store::find_by_id(serial, id)? {
        if let Some(path) = body_file {
            return write_body_file(id, flow.resp_body.as_deref(), flow.resp_truncated, path);
        }
        println!("{}", serde_json::to_string(&flow.detail(body))?);
        return Ok(());
    }
    if control::is_running(serial).await {
        // Ask the daemon with bodies so `--body-file` works on a held flow too.
        if let Ok(reply) = control::request(serial, json!({"op": "show", "id": id})).await {
            if let Some(flow) = reply.get("flow").filter(|v| !v.is_null()) {
                if let Some(path) = body_file {
                    let resp_body = flow.get("resp_body").and_then(|v| v.as_str());
                    let truncated = flow
                        .get("resp_truncated")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    return write_body_file(id, resp_body, truncated, path);
                }
                println!("{}", serde_json::to_string(flow)?);
                return Ok(());
            }
        }
    }
    bail!("no flow `{id}` in the session log or held set (try `net log`)");
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
) -> Result<()> {
    let Some(b) = resp_body else {
        bail!("flow `{id}` has no captured response body (binary, empty, or non-textual content-type)");
    };
    std::fs::write(path, b).with_context(|| format!("writing {}", path.display()))?;
    emit(
        "net_show",
        json!({
            "id": id,
            "saved_body": path.display().to_string(),
            "bytes": b.len(),
            "truncated": truncated,
        }),
    );
    Ok(())
}

// ── not yet implemented (later tasks) ─────────────────────────

pub async fn check(serial: &Serial, package: &str) -> Result<()> {
    crate::net::check::run(serial, package).await
}

pub async fn trust(serial: &Serial, system: bool, ui: bool) -> Result<()> {
    crate::net::trust::run(serial, system, ui).await
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
        "fixtures" => {
            let out = out.unwrap_or_else(|| PathBuf::from("shadowdroid-fixtures"));
            let summary = crate::net::export::write_fixtures(&flows, &out)?;
            println!("{}", serde_json::to_string(&summary)?);
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
    let reply = control::request(
        serial,
        json!({"op": "intercept", "matcher": matcher, "at": at, "hold_ms": hold_ms, "on_timeout": on_timeout}),
    )
    .await?;
    emit("net_intercept", reply);
    Ok(())
}

pub async fn resume(serial: &Serial, id: &str, mutation: Mutation) -> Result<()> {
    let reply = control::request(
        serial,
        json!({"op": "resume", "id": id, "mutation": mutation}),
    )
    .await?;
    emit("net_resume", reply);
    Ok(())
}

pub async fn drop_flow(serial: &Serial, id: &str, status: Option<u16>) -> Result<()> {
    let reply = control::request(serial, json!({"op": "drop", "id": id, "status": status})).await?;
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
    let reply = control::request(
        serial,
        json!({"op": "respond", "id": id, "status": status, "body": body.unwrap_or_default(), "headers": headers}),
    )
    .await?;
    emit("net_respond", reply);
    Ok(())
}

pub async fn rule_add(serial: &Serial, spec: RuleSpec) -> Result<()> {
    let warning = map_remote_path_warning(&spec);
    let mut reply = control::request(serial, json!({"op": "rule_add", "spec": spec})).await?;
    if let Some(w) = warning {
        if let Some(obj) = reply.as_object_mut() {
            obj.insert("warning".into(), json!(w));
        }
    }
    emit("net_rule_add", reply);
    Ok(())
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
    let reply = control::request(serial, json!({"op": "rule_list"})).await?;
    emit("net_rule_list", reply);
    Ok(())
}

pub async fn rule_rm(serial: &Serial, id: &str) -> Result<()> {
    let reply = control::request(serial, json!({"op": "rule_rm", "id": id})).await?;
    emit("net_rule_rm", reply);
    Ok(())
}

pub async fn rule_clear(serial: &Serial) -> Result<()> {
    let reply = control::request(serial, json!({"op": "rule_clear"})).await?;
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
    let reply = control::request(serial, json!({"op": "replay", "flows": flows})).await?;
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
}
