//! Agent-facing config helpers.
//!
//! The runtime loader lives in [crate::config]. This command exposes the same
//! shape as a discoverable CLI surface so agents can generate, validate, and
//! inspect config without reading Rust source.

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::Path;

use crate::config as cfg;
use crate::config::{
    AppConfig, ProxyConfig, RedactionConfig, ShadowDroidConfig, TargetFormFactor, TargetStartPolicy,
};

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub cmd: ConfigCmd,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print config paths, precedence, and currently loaded config files.
    Paths {
        /// Emit JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Print the machine-readable config schema and example.
    Schema {
        /// Emit JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Explain how config is loaded and how agents should use it.
    Explain {
        /// Emit JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Create or update a user/project config file from CLI values.
    Init(Box<ConfigInitArgs>),
    /// Parse and validate all discovered config files.
    Validate {
        /// Emit JSON instead of human text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct ConfigInitArgs {
    /// Write ~/.shadowdroid/config.json.
    #[arg(long, conflicts_with = "project")]
    pub user: bool,
    /// Write ./.shadowdroid/config.json. This is the default.
    #[arg(long, conflicts_with = "user")]
    pub project: bool,
    /// Default app alias agents can use, e.g. Livd.
    #[arg(long)]
    pub app: Option<String>,
    /// Package for --app, e.g. com.example.app.
    #[arg(long)]
    pub package: Option<String>,
    /// Default ADB serial.
    #[arg(long)]
    pub device: Option<String>,
    /// Default named target used when --device/--target are absent.
    #[arg(long, value_name = "NAME")]
    pub default_target: Option<String>,
    /// Target entry to create/update. Pair with --target-avd or --target-serial.
    #[arg(long, value_name = "NAME")]
    pub target_name: Option<String>,
    /// Stable AVD name for --target-name.
    #[arg(
        long,
        value_name = "AVD",
        requires = "target_name",
        conflicts_with = "target_serial"
    )]
    pub target_avd: Option<String>,
    /// Stable physical/remote adb serial for --target-name.
    #[arg(
        long,
        value_name = "SERIAL",
        requires = "target_name",
        conflicts_with = "target_avd"
    )]
    pub target_serial: Option<String>,
    /// Target startup policy. `if-needed` is valid only for AVD targets.
    #[arg(long, value_enum, requires = "target_name")]
    pub target_start: Option<TargetStartPolicy>,
    /// Assert the resolved target is a mobile or TV device.
    #[arg(long, value_enum, requires = "target_name")]
    pub target_form_factor: Option<TargetFormFactor>,
    /// Cold-boot the AVD when ShadowDroid starts it.
    #[arg(long, requires = "target_name")]
    pub target_cold_boot: bool,
    /// AVD boot timeout in seconds (10..=900).
    #[arg(long, requires = "target_name", value_name = "SECONDS")]
    pub target_boot_timeout: Option<u64>,
    /// Named target for --app. The alias must be created in the same command or already exist.
    #[arg(long, requires = "app", value_name = "NAME")]
    pub app_target: Option<String>,
    /// Default Android Studio project name or absolute path.
    #[arg(long, value_name = "PATH_OR_NAME")]
    pub project_path: Option<String>,
    /// Default Android Studio debugger bridge URL.
    #[arg(long)]
    pub studio_url: Option<String>,
    /// Android Studio installation path.
    #[arg(long, value_name = "PATH")]
    pub android_studio: Option<String>,
    /// Local ShadowDroid Studio plugin ZIP path.
    #[arg(long, value_name = "PATH")]
    pub studio_plugin: Option<String>,
    /// Android debugger id/display name.
    #[arg(long)]
    pub debugger: Option<String>,
    /// Default debugger mode: auto, java, native, or mixed.
    #[arg(long, value_name = "MODE")]
    pub debug_mode: Option<String>,
    /// Android Studio run configuration whose debugger settings should be reused.
    #[arg(long)]
    pub run_configuration: Option<String>,
    /// Proxy signing CA certificate path (absolute or ~/). Sets proxy.ca_cert.
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<String>,
    /// Proxy signing CA private key path (absolute or ~/). Sets proxy.ca_key.
    #[arg(long, value_name = "PATH")]
    pub ca_key: Option<String>,
    /// Assert the proxy CA is already trusted on the device (skip install +
    /// readback). Sets proxy.ca_trusted.
    #[arg(long)]
    pub ca_trusted: bool,
    /// Replace an existing app alias if it points at a different package.
    #[arg(long)]
    pub force: bool,
    /// Emit JSON instead of human text.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &ConfigArgs) -> Result<()> {
    match &args.cmd {
        ConfigCmd::Paths { json: as_json } => print_value(paths_value()?, *as_json),
        ConfigCmd::Schema { json: as_json } => print_value(schema_value(), *as_json),
        ConfigCmd::Explain { json: as_json } => print_value(explain_value()?, *as_json),
        ConfigCmd::Init(args) => init_config(args),
        ConfigCmd::Validate { json: as_json } => {
            let report = validate_value()?;
            if report.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "config_invalid",
                    "config",
                    "one or more ShadowDroid config files are invalid",
                )
                .detail(report)
                .next_actions([
                    "fix the first entry in detail.files whose ok field is false",
                    "rerun `shadowdroid config validate --json`",
                ])
                .into());
            }
            print_value(report, *as_json)
        }
    }
}

