//! Host-side environment lookups: the user's home directory, ShadowDroid's
//! `~/.shadowdroid` data directory, and truthy env-var toggles. One home for
//! helpers that config, skill, aar, studio, and the installer previously
//! duplicated per module (with slightly diverged fallback behavior).

use std::path::PathBuf;

use anyhow::{anyhow, Result};

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
    std::env::var(name)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_truthy_accepts_common_spellings() {
        // Unique var name keeps this independent of other (parallel) tests.
        let key = "SHADOWDROID_TEST_ENV_TRUTHY";
        for v in ["1", "true", "TRUE", "yes", " on ", "On"] {
            std::env::set_var(key, v);
            assert!(env_truthy(key), "{v:?} should be truthy");
        }
        for v in ["0", "false", "no", "off", "", "banana"] {
            std::env::set_var(key, v);
            assert!(!env_truthy(key), "{v:?} should be falsy");
        }
        std::env::remove_var(key);
        assert!(!env_truthy(key), "unset should be falsy");
    }
}
