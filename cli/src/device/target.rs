//! Project-scoped device target resolution.
//!
//! A target binds a human/agent-facing role (`mobile`, `tv`, …) to either a
//! stable Android Virtual Device name or a physical adb serial. AVD names are
//! stable across boots; their `emulator-5554`-style adb serials are not. This
//! module reuses an already-running matching AVD, starts it only when the
//! project explicitly opts into `start: "if-needed"`, and verifies boot and
//! form-factor postconditions before returning a serial.

use crate::config::{
    self, DeviceTargetConfig, ShadowDroidConfig, TargetFormFactor, TargetStartPolicy,
};
use crate::device::{adb, installer};
use crate::hostenv::{home_dir, shadowdroid_home};
use crate::ids::Serial;
use anyhow::{anyhow, Context, Result};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::Duration;
use tokio::process::Command as TokioCommand;
use tracing::info;

const DEFAULT_BOOT_TIMEOUT_SECONDS: u64 = 180;
const MIN_BOOT_TIMEOUT_SECONDS: u64 = 10;
const MAX_BOOT_TIMEOUT_SECONDS: u64 = 900;
const EMULATOR_LIST_TIMEOUT: Duration = Duration::from_secs(15);
const ADB_START_TIMEOUT: Duration = Duration::from_secs(15);
const BOOT_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AvdOwner {
    avd: String,
    project_root: String,
    target: String,
}

/// Resolve one configured target to an online, fully-booted adb serial.
pub async fn resolve(
    config: &ShadowDroidConfig,
    requested_name: &str,
    takeover: bool,
) -> Result<Serial> {
    resolve_with_start(config, requested_name, takeover, true).await
}

/// Resolve a target without starting a missing AVD. Cleanup commands use this
/// so `disconnect` never boots an emulator merely to stop ShadowDroid.
pub async fn resolve_existing(
    config: &ShadowDroidConfig,
    requested_name: &str,
    takeover: bool,
) -> Result<Serial> {
    resolve_with_start(config, requested_name, takeover, false).await
}

async fn resolve_with_start(
    config: &ShadowDroidConfig,
    requested_name: &str,
    takeover: bool,
    allow_start: bool,
) -> Result<Serial> {
    let requested_name = requested_name.trim();
    let Some((target_name, target)) = config.target(requested_name) else {
        let available = config.targets.keys().cloned().collect::<Vec<_>>();
        return Err(crate::diagnostic::DiagnosticError::new(
            "target_not_configured",
            "device_target",
            format!("device target `{requested_name}` is not configured"),
        )
        .detail(serde_json::json!({
            "target_name": requested_name,
            "available_targets": available,
        }))
        .next_actions([
            "run `shadowdroid config schema --json` to inspect the targets shape",
            "add the target to .shadowdroid/config.json and run `shadowdroid config validate --json`",
        ])
        .into());
    };
    if let Err(error) = validate_definition(target_name, target) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "target_config_invalid",
            "device_target",
            format!("device target `{target_name}` is invalid: {error}"),
        )
        .detail(serde_json::json!({
            "target_name": target_name,
            "error": error.to_string(),
        }))
        .next_actions([
            "run `shadowdroid config validate --json` for the full config report",
            "fix the target entry using `shadowdroid config schema --json` as the contract",
        ])
        .into());
    }

    match (target.avd.as_deref(), target.serial.as_deref()) {
        (Some(avd), None) => {
            resolve_avd(config, target_name, avd, target, takeover, allow_start).await
        }
        (None, Some(serial)) => resolve_physical(target_name, serial, target).await,
        _ => unreachable!("validated target has exactly one binding"),
    }
}

