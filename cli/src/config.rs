//! User and project defaults for low-friction ShadowDroid runs.
//!
//! Config is intentionally JSON to match the rest of ShadowDroid's machine
//! interface. Values from `~/.shadowdroid/config.json` are loaded first, then
//! one `.shadowdroid/config.json` per ancestor directory is layered on top,
//! nearest project file winning.
//!
//! The project `.shadowdroid/` folder also holds companion files that are not
//! config — most importantly a per-project proxy CA (`ca.{crt,key}`, git-ignored)
//! that the `net` MITM proxy signs with. See [crate::net::ca].

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::device::adb;
use crate::hostenv::home_dir;

pub const USER_CONFIG_REL: &str = ".shadowdroid/config.json";
/// The project config directory (`<project>/.shadowdroid/`).
pub const PROJECT_CONFIG_DIR: &str = ".shadowdroid";
/// The project config file (`<project>/.shadowdroid/config.json`).
pub const PROJECT_CONFIG_REL: &str = ".shadowdroid/config.json";

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
    /// Opt-in local usage log (`shadowdroid usage`): verb + duration + error
    /// code per invocation, written to ~/.shadowdroid/usage.jsonl. Never
    /// argument values; never leaves the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_log: Option<bool>,
    /// MITM proxy (`net`) defaults: the signing CA, a trusted-CA assertion, and
    /// per-project defaults for `net start`/`net trust`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, AppConfig>,

    #[serde(skip)]
    pub sources: Vec<PathBuf>,
}

/// `net` proxy configuration. All fields optional; nested `deny_unknown_fields`
/// so a typo inside `proxy` is caught by `config validate` like the top level.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Path to a signing CA certificate (PEM). Absolute or `~/`-prefixed only —
    /// a bare relative path can't be resolved reliably once configs are merged,
    /// so it is rejected by `config validate`. Leave unset to use the per-project
    /// convention CA (`.shadowdroid/ca.{crt,key}`) or the global CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<String>,
    /// Path to the signing CA private key (PEM). Same path rules as `ca_cert`;
    /// required when `ca_cert` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_key: Option<String>,
    /// Assert the CA is already trusted on the device, so `net trust`/`net check`
    /// skip the adb install + trust-store readback and report the basis as
    /// `asserted`. For CAs baked into a custom emulator image, or pre-installed
    /// out of band. Does not override the app-level Network-Security-Config
    /// verdict (whether the *app* trusts user CAs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_trusted: Option<bool>,
    /// Default device-facing proxy port for `net start` (default 8080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Default host allowlist (globs) for `net start`/`net log`/`intercept`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// Preferred device trust store for `net trust`: `system`, `user`, or `ui`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_store: Option<String>,
    /// Default for `net start --verify-upstream`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_upstream: Option<bool>,
    /// Default for `net start --anticache`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anticache: Option<bool>,
    /// Default for `net start --anticomp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anticomp: Option<bool>,
    /// Default for `net start --redact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact: Option<bool>,
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
                PROJECT_CONFIG_REL
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
        self.usage_log = other.usage_log.or(self.usage_log.take());
        self.proxy = merge_proxy(self.proxy.take(), other.proxy);
        self.apps.extend(other.apps);
    }
}

/// Layer `over` (the more-specific config) onto `base` field-by-field: a field
/// set in `over` wins; an unset field falls back to `base`. Mirrors the
/// nearest-wins semantics [`ShadowDroidConfig::merge`] applies to the top level.
fn merge_proxy(base: Option<ProxyConfig>, over: Option<ProxyConfig>) -> Option<ProxyConfig> {
    match (base, over) {
        (base, None) => base,
        (None, over) => over,
        (Some(mut b), Some(o)) => {
            b.ca_cert = o.ca_cert.or(b.ca_cert);
            b.ca_key = o.ca_key.or(b.ca_key);
            b.ca_trusted = o.ca_trusted.or(b.ca_trusted);
            b.port = o.port.or(b.port);
            if !o.hosts.is_empty() {
                b.hosts = o.hosts;
            }
            b.trust_store = o.trust_store.or(b.trust_store);
            b.verify_upstream = o.verify_upstream.or(b.verify_upstream);
            b.anticache = o.anticache.or(b.anticache);
            b.anticomp = o.anticomp.or(b.anticomp);
            b.redact = o.redact.or(b.redact);
            Some(b)
        }
    }
}

pub fn user_config_path() -> Result<PathBuf> {
    home_dir().map(|home| home.join(USER_CONFIG_REL))
}

/// The project config file ShadowDroid writes: the folder form in the current
/// directory (`./.shadowdroid/config.json`).
pub fn project_config_path() -> Result<PathBuf> {
    Ok(std::env::current_dir()
        .context("resolve current directory")?
        .join(PROJECT_CONFIG_REL))
}

/// The project `.shadowdroid/` directory that owns companion files (the proxy
/// CA and its gitignore): the nearest ancestor that already has one, else the
/// current directory's `.shadowdroid/`. Used to locate a per-project convention
/// CA and to scope `net ca *--project`.
pub fn project_shadowdroid_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    Ok(project_shadowdroid_dir_in(&cwd))
}

fn project_shadowdroid_dir_in(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .map(|dir| dir.join(PROJECT_CONFIG_DIR))
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| cwd.join(PROJECT_CONFIG_DIR))
}

