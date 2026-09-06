//! Local-only recovery handles. A redacted command can select the same device
//! without printing its serial. Handles are not credentials and never travel
//! to adb; their protected mapping is resolved before device selection.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

const PREFIX: &str = "@sd-";
static HANDLES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

pub fn register(serial: &str) -> Result<String> {
    let mut handles = HANDLES.get_or_init(Mutex::default).lock().unwrap();
    if let Some(handle) = handles.get(serial) {
        return Ok(handle.clone());
    }
    let handle = register_at(
        &crate::hostenv::shadowdroid_home()?.join("device-refs"),
        serial,
    )?;
    handles.insert(serial.into(), handle.clone());
    Ok(handle)
}

fn register_at(dir: &Path, serial: &str) -> Result<String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    if dir.is_symlink() {
        anyhow::bail!("device handle directory must not be a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let index = dir.join(format!(
        "lookup-{}",
        blake3::hash(serial.as_bytes()).to_hex()
    ));
    if let Ok(handle) = std::fs::read_to_string(&index)
        && resolve_at(dir, &handle).is_ok_and(|saved| saved == serial)
    {
        return Ok(handle);
    }
    let mut file = tempfile::Builder::new()
        .prefix("device-")
        .rand_bytes(16)
        .tempfile_in(dir)?;
    file.write_all(serial.as_bytes())?;
    file.as_file().sync_all()?;
    let name = file
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    file.keep().context("save local device recovery handle")?;
    let handle = format!("{PREFIX}{name}");
    let mut lookup = tempfile::NamedTempFile::new_in(dir)?;
    lookup.write_all(handle.as_bytes())?;
    lookup.persist(index).context("save device handle lookup")?;
    Ok(handle)
}

pub fn resolve(value: &str) -> Result<String> {
    if !value.starts_with(PREFIX) {
        return Ok(value.to_owned());
    }
    resolve_at(
        &crate::hostenv::shadowdroid_home()?.join("device-refs"),
        value,
    )
}

fn resolve_at(dir: &Path, value: &str) -> Result<String> {
    let name = value.strip_prefix(PREFIX).unwrap_or_default();
    let valid = name.starts_with("device-")
        && name.len() <= 80
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let serial = valid
        .then(|| std::fs::read_to_string(dir.join(name)).ok())
        .flatten();
    serial.filter(|s| !s.is_empty()).ok_or_else(|| {
        crate::diagnostic::DiagnosticError::new(
            "device_ref_unavailable",
            "device",
            "local device handle is invalid or no longer available",
        )
        .next_actions([
            "shadowdroid devices",
            "select a device again with --target or --device",
        ])
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_round_trip_without_disclosing_serial_and_reject_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let handle = register_at(dir.path(), "192.0.2.1:5555").unwrap();
        assert!(!handle.contains("192.0.2"));
        assert_eq!(register_at(dir.path(), "192.0.2.1:5555").unwrap(), handle);
        assert_eq!(resolve_at(dir.path(), &handle).unwrap(), "192.0.2.1:5555");
        assert!(resolve_at(dir.path(), "@sd-../../secret").is_err());
        assert!(resolve_at(dir.path(), "@sd-device-missing").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.path().join(handle.strip_prefix(PREFIX).unwrap());
            assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