fn init_config(args: &ConfigInitArgs) -> Result<()> {
    let scope = if args.user { "user" } else { "project" };
    let path = if args.user {
        cfg::user_config_path()?
    } else {
        cfg::project_config_path()?
    };
    let mut config = if path.is_file() {
        cfg::parse_config_file(&path)?
    } else {
        ShadowDroidConfig::default()
    };
    let mut changed = Vec::new();

    apply_top_level(&mut config.device, &args.device, "device", &mut changed);
    apply_top_level(
        &mut config.default_target,
        &args.default_target,
        "default_target",
        &mut changed,
    );
    apply_top_level(
        &mut config.studio_url,
        &args.studio_url,
        "studio_url",
        &mut changed,
    );
    apply_top_level(
        &mut config.android_studio,
        &args.android_studio,
        "android_studio",
        &mut changed,
    );
    apply_top_level(
        &mut config.studio_plugin,
        &args.studio_plugin,
        "studio_plugin",
        &mut changed,
    );
    apply_top_level(
        &mut config.project,
        &args.project_path,
        "project",
        &mut changed,
    );

    if let Some(app) = args.app.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        config.app = Some(app.to_string());
        changed.push("app".to_string());
    }

    match (args.app.as_deref(), args.package.as_deref()) {
        (Some(app), Some(package)) => {
            let app = app.trim();
            let package = package.trim();
            if app.is_empty() || package.is_empty() {
                bail!("--app and --package must not be empty");
            }
            upsert_app_config(
                &mut config,
                app,
                package,
                AppConfigUpdate {
                    run_configuration: &args.run_configuration,
                    debugger: &args.debugger,
                    debug_mode: &args.debug_mode,
                    target: &args.app_target,
                },
                args.force,
            )?;
            changed.push(format!("apps.{app}"));
        }
        (None, Some(package)) => {
            let package = package.trim();
            if package.is_empty() {
                bail!("--package must not be empty");
            }
            cfg::validate_android_package(package)?;
            config.app = Some(package.to_string());
            changed.push("app".to_string());
        }
        _ => {}
    }

    if args.package.is_none()
        && let (Some(app), Some(target)) = (args.app.as_deref(), args.app_target.as_deref())
    {
        let (_, entry) = config
            .apps
            .iter_mut()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(app.trim()))
            .ok_or_else(|| {
                anyhow!(
                    "app alias `{}` does not exist; pass --package to create it",
                    app.trim()
                )
            })?;
        entry.target = Some(target.trim().to_string());
        changed.push(format!("apps.{}.target", app.trim()));
    }

    if let Some(name) = args
        .target_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let key = config
            .targets
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(name))
            .cloned()
            .unwrap_or_else(|| name.to_string());
        let entry = config.targets.entry(key).or_default();
        if let Some(avd) = args
            .target_avd
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            if entry.serial.is_some() && !args.force {
                bail!(
                    "target `{name}` is serial-bound; pass --force to replace it with AVD `{avd}`"
                );
            }
            entry.serial = None;
            entry.avd = Some(avd.to_string());
            changed.push(format!("targets.{name}.avd"));
        }
        if let Some(serial) = args
            .target_serial
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            if entry.avd.is_some() && !args.force {
                bail!(
                    "target `{name}` is AVD-bound; pass --force to replace it with serial `{serial}`"
                );
            }
            entry.avd = None;
            entry.serial = Some(serial.to_string());
            changed.push(format!("targets.{name}.serial"));
        }
        if let Some(start) = args.target_start {
            entry.start = Some(start);
            changed.push(format!("targets.{name}.start"));
        }
        if let Some(form_factor) = args.target_form_factor {
            entry.form_factor = Some(form_factor);
            changed.push(format!("targets.{name}.form_factor"));
        }
        if args.target_cold_boot {
            entry.cold_boot = Some(true);
            changed.push(format!("targets.{name}.cold_boot"));
        }
        if let Some(timeout) = args.target_boot_timeout {
            entry.boot_timeout_seconds = Some(timeout);
            changed.push(format!("targets.{name}.boot_timeout_seconds"));
        }
    } else if args.target_name.is_some() {
        bail!("--target-name must not be empty");
    }

    if args.app.is_none() {
        apply_top_level(
            &mut config.run_configuration,
            &args.run_configuration,
            "run_configuration",
            &mut changed,
        );
        apply_top_level(
            &mut config.debugger,
            &args.debugger,
            "debugger",
            &mut changed,
        );
        apply_top_level(
            &mut config.debug_mode,
            &args.debug_mode,
            "debug_mode",
            &mut changed,
        );
    }

    // Proxy CA fields. Preserve an existing proxy block even when this run does
    // not touch it, so re-running `config init` never drops proxy settings.
    if args.ca_cert.is_some() || args.ca_key.is_some() || args.ca_trusted || config.proxy.is_some()
    {
        let mut proxy: ProxyConfig = config.proxy.take().unwrap_or_default();
        if let Some(v) = args
            .ca_cert
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            proxy.ca_cert = Some(v.to_string());
            changed.push("proxy.ca_cert".to_string());
        }
        if let Some(v) = args
            .ca_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            proxy.ca_key = Some(v.to_string());
            changed.push("proxy.ca_key".to_string());
        }
        if args.ca_trusted {
            proxy.ca_trusted = Some(true);
            changed.push("proxy.ca_trusted".to_string());
        }
        config.proxy = Some(proxy);
    }

    let mut validation_errors = Vec::new();
    let mut validation_warnings = Vec::new();
    validate_config(
        &path,
        &config,
        &mut validation_errors,
        &mut validation_warnings,
    );
    if !validation_errors.is_empty() {
        bail!(
            "refusing to write invalid config:\n{}",
            validation_errors.join("\n")
        );
    }

    cfg::write_config_file(&path, &config)?;

    // For project scope the folder lives inside the repo, so keep the CA secrets
    // out of git. User scope is under $HOME (not a repo) — nothing to ignore.
    let gitignore_added = match path.parent() {
        Some(dir) if !args.user => cfg::ensure_shadowdroid_gitignore(dir)?,
        _ => Vec::new(),
    };

    let value = json!({
        "type": "shadowdroid_config_init",
        "ok": true,
        "scope": scope,
        "path": path.display().to_string(),
        "changed": changed,
        "gitignore_added": gitignore_added,
        "config": config,
        "next_actions": [
            "shadowdroid config validate --json",
            "shadowdroid debug auto"
        ],
    });
    print_value(value, args.json)
}

