//! Per-serial host-port assignment, so concurrent ShadowDroid sessions driving
//! *different* devices on one machine don't collide on a fixed loopback port.
//!
//! Historically the host side of every `adb forward` was a hardcoded constant
//! (the UI server on `7912`, the in-app agent on `8129`). adb keys a forward by
//! its host port, so a second session forwarding the same port to a *different*
//! device silently rebinds the first — both sessions then talk to whichever
//! device forwarded last. Scoping the host port to the serial removes that
//! cross-wiring.
//!
//! Two flavours:
//!   - [`free_loopback_port`] — a fresh OS-chosen free port the caller wires up
//!     immediately and tears down right after (one-shot `adb forward`, or the
//!     net daemon's proxy bind). No persistence.
//!   - [`publish_forward`] / [`peek`] / [`release`] — a *stable* per-serial port
//!     persisted under `~/.shadowdroid/ports/`, so the many short-lived UI
//!     commands reuse one verified `adb forward` rule instead of leaking a fresh
//!     one each invocation.

use crate::hostenv::shadowdroid_home;
use crate::ids::{Serial, stable_file_component};
use anyhow::{Context, Result, anyhow};
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;

/// A free loopback TCP port chosen by the OS. The probe listener is dropped
/// before returning, so there is a small TOCTOU window before the caller binds
/// or forwards the port; callers treat a later bind/forward failure as "the port
/// got taken, pick another".
pub fn free_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("allocate a free loopback port")?;
    Ok(listener.local_addr()?.port())
}

/// Cross-process reservation for the host-port allocation/publication window.
/// Keep this alive until the listener or ADB forward has actually been
/// published; per-device lifecycle locks cannot prevent two different devices
/// from racing for the same host port.
pub struct LoopbackAllocation {
    _lock: std::fs::File,
    port: u16,
}

impl LoopbackAllocation {
    pub fn port(&self) -> u16 {
        self.port
    }
}

fn allocation_lock() -> Result<std::fs::File> {
    let dir = ports_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(".allocation.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

/// Reserve a fresh port and hold the global allocation lock until the caller
/// has bound/published it.
pub fn reserve_loopback_port() -> Result<LoopbackAllocation> {
    let lock = allocation_lock()?;
    let port = choose_unused_port(None)?;
    Ok(LoopbackAllocation { _lock: lock, port })
}

fn ports_dir() -> Result<PathBuf> {
    Ok(shadowdroid_home()?.join("ports"))
}

fn slot_path(serial: &Serial, channel: &str) -> Result<PathBuf> {
    let dir = ports_dir()?;
    Ok(dir.join(format!(
        "{}__{channel}.port",
        stable_file_component(serial.as_str())
    )))
}

/// The persisted host port for `(serial, channel)`, if one was recorded and
/// still parses.
pub fn peek(serial: &Serial, channel: &str) -> Option<u16> {
    let path = slot_path(serial, channel).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Allocate and publish an ADB forward while holding the one machine-wide port
/// lock. This closes the gap where another ShadowDroid process could select the
/// same host port and let ADB silently rebind it to a different device.
pub async fn publish_forward(serial: &Serial, channel: &str, device_port: u16) -> Result<u16> {
    let _lock = allocation_lock()?;
    let port = assign_locked(serial, channel)?;
    if super::adb::forward(serial, port, device_port).await.is_ok() {
        return Ok(port);
    }
    let port = reassign_locked(serial, channel)?;
    super::adb::forward(serial, port, device_port).await?;
    Ok(port)
}

fn assign_locked(serial: &Serial, channel: &str) -> Result<u16> {
    let own_path = slot_path(serial, channel)?;
    if let Some(port) = peek(serial, channel)
        && !persisted_ports(Some(&own_path))?.contains(&port)
    {
        return Ok(port);
    }
    reassign_locked(serial, channel)
}

fn reassign_locked(serial: &Serial, channel: &str) -> Result<u16> {
    let dir = ports_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = slot_path(serial, channel)?;
    let port = choose_unused_port(Some(&path))?;
    let mut temp = tempfile::NamedTempFile::new_in(&dir)
        .with_context(|| format!("create temporary port slot in {}", dir.display()))?;
    write!(temp, "{port}").context("write per-serial host port")?;
    temp.as_file()
        .sync_all()
        .context("sync per-serial host port")?;
    temp.persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish {}", path.display()))?;
    Ok(port)
}

fn persisted_ports(excluding: Option<&std::path::Path>) -> Result<std::collections::HashSet<u16>> {
    let dir = ports_dir()?;
    persisted_ports_in(&dir, excluding)
}

fn persisted_ports_in(
    dir: &std::path::Path,
    excluding: Option<&std::path::Path>,
) -> Result<std::collections::HashSet<u16>> {
    let mut ports = std::collections::HashSet::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ports),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if excluding.is_some_and(|excluded| excluded == path) {
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("port") {
            continue;
        }
        if let Ok(value) = std::fs::read_to_string(path)
            && let Ok(port) = value.trim().parse::<u16>()
            && port != 0
        {
            ports.insert(port);
        }
    }
    Ok(ports)
}

fn choose_unused_port(excluding: Option<&std::path::Path>) -> Result<u16> {
    let persisted = persisted_ports(excluding)?;
    for _ in 0..32 {
        let port = free_loopback_port()?;
        if !persisted.contains(&port) {
            return Ok(port);
        }
    }
    Err(anyhow!(
        "could not allocate a unique loopback port after 32 attempts"
    ))
}

/// Forget the persisted mapping (on teardown / `disconnect`), returning the port
/// that was recorded so the caller can remove its `adb forward` rule.
pub fn release(serial: &Serial, channel: &str) -> Option<u16> {
    let _lock = allocation_lock().ok()?;
    let path = slot_path(serial, channel).ok()?;
    let port = std::fs::read_to_string(&path).ok()?.trim().parse().ok();
    let _ = std::fs::remove_file(&path);
    port
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_loopback_port_is_nonzero_and_distinct() {
        let a = free_loopback_port().unwrap();
        let b = free_loopback_port().unwrap();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        // Two back-to-back allocations from the ephemeral range effectively
        // never collide; this guards against a degenerate "always returns 0".
        assert_ne!(a, b);
    }

    #[test]
    fn serial_slot_files_do_not_collide_after_sanitizing() {
        let a = slot_path(&Serial::from("device:5555"), "ui").unwrap();
        let b = slot_path(&Serial::from("device/5555"), "ui").unwrap();
        assert_ne!(a, b);
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("__ui.port")
        );
    }

    #[test]
    fn persisted_port_scan_excludes_only_the_requested_slot() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("device-a__ui.port");
        let b = temp.path().join("device-b__ui.port");
        std::fs::write(&a, "41001").unwrap();
        std::fs::write(&b, "41001").unwrap();
        let used = persisted_ports_in(temp.path(), Some(&a)).unwrap();
        assert!(used.contains(&41001));
    }
}