pub fn validate_definition(name: &str, target: &DeviceTargetConfig) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("target name must not be empty");
    }
    match (target.avd.as_deref(), target.serial.as_deref()) {
        (Some(avd), None) if valid_binding(avd) => {}
        (None, Some(serial)) if valid_binding(serial) => {}
        (Some(_), Some(_)) => {
            anyhow::bail!("targets.{name} must set exactly one of `avd` or `serial`, not both")
        }
        (None, None) => anyhow::bail!("targets.{name} must set exactly one of `avd` or `serial`"),
        (Some(_), None) => {
            anyhow::bail!("targets.{name}.avd must not be empty or contain control characters")
        }
        (None, Some(_)) => {
            anyhow::bail!("targets.{name}.serial must not be empty or contain control characters")
        }
    }
    if target.serial.is_some() {
        if target.start == Some(TargetStartPolicy::IfNeeded) {
            anyhow::bail!("targets.{name}.start cannot be `if-needed` for a physical serial");
        }
        if target.cold_boot == Some(true) {
            anyhow::bail!("targets.{name}.cold_boot is only valid for an AVD target");
        }
    }
    if let Some(timeout) = target.boot_timeout_seconds {
        if !(MIN_BOOT_TIMEOUT_SECONDS..=MAX_BOOT_TIMEOUT_SECONDS).contains(&timeout) {
            anyhow::bail!(
                "targets.{name}.boot_timeout_seconds must be between {MIN_BOOT_TIMEOUT_SECONDS} and {MAX_BOOT_TIMEOUT_SECONDS}"
            );
        }
    }
    Ok(())
}

fn valid_binding(value: &str) -> bool {
    !value.trim().is_empty() && value.trim() == value && !value.chars().any(char::is_control)
}

async fn resolve_physical(
    target_name: &str,
    serial: &str,
    target: &DeviceTargetConfig,
) -> Result<Serial> {
    let devices = adb::list_devices().await.context("listing devices")?;
    if !devices.iter().any(|candidate| candidate == serial) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "target_device_unavailable",
            "device_target",
            format!("target `{target_name}` expects adb device `{serial}`, but it is not online"),
        )
        .retryable(true)
        .detail(serde_json::json!({
            "target_name": target_name,
            "serial": serial,
            "online_devices": devices,
        }))
        .next_actions([
            "connect and authorize the configured physical device, then retry",
            "run `shadowdroid devices` to inspect current device states",
        ])
        .into());
    }
    let serial = Serial::from(serial);
    validate_form_factor(target_name, &serial, target.form_factor).await?;
    info!(target = target_name, device = %serial, "resolved physical device target");
    Ok(serial)
}

async fn resolve_avd(
    config: &ShadowDroidConfig,
    target_name: &str,
    avd: &str,
    target: &DeviceTargetConfig,
    takeover: bool,
    allow_start: bool,
) -> Result<Serial> {
    // Reuse the existing cross-process lifecycle lock machinery with a
    // namespaced synthetic key. It serializes the check/claim/start/recheck
    // sequence without colliding with a real device serial lock.
    let lock_key = Serial::new(format!("avd:{avd}"));
    let _guard = installer::acquire_lifecycle_lock(&lock_key)?;

    let timeout = Duration::from_secs(
        target
            .boot_timeout_seconds
            .unwrap_or(DEFAULT_BOOT_TIMEOUT_SECONDS),
    );
    if let Some(serial) = find_running_avd(avd).await? {
        let serial = wait_for_boot(avd, target_name, Some(serial), timeout, None, None).await?;
        validate_form_factor(target_name, &serial, target.form_factor).await?;
        let project_root = project_root(config)?;
        claim_avd(
            &shadowdroid_home()?,
            &project_root,
            target_name,
            avd,
            takeover,
        )?;
        info!(target = target_name, avd, device = %serial, "reused running AVD target");
        return Ok(serial);
    }

    if !allow_start
        || target.start.unwrap_or(TargetStartPolicy::Never) != TargetStartPolicy::IfNeeded
    {
        return Err(crate::diagnostic::DiagnosticError::new(
            "target_avd_not_running",
            "device_target",
            format!("target `{target_name}` is bound to AVD `{avd}`, but that AVD is not running"),
        )
        .retryable(true)
        .detail(serde_json::json!({
            "target_name": target_name,
            "avd": avd,
            "configured_start": target.start.unwrap_or(TargetStartPolicy::Never),
            "start_allowed_for_command": allow_start,
        }))
        .next_actions([
            format!("start AVD `{avd}` and retry"),
            format!("set targets.{target_name}.start to `if-needed` for commands that may start a target"),
        ])
        .into());
    }

    let emulator = emulator_program();
    let available = list_available_avds(&emulator).await?;
    if !available.iter().any(|candidate| candidate == avd) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "target_avd_missing",
            "device_target",
            format!("target `{target_name}` references AVD `{avd}`, but it is not installed"),
        )
        .detail(serde_json::json!({
            "target_name": target_name,
            "avd": avd,
            "available_avds": available,
            "emulator": emulator.display().to_string(),
        }))
        .next_actions([
            "create the configured AVD in Android Studio Device Manager",
            "update the target's avd field to one of detail.available_avds",
        ])
        .into());
    }

    // A second process may have completed startup while we inspected the SDK.
    if let Some(serial) = find_running_avd(avd).await? {
        let serial = wait_for_boot(avd, target_name, Some(serial), timeout, None, None).await?;
        validate_form_factor(target_name, &serial, target.form_factor).await?;
        let project_root = project_root(config)?;
        claim_avd(
            &shadowdroid_home()?,
            &project_root,
            target_name,
            avd,
            takeover,
        )?;
        return Ok(serial);
    }

    let project_root = project_root(config)?;
    claim_avd(
        &shadowdroid_home()?,
        &project_root,
        target_name,
        avd,
        takeover,
    )?;

    let (child, log_path) = start_avd(&emulator, avd, target.cold_boot.unwrap_or(false))?;
    let serial = wait_for_boot(
        avd,
        target_name,
        None,
        timeout,
        Some(child),
        Some(log_path.as_path()),
    )
    .await?;
    validate_form_factor(target_name, &serial, target.form_factor).await?;
    info!(target = target_name, avd, device = %serial, "started AVD target");
    Ok(serial)
}

