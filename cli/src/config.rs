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

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
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
    /// Default named device target. A target resolves to either a stable AVD
    /// name or a physical-device serial; unlike `device`, an AVD target can be
    /// started on demand and does not persist an ephemeral emulator serial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_target: Option<String>,
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
    /// Project/user-defined device targets such as `mobile` and `tv`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub targets: BTreeMap<String, DeviceTargetConfig>,

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
    #[serde(deserialize_with = "deserialize_android_package")]
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_configuration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debugger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_mode: Option<String>,
    /// Named device target for this app alias. It becomes the implicit target
    /// when the alias is also the config's default `app`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeviceTargetConfig {
    /// Stable Android Virtual Device name (`emulator -list-avds`). Mutually
    /// exclusive with `serial`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avd: Option<String>,
    /// Stable physical/remote adb serial. Mutually exclusive with `avd` and is
    /// never auto-started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// `never` (default) or `if-needed`. Auto-start is deliberately opt-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<TargetStartPolicy>,
    /// Optional target assertion checked after resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_factor: Option<TargetFormFactor>,
    /// Start the AVD without loading a snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_boot: Option<bool>,
    /// Maximum time to wait for an AVD to become adb-online and finish booting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum TargetStartPolicy {
    Never,
    IfNeeded,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum TargetFormFactor {
    Mobile,
    Tv,
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

    /// Resolve the implicit named target. An app-alias target is more specific
    /// than the top-level default because it lets one project model mobile/TV
    /// variants without guessing from Gradle flavor names.
    pub fn implicit_target(&self) -> Option<&str> {
        self.app
            .as_deref()
            .and_then(|app| self.app_config(app))
            .and_then(|entry| entry.target.as_deref())
            .or(self.default_target.as_deref())
    }

    pub fn target(&self, name: &str) -> Option<(&str, &DeviceTargetConfig)> {
        self.targets
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(name, target)| (name.as_str(), target))
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
        self.default_target = other.default_target.or(self.default_target.take());
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
        self.targets.extend(other.targets);
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
    let user_dir = user_config_path()?
        .parent()
        .map(Path::to_path_buf)
        .context("user config path has no parent")?;
    Ok(project_shadowdroid_dir_in(&cwd, &user_dir))
}

fn project_shadowdroid_dir_in(cwd: &Path, user_dir: &Path) -> PathBuf {
    cwd.ancestors()
        .map(|dir| dir.join(PROJECT_CONFIG_DIR))
        // `$HOME/.shadowdroid` is the user store, not a project marker. Without
        // this exclusion every repository below `$HOME` inherits the user
        // directory as its "project" CA location.
        .find(|dir| dir != user_dir && dir.is_dir())
        .unwrap_or_else(|| cwd.join(PROJECT_CONFIG_DIR))
}

/// The `.gitignore` entries that keep proxy CA secrets — and the `.bak` backups
/// `net ca import`/`reset` leave behind — out of version control, while leaving
/// `config.json` committable.
pub const GITIGNORE_LINES: &[&str] = &["ca.crt", "ca.key", "ca.*.bak", "ca.source", ".ca.lock"];

/// Idempotently ensure `<dir>/.gitignore` ignores the proxy CA material. Creates
/// `dir` and the file as needed, appends only the missing lines, and never lists
/// `config.json`. Returns the lines that were added (empty if already present).
pub fn ensure_shadowdroid_gitignore(dir: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
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
    let text = std::fs::read_to_string(path).map_err(|error| {
        crate::diagnostic::DiagnosticError::new(
            "config_read",
            "config",
            format!("failed to read ShadowDroid config {}", path.display()),
        )
        .detail(serde_json::json!({
            "path": path.display().to_string(),
            "error": error.to_string(),
        }))
        .next_actions([
            "shadowdroid config paths --json",
            "check the config file permissions and retry",
        ])
    })?;
    serde_json::from_str(&text).map_err(|error: serde_json::Error| {
        crate::diagnostic::DiagnosticError::new(
            "config_parse",
            "config",
            format!("invalid ShadowDroid config {}", path.display()),
        )
        .detail(serde_json::json!({
            "path": path.display().to_string(),
            "line": error.line(),
            "column": error.column(),
            "error": error.to_string(),
        }))
        .next_actions([
            "shadowdroid config validate --json",
            "shadowdroid config schema --json",
        ])
        .into()
    })
}

pub fn write_config_file(path: &Path, config: &ShadowDroidConfig) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let mut text = serde_json::to_string_pretty(config)?;
    text.push('\n');
    let existing_permissions = std::fs::metadata(path).ok().map(|meta| meta.permissions());

    // Never truncate the live config before the replacement is complete. The
    // temporary file lives in the same directory, so `persist` is a same-volume
    // atomic rename on supported platforms. Sync both contents and directory
    // metadata so a reported success survives a host crash as far as the OS can
    // guarantee.
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary config in {}", parent.display()))?;
    temp.write_all(text.as_bytes())
        .with_context(|| format!("write temporary config for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("flush temporary config for {}", path.display()))?;
    if let Some(permissions) = existing_permissions {
        temp.as_file()
            .set_permissions(permissions)
            .with_context(|| format!("preserve config permissions for {}", path.display()))?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let user_only = user_config_path().is_ok_and(|user| user == path);
            let mode = if user_only { 0o600 } else { 0o644 };
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("set config permissions for {}", path.display()))?;
        }
    }
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync temporary config for {}", path.display()))?;
    let file = temp.persist(path).map_err(|err| {
        anyhow!(
            "atomically replace {} with temporary config: {}",
            path.display(),
            err.error
        )
    })?;
    file.sync_all()
        .with_context(|| format!("sync config {}", path.display()))?;
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
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
    validate_android_package(value).is_ok()
}

