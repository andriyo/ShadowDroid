//! Shared, crash-safe JSON artifact writing for commands with `--out`.
//!
//! Artifact-producing commands still emit one small terminal action on stdout;
//! the potentially large payload lives at the requested path.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

pub fn write_json_and_emit(cmd: &str, path: &Path, value: &Value) -> Result<()> {
    let bytes = write_json(path, value)?;
    crate::events::emit_action(
        cmd,
        &json!({
            "artifact": path.display().to_string(),
            "bytes": bytes,
            "artifact_type": value.get("type"),
            "schema_version": value.get("schema_version"),
            "next_actions": [format!("read {} for the complete result", path.display())],
        }),
    );
    Ok(())
}

/// Atomically replace a JSON artifact after its complete contents are synced.
pub fn write_json(path: &Path, value: &Value) -> Result<u64> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating artifact directory {}", parent.display()))?;

    let existing_permissions = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary artifact beside {}", path.display()))?;
    temp.write_all(&bytes)
        .with_context(|| format!("writing temporary artifact for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("flushing temporary artifact for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary artifact for {}", path.display()))?;
    if let Some(permissions) = existing_permissions {
        temp.as_file()
            .set_permissions(permissions)
            .with_context(|| format!("preserving permissions for {}", path.display()))?;
    }
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replacing artifact {}", path.display()))?;
    Ok(bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomically_replaces_complete_json_and_keeps_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        std::fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }

        let bytes = write_json(&path, &json!({"complete": true})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(bytes as usize, text.len());
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["complete"],
            true
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }
}