/// The `.gitignore` entries that keep proxy CA secrets — and the `.bak` backups
/// `net ca import`/`reset` leave behind — out of version control, while leaving
/// `config.json` committable.
pub const GITIGNORE_LINES: &[&str] = &["ca.crt", "ca.key", "ca.*.bak", "ca.source"];

/// Idempotently ensure `<dir>/.gitignore` ignores the proxy CA material. Creates
/// `dir` and the file as needed, appends only the missing lines, and never lists
/// `config.json`. Returns the lines that were added (empty if already present).
pub fn ensure_shadowdroid_gitignore(dir: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let present: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<&str> = GITIGNORE_LINES
        .iter()
        .copied()
        .filter(|line| !present.contains(line))
        .collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = existing.clone();
    if out.is_empty() {
        out.push_str(
            "# ShadowDroid — proxy CA material and import backups (secrets; do not commit)\n",
        );
    } else if !out.ends_with('\n') {
        out.push('\n');
    }
    for line in &missing {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(&path, &out).with_context(|| format!("write {}", path.display()))?;
    Ok(missing.iter().map(|s| s.to_string()).collect())
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
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let user = user_config_path()?;
    Ok(discover_config_paths_in(&cwd, &user))
}

/// Ordered list of config files to layer, least- to most-specific: the
/// user/global config first, then each ancestor directory's
/// `.shadowdroid/config.json` from filesystem root down to `cwd` (nearest wins
/// because it is applied last).
///
/// Any ancestor file equal to `user_cfg` is skipped, so the user config is not
/// merged twice when `cwd` lives under `$HOME` (its `.shadowdroid/config.json`
/// *is* the user config).
fn discover_config_paths_in(cwd: &Path, user_cfg: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if user_cfg.is_file() {
        paths.push(user_cfg.to_path_buf());
    }
    let mut project_paths = cwd
        .ancestors()
        .map(|dir| dir.join(PROJECT_CONFIG_REL))
        .filter(|path| path.is_file() && path != user_cfg)
        .collect::<Vec<_>>();
    project_paths.reverse();
    paths.extend(project_paths);
    paths
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

    #[test]
    fn discovery_layers_ancestors_and_dedupes_user_cfg() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let a = root.join("a");
        let b = a.join("b");
        std::fs::create_dir_all(&b).unwrap();

        for dir in [root, &a, &b] {
            std::fs::create_dir_all(dir.join(PROJECT_CONFIG_DIR)).unwrap();
            std::fs::write(dir.join(PROJECT_CONFIG_REL), "{}").unwrap();
        }

        // The user config lives at a/.shadowdroid/config.json — the ancestor walk
        // must not re-add it as a "project" file.
        let user_cfg = a.join(PROJECT_CONFIG_REL);
        let paths = discover_config_paths_in(&b, &user_cfg);
        assert_eq!(
            paths,
            vec![
                user_cfg.clone(),           // user config, applied first (least specific)
                root.join(PROJECT_CONFIG_REL),
                b.join(PROJECT_CONFIG_REL), // a skipped (== user_cfg); nearest (b) last
            ]
        );
    }

    #[test]
    fn gitignore_writer_is_idempotent_and_keeps_config_committable() {
        let tmp = tempfile::tempdir().unwrap();
        let sd = tmp.path().join(PROJECT_CONFIG_DIR);

        let added = ensure_shadowdroid_gitignore(&sd).unwrap();
        assert!(added.contains(&"ca.key".to_string()));
        let path = sd.join(".gitignore");
        let text = std::fs::read_to_string(&path).unwrap();
        for needle in GITIGNORE_LINES {
            assert!(text.contains(needle), "missing {needle}");
        }
        assert!(!text.contains("config.json"), "config.json must stay committable");

        // Re-running adds nothing and doesn't duplicate.
        assert!(ensure_shadowdroid_gitignore(&sd).unwrap().is_empty());
        assert_eq!(text, std::fs::read_to_string(&path).unwrap());

        // Pre-existing unrelated content is preserved.
        std::fs::write(&path, "# mine\nbuild/\n").unwrap();
        ensure_shadowdroid_gitignore(&sd).unwrap();
        let merged = std::fs::read_to_string(&path).unwrap();
        assert!(merged.contains("# mine") && merged.contains("build/"));
        assert!(merged.contains("ca.key"));
    }

    #[test]
    fn merge_proxy_nearest_wins_field_by_field() {
        let base = ProxyConfig {
            ca_trusted: Some(false),
            port: Some(8080),
            redact: Some(true),
            ..Default::default()
        };
        let over = ProxyConfig {
            ca_trusted: Some(true),
            hosts: vec!["*.example.com".into()],
            ..Default::default()
        };
        let merged = merge_proxy(Some(base), Some(over)).unwrap();
        assert_eq!(merged.ca_trusted, Some(true)); // over wins
        assert_eq!(merged.port, Some(8080)); // base kept where over is unset
        assert_eq!(merged.redact, Some(true));
        assert_eq!(merged.hosts, vec!["*.example.com".to_string()]);

        assert!(merge_proxy(None, None).is_none());
        assert_eq!(
            merge_proxy(
                Some(ProxyConfig {
                    port: Some(1),
                    ..Default::default()
                }),
                None
            )
            .unwrap()
            .port,
            Some(1)
        );
    }
}