/// Validate an Android application id before it can cross a device-shell
/// boundary. User application ids contain at least two dot-separated Java
/// identifiers; every segment starts with an ASCII letter and then contains
/// only letters, digits, or `_`. The platform package `android` is the one
/// intentional single-segment exception.
pub fn validate_android_package(value: &str) -> Result<()> {
    if value == "android" {
        return Ok(());
    }
    validate_qualified_identifier(value, "Android package", true, false)
}

/// Validate a runtime permission name such as `android.permission.CAMERA` or a
/// custom app permission. This intentionally accepts upper-case segments but
/// rejects whitespace and every shell metacharacter.
pub fn validate_android_permission(value: &str) -> Result<()> {
    validate_qualified_identifier(value, "Android permission", true, true)
}

/// Validate an app-op identifier. Android accepts debug names such as `CAMERA`,
/// public string names such as `android:camera`, and numeric operation ids.
pub fn validate_android_app_op(value: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value {
        return Err(invalid_identifier_error(
            "invalid_android_app_op",
            "Android app-op must not be empty or contain surrounding whitespace",
            value,
            "use an app-op such as CAMERA, android:camera, or a numeric id",
        ));
    }
    if value.chars().all(|c| c.is_ascii_digit()) {
        return Ok(());
    }
    if value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
    {
        return Ok(());
    }
    Err(invalid_identifier_error(
        "invalid_android_app_op",
        format!("invalid Android app-op {value:?}; expected a literal app-op token or numeric id"),
        value,
        "use an app-op such as CAMERA, android:camera, or a numeric id",
    ))
}

/// Validate an app-op mode without freezing ShadowDroid to a specific Android
/// release's enum. Safe lower-case tokens are accepted; shell separators,
/// substitutions, quotes, and whitespace are not.
pub fn validate_android_app_op_mode(value: &str) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || !value.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        || !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(invalid_identifier_error(
            "invalid_android_app_op_mode",
            format!("invalid Android app-op mode {value:?}; expected a lower-case mode token"),
            value,
            "use a lower-case mode such as allow, deny, ignore, default, or foreground",
        ));
    }
    Ok(())
}

/// Quote one argument for Android's POSIX-like device shell. Callers should
/// still validate semantic identifiers first; quoting is the second barrier
/// that keeps future grammar relaxations from becoming command injection.
pub fn quote_device_shell_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn validate_qualified_identifier(
    value: &str,
    kind: &str,
    require_multiple_segments: bool,
    allow_underscore_start: bool,
) -> Result<()> {
    let code = if kind == "Android package" {
        "invalid_android_package"
    } else {
        "invalid_android_permission"
    };
    let next = if kind == "Android package" {
        "use a literal Android package such as com.example.app"
    } else {
        "use a qualified permission such as android.permission.CAMERA"
    };
    if value.is_empty() || value.trim() != value {
        return Err(invalid_identifier_error(
            code,
            format!("{kind} must not be empty or contain surrounding whitespace"),
            value,
            next,
        ));
    }
    let segments = value.split('.').collect::<Vec<_>>();
    if require_multiple_segments && segments.len() < 2 {
        return Err(invalid_identifier_error(
            code,
            format!("invalid {kind} {value:?}; expected a qualified name"),
            value,
            next,
        ));
    }
    for segment in segments {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return Err(invalid_identifier_error(
                code,
                format!("invalid {kind} {value:?}; name segments must not be empty"),
                value,
                next,
            ));
        };
        if !(first.is_ascii_alphabetic() || (allow_underscore_start && first == '_'))
            || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(invalid_identifier_error(
                code,
                format!(
                    "invalid {kind} {value:?}; only dot-separated ASCII identifiers are allowed"
                ),
                value,
                next,
            ));
        }
    }
    Ok(())
}

fn invalid_identifier_error(
    code: &str,
    message: impl Into<String>,
    value: &str,
    next_action: &str,
) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(code, "input", message)
        .detail(serde_json::json!({"value": value}))
        .next_actions([next_action])
        .into()
}