/// Update only values explicitly supplied by this invocation. In particular,
/// re-running `config init --app ... --package ...` must not erase the alias's
/// project/debugger/run-configuration fields.
struct AppConfigUpdate<'a> {
    run_configuration: &'a Option<String>,
    debugger: &'a Option<String>,
    debug_mode: &'a Option<String>,
    target: &'a Option<String>,
}

fn upsert_app_config(
    config: &mut ShadowDroidConfig,
    app: &str,
    package: &str,
    update: AppConfigUpdate<'_>,
    force: bool,
) -> Result<()> {
    cfg::validate_android_package(package)?;
    let key = config
        .apps
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(app))
        .cloned()
        .unwrap_or_else(|| app.to_string());
    let entry = config.apps.entry(key).or_insert_with(|| AppConfig {
        package: package.to_string(),
        ..Default::default()
    });
    if entry.package != package && !force {
        bail!(
            "app alias `{}` already points at `{}`. Pass --force to replace it with `{}`.",
            app,
            entry.package,
            package
        );
    }
    entry.package = package.to_string();
    let mut ignored_changes = Vec::new();
    apply_top_level(
        &mut entry.run_configuration,
        update.run_configuration,
        "run_configuration",
        &mut ignored_changes,
    );
    apply_top_level(
        &mut entry.debugger,
        update.debugger,
        "debugger",
        &mut ignored_changes,
    );
    apply_top_level(
        &mut entry.debug_mode,
        update.debug_mode,
        "debug_mode",
        &mut ignored_changes,
    );
    apply_top_level(
        &mut entry.target,
        update.target,
        "target",
        &mut ignored_changes,
    );
    Ok(())
}