async fn find_running_avd(avd: &str) -> Result<Option<Serial>> {
    let serials = list_target_devices().await?;
    let probes = serials.into_iter().map(|serial| async move {
        let name = avd_name(&serial).await;
        (serial, name)
    });
    let matches = join_all(probes)
        .await
        .into_iter()
        .filter_map(|(serial, name)| (name.as_deref() == Some(avd)).then(|| Serial::from(serial)))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [serial] => Ok(Some(serial.clone())),
        _ => Err(crate::diagnostic::DiagnosticError::new(
            "target_avd_ambiguous",
            "device_target",
            format!("more than one online emulator reports AVD name `{avd}`"),
        )
        .detail(serde_json::json!({"avd": avd, "devices": matches}))
        .next_actions([
            "stop duplicate AVD instances, then retry",
            "pass an explicit --device serial only if duplicate instances are intentional",
        ])
        .into()),
    }
}

/// The wire client deliberately does not shell out to `adb`, but a fresh host
/// may not have an ADB server yet. Named AVD targets already require an Android
/// SDK emulator, so use the colocated platform tool once to start the server,
/// then return to the wire protocol for all device operations.
async fn list_target_devices() -> Result<Vec<String>> {
    let initial = match adb::list_devices().await {
        Ok(devices) => return Ok(devices),
        Err(error) => error,
    };
    let adb_program = adb_program();
    if let Err(start_error) = start_adb_server(&adb_program).await {
        return Err(initial.context(format!(
            "listing devices; also failed to start the ADB server with `{}`: {start_error:#}",
            adb_program.display()
        )));
    }
    adb::list_devices().await.with_context(|| {
        format!(
            "listing devices after starting the ADB server with `{}`",
            adb_program.display()
        )
    })
}

