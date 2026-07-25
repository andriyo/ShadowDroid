//! Per-device host state for the video daemon.

use crate::ids::{Serial, stable_file_component};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn dir() -> Result<PathBuf> {
    Ok(crate::hostenv::shadowdroid_home()?.join("video"))
}

pub fn ensure_dir() -> Result<PathBuf> {
    let path = dir()?;
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create video state directory {}", path.display()))?;
    protect_dir(&path)?;
    Ok(path)
}

fn serial_path(serial: &Serial, suffix: &str) -> Result<PathBuf> {
    Ok(dir()?.join(format!(
        "{}.{suffix}",
        stable_file_component(serial.as_str())
    )))
}

pub fn pid(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "pid")
}

pub fn control(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "ctl")
}

pub fn state(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "state.json")
}

pub fn log(serial: &Serial) -> Result<PathBuf> {
    serial_path(serial, "log")
}

pub fn protect_dir(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("set private permissions on {}", path.display()))?;
    }
    Ok(())
}

pub fn protect_file(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set private permissions on {}", path.display()))?;
    }
    Ok(())
}

pub const fn directory_mode_label() -> &'static str {
    if cfg!(unix) {
        "0700"
    } else {
        "platform_default"
    }
}

pub const fn file_mode_label() -> &'static str {
    if cfg!(unix) {
        "0600"
    } else {
        "platform_default"
    }
}