fn apply_top_level(
    slot: &mut Option<String>,
    value: &Option<String>,
    name: &str,
    changed: &mut Vec<String>,
) {
    if let Some(value) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        *slot = Some(value.to_string());
        changed.push(name.to_string());
    }
}

fn paths_value() -> Result<Value> {
    let user = cfg::user_config_path()?;
    let project = cfg::project_config_path()?;
    let loaded = cfg::discovered_config_paths()?;
    Ok(json!({
        "type": "shadowdroid_config_paths",
        "format": "json",
        "user_config": user.display().to_string(),
        "project_config": project.display().to_string(),
        "loaded": loaded
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        "precedence": [
            "~/.shadowdroid/config.json",
            ".shadowdroid/config.json from ancestors, root to current directory",
            "explicit CLI flags"
        ],
        "project_config_wins_over_user": true,
    }))
}

fn schema_value() -> Value {
    json!({
        "type": "shadowdroid_config_schema",
        "format": "json",
        "user_config": "~/.shadowdroid/config.json",
        "project_config": ".shadowdroid/config.json",
        "precedence": [
            "user config",
            "project configs from ancestors, root to current directory",
            "explicit CLI flags"
        ],
        "fields": {
            "device": {"type": "string", "optional": true, "description": "Legacy/direct default ADB serial. Prefer default_target + targets for emulator projects."},
            "default_target": {"type": "string", "optional": true, "description": "Default named device target. An app alias target takes precedence when that alias is the default app."},
            "app": {"type": "string", "optional": true, "description": "Default app alias, package, or installed app name."},
            "project": {"type": "string", "optional": true, "description": "Android Studio project name or absolute project path."},
            "studio_url": {"type": "string", "optional": true, "description": "Android Studio debugger bridge URL."},
            "android_studio": {"type": "string", "optional": true, "description": "Android Studio installation path."},
            "studio_plugin": {"type": "string", "optional": true, "description": "Local Studio plugin ZIP path."},
            "debugger": {"type": "string", "optional": true, "description": "Default Android debugger id/display name."},
            "debug_mode": {"type": "string", "optional": true, "enum": ["auto", "java", "native", "mixed"], "description": "Default semantic debugger mode."},
            "run_configuration": {"type": "string", "optional": true, "description": "Default Android Studio run configuration."},
            "usage_log": {"type": "boolean", "optional": true, "description": "Opt-in local usage log (verb, duration, error code — never argument values) at ~/.shadowdroid/usage.jsonl; see `shadowdroid usage`."},
            "redaction": {
                "type": "object",
                "optional": true,
                "description": "Cross-command output, capture, and diagnostic-artifact redaction policy.",
                "properties": {"$ref": "#/redaction_entry"}
            },
            "proxy": {
                "type": "object",
                "optional": true,
                "description": "MITM proxy (`net`) defaults. Requires a CLI new enough to know this block (older CLIs reject unknown fields).",
                "properties": {"$ref": "#/proxy_entry"}
            },
            "apps": {
                "type": "object",
                "optional": true,
                "description": "Map of app aliases to package/debugger defaults.",
                "additional_properties": {"$ref": "#/app_entry"}
            },
            "targets": {
                "type": "object",
                "optional": true,
                "description": "Named project device targets. Each binds exactly one stable AVD name or physical adb serial.",
                "additional_properties": {"$ref": "#/target_entry"}
            }
        },
        "proxy_entry": {
            "ca_cert": {"type": "string", "optional": true, "description": "Signing CA certificate path (absolute or ~/). Leave unset to use the per-project convention CA .shadowdroid/ca.{crt,key} or the global CA."},
            "ca_key": {"type": "string", "optional": true, "description": "Signing CA private key path (absolute or ~/); required when ca_cert is set."},
            "ca_trusted": {"type": "boolean", "optional": true, "description": "Assert the CA is already trusted on the device: net trust/net check skip the adb install + readback and report basis 'asserted'. Does not override the app's Network-Security-Config verdict."},
            "port": {"type": "integer", "optional": true, "description": "Default device-facing proxy port for net start (default 8080)."},
            "hosts": {"type": "array", "optional": true, "description": "Default host allowlist (globs) for net start/log/intercept."},
            "trust_store": {"type": "string", "optional": true, "enum": ["system", "user", "push", "ui"], "description": "Preferred device trust path for net trust. push stages the CA for manual Settings installation; ui is a legacy alias."},
            "verify_upstream": {"type": "boolean", "optional": true, "description": "Default for net start --verify-upstream."},
            "anticache": {"type": "boolean", "optional": true, "description": "Default for net start --anticache."},
            "anticomp": {"type": "boolean", "optional": true, "description": "Default for net start --anticomp."},
            "redact": {"type": "boolean", "optional": true, "description": "Default for net start --redact."}
        },
        "redaction_entry": {
            "enabled": {"type": "boolean", "optional": true, "default": false, "description": "Enable redaction for every supported command without passing --redact."},
            "json_keys": {"type": "array", "optional": true, "items": "string", "description": "Additional case/punctuation-insensitive JSON key names to redact."},
            "patterns": {"type": "array", "optional": true, "items": "string", "description": "Additional Rust-regex patterns replaced in string, UI, log, body, and artifact output."}
        },
        "app_entry": {
            "package": {"type": "string", "required": true, "description": "Android package/process name."},
            "project": {"type": "string", "optional": true, "description": "Project override for this app."},
            "run_configuration": {"type": "string", "optional": true, "description": "Run configuration override for this app."},
            "debugger": {"type": "string", "optional": true, "description": "Debugger override for this app."},
            "debug_mode": {"type": "string", "optional": true, "enum": ["auto", "java", "native", "mixed"], "description": "Debugger mode override for this app."},
            "target": {"type": "string", "optional": true, "description": "Named device target for this app alias. Used implicitly when this alias is the default app."}
        },
        "target_entry": {
            "avd": {"type": "string", "optional": true, "description": "Stable Android Virtual Device name from emulator -list-avds. Mutually exclusive with serial."},
            "serial": {"type": "string", "optional": true, "description": "Stable physical/remote adb serial. Mutually exclusive with avd and never auto-started."},
            "start": {"type": "string", "optional": true, "enum": ["never", "if-needed"], "default": "never", "description": "Whether ShadowDroid may start a missing AVD. Automatic startup is opt-in."},
            "form_factor": {"type": "string", "optional": true, "enum": ["mobile", "tv"], "description": "Post-resolution device assertion."},
            "cold_boot": {"type": "boolean", "optional": true, "description": "Start the AVD without loading a snapshot."},
            "boot_timeout_seconds": {"type": "integer", "optional": true, "minimum": 10, "maximum": 900, "default": 180, "description": "Maximum AVD boot wait."}
        },
        "example": example_config(),
        "recommended_agent_flow": [
            "shadowdroid config schema --json",
            "shadowdroid config init --project --app Example --package com.example.app --default-target mobile --target-name mobile --target-avd Project_Pixel_9 --target-start if-needed --target-form-factor mobile --project-path /path/to/project",
            "shadowdroid config validate --json",
            "shadowdroid connect"
        ]
    })
}

