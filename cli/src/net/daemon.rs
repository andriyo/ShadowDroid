//! The proxy daemon: spawned detached by `net start`, it runs the MITM proxy
//! ([crate::net::proxy]) + the control socket ([crate::net::control]) until
//! `net stop` (or Ctrl-C). Entry point for the hidden `net daemon` subcommand.

use crate::ids::Serial;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::events::{self, Event};
use crate::net::ca::CertAuthority;
use crate::net::control::{self, DaemonState};
use crate::net::flow::FlowRecord;
use crate::net::proxy::{self, ProxyContext, SharedState};
use crate::net::{paths, store, DaemonConfig};

/// Run the daemon in the foreground (this process IS the detached daemon).
pub async fn run(cfg: DaemonConfig) -> Result<()> {
    paths::ensure_net_dir()?;
    // Each `net start` is a fresh capture session.
    let _ = store::clear(&cfg.serial);
    std::fs::write(
        paths::pid_path(&cfg.serial)?,
        std::process::id().to_string(),
    )
    .context("write pidfile")?;

    let ca = CertAuthority::load_or_generate()?;
    let (flow_tx, mut flow_rx) = mpsc::unbounded_channel::<FlowRecord>();
    let (event_tx, _event_rx) = broadcast::channel::<Arc<Event>>(1024);
    let shared = Arc::new(SharedState {
        anticache: cfg.anticache,
        anticomp: cfg.anticomp,
        host_filters: cfg.app_filters.clone(),
        intercept: RwLock::new(None),
        held: Mutex::new(HashMap::new()),
        events: event_tx.clone(),
        rules: RwLock::new(Vec::new()),
        replay: RwLock::new(None),
    });

    let ctx = Arc::new(ProxyContext {
        ca,
        client: proxy::build_upstream_client(),
        flow_tx,
        shared: shared.clone(),
    });

    let state = Arc::new(DaemonState {
        port: cfg.port,
        started: events::now_ts(),
        flow_count: AtomicU64::new(0),
        events: event_tx,
    });

    // Drain completed flows → session log + live broadcast.
    {
        let state = state.clone();
        let serial = cfg.serial.clone();
        tokio::spawn(async move {
            while let Some(rec) = flow_rx.recv().await {
                state.flow_count.fetch_add(1, Ordering::Relaxed);
                let _ = store::append(&serial, &rec);
                let _ = state.events.send(Arc::new(rec.http_event()));
            }
        });
    }

    // Proxy listener. Binds the per-serial host port (`adb reverse` maps the
    // device-facing `cfg.port` to it), not `cfg.port` itself, so daemons for
    // different devices don't collide on one loopback port.
    let (proxy_stop_tx, proxy_stop_rx) = oneshot::channel();
    let addr: SocketAddr = ([127, 0, 0, 1], cfg.host_port).into();
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = proxy::run(ctx, addr, proxy_stop_rx).await {
                tracing::error!("proxy run error: {e}");
                // Binding failed (port in use?) — exit so `net start` times out
                // cleanly instead of hanging on an unusable daemon.
                std::process::exit(1);
            }
        });
    }

    // Control socket — loopback TCP on an ephemeral port (cross-platform; a
    // Unix domain socket wouldn't build on Windows). The chosen port is written
    // to a `.ctl` file that clients read to find us.
    let listener = TcpListener::bind(("127.0.0.1", 0u16))
        .await
        .context("bind control socket")?;
    let ctl_port = listener.local_addr().context("control addr")?.port();
    let ctl_path = paths::ctl_path(&cfg.serial)?;
    std::fs::write(&ctl_path, ctl_port.to_string()).context("write control port file")?;
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
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(paths::pid_path(&cfg.serial)?);
    tracing::info!("net daemon stopped");
    Ok(())
}

/// Launch the daemon as a detached background process (re-exec `net daemon`),
/// returning its pid. Used by `net start`. stdio is redirected to the daemon
/// log; on Unix the child gets its own process group so terminal signals to the
/// short-lived `net start` don't reach it.
pub fn spawn(cfg: &DaemonConfig) -> Result<u32> {
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
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--host-port")
        .arg(cfg.host_port.to_string());
    for app in &cfg.app_filters {
        cmd.arg("--host").arg(app);
    }
    if cfg.anticache {
        cmd.arg("--anticache");
    }
    if cfg.anticomp {
        cmd.arg("--anticomp");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().context("spawn net daemon process")?;
    Ok(child.id())
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

/// Wait (up to `timeout_ms`) for a freshly-spawned daemon's control socket to
/// accept connections. Returns false on timeout.
pub async fn await_ready(serial: &Serial, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if control::is_running(serial).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

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
}
