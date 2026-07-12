//! The proxy daemon: spawned detached by `net start`, it runs the MITM proxy
//! ([crate::net::proxy]) + the control socket ([crate::net::control]) until
//! `net stop` (or Ctrl-C). Entry point for the hidden `net daemon` subcommand.

use crate::ids::Serial;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::events::{self, Event};
use crate::net::ca::CertAuthority;
use crate::net::control::{self, DaemonState};
use crate::net::flow::FlowRecord;
use crate::net::proxy::{self, ProxyContext, SharedState};
use crate::net::{DaemonConfig, paths, store};

struct BoundDaemonSockets {
    proxy: TcpListener,
    control: TcpListener,
    control_port: u16,
}

fn write_marker_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("readiness marker has no parent: {}", path.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary marker in {}", parent.display()))?;
    temp.write_all(contents.as_bytes())
        .with_context(|| format!("write temporary marker for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync temporary marker for {}", path.display()))?;
    let file = temp
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically publish {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync published marker {}", path.display()))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync marker directory {}", parent.display()))?;
    Ok(())
}

fn remove_readiness_if_owned(pid_path: &Path, ctl_path: &Path, pid: u32, ctl_port: u16) {
    let owns_pid = std::fs::read_to_string(pid_path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        == Some(pid);
    let owns_ctl = match std::fs::read_to_string(ctl_path) {
        Ok(value) => value.trim().parse::<u16>().ok() == Some(ctl_port),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    };
    if owns_pid && owns_ctl {
        let _ = std::fs::remove_file(ctl_path);
        let _ = std::fs::remove_file(pid_path);
    }
}

/// Bind both listeners before publishing either readiness file. The proxy bind
/// happens first because a reachable control socket is the daemon's public
/// readiness signal; it must never advertise a proxy port it does not own.
async fn bind_prepare_and_publish<F>(
    proxy_addr: SocketAddr,
    pid_path: &Path,
    ctl_path: &Path,
    prepare: F,
) -> Result<BoundDaemonSockets>
where
    F: FnOnce() -> Result<()>,
{
    let sockets = bind_daemon_sockets(proxy_addr).await?;
    prepare()?;
    publish_readiness(pid_path, ctl_path, sockets.control_port)?;
    Ok(sockets)
}

async fn bind_daemon_sockets(proxy_addr: SocketAddr) -> Result<BoundDaemonSockets> {
    let proxy = proxy::bind(proxy_addr).await?;
    let control = TcpListener::bind(("127.0.0.1", 0u16))
        .await
        .context("bind control socket")?;
    let control_port = control.local_addr().context("control addr")?.port();

    Ok(BoundDaemonSockets {
        proxy,
        control,
        control_port,
    })
}

fn publish_readiness(pid_path: &Path, ctl_path: &Path, control_port: u16) -> Result<()> {
    write_marker_atomic(pid_path, &std::process::id().to_string()).context("write pidfile")?;
    if let Err(error) = write_marker_atomic(ctl_path, &control_port.to_string()) {
        // A pidfile without a discoverable control endpoint is not ready and
        // would make `net stop` target a daemon that never finished startup.
        let _ = std::fs::remove_file(pid_path);
        return Err(error).context("write control port file");
    }
    Ok(())
}

/// Run the daemon in the foreground (this process IS the detached daemon).
pub async fn run(cfg: DaemonConfig) -> Result<()> {
    paths::ensure_net_dir()?;

    // Load the CA the parent resolved for us; never generate here (the daemon
    // has no config context and can't know which CA the project wants).
    let ca = CertAuthority::load_from_files(&cfg.ca_cert, &cfg.ca_key)?;
    let ca_fingerprint = ca.fingerprint().to_string();
    // Each completed flow can contain multi-megabyte bodies. A bounded queue
    // prevents a slow disk from turning a traffic burst into unbounded memory.
    let (flow_tx, mut flow_rx) = mpsc::channel::<FlowRecord>(16);
    let (event_tx, _event_rx) = broadcast::channel::<Arc<Event>>(1024);
    let shared = Arc::new(SharedState {
        anticache: cfg.anticache,
        anticomp: cfg.anticomp,
        redact: cfg.redact,
        host_filters: cfg.app_filters.clone(),
        intercept: RwLock::new(None),
        held: Mutex::new(HashMap::new()),
        events: event_tx.clone(),
        rules: RwLock::new(Vec::new()),
        replay: RwLock::new(None),
        tls_errors_seen: Mutex::new(HashSet::new()),
        dropped_flows: AtomicU64::new(0),
        persistence_errors: AtomicU64::new(0),
        held_bytes: Arc::new(AtomicU64::new(0)),
        rejected_holds: AtomicU64::new(0),
    });

    let proxy_tasks = TaskTracker::new();
    let proxy_shutdown = CancellationToken::new();
    let ctx = Arc::new(ProxyContext {
        ca,
        client: proxy::build_upstream_client(cfg.verify_upstream),
        flow_tx,
        shared: shared.clone(),
        serial: cfg.serial.clone(),
        verify_upstream: cfg.verify_upstream,
        tasks: proxy_tasks.clone(),
        shutdown: proxy_shutdown.clone(),
    });

    let state = Arc::new(DaemonState {
        serial: cfg.serial.clone(),
        port: cfg.port,
        host_port: cfg.host_port,
        startup_id: cfg.startup_id.clone(),
        pid: std::process::id(),
        started: events::now_ts(),
        // Derived from the exact PEM loaded above, so a concurrent managed-CA
        // replacement cannot make status describe different bytes than the
        // in-memory signer.
        ca_fingerprint,
        flow_count: AtomicU64::new(0),
        events: event_tx,
    });

    // Bind the proxy first, then the control listener, and only then publish
    // pid/control readiness. `adb reverse` maps the device-facing `cfg.port` to
    // this per-serial host port, so daemons for different devices do not
    // collide on one loopback port.
    let addr: SocketAddr = ([127, 0, 0, 1], cfg.host_port).into();
    let pid_path = paths::pid_path(&cfg.serial)?;
    let ctl_path = paths::ctl_path(&cfg.serial)?;
    let sockets = bind_prepare_and_publish(addr, &pid_path, &ctl_path, || {
        // Each successful listener bind starts a fresh capture session. Failure
        // to clear is fatal and readiness remains unpublished, so an old log is
        // never silently mixed into a session that claims to be fresh.
        store::clear(&cfg.serial).context("clear previous network capture session")
    })
    .await?;
    let BoundDaemonSockets {
        proxy: proxy_listener,
        control: listener,
        control_port: ctl_port,
    } = sockets;

    // Drain completed flows → session log + live broadcast. Retain the handle
    // so shutdown waits for all records already accepted by the bounded queue.
    let flow_writer = {
        let state = state.clone();
        let shared = shared.clone();
        let serial = cfg.serial.clone();
        tokio::spawn(async move {
            while let Some(rec) = flow_rx.recv().await {
                match store::append(&serial, &rec) {
                    Ok(()) => {
                        state.flow_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) => shared.record_persistence_error("http_flow", &error),
                }
                let _ = state.events.send(Arc::new(rec.http_event(&serial)));
            }
        })
    };

    let (proxy_stop_tx, proxy_stop_rx) = oneshot::channel();
    let mut proxy_task = tokio::spawn(proxy::serve(ctx, proxy_listener, proxy_stop_rx));

    tracing::info!(
        "net daemon up: proxy device :{} -> host 127.0.0.1:{}, control 127.0.0.1:{}",
        cfg.port,
        cfg.host_port,
        ctl_port
    );

    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted { Ok((s, _)) => s, Err(_) => continue };
                let state = state.clone();
                let shared = shared.clone();
                let stop_tx = stop_tx.clone();
                tokio::spawn(async move {
                    let _ = control::serve_client(stream, state, shared, stop_tx).await;
                });
            }
            _ = stop_rx.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    // Teardown.
    let _ = proxy_stop_tx.send(());
    proxy_shutdown.cancel();
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut proxy_task)
        .await
        .is_err()
    {
        tracing::warn!("proxy accept loop did not stop within shutdown grace; aborting it");
        proxy_task.abort();
        let _ = proxy_task.await;
    }

    proxy_tasks.close();
    let proxy_drained = tokio::time::timeout(std::time::Duration::from_secs(3), proxy_tasks.wait())
        .await
        .is_ok();
    if !proxy_drained {
        tracing::warn!("active proxy connections did not drain within shutdown grace");
    }

    let mut flow_writer = flow_writer;
    if proxy_drained {
        if tokio::time::timeout(std::time::Duration::from_secs(3), &mut flow_writer)
            .await
            .is_err()
        {
            tracing::warn!("flow persistence queue did not drain within shutdown grace");
            flow_writer.abort();
        }
    } else {
        flow_writer.abort();
    }
    remove_readiness_if_owned(&pid_path, &ctl_path, state.pid, ctl_port);
    tracing::info!("net daemon stopped");
    Ok(())
}

