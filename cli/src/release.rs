//! Fetching ShadowDroid GitHub-release assets and verifying their SHA-256
//! checksums. One home for the pipeline the server-APK installer
//! ([crate::device::installer]), `aar` ([crate::cmd::aar]), and the Studio
//! plugin installer ([crate::cmd::studio]) previously carried as per-module
//! copies (which had already started to drift).

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// The checksums manifest published alongside every release's assets.
pub const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// Base URL for the release matching `version`. `SHADOWDROID_RELEASE_BASE_URL`
/// is a compile-time override (with `{version}` substitution) so forks and
/// mirrors can redirect downloads without patching call sites.
pub fn release_base_url(version: &str) -> String {
    let template = option_env!("SHADOWDROID_RELEASE_BASE_URL")
        .unwrap_or("https://github.com/andriyo/ShadowDroid/releases/download/v{version}");
    template
        .replace("{version}", version)
        .trim_end_matches('/')
        .to_string()
}

pub fn release_asset_url(base: &str, asset: &str) -> String {
    format!("{}/{asset}", base.trim_end_matches('/'))
}

pub async fn download_text(url: &str) -> Result<String> {
    let resp = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "shadowdroid")
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.text().await?)
}

pub async fn download_file(url: &str, path: &Path) -> Result<()> {
    let resp = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "shadowdroid")
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?;
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

/// The digest an asset must hash to: a compile-time embedded override when the
/// build pinned one, otherwise the entry from the downloaded checksums manifest.
pub fn expected_sha(embedded: Option<&str>, checksums: &str, asset: &str) -> Result<String> {
    if let Some(value) = embedded.map(str::trim).filter(|s| !s.is_empty()) {
        return normalize_sha256(value);
    }
    checksum_for(checksums, asset)
        .ok_or_else(|| anyhow!("no checksum found for {asset} in {CHECKSUMS_ASSET}"))
}

/// Look `asset` up in `sha256sum`-format output (`<sha>  <name>`; a leading
/// `*` on the name marks binary mode and is ignored).
pub fn checksum_for(checksums: &str, asset: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let sha = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == asset {
            normalize_sha256(sha).ok()
        } else {
            None
        }
    })
}

pub fn normalize_sha256(value: &str) -> Result<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.len() == 64 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(lower)
    } else {
        bail!("invalid SHA-256 digest: {value}")
    }
}

pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    Ok(hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sha256sums_lines() {
        let sums = "\
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  shadowdroid-server-main.apk
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa *shadowdroid-server-test.apk
";
        assert_eq!(
            checksum_for(sums, "shadowdroid-server-main.apk").as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            checksum_for(sums, "shadowdroid-server-test.apk").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(checksum_for(sums, "missing.apk").is_none());
    }

    #[test]
    fn rejects_invalid_sha256() {
        assert!(normalize_sha256("abc").is_err());
        assert!(normalize_sha256(
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
        )
        .is_err());
    }

    #[test]
    fn embedded_sha_overrides_manifest() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  a.zip\n";
        let embedded = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(
            expected_sha(Some(embedded), sums, "a.zip").unwrap(),
            embedded.to_ascii_lowercase()
        );
        // Blank embedded value falls through to the manifest.
        assert_eq!(
            expected_sha(Some("  "), sums, "a.zip").unwrap(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert!(expected_sha(None, sums, "missing.zip").is_err());
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        assert_eq!(
            release_asset_url("https://x/y/", "a.zip"),
            "https://x/y/a.zip"
        );
        assert!(release_base_url("1.2.3").ends_with("v1.2.3"));
    }
}
