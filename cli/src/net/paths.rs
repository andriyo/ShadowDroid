//! Filesystem layout for the `net` namespace, all under `~/.shadowdroid/net/`
//! (the established store root — see [crate::config]).
//!
//! Per-serial files so two devices can be proxied independently:
//!   - `<serial>.sock`  — the daemon's Unix control socket
//!   - `<serial>.jsonl` — the session event log (backs `net log`)
//!   - `<serial>.log`   — the daemon's own stdout/stderr (diagnostics)
//!   - `<serial>.pid`   — the daemon pid (liveness + teardown)
//! The CA is device-independent:
//!   - `ca.crt` / `ca.key` — the ShadowDroid root CA (generated once, installed
//!     into the device trust store).

use crate::ids::Serial;
use anyhow::{anyhow, Result};
use std::path::PathBuf;

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("$HOME not set"))
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

pub fn ca_cert_path() -> Result<PathBuf> {
    Ok(net_dir()?.join(CA_CERT_FILE))
}

/// The control endpoint file — stores the daemon's loopback-TCP control port.
/// (TCP rather than a Unix socket so `net` builds + runs on Windows too.)
pub fn ctl_path(serial: &Serial) -> Result<PathBuf> {
    Ok(net_dir()?.join(format!("{}.ctl", sanitize(serial))))
}

pub fn session_log_path(serial: &Serial) -> Result<PathBuf> {
    Ok(net_dir()?.join(format!("{}.jsonl", sanitize(serial))))
}

pub fn daemon_log_path(serial: &Serial) -> Result<PathBuf> {
    Ok(net_dir()?.join(format!("{}.log", sanitize(serial))))
}

pub fn pid_path(serial: &Serial) -> Result<PathBuf> {
    Ok(net_dir()?.join(format!("{}.pid", sanitize(serial))))
}

/// Make a serial safe as a filename component (`emulator-5554`, an IP:port, a
/// USB serial). Keeps alphanumerics, `-`, `_`; everything else → `_`.
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
    use super::sanitize;
    use crate::ids::Serial;

    #[test]
    fn sanitizes_serials() {
        assert_eq!(sanitize(&Serial::from("emulator-5554")), "emulator-5554");
        assert_eq!(
            sanitize(&Serial::from("192.168.1.5:5555")),
            "192_168_1_5_5555"
        );
        assert_eq!(sanitize(&Serial::from("R5CT80ABCDE")), "R5CT80ABCDE");
    }
}
