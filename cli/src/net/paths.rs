//! Filesystem layout for the `net` namespace, all under `~/.shadowdroid/net/`
//! (the established store root — see [crate::config]).
//!
//! Per-serial files so two devices can be proxied independently:
//!   - `<serial>.sock`  — the daemon's Unix control socket
//!   - `<serial>.jsonl` — the session event log (backs `net log`)
//!   - `<serial>.log`   — the daemon's own stdout/stderr (diagnostics)
//!   - `<serial>.pid`   — the daemon pid (liveness + teardown)
//!   - `<serial>.state.json` — device networking state captured before wiring
//!
//! The CA is device-independent:
//!   - `ca.crt` / `ca.key` — the ShadowDroid root CA (generated once, installed
//!     into the device trust store).

use crate::ids::{Serial, stable_file_component};
use anyhow::{Result, anyhow};
use std::path::PathBuf;

fn home() -> Result<PathBuf> {
    // Delegate to the shared helper so `net` honors `%USERPROFILE%` on Windows,
    // matching the rest of the CLI (the daemon + control socket are TCP precisely
    // so `net` runs on Windows too).
    crate::hostenv::home_dir()
}

/// `~/.shadowdroid/net/` — does not create it.
pub fn net_dir() -> Result<PathBuf> {
    Ok(home()?.join(".shadowdroid").join("net"))
}

/// `~/.shadowdroid/net/`, created if missing.
pub fn ensure_net_dir() -> Result<PathBuf> {
    let dir = net_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| anyhow!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// CA filenames within [`net_dir`]. Single source of truth: [`ca_cert_path`]
/// (the live path) and [`crate::net::ca`]'s dir-scoped helpers both build from
/// these, so a generated and an imported CA always land on the same names.
pub const CA_CERT_FILE: &str = "ca.crt";
pub const CA_KEY_FILE: &str = "ca.key";
/// Provenance marker: `generated` (ShadowDroid minted it) or `imported` (a
/// user-provided CA installed via `net ca import`). Advisory — lets `net ca
/// info` report where the CA came from.
pub const CA_SOURCE_FILE: &str = "ca.source";

fn serial_path(serial: &Serial, suffix: &str) -> Result<PathBuf> {
    let dir = net_dir()?;
    Ok(dir.join(format!(
        "{}.{suffix}",
        stable_file_component(serial.as_str())
    )))
}

pub fn ca_cert_path() -> Result<PathBuf> {
    Ok(net_dir()?.join(CA_CERT_FILE))
}

pub fn ca_key_path() -> Result<PathBuf> {
    Ok(net_dir()?.join(CA_KEY_FILE))
}

/// Per-serial verify-once trust cache: records that a CA (by fingerprint) was
/// observed installed on this device, so a repeat `net trust`/`net check` can
/// skip the adb readback. Global + `$HOME`-keyed (like the other per-serial
/// state); never in the project folder — trust is a `(CA, device)` fact, not a
/// project fact.
pub fn trust_cache_path(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "trust.json")
}

/// The control endpoint file — stores the daemon's loopback-TCP control port.
/// (TCP rather than a Unix socket so `net` builds + runs on Windows too.)
pub fn ctl_path(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "ctl")
}

pub fn session_log_path(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "jsonl")
}

pub fn daemon_log_path(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "log")
}

pub fn pid_path(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "pid")
}

/// Device networking state captured immediately before `net start` changes it.
/// `net stop` consumes this file only after a successful restore, so a crashed
/// daemon or interrupted teardown can be recovered by a later invocation.
pub fn device_state_path(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "state.json")
}

#[cfg(test)]
mod tests {
    use super::{ctl_path, pid_path};
    use crate::ids::Serial;

    #[test]
    fn serial_paths_are_safe_and_collision_resistant() {
        let colon = ctl_path(&Serial::from("device:5555")).unwrap();
        let slash = ctl_path(&Serial::from("device/5555")).unwrap();
        assert_ne!(colon, slash);
        assert_ne!(colon, pid_path(&Serial::from("device:5555")).unwrap());
        assert!(!colon.file_name().unwrap().to_string_lossy().contains(':'));
    }
}
