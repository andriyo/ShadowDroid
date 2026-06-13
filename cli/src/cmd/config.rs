//! Agent-facing config helpers.
//!
//! The runtime loader lives in [crate::config]. This command exposes the same
//! shape as a discoverable CLI surface so agents can generate, validate, and
//! inspect config without reading Rust source.

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::config as cfg;
use crate::config::{AppConfig, ShadowDroidConfig};

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
    Init(ConfigInitArgs),
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
    /// Write ./.shadowdroid.json. This is the default.
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
        ConfigCmd::Validate { json: as_json } => print_value(validate_value()?, *as_json),
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
            if let Some(existing) = config.apps.get(app) {
                if existing.package != package && !args.force {
                    bail!(
                        "app alias `{}` already points at `{}`. Pass --force to replace it with `{}`.",
                        app,
                        existing.package,
                        package
                    );
                }
            }
            config.apps.insert(
                app.to_string(),
                AppConfig {
                    package: package.to_string(),
                    project: None,
                    run_configuration: args.run_configuration.clone(),
                    debugger: args.debugger.clone(),
                    debug_mode: args.debug_mode.clone(),
                },
            );
            changed.push(format!("apps.{app}"));
        }
        (None, Some(package)) => {
            let package = package.trim();
            if package.is_empty() {
                bail!("--package must not be empty");
            }
            config.app = Some(package.to_string());
            changed.push("app".to_string());
        }
        _ => {}
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

    cfg::write_config_file(&path, &config)?;
    let value = json!({
        "type": "shadowdroid_config_init",
        "ok": true,
        "scope": scope,
        "path": path.display().to_string(),
        "changed": changed,
        "config": config,
        "next_commands": [
            "shadowdroid config validate --json",
            "shadowdroid debug auto"
        ],
    });
    print_value(value, args.json)
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
        "loaded": loaded.iter().map(display_path).collect::<Vec<_>>(),
        "precedence": [
            "~/.shadowdroid/config.json",
            ".shadowdroid.json files from ancestors, root to current directory",
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
        "project_config": ".shadowdroid.json",
        "precedence": [
            "user config",
            "project configs from ancestors, root to current directory",
            "explicit CLI flags"
        ],
        "fields": {
            "device": {"type": "string", "optional": true, "description": "Default ADB serial."},
            "app": {"type": "string", "optional": true, "description": "Default app alias, package, or installed app name."},
            "project": {"type": "string", "optional": true, "description": "Android Studio project name or absolute project path."},
            "studio_url": {"type": "string", "optional": true, "description": "Android Studio debugger bridge URL."},
            "android_studio": {"type": "string", "optional": true, "description": "Android Studio installation path."},
            "studio_plugin": {"type": "string", "optional": true, "description": "Local Studio plugin ZIP path."},
            "debugger": {"type": "string", "optional": true, "description": "Default Android debugger id/display name."},
            "debug_mode": {"type": "string", "optional": true, "enum": ["auto", "java", "native", "mixed"], "description": "Default semantic debugger mode."},
            "run_configuration": {"type": "string", "optional": true, "description": "Default Android Studio run configuration."},
            "apps": {
                "type": "object",
                "optional": true,
                "description": "Map of app aliases to package/debugger defaults.",
                "additional_properties": {"$ref": "#/app_entry"}
            }
        },
        "app_entry": {
            "package": {"type": "string", "required": true, "description": "Android package/process name."},
            "project": {"type": "string", "optional": true, "description": "Project override for this app."},
            "run_configuration": {"type": "string", "optional": true, "description": "Run configuration override for this app."},
            "debugger": {"type": "string", "optional": true, "description": "Debugger override for this app."},
            "debug_mode": {"type": "string", "optional": true, "enum": ["auto", "java", "native", "mixed"], "description": "Debugger mode override for this app."}
        },
        "example": example_config(),
        "recommended_agent_flow": [
            "shadowdroid config schema --json",
            "shadowdroid config init --project --app Example --package com.example.app --project-path /path/to/project",
            "shadowdroid config validate --json",
            "shadowdroid debug auto"
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
            "apps": {
                "Example": {
                    "package": "com.example.app"
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
                errors.push(format!("{}: {err}", path.display()));
                files.push(json!({
                    "path": path.display().to_string(),
                    "ok": false,
                    "error": err.to_string(),
                }));
            }
        }
    }

    let merged = if errors.is_empty() {
        Some(ShadowDroidConfig::load()?)
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
        if !configured && !looks_like_package(app) {
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
        } else if !looks_like_package(&entry.package) {
            warnings.push(format!(
                "{}: apps.{alias}.package `{}` does not look like an Android package",
                path.display(),
                entry.package
            ));
        }
        if let Some(mode) = entry.debug_mode.as_deref() {
            validate_debug_mode(path, &format!("apps.{alias}.debug_mode"), mode, errors);
        }
    }
}

fn validate_debug_mode(path: &Path, field: &str, mode: &str, errors: &mut Vec<String>) {
    match mode.trim().to_ascii_lowercase().as_str() {
        "auto" | "java" | "native" | "mixed" => {}
        _ => errors.push(format!(
            "{}: {field} must be one of auto, java, native, mixed",
            path.display()
        )),
    }
}

fn example_config() -> Value {
    json!({
        "device": "emulator-5554",
        "app": "Example",
        "project": "/path/to/android/project",
        "apps": {
            "Example": {
                "package": "com.example.app",
                "run_configuration": "app",
                "debugger": "Android Debugger"
            }
        }
    })
}

fn print_value(value: Value, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(&value)?);
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
            println!("Project config: .shadowdroid.json");
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

fn display_path(path: &PathBuf) -> String {
    path.display().to_string()
}

fn looks_like_package(value: &str) -> bool {
    value.contains('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}