fn deserialize_android_package<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_android_package(&value).map_err(serde::de::Error::custom)?;
    Ok(value)
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
        if let Some(rest) = raw.strip_prefix("~/")
            && let Ok(home) = home_dir()
        {
            return home.join(rest);
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
            default_target: Some("fallback".into()),
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
            default_target: Some("mobile".into()),
            project: Some("/work/Livd".into()),
            ..Default::default()
        };
        project.apps.insert(
            "Livd".into(),
            AppConfig {
                package: "com.livd".into(),
                target: Some("tv".into()),
                ..Default::default()
            },
        );
        project.targets.insert(
            "mobile".into(),
            DeviceTargetConfig {
                avd: Some("Livd_Pixel".into()),
                ..Default::default()
            },
        );
        project.targets.insert(
            "tv".into(),
            DeviceTargetConfig {
                avd: Some("Livd_TV".into()),
                form_factor: Some(TargetFormFactor::Tv),
                ..Default::default()
            },
        );

        user.merge(project);

        assert_eq!(user.device.as_deref(), Some("emulator-1"));
        assert_eq!(user.app.as_deref(), Some("Livd"));
        assert_eq!(user.default_target.as_deref(), Some("mobile"));
        assert_eq!(user.implicit_target(), Some("tv"));
        assert_eq!(
            user.target("TV")
                .and_then(|(_, target)| target.avd.as_deref()),
            Some("Livd_TV")
        );
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
        assert!(!looks_like_package("com.example;id"));
        assert!(!looks_like_package("com.example\nother"));
        assert!(!looks_like_package("com.$(id)"));
        assert!(!looks_like_package("com.'example'"));
        assert!(!looks_like_package("1com.example"));
        assert!(!looks_like_package("com..example"));
    }

    #[test]
    fn config_deserialization_rejects_shell_metacharacters_in_packages() {
        for package in [
            "com.example;id",
            "com.example\nother",
            "com.$(id)",
            "com.'example'",
            "com.\"example\"",
        ] {
            let value = serde_json::json!({
                "apps": {"Example": {"package": package}}
            });
            let err = serde_json::from_value::<ShadowDroidConfig>(value).unwrap_err();
            assert!(
                err.to_string().contains("invalid Android package"),
                "unexpected error for {package:?}: {err}"
            );
        }
    }

    #[test]
    fn validates_permission_appop_and_mode_tokens() {
        assert!(validate_android_permission("android.permission.CAMERA").is_ok());
        assert!(validate_android_permission("com.example.permission.READ_THING_2").is_ok());
        assert!(validate_android_permission("android.permission.CAMERA;id").is_err());
        assert!(validate_android_permission("android.permission.$(id)").is_err());
        assert!(validate_android_permission("android.permission.CAMERA\nNEXT").is_err());

        assert!(validate_android_app_op("CAMERA").is_ok());
        assert!(validate_android_app_op("android:camera").is_ok());
        assert!(validate_android_app_op("42").is_ok());
        assert!(validate_android_app_op("CAMERA;id").is_err());
        assert!(validate_android_app_op("$(id)").is_err());

        assert!(validate_android_app_op_mode("foreground").is_ok());
        assert!(validate_android_app_op_mode("allow_once").is_ok());
        assert!(validate_android_app_op_mode("allow;id").is_err());
        assert!(validate_android_app_op_mode("$(id)").is_err());
    }

    #[test]
    fn shell_arguments_are_single_quoted() {
        assert_eq!(quote_device_shell_arg("com.example"), "'com.example'");
        assert_eq!(
            quote_device_shell_arg("a'b;$(id)\nnext"),
            "'a'\"'\"'b;$(id)\nnext'"
        );
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
                user_cfg.clone(), // user config, applied first (least specific)
                root.join(PROJECT_CONFIG_REL),
                b.join(PROJECT_CONFIG_REL), // a skipped (== user_cfg); nearest (b) last
            ]
        );
    }

    #[test]
    fn project_shadowdroid_dir_never_selects_the_user_store() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = home.join("work/repo");
        let user_dir = home.join(PROJECT_CONFIG_DIR);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&user_dir).unwrap();

        assert_eq!(
            project_shadowdroid_dir_in(&repo, &user_dir),
            repo.join(PROJECT_CONFIG_DIR)
        );

        let workspace_dir = home.join("work").join(PROJECT_CONFIG_DIR);
        std::fs::create_dir_all(&workspace_dir).unwrap();
        assert_eq!(project_shadowdroid_dir_in(&repo, &user_dir), workspace_dir);
    }

    #[test]
    fn config_write_atomically_replaces_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(PROJECT_CONFIG_REL);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"device\":\"old\"}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let config = ShadowDroidConfig {
            device: Some("emulator-5554".into()),
            ..Default::default()
        };

        write_config_file(&path, &config).unwrap();

        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.device.as_deref(), Some("emulator-5554"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        let entries = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from("config.json")]);
    }

    #[test]
    fn malformed_config_reports_a_typed_location_and_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{\n  \"app\":\n}\n").unwrap();

        let error = parse_config_file(&path).unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.code, "config_parse");
        assert_eq!(diagnostic.stage, "config");
        assert_eq!(diagnostic.detail["path"], path.display().to_string());
        assert_eq!(diagnostic.detail["line"], 3);
        assert!(diagnostic.detail["column"].as_u64().unwrap() > 0);
        assert_eq!(
            diagnostic.next_actions[0],
            "shadowdroid config validate --json"
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
        assert!(
            !text.contains("config.json"),
            "config.json must stay committable"
        );

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
