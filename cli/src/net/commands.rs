//! Host-side handlers for the `net` verbs. Most are thin clients that talk to
//! the daemon over the control socket ([crate::net::control]); `check`/`trust`
//! run host-only logic. `cli::dispatch_net` routes the parsed clap command here.

use crate::ids::Serial;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
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
}

const NETWORK_STATE_SCHEMA: u32 = 1;
const RAW_IP_CANARY: &str = "8.8.8.8";

/// The device-side state ShadowDroid owns for one proxy session. Persist this
/// before wiring so `net stop` can recover after either process crashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceNetworkState {
    schema_version: u32,
    serial: String,
    /// `None` means the setting did not exist (`settings get` returned null).
    prior_http_proxy: Option<String>,
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
    } = opts;
    // A live daemon may outlive the device-side reverse/proxy settings (most
    // commonly after a device reboot). Reuse its actual ports and repair the
    // wiring idempotently instead of forcing stop/start and losing rules.
    if control::is_running(serial).await {
        let daemon_status = control::request(serial, json!({"op": "status"})).await?;
        let daemon_port = daemon_status
            .get("port")
            .and_then(|value| value.as_u64())
            .map(|value| value as u16)
            .unwrap_or(port);
        let Some(host_port) = daemon_status
            .get("host_port")
            .and_then(|value| value.as_u64())
            .map(|value| value as u16)
        else {
            bail!(
                "the running net daemon predates automatic rewiring metadata; run `shadowdroid net stop` once, then `net start`"
            );
        };

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
                prior_http_proxy: prior,
                device_port: daemon_port,
                host_port,
                captured_at: events::now_ts(),
            })?;
        }
        setup_wiring(serial, daemon_port, host_port).await?;
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
            }),
        );
        return Ok(());
    }

    // A dead daemon can leave its state file and wiring behind. Restore it
    // before starting a genuinely new session so the new snapshot is truthful.
    if let Some(stale) = load_device_network_state(serial)? {
        restore_network_state(serial, &stale).await?;
        remove_device_network_state(serial)?;
    }

    // Host loopback port is per-serial so concurrent daemons for different
    // devices don't fight over one port; the device-facing `port` stays stable.
    let host_port = crate::device::portmap::free_loopback_port()?;
    let device_state = DeviceNetworkState {
        schema_version: NETWORK_STATE_SCHEMA,
        serial: serial.to_string(),
        prior_http_proxy: read_http_proxy(serial).await?,
        device_port: port,
        host_port,
        captured_at: events::now_ts(),
    };
    save_device_network_state(&device_state)?;
    let cfg = DaemonConfig {
        serial: serial.clone(),
        port,
        host_port,
        app_filters: apps.clone(),
        anticache,
        anticomp,
        verify_upstream,
        redact,
    };

    if foreground {
        if let Err(err) = setup_wiring(serial, port, host_port).await {
            restore_and_consume_network_state(serial, &device_state).await?;
            return Err(err);
        }
        emit(
            "net_start",
            json!({"device": serial, "port": port, "mode": "foreground"}),
        );
        let r = daemon::run(cfg).await;
        let restore = restore_and_consume_network_state(serial, &device_state).await;
        r?;
        restore?;
        return Ok(());
    }

    let daemon_pid = match daemon::spawn(&cfg) {
        Ok(pid) => pid,
        Err(err) => {
            remove_device_network_state(serial)?;
            return Err(err);
        }
    };
    if !daemon::await_ready(serial, 5000).await {
        kill_pid(daemon_pid);
        let _ = std::fs::remove_file(paths::pid_path(serial)?);
        let _ = std::fs::remove_file(paths::ctl_path(serial)?);
        let log = paths::daemon_log_path(serial)?;
        let reason = match daemon::log_tail(&log, 10) {
            Some(t) => format!("last log lines:\n{t}"),
            None => format!("no output in {}", log.display()),
        };
        remove_device_network_state(serial)?;
        bail!(
            "net daemon did not come up within 5s ({}). {}",
            log.display(),
            reason
        );
    }
    if let Err(err) = setup_wiring(serial, port, host_port).await {
        let _ = control::request(serial, json!({"op": "stop"})).await;
        restore_and_consume_network_state(serial, &device_state).await?;
        return Err(err);
    }
    emit(
        "net_start",
        json!({
            "device": serial,
            "port": port,
            "host_port": host_port,
            "proxy": format!("localhost:{port}"),
            "apps": apps,
            "anticache": anticache,
            "anticomp": anticomp,
            "verify_upstream": verify_upstream,
            "redact": redact,
            "ca": paths::ca_cert_path()?.display().to_string(),
            "hint": "next: `net check <pkg>` to confirm trust; `watch` streams HTTP events alongside screen/crash events",
        }),
    );
    Ok(())
}

