//! Host-side environment lookups: the user's home directory, ShadowDroid's
//! `~/.shadowdroid` data directory, and truthy env-var toggles. One home for
//! helpers that config, skill, aar, studio, and the installer previously
//! duplicated per module (with slightly diverged fallback behavior).

use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// The user's home directory: `$HOME`, then `%USERPROFILE%` (Windows).
pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot determine home directory ($HOME/%USERPROFILE% unset)"))
}

/// ShadowDroid's host data directory (APK/AAR/plugin caches, bridge registry).
/// Prefers the documented `~/.shadowdroid`; only when no home directory is
/// discoverable does it fall back to the platform config dir.
pub fn shadowdroid_home() -> Result<PathBuf> {
    if let Ok(home) = home_dir() {
        return Ok(home.join(".shadowdroid"));
    }
    let dirs = directories::ProjectDirs::from("io.github", "andriyo", "ShadowDroid")
        .ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(dirs.config_dir().to_path_buf())
}

/// Truthy-env check shared across the CLI. Accepts `1`/`true`/`yes`/`on`
/// (case-insensitive, trimmed); unset or anything else (including `0`/`no`/
/// `off`) is false.
pub fn env_truthy(name: &str) -> bool {
    let value = std::env::var(name).ok();
    env_value_truthy(value.as_deref())
}

fn env_value_truthy(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_truthy_accepts_common_spellings() {
        for value in ["1", "true", "TRUE", "yes", " on ", "On"] {
            assert!(env_value_truthy(Some(value)), "{value:?} should be truthy");
        }
        for value in ["0", "false", "no", "off", "", "banana"] {
            assert!(!env_value_truthy(Some(value)), "{value:?} should be falsy");
        }
        assert!(!env_value_truthy(None), "unset should be falsy");
    }
}