async fn start_adb_server(adb_program: &Path) -> Result<()> {
    let mut command = TokioCommand::new(adb_program);
    command.arg("start-server").kill_on_drop(true);
    let output = tokio::time::timeout(ADB_START_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow!("adb start-server timed out after 15 seconds"))?
        .with_context(|| format!("run {} start-server", adb_program.display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "adb start-server exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

pub async fn avd_name(serial: &str) -> Option<String> {
    for property in ["ro.boot.qemu.avd_name", "ro.kernel.qemu.avd_name"] {
        let Ok(value) = adb::shell(serial, format!("getprop {property}")).await else {
            continue;
        };
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

async fn wait_for_boot(
    avd: &str,
    target_name: &str,
    initial_serial: Option<Serial>,
    timeout: Duration,
    mut child: Option<Child>,
    log_path: Option<&Path>,
) -> Result<Serial> {
    let started = tokio::time::Instant::now();
    let mut serial = initial_serial;
    loop {
        if serial.is_none() {
            serial = find_running_avd(avd).await?;
        }
        if let Some(candidate) = serial.as_ref() {
            let booted = adb::shell(candidate, "getprop sys.boot_completed")
                .await
                .map(|value| value.trim() == "1")
                .unwrap_or(false);
            if booted {
                return Ok(candidate.clone());
            }
        }

        if let Some(running) = child.as_mut() {
            if let Some(status) = running.try_wait().context("checking emulator process")? {
                if !status.success() {
                    return Err(crate::diagnostic::DiagnosticError::new(
                        "target_avd_start_failed",
                        "device_target",
                        format!("emulator process for AVD `{avd}` exited with {status}"),
                    )
                    .retryable(true)
                    .detail(serde_json::json!({
                        "target_name": target_name,
                        "avd": avd,
                        "status": status.code(),
                        "log": log_path.map(|path| path.display().to_string()),
                    }))
                    .next_actions([
                        "inspect detail.log for the emulator startup error",
                        "start the AVD once from Android Studio Device Manager, then retry",
                    ])
                    .into());
                }
                // Some emulator launchers hand off to another process. A zero
                // exit is not a failure; keep waiting for adb discovery.
                child = None;
            }
        }

        if started.elapsed() >= timeout {
            return Err(crate::diagnostic::DiagnosticError::new(
                "target_avd_boot_timeout",
                "device_target",
                format!(
                    "AVD `{avd}` did not finish booting within {} seconds",
                    timeout.as_secs()
                ),
            )
            .retryable(true)
            .detail(serde_json::json!({
                "target_name": target_name,
                "avd": avd,
                "device": serial,
                "timeout_seconds": timeout.as_secs(),
                "log": log_path.map(|path| path.display().to_string()),
            }))
            .next_actions([
                "inspect the emulator window and detail.log, then retry",
                "increase boot_timeout_seconds only if the AVD is healthy but consistently slow",
            ])
            .into());
        }
        tokio::time::sleep(BOOT_POLL_INTERVAL).await;
    }
}

async fn validate_form_factor(
    target_name: &str,
    serial: &Serial,
    expected: Option<TargetFormFactor>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let characteristics = adb::shell(serial, "getprop ro.build.characteristics")
        .await
        .unwrap_or_default();
    let leanback = adb::shell(serial, "pm has-feature android.software.leanback")
        .await
        .unwrap_or_default();
    let is_tv = characteristics
        .split(',')
        .any(|value| value.trim().eq_ignore_ascii_case("tv"))
        || leanback.trim().ends_with("true");
    let actual = if is_tv {
        TargetFormFactor::Tv
    } else {
        TargetFormFactor::Mobile
    };
    if actual != expected {
        return Err(crate::diagnostic::DiagnosticError::new(
            "target_form_factor_mismatch",
            "device_target",
            format!(
                "target `{target_name}` expected form factor {}, but device `{serial}` is {}",
                form_factor_name(expected),
                form_factor_name(actual)
            ),
        )
        .detail(serde_json::json!({
            "target_name": target_name,
            "device": serial,
            "expected": form_factor_name(expected),
            "actual": form_factor_name(actual),
            "build_characteristics": characteristics.trim(),
            "leanback_feature": leanback.trim(),
        }))
        .next_actions([
            "bind the target to an AVD/device with the expected form factor",
            "remove form_factor only if this assertion is intentionally unnecessary",
        ])
        .into());
    }
    Ok(())
}

fn form_factor_name(value: TargetFormFactor) -> &'static str {
    match value {
        TargetFormFactor::Mobile => "mobile",
        TargetFormFactor::Tv => "tv",
    }
}

fn emulator_program() -> PathBuf {
    if let Some(explicit) = std::env::var_os("SHADOWDROID_EMULATOR") {
        return PathBuf::from(explicit);
    }
    for variable in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(root) = std::env::var_os(variable) {
            let candidate = PathBuf::from(root).join("emulator").join(if cfg!(windows) {
                "emulator.exe"
            } else {
                "emulator"
            });
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if let Ok(home) = home_dir() {
        let candidates = if cfg!(target_os = "macos") {
            vec![home.join("Library/Android/sdk/emulator/emulator")]
        } else if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|root| root.join("Android/Sdk/emulator/emulator.exe"))
                .into_iter()
                .collect()
        } else {
            vec![home.join("Android/Sdk/emulator/emulator")]
        };
        if let Some(candidate) = candidates.into_iter().find(|path| path.is_file()) {
            return candidate;
        }
    }
    PathBuf::from(if cfg!(windows) {
        "emulator.exe"
    } else {
        "emulator"
    })
}

fn adb_program() -> PathBuf {
    if let Some(explicit) = std::env::var_os("SHADOWDROID_ADB") {
        return PathBuf::from(explicit);
    }
    for variable in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(root) = std::env::var_os(variable) {
            let candidate = PathBuf::from(root)
                .join("platform-tools")
                .join(if cfg!(windows) { "adb.exe" } else { "adb" });
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if let Ok(home) = home_dir() {
        let candidates = if cfg!(target_os = "macos") {
            vec![home.join("Library/Android/sdk/platform-tools/adb")]
        } else if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .map(|root| root.join("Android/Sdk/platform-tools/adb.exe"))
                .into_iter()
                .collect()
        } else {
            vec![home.join("Android/Sdk/platform-tools/adb")]
        };
        if let Some(candidate) = candidates.into_iter().find(|path| path.is_file()) {
            return candidate;
        }
    }
    PathBuf::from(if cfg!(windows) { "adb.exe" } else { "adb" })
}

async fn list_available_avds(emulator: &Path) -> Result<Vec<String>> {
    let mut command = TokioCommand::new(emulator);
    command.arg("-list-avds").kill_on_drop(true);
    let output = match tokio::time::timeout(EMULATOR_LIST_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(crate::diagnostic::DiagnosticError::new(
                "emulator_unavailable",
                "device_target",
                format!(
                    "could not run Android emulator executable `{}`",
                    emulator.display()
                ),
            )
            .detail(serde_json::json!({
                "emulator": emulator.display().to_string(),
                "error": error.to_string(),
            }))
            .next_actions([
                "install Android Emulator from Android Studio SDK Manager",
                "set ANDROID_SDK_ROOT or SHADOWDROID_EMULATOR to the emulator executable",
            ])
            .into())
        }
        Err(_) => {
            return Err(crate::diagnostic::DiagnosticError::new(
                "emulator_list_timeout",
                "device_target",
                "Android emulator did not list AVDs within 15 seconds",
            )
            .retryable(true)
            .detail(serde_json::json!({"emulator": emulator.display().to_string()}))
            .next_actions([
                "retry after Android Studio and SDK updates finish",
                "run the emulator executable with -list-avds to diagnose it directly",
            ])
            .into())
        }
    };
    if !output.status.success() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "emulator_list_failed",
            "device_target",
            "Android emulator failed to list installed AVDs",
        )
        .detail(serde_json::json!({
            "emulator": emulator.display().to_string(),
            "status": output.status.code(),
            "stderr": String::from_utf8_lossy(&output.stderr).trim(),
        }))
        .next_actions([
            "run the emulator executable with -list-avds to diagnose the SDK installation",
            "repair Android Emulator from Android Studio SDK Manager",
        ])
        .into());
    }
    Ok(parse_avd_list(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_avd_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn start_avd(emulator: &Path, avd: &str, cold_boot: bool) -> Result<(Child, PathBuf)> {
    let log_dir = shadowdroid_home()?.join("logs");
    std::fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
    let log_path = log_dir.join(format!("emulator-{}.log", stable_file_component(avd)));
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    writeln!(log, "\n=== ShadowDroid starting AVD {avd} ===")?;
    let stdout = log.try_clone()?;
    let mut command = StdCommand::new(emulator);
    command
        .args(["-avd", avd])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(log));
    // The emulator must outlive this short-lived CLI process. Give it a
    // separate process group so terminal/session cleanup (including another
    // project launching concurrently) cannot forward signals to the AVD.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if cold_boot {
        command.arg("-no-snapshot-load");
    }
    let child = command.spawn().map_err(|error| {
        crate::diagnostic::DiagnosticError::new(
            "target_avd_start_failed",
            "device_target",
            format!("could not start AVD `{avd}`"),
        )
        .retryable(true)
        .detail(serde_json::json!({
            "avd": avd,
            "emulator": emulator.display().to_string(),
            "error": error.to_string(),
            "log": log_path.display().to_string(),
        }))
        .next_actions([
            "inspect detail.log and verify the AVD starts in Android Studio Device Manager",
            "check free disk/RAM and retry",
        ])
    })?;
    Ok((child, log_path))
}