pub async fn stop(serial: &Serial, revoke_ca: bool, canary_host: &str) -> Result<()> {
    let state = load_device_network_state(serial)?;
    let daemon_status = control::request(serial, json!({"op": "status"})).await.ok();
    let pid = control::daemon_pid(serial);
    let initial_http_proxy = read_http_proxy(serial).await?;
    let dangling_legacy_proxy = daemon_status.is_none()
        && pid.is_none()
        && state.is_none()
        && initial_http_proxy
            .as_deref()
            .is_some_and(|value| value.starts_with("localhost:"));
    let already_stopped =
        daemon_status.is_none() && pid.is_none() && state.is_none() && !dangling_legacy_proxy;

    let stopped = if daemon_status.is_some() {
        control::request(serial, json!({"op": "stop"}))
            .await
            .is_ok()
    } else if let Some(pid) = pid {
        // Socket unreachable — kill a possibly-wedged daemon by pid.
        kill_pid(pid);
        true
    } else {
        false
    };

    let mut warnings = Vec::<String>::new();
    let (http_proxy_restored, adb_reverse_removed, prior_http_proxy) = if let Some(state) = &state {
        restore_network_state(serial, state).await?;
        remove_device_network_state(serial)?;
        (true, true, state.prior_http_proxy.clone())
    } else if daemon_status.is_some() || pid.is_some() || dangling_legacy_proxy {
        // Compatibility cleanup for an old daemon that has no persisted
        // state. Be explicit that exact restoration was impossible.
        let port = daemon_status
            .as_ref()
            .and_then(|value| value.get("port"))
            .and_then(|value| value.as_u64())
            .map(|value| value as u16)
            .unwrap_or(crate::net::DEFAULT_PROXY_PORT);
        legacy_teardown_wiring(serial, port).await?;
        warnings.push(
            "no pre-proxy state was available; cleared http_proxy using the legacy :0 fallback"
                .into(),
        );
        (false, true, None)
    } else {
        // Idempotent already-stopped path: do not overwrite a proxy setting
        // owned by the user or another tool.
        (false, false, read_http_proxy(serial).await?)
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
        crate::net::trust::remove(serial).await.unwrap_or(false)
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
            "adb_reverse_removed": adb_reverse_removed,
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
    let host_port = daemon
        .as_ref()
        .and_then(|d| d.get("host_port").and_then(|p| p.as_u64()))
        .map(|p| p as u16);

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
            "ca_generated": paths::ca_cert_path().map(|p| p.exists()).unwrap_or(false),
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

/// Point the device at the host proxy: `adb reverse` so the device's
/// `localhost:<port>` tunnels to the host's `localhost:<host_port>` (where the
/// daemon binds), then set the system `http_proxy` to the device-facing port.
/// `port` and `host_port` differ so concurrent devices share the device-side
/// port but each own a distinct host port.
async fn setup_wiring(serial: &Serial, port: u16, host_port: u16) -> Result<()> {
    adb::reverse(serial, port, host_port).await?;
    adb::shell(
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
    adb::shell(serial, command).await?;
    Ok(())
}

async fn restore_network_state(serial: &Serial, state: &DeviceNetworkState) -> Result<()> {
    // Remove the tunnel first so no app can send new traffic through it while
    // the original proxy setting is being restored.
    let _ = adb::reverse_remove(serial, state.device_port).await;
    restore_http_proxy(serial, &state.prior_http_proxy).await
}

async fn restore_and_consume_network_state(
    serial: &Serial,
    state: &DeviceNetworkState,
) -> Result<()> {
    restore_network_state(serial, state).await?;
    remove_device_network_state(serial)
}

async fn legacy_teardown_wiring(serial: &Serial, port: u16) -> Result<()> {
    let _ = adb::reverse_remove(serial, port).await;
    adb::shell(serial, "settings put global http_proxy :0").await?;
    Ok(())
}

fn load_device_network_state(serial: &Serial) -> Result<Option<DeviceNetworkState>> {
    let path = paths::device_state_path(serial)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let state: DeviceNetworkState =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if state.schema_version != NETWORK_STATE_SCHEMA || state.serial != serial.as_str() {
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
    let flows = store::read_filtered(serial, &matcher, limit)?;
    let tls_errors = store::read_tls_errors(serial, matcher.host.as_deref())?;
    // Interleave completed flows with any TLS-handshake failures (issue #5) so a
    // "why is nothing captured?" moment shows the rejected host inline.
    let items = merge_timeline(&flows, tls_errors, limit);
    for v in &items {
        events::emit(v);
    }
    emit("net_log", json!({"count": items.len(), "limit": limit}));
    Ok(())
}

/// Merge completed flows (as `http` events) with `tls_error` markers into one
/// list ordered by `ts`, keeping the most recent `limit` events overall.
fn merge_timeline(
    flows: &[crate::net::flow::FlowRecord],
    tls_errors: Vec<serde_json::Value>,
    limit: usize,
) -> Vec<serde_json::Value> {
    let mut items: Vec<(f64, serde_json::Value)> =
        Vec::with_capacity(flows.len() + tls_errors.len());
    for f in flows {
        if let Ok(v) = serde_json::to_value(f.http_event()) {
            items.push((f.ts, v));
        }
    }
    for v in tls_errors {
        let ts = v.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0);
        items.push((ts, v));
    }
    items.sort_by(|a, b| a.0.total_cmp(&b.0));
    let start = items.len().saturating_sub(limit);
    items.split_off(start).into_iter().map(|(_, v)| v).collect()
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
fn write_body_file(id: &str, resp_body: Option<&str>, truncated: bool, path: &Path) -> Result<()> {
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

pub async fn trust(serial: &Serial, auto: bool, system: bool, ui: bool) -> Result<()> {
    crate::net::trust::run(serial, auto, system, ui).await
}

// ── CA management (`net ca`) ──────────────────────────────────

/// Best-effort "is a proxy daemon live for this serial?" — used only to decide
/// whether to tell the user to restart it. `net ca` may run with no device
/// attached (empty sentinel serial), in which case there's nothing to check.
async fn proxy_running(serial: &Serial) -> bool {
    !serial.as_str().is_empty() && control::is_running(serial).await
}

/// `net ca import` — install a user-provided CA as the proxy's signing CA.
pub async fn ca_import(serial: &Serial, cert: &Path, key: Option<&Path>) -> Result<()> {
    let (info, warnings) = crate::net::ca::import_ca(cert, key)?;

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
            "ca": info,
            "warnings": warnings,
            "backup": "the previous CA (if any) was saved alongside as ca.crt.bak / ca.key.bak",
            "next": next,
        }),
    );
    Ok(())
}

/// `net ca info` — describe the CA currently in use.
pub async fn ca_info() -> Result<()> {
    let info = crate::net::ca::ca_info()?;
    emit("net_ca_info", serde_json::to_value(&info)?);
    Ok(())
}

/// `net ca reset` — regenerate a fresh ShadowDroid CA (the current one is backed up).
pub async fn ca_reset(serial: &Serial) -> Result<()> {
    let info = crate::net::ca::reset_ca()?;
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
            "ca": info,
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
    let reply = control::request(serial, json!({"op": "rule_add", "spec": spec})).await?;
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
    use crate::net::flow::FlowRecord;
    use crate::net::Matcher;

    #[test]
    fn merge_timeline_orders_by_ts_and_keeps_last_n() {
        let flow = |id: &str, ts: f64| FlowRecord {
            id: id.into(),
            ts,
            method: "GET".into(),
            scheme: "https".into(),
            host: "api.example.com".into(),
            path: "/x".into(),
            ..Default::default()
        };
        let flows = vec![flow("f1", 1.0), flow("f3", 3.0)];
        let tls = vec![
            serde_json::json!({"type":"tls_error","ts":2.0,"host":"api.example.com","reason":"r"}),
        ];

        // All three, chronological.
        let all = merge_timeline(&flows, tls.clone(), 10);
        let ts: Vec<f64> = all.iter().map(|v| v["ts"].as_f64().unwrap()).collect();
        assert_eq!(ts, [1.0, 2.0, 3.0]);
        assert_eq!(all[1]["type"], "tls_error");

        // limit keeps the most recent N across both kinds.
        let last2 = merge_timeline(&flows, tls, 2);
        let ts: Vec<f64> = last2.iter().map(|v| v["ts"].as_f64().unwrap()).collect();
        assert_eq!(ts, [2.0, 3.0]);
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