fn explain_value() -> Result<Value> {
    let mut value = schema_value();
    value["type"] = json!("shadowdroid_config_explain");
    value["paths"] = paths_value()?;
    value["usage"] = json!({
        "why": "Config stores repeated app/device/project/debugger parameters so agents can spend fewer tokens per command.",
        "safe_generation": "Prefer config init for simple files. If writing JSON directly, run config validate --json before relying on it.",
        "minimal_project_config": {
            "app": "Example",
            "default_target": "mobile",
            "apps": {
                "Example": {
                    "package": "com.example.app",
                    "target": "mobile"
                }
            },
            "targets": {
                "mobile": {
                    "avd": "Project_Pixel_9",
                    "start": "if-needed",
                    "form_factor": "mobile"
                }
            }
        }
    });
    Ok(value)
}

fn validate_value() -> Result<Value> {
    let paths = cfg::discovered_config_paths()?;
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if paths.is_empty() {
        warnings.push("no ShadowDroid config files found".to_string());
    }

    for path in paths {
        match cfg::parse_config_file(&path) {
            Ok(config) => {
                validate_config(&path, &config, &mut errors, &mut warnings);
                files.push(json!({
                    "path": path.display().to_string(),
                    "ok": true,
                    "config": config,
                }));
            }
            Err(err) => {
                if let Some(diagnostic) = err
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<crate::diagnostic::DiagnosticError>())
                {
                    let source_error = diagnostic
                        .detail
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or(&diagnostic.message);
                    let location = match (
                        diagnostic.detail.get("line").and_then(Value::as_u64),
                        diagnostic.detail.get("column").and_then(Value::as_u64),
                    ) {
                        (Some(line), Some(column)) => format!(":{line}:{column}"),
                        _ => String::new(),
                    };
                    errors.push(format!("{}{}: {}", path.display(), location, source_error));
                    files.push(json!({
                        "path": path.display().to_string(),
                        "ok": false,
                        "code": &diagnostic.code,
                        "error": &diagnostic.message,
                        "detail": &diagnostic.detail,
                        "next_actions": &diagnostic.next_actions,
                    }));
                } else {
                    errors.push(format!("{}: {err}", path.display()));
                    files.push(json!({
                        "path": path.display().to_string(),
                        "ok": false,
                        "error": err.to_string(),
                    }));
                }
            }
        }
    }

    let merged = if errors.is_empty() {
        let merged = ShadowDroidConfig::load()?;
        validate_target_references(&merged, &mut errors);
        Some(merged)
    } else {
        None
    };
    Ok(json!({
        "type": "shadowdroid_config_validate",
        "ok": errors.is_empty(),
        "files": files,
        "warnings": warnings,
        "errors": errors,
        "merged": merged,
    }))
}