fn project_root(config: &ShadowDroidConfig) -> Result<PathBuf> {
    let user_config = config::user_config_path()?;
    let configured_root = config
        .sources
        .iter()
        .rev()
        .find(|path| **path != user_config)
        .and_then(|path| path.parent())
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let declared_root = config
        .project
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir());
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let repository_root = cwd
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf);
    let root = configured_root
        .or(declared_root)
        .or(repository_root)
        .unwrap_or(cwd);
    Ok(root.canonicalize().unwrap_or(root))
}

fn claim_avd(
    shadowdroid_dir: &Path,
    project_root: &Path,
    target_name: &str,
    avd: &str,
    takeover: bool,
) -> Result<()> {
    let dir = shadowdroid_dir.join("targets").join("claims");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.json", stable_file_component(avd)));
    let owner = AvdOwner {
        avd: avd.to_string(),
        project_root: project_root.display().to_string(),
        target: target_name.to_string(),
    };
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            write_owner(&mut file, &owner)?;
            file.sync_all()?;
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }

    let existing_text = std::fs::read_to_string(&path)
        .with_context(|| format!("read AVD ownership claim {}", path.display()))?;
    let existing = serde_json::from_str::<AvdOwner>(&existing_text).ok();
    if existing
        .as_ref()
        .is_some_and(|value| value.avd == avd && value.project_root == owner.project_root)
    {
        return Ok(());
    }
    if takeover {
        let mut temp = tempfile::NamedTempFile::new_in(&dir)
            .with_context(|| format!("create temporary AVD claim in {}", dir.display()))?;
        write_owner(&mut temp, &owner)?;
        temp.as_file().sync_all()?;
        temp.persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace AVD ownership claim {}", path.display()))?;
        return Ok(());
    }

    Err(crate::diagnostic::DiagnosticError::new(
        "target_avd_owned_by_other_project",
        "device_target",
        format!("AVD `{avd}` is already associated with another project"),
    )
    .detail(serde_json::json!({
        "avd": avd,
        "target_name": target_name,
        "project_root": owner.project_root,
        "owner": existing,
        "claim": path.display().to_string(),
    }))
    .next_actions([
        "bind this project target to a different AVD for isolation",
        "retry the same command with --takeover only if reassignment is intentional",
    ])
    .into())
}

