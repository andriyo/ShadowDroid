//! Host CLI update checks.
//!
//! This module deliberately keeps `shadowdroid update --check` non-mutating:
//! it reports the latest GitHub Release and the appropriate package-manager or
//! installer command for the detected install method.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::Path;

const DEFAULT_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/andriyo/ShadowDroid/releases/latest";
const DIRECT_UNIX_UPDATE_COMMAND: &str = "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | sh";
const DIRECT_WINDOWS_UPDATE_COMMAND: &str = "powershell -ExecutionPolicy Bypass -c \"irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex\"";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMethod {
    Homebrew,
    Scoop,
    Cargo,
    Direct,
    Unknown,
}

impl InstallMethod {
    fn label(self) -> &'static str {
        match self {
            Self::Homebrew => "homebrew",
            Self::Scoop => "scoop",
            Self::Cargo => "cargo",
            Self::Direct => "direct_installer",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UpdateCheck {
    pub current_version: String,
    pub latest_version: String,
    pub latest_tag: String,
    pub up_to_date: bool,
    pub install_method: InstallMethod,
    pub install_path: String,
    pub update_command: String,
    pub release_url: String,
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
}

pub async fn cmd_update(_check: bool, json: bool) -> Result<()> {
    let check = check_latest().await?;
    if json {
        println!("{}", serde_json::to_string(&check)?);
    } else {
        print_human(&check);
    }
    Ok(())
}

async fn check_latest() -> Result<UpdateCheck> {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let latest = latest_release().await?;
    let latest_version = normalize_tag(&latest.tag_name);
    let install_path =
        std::env::current_exe().context("cannot determine current shadowdroid executable path")?;
    let install_method = detect_install_method(&install_path);
    let update_command = update_command(install_method);

    Ok(UpdateCheck {
        up_to_date: compare_versions(&current_version, &latest_version) != Ordering::Less,
        current_version,
        latest_version,
        latest_tag: latest.tag_name,
        install_method,
        install_path: install_path.display().to_string(),
        update_command,
        release_url: latest.html_url,
    })
}

async fn latest_release() -> Result<LatestRelease> {
    let url = std::env::var("SHADOWDROID_UPDATE_LATEST_URL")
        .unwrap_or_else(|_| DEFAULT_LATEST_RELEASE_URL.to_string());
    let resp = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::USER_AGENT, "shadowdroid")
        .send()
        .await
        .with_context(|| format!("request latest release from {url}"))?
        .error_for_status()
        .with_context(|| format!("latest release request failed for {url}"))?;
    Ok(resp.json::<LatestRelease>().await?)
}

fn print_human(check: &UpdateCheck) {
    if check.up_to_date {
        println!(
            "shadowdroid {} is up to date (latest {}).",
            check.current_version, check.latest_tag
        );
    } else {
        println!(
            "shadowdroid update available: {} -> {}.",
            check.current_version, check.latest_tag
        );
    }
    println!(
        "install method: {} ({})",
        check.install_method.label(),
        check.install_path
    );
    println!("release: {}", check.release_url);
    println!("update command:");
    println!("  {}", check.update_command);
}

fn detect_install_method(path: &Path) -> InstallMethod {
    let normalized = normalize_path(path);
    if normalized.contains("/cellar/shadowdroid/")
        || normalized.contains("/homebrew/cellar/shadowdroid/")
    {
        return InstallMethod::Homebrew;
    }
    if normalized.contains("/scoop/apps/shadowdroid/")
        || normalized.contains("/scoop/shims/shadowdroid")
    {
        return InstallMethod::Scoop;
    }
    if normalized.contains("/.cargo/bin/shadowdroid") {
        return InstallMethod::Cargo;
    }
    if normalized.ends_with("/.local/bin/shadowdroid")
        || normalized.contains("/shadowdroid/bin/shadowdroid")
    {
        return InstallMethod::Direct;
    }
    InstallMethod::Unknown
}

fn normalize_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn update_command(method: InstallMethod) -> String {
    match method {
        InstallMethod::Homebrew => "brew upgrade shadowdroid".to_string(),
        InstallMethod::Scoop => "scoop update shadowdroid".to_string(),
        InstallMethod::Cargo => "cargo install shadowdroid --locked --force".to_string(),
        InstallMethod::Direct => direct_update_command(),
        InstallMethod::Unknown => format!(
            "brew upgrade shadowdroid  # or: scoop update shadowdroid  # or: {}",
            direct_update_command()
        ),
    }
}

fn direct_update_command() -> String {
    if cfg!(windows) {
        DIRECT_WINDOWS_UPDATE_COMMAND.to_string()
    } else {
        DIRECT_UNIX_UPDATE_COMMAND.to_string()
    }
}

fn normalize_tag(value: &str) -> String {
    value.trim().trim_start_matches('v').to_string()
}

fn compare_versions(a: &str, b: &str) -> Ordering {
    let a_parts = version_parts(a);
    let b_parts = version_parts(b);
    for i in 0..a_parts.len().max(b_parts.len()) {
        let left = *a_parts.get(i).unwrap_or(&0);
        let right = *b_parts.get(i).unwrap_or(&0);
        match left.cmp(&right) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

fn version_parts(value: &str) -> Vec<u64> {
    normalize_tag(value)
        .split(['.', '-'])
        .take_while(|part| part.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|part| part.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_like_versions() {
        assert_eq!(compare_versions("0.1.10", "0.1.2"), Ordering::Greater);
        assert_eq!(compare_versions("v0.1.2", "0.1.2"), Ordering::Equal);
        assert_eq!(compare_versions("0.1.2", "0.2.0"), Ordering::Less);
    }

    #[test]
    fn detects_common_install_methods() {
        assert_eq!(
            detect_install_method(Path::new(
                "/opt/homebrew/Cellar/shadowdroid/0.1.2/bin/shadowdroid"
            )),
            InstallMethod::Homebrew
        );
        assert_eq!(
            detect_install_method(Path::new(
                "C:\\Users\\me\\scoop\\apps\\shadowdroid\\current\\shadowdroid.exe"
            )),
            InstallMethod::Scoop
        );
        assert_eq!(
            detect_install_method(Path::new("/Users/me/.cargo/bin/shadowdroid")),
            InstallMethod::Cargo
        );
        assert_eq!(
            detect_install_method(Path::new("/Users/me/.local/bin/shadowdroid")),
            InstallMethod::Direct
        );
    }
}