/// Launch the daemon as a detached background process (re-exec `net daemon`),
/// returning its child handle. The parent retains the handle through readiness
/// and wiring so a failed startup can be killed and reaped without trusting a
/// pidfile or leaving a zombie. stdio is redirected to the daemon
/// log; on Unix the child gets its own process group so terminal signals to the
/// short-lived `net start` don't reach it.
pub fn spawn(cfg: &DaemonConfig) -> Result<std::process::Child> {
    paths::ensure_net_dir()?;
    let exe = std::env::current_exe().context("resolve current exe")?;
    let log = paths::daemon_log_path(&cfg.serial)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    let log_file2 = log_file.try_clone()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("net")
        .arg("daemon")
        .arg("--serial")
        .arg(cfg.serial.as_str())
        .arg("--startup-id")
        .arg(&cfg.startup_id)
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--host-port")
        .arg(cfg.host_port.to_string())
        .arg("--ca-cert")
        .arg(&cfg.ca_cert)
        .arg("--ca-key")
        .arg(&cfg.ca_key);
    for app in &cfg.app_filters {
        cmd.arg("--host").arg(app);
    }
    if cfg.anticache {
        cmd.arg("--anticache");
    }
    if cfg.anticomp {
        cmd.arg("--anticomp");
    }
    if cfg.verify_upstream {
        cmd.arg("--verify-upstream");
    }
    if cfg.redact {
        cmd.arg("--redact");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("spawn net daemon process")
}

/// Last `n` non-empty lines of the daemon log — used to surface a startup
/// failure reason (e.g. "Address already in use") when [`await_ready`] times
/// out, instead of leaving the caller to guess why the daemon never came up.
/// ANSI color codes (the tracing subscriber emits them even to the log file)
/// are stripped so the reason reads cleanly inside the JSON error envelope.
pub fn log_tail(path: &std::path::Path, n: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    let start = lines.len().saturating_sub(n);
    Some(strip_ansi(&lines[start..].join("\n")))
}

/// Drop ANSI SGR escape sequences (`\x1b[…m`) from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until the sequence terminator (`m` for the SGR codes tracing
            // emits); tolerate a missing terminator by consuming to end.
            for e in chars.by_ref() {
                if e == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn status_matches_startup(
    status: &serde_json::Value,
    serial: &Serial,
    startup_id: &str,
    pid: u32,
) -> bool {
    !startup_id.is_empty()
        && pid != 0
        && status.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        && status.get("running").and_then(serde_json::Value::as_bool) == Some(true)
        && status.get("serial").and_then(serde_json::Value::as_str) == Some(serial.as_str())
        && status.get("startup_id").and_then(serde_json::Value::as_str) == Some(startup_id)
        && status.get("pid").and_then(serde_json::Value::as_u64) == Some(u64::from(pid))
}

/// Wait (up to `timeout_ms`) for the exact freshly-spawned daemon to report
/// ready. Each status probe has its own short deadline so the control client's
/// normal five-second request timeout cannot consume this entire polling loop.
pub async fn await_ready(serial: &Serial, startup_id: &str, pid: u32, timeout_ms: u64) -> bool {
    if startup_id.is_empty() || pid == 0 {
        return false;
    }

    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let probe_timeout = PROBE_TIMEOUT.min(deadline.saturating_duration_since(now));
        let probe = tokio::time::timeout(
            probe_timeout,
            control::request(serial, serde_json::json!({"op": "status"})),
        )
        .await;
        if let Ok(Ok(status)) = probe
            && status_matches_startup(&status, serial, startup_id, pid)
        {
            return true;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bind_prepare_and_publish, remove_readiness_if_owned, status_matches_startup, strip_ansi,
        write_marker_atomic,
    };
    use crate::ids::Serial;
    use serde_json::json;
    use std::net::SocketAddr;

    #[test]
    fn strip_ansi_removes_sgr_codes() {
        let raw = "\u{1b}[2m2026\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m proxy run error: bind 127.0.0.1:9988: Address already in use";
        assert_eq!(
            strip_ansi(raw),
            "2026 ERROR proxy run error: bind 127.0.0.1:9988: Address already in use"
        );
        // No escapes — unchanged.
        assert_eq!(strip_ansi("plain line"), "plain line");
    }

    #[tokio::test]
    async fn proxy_bind_failure_does_not_publish_readiness() {
        let occupied = tokio::net::TcpListener::bind(("127.0.0.1", 0u16))
            .await
            .unwrap();
        let addr: SocketAddr = occupied.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("daemon.pid");
        let ctl_path = dir.path().join("daemon.ctl");
        let old_log = dir.path().join("capture.jsonl");
        std::fs::write(&old_log, "previous capture\n").unwrap();

        let error = bind_prepare_and_publish(addr, &pid_path, &ctl_path, || {
            std::fs::remove_file(&old_log)?;
            Ok(())
        })
        .await
        .err()
        .expect("occupied proxy port must fail startup");

        assert!(error.to_string().contains("bind proxy"));
        assert_eq!(
            std::fs::read_to_string(&old_log).unwrap(),
            "previous capture\n"
        );
        assert!(!pid_path.exists(), "pidfile advertised failed startup");
        assert!(!ctl_path.exists(), "control file advertised failed startup");
    }

    #[test]
    fn readiness_requires_exact_running_startup_identity() {
        let serial = Serial::new("emulator-5554");
        let matching = json!({
            "ok": true,
            "running": true,
            "serial": "emulator-5554",
            "startup_id": "startup-a",
            "pid": 42,
        });
        assert!(status_matches_startup(&matching, &serial, "startup-a", 42));

        for stale in [
            json!({"ok": true, "running": true, "serial": "other", "startup_id": "startup-a", "pid": 42}),
            json!({"ok": true, "running": true, "serial": "emulator-5554", "startup_id": "startup-b", "pid": 42}),
            json!({"ok": true, "running": true, "serial": "emulator-5554", "startup_id": "startup-a", "pid": 43}),
            json!({"ok": false, "running": true, "serial": "emulator-5554", "startup_id": "startup-a", "pid": 42}),
            json!({"ok": true, "running": false, "serial": "emulator-5554", "startup_id": "startup-a", "pid": 42}),
            json!({"ok": true, "running": true, "serial": "emulator-5554", "startup_id": "startup-a"}),
        ] {
            assert!(!status_matches_startup(&stale, &serial, "startup-a", 42));
        }
        assert!(!status_matches_startup(&matching, &serial, "", 42));
        assert!(!status_matches_startup(&matching, &serial, "startup-a", 0));
    }

    #[test]
    fn teardown_only_removes_the_markers_it_published() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("daemon.pid");
        let ctl_path = dir.path().join("daemon.ctl");
        write_marker_atomic(&pid_path, "42").unwrap();
        write_marker_atomic(&ctl_path, "5000").unwrap();

        remove_readiness_if_owned(&pid_path, &ctl_path, 41, 5000);
        assert!(pid_path.exists());
        assert!(ctl_path.exists());
        remove_readiness_if_owned(&pid_path, &ctl_path, 42, 5001);
        assert!(pid_path.exists());
        assert!(ctl_path.exists());

        remove_readiness_if_owned(&pid_path, &ctl_path, 42, 5000);
        assert!(!pid_path.exists());
        assert!(!ctl_path.exists());
    }
}
