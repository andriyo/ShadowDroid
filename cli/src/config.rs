//! User and project defaults for low-friction ShadowDroid runs.
//!
//! Config is intentionally JSON to match the rest of ShadowDroid's machine
//! interface. Values from `~/.shadowdroid/config.json` are loaded first, then
//! every `.shadowdroid.json` from the current directory's ancestors is layered
//! on top, nearest project file winning.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::device::adb;
use crate::hostenv::home_dir;

pub const USER_CONFIG_REL: &str = ".shadowdroid/config.json";
pub const PROJECT_CONFIG_FILE: &str = ".shadowdroid.json";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowDroidConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_studio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub studio_plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debugger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_configuration: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, AppConfig>,

    #[serde(skip)]
    pub sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_configuration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debugger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedApp {
    pub input: Option<String>,
    pub package: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_configuration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debugger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_mode: Option<String>,
}

impl ShadowDroidConfig {
    pub fn load() -> Result<Self> {
        let mut loaded = ShadowDroidConfig::default();
        for path in config_paths()? {
            let mut next = parse_config_file(&path)?;
            next.sources.clear();
            loaded.merge(next);
            loaded.sources.push(path);
        }
        Ok(loaded)
    }

    pub fn default_app(&self) -> Option<String> {
        self.app.clone()
    }

    pub fn configured_package_for(&self, name_or_package: &str) -> Option<String> {
        self.app_config(name_or_package)
            .map(|entry| entry.package.clone())
            .or_else(|| looks_like_package(name_or_package).then(|| name_or_package.to_string()))
    }

    pub fn default_project_for(&self, app: Option<&str>) -> Option<String> {
        app.and_then(|app| self.app_config(app))
            .and_then(|entry| entry.project.clone())
            .or_else(|| self.project.clone())
    }

    pub fn default_run_configuration_for(&self, app: Option<&str>) -> Option<String> {
        app.and_then(|app| self.app_config(app))
            .and_then(|entry| entry.run_configuration.clone())
            .or_else(|| self.run_configuration.clone())
    }

    pub fn default_debugger_for(&self, app: Option<&str>) -> Option<String> {
        app.and_then(|app| self.app_config(app))
            .and_then(|entry| entry.debugger.clone())
            .or_else(|| self.debugger.clone())
    }

    pub fn default_debug_mode_for(&self, app: Option<&str>) -> Option<String> {
        app.and_then(|app| self.app_config(app))
            .and_then(|entry| entry.debug_mode.clone())
            .or_else(|| self.debug_mode.clone())
    }

    pub async fn resolve_app(
        &self,
        serial: Option<&str>,
        requested: Option<&str>,
    ) -> Result<ResolvedApp> {
        let input = requested
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.default_app());

        let Some(input_value) = input.clone() else {
            return Ok(ResolvedApp {
                input,
                package: None,
                source: "not_configured".into(),
                project: self.project.clone(),
                run_configuration: self.run_configuration.clone(),
                debugger: self.debugger.clone(),
                debug_mode: self.debug_mode.clone(),
            });
        };

        if let Some(entry) = self.app_config(&input_value) {
            return Ok(ResolvedApp {
                input,
                package: Some(entry.package.clone()),
                source: "config_alias".into(),
                project: entry.project.clone().or_else(|| self.project.clone()),
                run_configuration: entry
                    .run_configuration
                    .clone()
                    .or_else(|| self.run_configuration.clone()),
                debugger: entry.debugger.clone().or_else(|| self.debugger.clone()),
                debug_mode: entry.debug_mode.clone().or_else(|| self.debug_mode.clone()),
            });
        }

        if looks_like_package(&input_value) {
            return Ok(ResolvedApp {
                input,
                package: Some(input_value),
                source: "package".into(),
                project: self.project.clone(),
                run_configuration: self.run_configuration.clone(),
                debugger: self.debugger.clone(),
                debug_mode: self.debug_mode.clone(),
            });
        }

        let Some(serial) = serial else {
            return Ok(ResolvedApp {
                input,
                package: None,
                source: "needs_device_for_name_lookup".into(),
                project: self.project.clone(),
                run_configuration: self.run_configuration.clone(),
                debugger: self.debugger.clone(),
                debug_mode: self.debug_mode.clone(),
            });
        };