fn validate_config(
    path: &Path,
    config: &ShadowDroidConfig,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if let Some(app) = config.app.as_deref() {
        let configured = config
            .apps
            .iter()
            .any(|(alias, entry)| alias.eq_ignore_ascii_case(app) || entry.package == app);
        if !configured && cfg::validate_android_package(app).is_err() {
            warnings.push(format!(
                "{}: default app `{}` is not an app alias or package; it will require device lookup",
                path.display(),
                app
            ));
        }
    }
    if let Some(mode) = config.debug_mode.as_deref() {
        validate_debug_mode(path, "debug_mode", mode, errors);
    }
    for (alias, entry) in &config.apps {
        if alias.trim().is_empty() {
            errors.push(format!("{}: apps contains an empty alias", path.display()));
        }
        if entry.package.trim().is_empty() {
            errors.push(format!(
                "{}: apps.{alias}.package must not be empty",
                path.display()
            ));
        } else {
            let validation = cfg::validate_android_package(&entry.package);
            if let Err(err) = validation {
                errors.push(format!(
                    "{}: apps.{alias}.package `{}` is invalid: {err}",
                    path.display(),
                    entry.package
                ));
            }
        }
        if let Some(mode) = entry.debug_mode.as_deref() {
            validate_debug_mode(path, &format!("apps.{alias}.debug_mode"), mode, errors);
        }
    }
    for (name, target) in &config.targets {
        if let Err(error) = crate::device::target::validate_definition(name, target) {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
    if let Some(proxy) = &config.proxy {
        validate_proxy(path, proxy, errors);
    }
    if let Some(redaction) = &config.redaction {
        validate_redaction(path, redaction, errors);
    }
}

fn validate_redaction(path: &Path, redaction: &RedactionConfig, errors: &mut Vec<String>) {
    if let Err(error) = crate::redaction::Policy::new(redaction.policy_spec()) {
        let detail = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .map(|diagnostic| diagnostic.message.as_str())
            .unwrap_or("invalid redaction policy");
        errors.push(format!("{}: {detail}", path.display()));
    }
    for (index, key) in redaction.json_keys.iter().enumerate() {
        if key.trim().is_empty() {
            errors.push(format!(
                "{}: redaction.json_keys[{index}] must not be empty",
                path.display()
            ));
        }
    }
}

fn validate_target_references(config: &ShadowDroidConfig, errors: &mut Vec<String>) {
    if let Some(name) = config.default_target.as_deref()
        && config.target(name).is_none()
    {
        errors.push(format!(
            "merged config: default_target `{name}` is not present in targets"
        ));
    }
    for (alias, app) in &config.apps {
        if let Some(name) = app.target.as_deref()
            && config.target(name).is_none()
        {
            errors.push(format!(
                "merged config: apps.{alias}.target `{name}` is not present in targets"
            ));
        }
    }
}

fn validate_proxy(path: &Path, proxy: &ProxyConfig, errors: &mut Vec<String>) {
    // Explicit CA paths must be absolute or ~/-prefixed — a bare relative path
    // can't be resolved once configs are merged (see config::resolve_ca).
    for (field, value) in [
        ("proxy.ca_cert", &proxy.ca_cert),
        ("proxy.ca_key", &proxy.ca_key),
    ] {
        if let Some(raw) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let absolute = raw.starts_with("~/") || Path::new(raw).is_absolute();
            if !absolute {
                errors.push(format!(
                    "{}: {field} must be an absolute path or start with `~/` (got {raw:?})",
                    path.display()
                ));
            }
        }
    }
    match (proxy.ca_cert.is_some(), proxy.ca_key.is_some()) {
        (true, false) => errors.push(format!(
            "{}: proxy.ca_cert is set but proxy.ca_key is missing (both are required)",
            path.display()
        )),
        (false, true) => errors.push(format!(
            "{}: proxy.ca_key is set but proxy.ca_cert is missing (both are required)",
            path.display()
        )),
        _ => {}
    }
    if let Some(store) = proxy.trust_store.as_deref()
        && !matches!(store, "system" | "user" | "push" | "ui")
    {
        errors.push(format!(
            "{}: proxy.trust_store must be one of system, user, push (legacy ui is also accepted; got {store:?})",
            path.display()
        ));
    }
    if proxy.port == Some(0) {
        errors.push(format!("{}: proxy.port must not be 0", path.display()));
    }
}

fn validate_debug_mode(path: &Path, field: &str, mode: &str, errors: &mut Vec<String>) {
    if crate::cmd::debugger::DebugMode::from_config(mode).is_none() {
        errors.push(format!(
            "{}: {field} must be one of {}",
            path.display(),
            crate::cmd::debugger::DebugMode::allowed_values()
        ));
    }
}

fn example_config() -> Value {
    json!({
        "default_target": "mobile",
        "app": "Example",
        "project": "/path/to/android/project",
        "redaction": {
            "enabled": true,
            "json_keys": ["customerId"],
            "patterns": ["ORDER-[0-9]+"]
        },
        "apps": {
            "Example": {
                "package": "com.example.app",
                "run_configuration": "app",
                "debugger": "Android Debugger",
                "target": "mobile"
            }
        },
        "targets": {
            "mobile": {
                "avd": "Project_Pixel_9_API_36",
                "start": "if-needed",
                "form_factor": "mobile"
            },
            "tv": {
                "avd": "Project_TV_API_35",
                "start": "if-needed",
                "form_factor": "tv"
            }
        }
    })
}

fn print_value(value: Value, as_json: bool) -> Result<()> {
    if as_json {
        crate::events::emit_result(&value);
    } else {
        print_human(&value)?;
    }
    Ok(())
}

fn print_human(value: &Value) -> Result<()> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("config");
    match kind {
        "shadowdroid_config_paths" => {
            println!("Config paths");
            println!("  user: {}", value["user_config"].as_str().unwrap_or(""));
            println!(
                "  project: {}",
                value["project_config"].as_str().unwrap_or("")
            );
            println!("  loaded:");
            for path in value["loaded"].as_array().into_iter().flatten() {
                println!("    - {}", path.as_str().unwrap_or(""));
            }
            println!("Precedence: user -> ancestor project files -> CLI flags");
        }
        "shadowdroid_config_schema" | "shadowdroid_config_explain" => {
            println!("ShadowDroid config is JSON.");
            println!("User config: ~/.shadowdroid/config.json");
            println!("Project config: .shadowdroid/config.json");
            println!("Run `shadowdroid config schema --json` for a machine-readable schema.");
            println!("Minimal project config:");
            println!("{}", serde_json::to_string_pretty(&value["example"])?);
        }
        "shadowdroid_config_init" => {
            println!(
                "wrote {} config: {}",
                value["scope"].as_str().unwrap_or("project"),
                value["path"].as_str().unwrap_or("")
            );
            println!("next: shadowdroid config validate --json");
        }
        "shadowdroid_config_validate" => {
            if value["ok"].as_bool().unwrap_or(false) {
                println!("config ok");
            } else {
                println!("config has errors");
            }
            for warning in value["warnings"].as_array().into_iter().flatten() {
                println!("warning: {}", warning.as_str().unwrap_or(""));
            }
            for error in value["errors"].as_array().into_iter().flatten() {
                println!("error: {}", error.as_str().unwrap_or(""));
            }
        }
        other => return Err(anyhow!("unknown config output type: {other}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_init_deep_merges_existing_alias_fields() {
        let mut config = ShadowDroidConfig::default();
        config.apps.insert(
            "Example".into(),
            AppConfig {
                package: "com.example.old".into(),
                project: Some("/work/example".into()),
                run_configuration: Some("existing-run".into()),
                debugger: Some("Existing Debugger".into()),
                debug_mode: Some("mixed".into()),
                target: Some("mobile".into()),
            },
        );

        upsert_app_config(
            &mut config,
            "example",
            "com.example.new",
            AppConfigUpdate {
                run_configuration: &Some("new-run".into()),
                debugger: &None,
                debug_mode: &None,
                target: &None,
            },
            true,
        )
        .unwrap();

        assert_eq!(
            config.apps.len(),
            1,
            "case-insensitive alias was duplicated"
        );
        let app = config.apps.get("Example").unwrap();
        assert_eq!(app.package, "com.example.new");
        assert_eq!(app.project.as_deref(), Some("/work/example"));
        assert_eq!(app.run_configuration.as_deref(), Some("new-run"));
        assert_eq!(app.debugger.as_deref(), Some("Existing Debugger"));
        assert_eq!(app.debug_mode.as_deref(), Some("mixed"));
        assert_eq!(app.target.as_deref(), Some("mobile"));
    }

    #[test]
    fn app_init_rejects_injection_and_package_changes_without_force() {
        let mut config = ShadowDroidConfig::default();
        for package in [
            "com.example;id",
            "com.example\nother",
            "com.$(id)",
            "com.'example'",
        ] {
            assert!(
                upsert_app_config(
                    &mut config,
                    "Example",
                    package,
                    AppConfigUpdate {
                        run_configuration: &None,
                        debugger: &None,
                        debug_mode: &None,
                        target: &None,
                    },
                    false,
                )
                .is_err(),
                "accepted {package:?}"
            );
        }

        upsert_app_config(
            &mut config,
            "Example",
            "com.example.one",
            AppConfigUpdate {
                run_configuration: &None,
                debugger: &None,
                debug_mode: &None,
                target: &None,
            },
            false,
        )
        .unwrap();
        assert!(
            upsert_app_config(
                &mut config,
                "Example",
                "com.example.two",
                AppConfigUpdate {
                    run_configuration: &None,
                    debugger: &None,
                    debug_mode: &None,
                    target: &None,
                },
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn debug_mode_validation_follows_the_value_enum() {
        let path = Path::new(".shadowdroid/config.json");
        let mut errors = Vec::new();
        for ok in ["auto", "JAVA", " mixed "] {
            validate_debug_mode(path, "debug_mode", ok, &mut errors);
        }
        assert!(errors.is_empty(), "{errors:?}");

        validate_debug_mode(path, "debug_mode", "jdwp", &mut errors);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("auto, java, native, mixed"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn validate_proxy_flags_relative_paths_pair_store_and_port() {
        let path = Path::new(".shadowdroid/config.json");

        // Relative ca_cert + lone ca_key are both errors.
        let mut errors = Vec::new();
        validate_proxy(
            path,
            &ProxyConfig {
                ca_cert: Some("certs/ca.pem".into()),
                ..Default::default()
            },
            &mut errors,
        );
        assert!(errors.iter().any(|e| e.contains("absolute path")));
        assert!(errors.iter().any(|e| e.contains("ca_key is missing")));

        // Absolute (or ~/) paths + valid store + non-zero port pass.
        let mut ok = Vec::new();
        validate_proxy(
            path,
            &ProxyConfig {
                ca_cert: Some("~/ca.pem".into()),
                ca_key: Some("/keys/ca.key".into()),
                trust_store: Some("user".into()),
                port: Some(8080),
                ..Default::default()
            },
            &mut ok,
        );
        assert!(ok.is_empty(), "{ok:?}");

        // Bad store enum + zero port are errors.
        let mut bad = Vec::new();
        validate_proxy(
            path,
            &ProxyConfig {
                trust_store: Some("keychain".into()),
                port: Some(0),
                ..Default::default()
            },
            &mut bad,
        );
        assert!(bad.iter().any(|e| e.contains("trust_store must be one of")));
        assert!(bad.iter().any(|e| e.contains("port must not be 0")));
    }
}