fn write_owner(writer: &mut impl Write, owner: &AvdOwner) -> Result<()> {
    serde_json::to_writer_pretty(&mut *writer, owner)?;
    writeln!(writer)?;
    Ok(())
}

fn stable_file_component(value: &str) -> String {
    let prefix = value
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let digest = Sha256::digest(value.as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_target_bindings_and_start_policy() {
        let avd = DeviceTargetConfig {
            avd: Some("ProjectA_Pixel_9".into()),
            start: Some(TargetStartPolicy::IfNeeded),
            form_factor: Some(TargetFormFactor::Mobile),
            boot_timeout_seconds: Some(120),
            ..Default::default()
        };
        assert!(validate_definition("mobile", &avd).is_ok());

        let both = DeviceTargetConfig {
            avd: Some("Pixel".into()),
            serial: Some("ABC".into()),
            ..Default::default()
        };
        assert!(validate_definition("bad", &both).is_err());

        let physical_auto_start = DeviceTargetConfig {
            serial: Some("ABC".into()),
            start: Some(TargetStartPolicy::IfNeeded),
            ..Default::default()
        };
        assert!(validate_definition("phone", &physical_auto_start).is_err());
    }

    #[test]
    fn parses_avd_list_without_blank_entries() {
        assert_eq!(
            parse_avd_list("Pixel_9\n\n TV_API_35 \r\n"),
            vec!["Pixel_9", "TV_API_35"]
        );
    }

    #[test]
    fn avd_claim_requires_explicit_cross_project_takeover() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project_a = tmp.path().join("a");
        let project_b = tmp.path().join("b");
        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::create_dir_all(&project_b).unwrap();

        claim_avd(&home, &project_a, "mobile", "Pixel_9", false).unwrap();
        claim_avd(&home, &project_a, "other-alias", "Pixel_9", false).unwrap();
        let error = claim_avd(&home, &project_b, "mobile", "Pixel_9", false).unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.code, "target_avd_owned_by_other_project");

        claim_avd(&home, &project_b, "mobile", "Pixel_9", true).unwrap();
        let claim = std::fs::read_dir(home.join("targets/claims"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let owner: AvdOwner =
            serde_json::from_str(&std::fs::read_to_string(claim).unwrap()).unwrap();
        assert_eq!(owner.project_root, project_b.display().to_string());
    }

    #[test]
    fn claim_filenames_are_safe_and_collision_resistant() {
        let name = stable_file_component("Pixel/9; $(unsafe)");
        assert!(!name.contains('/'));
        assert!(!name.contains(';'));
        assert_ne!(
            stable_file_component("Pixel_9"),
            stable_file_component("Pixel-9")
        );
    }
}
