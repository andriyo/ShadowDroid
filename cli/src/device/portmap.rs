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
//!   - [`assign`] / [`peek`] / [`reassign`] / [`release`] — a *stable* per-serial
//!     port persisted under `~/.shadowdroid/ports/`, so the many short-lived UI
//!     commands reuse one `adb forward` rule instead of leaking a fresh one each
//!     invocation (and so the warm-probe fast path keeps hitting the same port).

use crate::ids::Serial;
use anyhow::{anyhow, Context, Result};
use std::net::TcpListener;
use std::path::PathBuf;

/// A free loopback TCP port chosen by the OS. The probe listener is dropped
/// before returning, so there is a small TOCTOU window before the caller binds
/// or forwards the port; callers treat a later bind/forward failure as "the port
/// got taken, pick another".
pub fn free_loopback_port() -> Result<u16> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("allocate a free loopback port")?;
    Ok(listener.local_addr()?.port())
}

fn ports_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("$HOME not set"))?;
    Ok(home.join(".shadowdroid").join("ports"))
}

fn slot_path(serial: &Serial, channel: &str) -> Result<PathBuf> {
    Ok(ports_dir()?.join(format!("{}__{channel}.port", sanitize(serial))))
}

/// The persisted host port for `(serial, channel)`, if one was recorded and
/// still parses.
pub fn peek(serial: &Serial, channel: &str) -> Option<u16> {
    let path = slot_path(serial, channel).ok()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The stable host port for `(serial, channel)`, allocating + persisting a fresh
/// free port the first time. Reused across invocations so repeated short-lived
/// commands share a single `adb forward` rule.
pub fn assign(serial: &Serial, channel: &str) -> Result<u16> {
    match peek(serial, channel) {
        Some(p) => Ok(p),
        None => reassign(serial, channel),
    }
}

/// Force a fresh allocation, overwriting any persisted port. Used when the
/// persisted port turns out to be unusable (e.g. an unrelated process grabbed it
/// after we released our forward).
pub fn reassign(serial: &Serial, channel: &str) -> Result<u16> {
    let dir = ports_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let port = free_loopback_port()?;
    std::fs::write(slot_path(serial, channel)?, port.to_string())
        .context("persist per-serial host port")?;
    Ok(port)
}

/// Forget the persisted mapping (on teardown / `disconnect`), returning the port
/// that was recorded so the caller can remove its `adb forward` rule.
pub fn release(serial: &Serial, channel: &str) -> Option<u16> {
    let path = slot_path(serial, channel).ok()?;
    let port = std::fs::read_to_string(&path).ok()?.trim().parse().ok();
    let _ = std::fs::remove_file(&path);
    port
}

/// Make a serial safe as a filename component (`emulator-5554`, an `IP:port`, a
/// USB serial). Keeps alphanumerics, `-`, `_`; everything else becomes `_`.
/// Mirrors the net store's own sanitizer.
fn sanitize(serial: &Serial) -> String {
    serial
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
    fn sanitizes_serials_for_slot_files() {
        assert_eq!(sanitize(&Serial::from("emulator-5554")), "emulator-5554");
        assert_eq!(
            sanitize(&Serial::from("192.168.1.5:5555")),
            "192_168_1_5_5555"
        );
    }
}
