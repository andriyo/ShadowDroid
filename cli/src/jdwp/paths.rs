//! Debug-daemon registry layout under `~/.shadowdroid/debug/<serial>/`:
//!
//!   - `<pid>.json`         — registry entry (daemon pid, socket, package, …)
//!   - `<pid>.sock`         — the daemon's `0600` unix control socket
//!   - `<pid>.log`          — the daemon's stdout/stderr
//!   - `<pid>.startup.json` — a structured startup failure for the parent
//!
//! Serial directories use the same collision-resistant component as the net
//! daemon, so devices never share a session. When the socket path would
//! exceed the platform's `sun_path` limit (~104 bytes on macOS), the socket
//! lives in the temp dir under a hashed name and the registry records it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::ids::stable_file_component;

/// Conservative `sun_path` budget shared by Linux (108) and macOS (104).
const MAX_SOCKET_PATH: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RegistryEntry {
    pub session_id: String,
    pub serial: String,
    pub pid: u32,
    pub package: Option<String>,
    pub daemon_pid: u32,
    pub socket: PathBuf,
    pub startup_id: String,
    pub attached_at: f64,
    pub log: PathBuf,
}

pub fn debug_dir() -> Result<PathBuf> {
    Ok(crate::hostenv::shadowdroid_home()?.join("debug"))
}

pub fn serial_dir(serial: &str) -> Result<PathBuf> {
    Ok(debug_dir()?.join(stable_file_component(serial)))
}

pub fn ensure_serial_dir(serial: &str) -> Result<PathBuf> {
    let dir = serial_dir(serial)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

pub fn session_id(serial: &str, pid: u32) -> String {
    format!("jdwp:{serial}:{pid}")
}

pub fn registry_path(serial: &str, pid: u32) -> Result<PathBuf> {
    Ok(serial_dir(serial)?.join(format!("{pid}.json")))
}

pub fn log_path(serial: &str, pid: u32) -> Result<PathBuf> {
    Ok(serial_dir(serial)?.join(format!("{pid}.log")))
}

pub fn startup_error_path(serial: &str, pid: u32) -> Result<PathBuf> {
    Ok(serial_dir(serial)?.join(format!("{pid}.startup.json")))
}

pub fn socket_path(serial: &str, pid: u32) -> Result<PathBuf> {
    let preferred = serial_dir(serial)?.join(format!("{pid}.sock"));
    Ok(short_socket_path(&preferred))
}

fn short_socket_path(preferred: &Path) -> PathBuf {
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH {
        return preferred.to_path_buf();
    }
    let hash = blake3::hash(preferred.as_os_str().to_string_lossy().as_bytes());
    std::env::temp_dir().join(format!("sd-debug-{}.sock", &hash.to_hex()[..16]))
}

/// Write `entry` atomically with owner-only permissions.
pub fn write_entry(entry: &RegistryEntry) -> Result<()> {
    let dir = ensure_serial_dir(&entry.serial)?;
    let path = registry_path(&entry.serial, entry.pid)?;
    let mut temp = tempfile::NamedTempFile::new_in(&dir)
        .with_context(|| format!("create temporary registry in {}", dir.display()))?;
    serde_json::to_writer_pretty(&mut temp, entry)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temp.persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

pub fn read_entry(path: &Path) -> Option<RegistryEntry> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Every registry entry, optionally for one serial, ordered by attach time.
pub fn entries(serial: Option<&str>) -> Vec<RegistryEntry> {
    let dirs: Vec<PathBuf> = match serial {
        Some(serial) => serial_dir(serial).into_iter().collect(),
        None => debug_dir()
            .ok()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect(),
    };
    let mut out: Vec<RegistryEntry> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && !path.to_string_lossy().ends_with(".startup.json")
        })
        .filter_map(|path| read_entry(&path))
        .collect();
    out.sort_by(|a, b| a.attached_at.total_cmp(&b.attached_at));
    out
}

/// Remove a registry entry and its socket if they still belong to
/// `startup_id` (a newer daemon for the same pid keeps its files).
pub fn remove_if_owned(serial: &str, pid: u32, startup_id: &str) {
    let Ok(path) = registry_path(serial, pid) else {
        return;
    };
    if let Some(entry) = read_entry(&path) {
        if entry.startup_id != startup_id {
            return;
        }
        let _ = std::fs::remove_file(&entry.socket);
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_socket_paths_fall_back_to_a_short_hashed_name() {
        let short = Path::new("/tmp/a.sock");
        assert_eq!(short_socket_path(short), short);
        let long = PathBuf::from(format!("/{}/123.sock", "x".repeat(120)));
        let fallback = short_socket_path(&long);
        assert!(fallback.as_os_str().len() < long.as_os_str().len());
        assert!(fallback.to_string_lossy().contains("sd-debug-"));
        assert_eq!(fallback, short_socket_path(&long), "stable");
    }

    #[test]
    fn session_ids_name_serial_and_pid() {
        assert_eq!(session_id("emulator-5554", 42), "jdwp:emulator-5554:42");
    }
}