        let packages = adb::list_packages(serial).await?;
        let needle = normalize_lookup(&input_value);
        let matches = packages
            .into_iter()
            .filter(|package| normalize_lookup(package).contains(&needle))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [package] => Ok(ResolvedApp {
                input,
                package: Some(package.clone()),
                source: "installed_package_name_match".into(),
                project: self.project.clone(),
                run_configuration: self.run_configuration.clone(),
                debugger: self.debugger.clone(),
                debug_mode: self.debug_mode.clone(),
            }),
            [] => Ok(ResolvedApp {
                input,
                package: None,
                source: "no_installed_package_match".into(),
                project: self.project.clone(),
                run_configuration: self.run_configuration.clone(),
                debugger: self.debugger.clone(),
                debug_mode: self.debug_mode.clone(),
            }),
            many => Err(anyhow!(
                "app name `{}` matched multiple installed packages: {}. Add an alias to {}.",
                input_value,
                many.join(", "),
                PROJECT_CONFIG_FILE
            )),
        }
    }

    fn app_config(&self, name_or_package: &str) -> Option<&AppConfig> {
        self.apps
            .iter()
            .find(|(name, entry)| {
                name.eq_ignore_ascii_case(name_or_package)
                    || entry.package.eq_ignore_ascii_case(name_or_package)
            })
            .map(|(_, entry)| entry)
    }

    fn merge(&mut self, other: ShadowDroidConfig) {
        self.device = other.device.or(self.device.take());
        self.app = other.app.or(self.app.take());
        self.project = other.project.or(self.project.take());
        self.studio_url = other.studio_url.or(self.studio_url.take());
        self.android_studio = other.android_studio.or(self.android_studio.take());
        self.studio_plugin = other.studio_plugin.or(self.studio_plugin.take());
        self.debugger = other.debugger.or(self.debugger.take());
        self.debug_mode = other.debug_mode.or(self.debug_mode.take());
        self.run_configuration = other.run_configuration.or(self.run_configuration.take());
        self.apps.extend(other.apps);
    }
}

pub fn user_config_path() -> Result<PathBuf> {
    home_dir().map(|home| home.join(USER_CONFIG_REL))
}

pub fn project_config_path() -> Result<PathBuf> {
    Ok(std::env::current_dir()
        .context("resolve current directory")?
        .join(PROJECT_CONFIG_FILE))
}

pub fn discovered_config_paths() -> Result<Vec<PathBuf>> {
    config_paths()
}

pub fn parse_config_file(path: &Path) -> Result<ShadowDroidConfig> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub fn write_config_file(path: &Path, config: &ShadowDroidConfig) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(config)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn config_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let user = user_config_path()?;
    if user.is_file() {
        paths.push(user);
    }

    let cwd = std::env::current_dir().context("resolve current directory")?;
    let mut project_paths = cwd
        .ancestors()
        .map(|dir| dir.join(PROJECT_CONFIG_FILE))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    project_paths.reverse();
    paths.extend(project_paths);
    Ok(paths)
}

fn looks_like_package(value: &str) -> bool {
    value.contains('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn normalize_lookup(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn expand_config_path(value: &Option<String>) -> Option<PathBuf> {
    value.as_deref().map(|raw| {
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Ok(home) = home_dir() {
                return home.join(rest);
            }
        }
        Path::new(raw).to_path_buf()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_config_overrides_user_config_and_merges_apps() {
        let mut user = ShadowDroidConfig {
            device: Some("emulator-1".into()),
            app: Some("old".into()),
            ..Default::default()
        };
        user.apps.insert(
            "Old".into(),
            AppConfig {
                package: "com.old".into(),
                ..Default::default()
            },
        );
        let mut project = ShadowDroidConfig {
            app: Some("Livd".into()),
            project: Some("/work/Livd".into()),
            ..Default::default()
        };
        project.apps.insert(
            "Livd".into(),
            AppConfig {
                package: "com.livd".into(),
                ..Default::default()
            },
        );

        user.merge(project);

        assert_eq!(user.device.as_deref(), Some("emulator-1"));
        assert_eq!(user.app.as_deref(), Some("Livd"));
        assert_eq!(
            user.default_project_for(Some("Livd")).as_deref(),
            Some("/work/Livd")
        );
        assert_eq!(user.app_config("Old").unwrap().package, "com.old");
        assert_eq!(user.app_config("com.livd").unwrap().package, "com.livd");
    }

    #[test]
    fn detects_package_names() {
        assert!(looks_like_package("com.livd"));
        assert!(!looks_like_package("Livd"));
        assert!(!looks_like_package("com livd"));
    }
}
