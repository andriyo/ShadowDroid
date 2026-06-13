//! Thin wrapper over the `adb_client` crate. Talks to the host `adbd` over
//! the ADB wire protocol (port 5037) — no shelling out to the `adb` binary,
//! so a single static Rust binary works on any machine with a running adbd
//! (no Android SDK required).
//!
//! All `adb_client` calls are synchronous. Public functions wrap each call
//! in `tokio::task::spawn_blocking` so they're safe to .await from the async
//! CLI dispatch without stalling the runtime. The blocking work itself is
//! always sub-second.

use adb_client::server::ADBServer;
use adb_client::server_device::ADBServerDevice;
use adb_client::ADBDeviceExt;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use tokio::task::spawn_blocking;
use tracing::debug;

/// Return serials of devices currently in "device" state. Skips offline /
/// unauthorized / no-permissions devices — those are not actionable.
pub async fn list_devices() -> Result<Vec<String>> {
    spawn_blocking(|| {
        let mut server = ADBServer::default();
        let devices = server.devices().map_err(|e| anyhow!("adb devices: {e}"))?;
        // DeviceShort stringifies as `<serial> <state>`; we want only "device"
        Ok(devices
            .into_iter()
            .filter(|d| format!("{d}").contains("device"))
            .map(|d| d.identifier)
            .collect())
    })
    .await
    .context("list_devices task panicked")?
}

/// Open a device handle by serial. Fails fast if the device isn't connected.
fn get_device_sync(serial: &str) -> Result<ADBServerDevice> {
    let mut server = ADBServer::default();
    server
        .get_device_by_name(serial)
        .map_err(|e| anyhow!("get device {serial}: {e}"))
}

/// Run an `adb shell` command on the device, return stdout. stderr is logged
/// at debug level. Returns the stdout as a String (lossy UTF-8 decode).
pub async fn shell(serial: impl Into<String>, cmd: impl Into<String>) -> Result<String> {
    let serial = serial.into();
    let cmd = cmd.into();
    spawn_blocking(move || {
        let mut device = get_device_sync(&serial)?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        device
            .shell_command(&cmd.as_str(), Some(&mut stdout), Some(&mut stderr))
            .map_err(|e| anyhow!("adb shell {cmd:?}: {e}"))?;
        if !stderr.is_empty() {
            debug!(
                "adb shell stderr ({serial}, {cmd:?}): {}",
                String::from_utf8_lossy(&stderr)
            );
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    })
    .await
    .context("shell task panicked")?
}

/// Install an APK on the device. Uses the ADB streaming `exec:install` path
/// under the hood (faster than `adb push` + `pm install`).
pub async fn install(serial: impl Into<String>, apk_path: impl Into<PathBuf>) -> Result<()> {
    let serial = serial.into();
    let apk_path = apk_path.into();
    spawn_blocking(move || {
        let mut device = get_device_sync(&serial)?;
        device
            .install(&apk_path, None)
            .map_err(|e| anyhow!("adb install {}: {e}", apk_path.display()))
    })
    .await
    .context("install task panicked")?
}

/// Uninstall a package by name. Idempotent-ish: errors (e.g. "not installed")
/// are surfaced to the caller, which usually treats them as best-effort.
pub async fn uninstall(serial: impl Into<String>, package: impl Into<String>) -> Result<()> {
    let serial = serial.into();
    let package = package.into();
    spawn_blocking(move || {
        let mut device = get_device_sync(&serial)?;
        device
            .uninstall(&package.as_str(), None)
            .map_err(|e| anyhow!("adb uninstall {package}: {e}"))
    })
    .await
    .context("uninstall task panicked")?
}

/// Push a local file to the device over the ADB protocol. Used as a fallback
/// when the on-device server can't reach the target path (e.g. `/sdcard` under
/// Android's scoped storage returns EPERM to the instrumentation uid).
pub async fn push(
    serial: impl Into<String>,
    local: impl Into<PathBuf>,
    remote: impl Into<String>,
) -> Result<()> {
    let serial = serial.into();
    let local = local.into();
    let remote = remote.into();
    spawn_blocking(move || {
        let mut device = get_device_sync(&serial)?;
        let mut file =
            std::fs::File::open(&local).with_context(|| format!("open {}", local.display()))?;
        device
            .push(&mut file, &remote.as_str())
            .map_err(|e| anyhow!("adb push {} -> {remote}: {e}", local.display()))
    })
    .await
    .context("push task panicked")?
}

/// Pull a device file to memory over the ADB protocol. Fallback counterpart to
/// [push] for paths the on-device server can't read.
pub async fn pull(serial: impl Into<String>, remote: impl Into<String>) -> Result<Vec<u8>> {
    let serial = serial.into();
    let remote = remote.into();
    spawn_blocking(move || {
        let mut device = get_device_sync(&serial)?;
        let mut buf: Vec<u8> = Vec::new();
        device
            .pull(&remote.as_str(), &mut buf)
            .map_err(|e| anyhow!("adb pull {remote}: {e}"))?;
        Ok(buf)
    })
    .await
    .context("pull task panicked")?
}

/// Set up `adb forward tcp:<host_port> tcp:<device_port>`.
/// A laptop-side connect to host_port is proxied to device_port.
pub async fn forward(serial: impl Into<String>, host_port: u16, device_port: u16) -> Result<()> {
    let serial = serial.into();
    spawn_blocking(move || {
        let mut device = get_device_sync(&serial)?;
        // adb_client's `forward(remote, local)` maps to the ADB protocol
        // `forward:<local>;<remote>`. We pass (remote=device, local=host).
        let local = format!("tcp:{host_port}");
        let remote = format!("tcp:{device_port}");
        device
            .forward(remote, local)
            .map_err(|e| anyhow!("adb forward tcp:{host_port} tcp:{device_port}: {e}"))
    })
    .await
    .context("forward task panicked")?
}

/// Remove a previously-set forward rule by host port.
pub async fn forward_remove(serial: impl Into<String>, host_port: u16) -> Result<()> {
    let serial = serial.into();
    spawn_blocking(move || {
        let mut device = get_device_sync(&serial)?;
        device
            .forward_remove(format!("tcp:{host_port}"))
            .map_err(|e| anyhow!("adb forward --remove tcp:{host_port}: {e}"))
    })
    .await
    .context("forward_remove task panicked")?
}

/// Force-stop a package via `am force-stop`. Idempotent — safe to call when
/// the package isn't running.
pub async fn am_force_stop(serial: impl Into<String>, package: impl AsRef<str>) -> Result<()> {
    shell(serial, format!("am force-stop {}", package.as_ref())).await?;
    Ok(())
}

/// Start an Android instrumentation, backgrounded on-device so the adb shell
/// exits immediately while the instrumentation keeps running. The `-w` flag
/// is needed for the runner to fully initialize before returning.
///
/// `runner` is `<test_package>/<runner_class_fqn>`, e.g.
/// `io.github.andriyo.shadowdroid.test/androidx.test.runner.AndroidJUnitRunner`.
/// `test_class` (optional) restricts execution to a single JUnit class.
pub async fn am_instrument(
    serial: impl Into<String>,
    runner: impl AsRef<str>,
    test_class: Option<&str>,
    log_path: impl AsRef<str>,
) -> Result<()> {
    let class_arg = test_class
        .map(|c| format!("-e class {c} "))
        .unwrap_or_default();
    let cmd = format!(
        "nohup am instrument -w -e debug false {class_arg}{runner} > {log_path} 2>&1 &",
        runner = runner.as_ref(),
        log_path = log_path.as_ref()
    );
    shell(serial, cmd).await?;
    Ok(())
}

/// Kill any lingering shell-owned `app_process` on device. ShadowDroid's
/// backgrounded `am instrument` wrapper uses one, and tools like openatx's
/// uiautomator2 `u2.jar` do too. Any live UiAutomation owner can make the next
/// `am instrument` fail with "UiAutomationService already registered!".
pub async fn kill_instrument_zombies(serial: impl Into<String>) -> Result<()> {
    let serial = serial.into();
    // First: kill any `app_process` shells. They run as uid=2000 (shell) and
    // are what `am instrument` left behind from prior runs.
    let _ = shell(
        &serial,
        "ps -A | grep app_process | awk '{print $2}' | xargs -r kill -9 2>/dev/null",
    )
    .await;
    // Then: nuke the actual test process by package. force-stop the app under
    // test too — its UiAutomation registration leaks into the system until the
    // process dies completely.
    let _ = shell(&serial, "am force-stop io.github.andriyo.shadowdroid.test").await;
    let _ = shell(&serial, "am force-stop io.github.andriyo.shadowdroid").await;
    // Give system_server a beat to actually release the UiAutomation slot.
    // Without this, the very next `am instrument` races and hits
    // "UiAutomationService already registered!".
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    Ok(())
}

/// Return the on-device path of an installed package, or None if not installed.
/// Used by the installer to decide whether to reinstall.
pub async fn pm_path(
    serial: impl Into<String>,
    package: impl AsRef<str>,
) -> Result<Option<String>> {
    let out = shell(serial, format!("pm path {}", package.as_ref())).await?;
    Ok(out
        .lines()
        .find(|l| l.starts_with("package:"))
        .and_then(|l| l.strip_prefix("package:").map(str::trim).map(String::from)))
}

/// Return the installed package's versionName, or None if not installed.
pub async fn pm_version(
    serial: impl Into<String>,
    package: impl AsRef<str>,
) -> Result<Option<String>> {
    let out = shell(
        serial,
        format!(
            "dumpsys package {} | grep versionName | head -n 1",
            package.as_ref()
        ),
    )
    .await?;
    Ok(out
        .trim()
        .strip_prefix("versionName=")
        .map(String::from)
        .filter(|s| !s.is_empty()))
}

/// Return installed package names. Used by low-friction app-name resolution
/// when a user types `Livd` instead of `com.livd`.
pub async fn list_packages(serial: impl Into<String>) -> Result<Vec<String>> {
    let out = shell(serial, "pm list packages").await?;
    Ok(out
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .map(str::trim)
        .filter(|package| !package.is_empty())
        .map(str::to_string)
        .collect())
}

/// Like `list_devices` but returns **every** device paired with its connection
/// state string (`"device"`, `"offline"`, `"unauthorized"`, `"noperm"`, …),
/// unfiltered. `list_devices` hides anything that isn't fully "device"; the
/// `doctor` command needs to *surface* those unhealthy states.
pub async fn list_devices_with_state() -> Result<Vec<(String, String)>> {
    spawn_blocking(|| {
        let mut server = ADBServer::default();
        let devices = server.devices().map_err(|e| anyhow!("adb devices: {e}"))?;
        Ok(devices
            .into_iter()
            .map(|d| (d.identifier, format!("{}", d.state)))
            .collect())
    })
    .await
    .context("list_devices_with_state task panicked")?
}

/// Raw `ps` lines for processes that can hold the single device-wide
/// UiAutomation slot: any `app_process` shell, openatx/uiautomator2's
/// `com.wetest.uia2.Main`, our own test process, or atx. Empty string when
/// none are present. Shared by `doctor` and the installer's failure hint so
/// the detection heuristic lives in one place.
pub async fn ps_ui_automation_owners(serial: impl Into<String>) -> Result<String> {
    let out = shell(
        serial,
        "ps -A -o USER,PID,PPID,NAME,ARGS \
         | grep -E 'app_process|uiautomator|shadowdroid|wetest|atx' \
         | grep -v grep",
    )
    .await?;
    Ok(out.trim().to_string())
}

/// A small map of device facts (`android_release`, `android_sdk`,
/// `device_model`, `device_manufacturer`) parsed from `getprop`. Shared by
/// crash events ([crate::watch]) and `collect`. Best-effort: missing props are
/// simply omitted.
pub async fn device_info(serial: impl Into<String>) -> serde_json::Value {
    let out = shell(serial, "getprop").await.unwrap_or_default();
    let wanted = [
        ("ro.build.version.release", "android_release"),
        ("ro.build.version.sdk", "android_sdk"),
        ("ro.product.model", "device_model"),
        ("ro.product.manufacturer", "device_manufacturer"),
    ];
    let mut info = serde_json::Map::new();
    for line in out.lines() {
        let Some((key, value)) = parse_getprop_line(line) else {
            continue;
        };
        if let Some((_, out_key)) = wanted.iter().find(|(prop, _)| *prop == key) {
            info.insert(
                (*out_key).to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
    serde_json::Value::Object(info)
}

/// The currently-foreground `package/activity` component, parsed from
/// `dumpsys activity activities` (the `ResumedActivity` line). `None` if it
/// can't be determined. Host-side — does not depend on the ShadowDroid server,
/// so it survives the server being evicted under memory pressure.
pub async fn foreground_activity(serial: impl Into<String>) -> Option<String> {
    let out = shell(serial, "dumpsys activity activities").await.ok()?;
    for line in out.lines() {
        if !line.contains("ResumedActivity") {
            continue;
        }
        // e.g. "topResumedActivity=ActivityRecord{hash u0 com.x/com.x.Main t8}"
        if let Some(tok) = line
            .split_whitespace()
            .find(|t| t.contains('/') && t.contains('.') && !t.contains('{'))
        {
            return Some(tok.trim_end_matches('}').to_string());
        }
    }
    None
}

/// The last `lines` of logcat in threadtime format. Best-effort; empty on error.
pub async fn recent_logcat(serial: impl Into<String>, lines: u32) -> Vec<String> {
    shell(serial, format!("logcat -d -v threadtime -t {lines}"))
        .await
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Parse a single `getprop` line of the form `[key]: [value]`. Returns `None`
/// for lines that don't match. Tolerates extra whitespace after the colon.
/// Equivalent to the regex `\[([^\]]+)\]:\s*\[([^\]]*)\]` but allocation-free.
fn parse_getprop_line(line: &str) -> Option<(&str, &str)> {
    let after_open = line.trim().strip_prefix('[')?;
    let (key, rest) = after_open.split_once(']')?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let value = rest.strip_prefix('[')?.strip_suffix(']')?;
    Some((key, value))
}

#[cfg(test)]
mod tests {
    use super::parse_getprop_line;

    #[test]
    fn parses_getprop_lines() {
        assert_eq!(
            parse_getprop_line("[ro.build.version.release]: [16]"),
            Some(("ro.build.version.release", "16"))
        );
        // values with spaces
        assert_eq!(
            parse_getprop_line("[ro.product.model]: [sdk gphone64 arm64]"),
            Some(("ro.product.model", "sdk gphone64 arm64"))
        );
        // empty value
        assert_eq!(
            parse_getprop_line("[persist.sys.timezone]: []"),
            Some(("persist.sys.timezone", ""))
        );
        // non-getprop noise
        assert_eq!(parse_getprop_line("not a prop line"), None);
        assert_eq!(parse_getprop_line(""), None);
    }
}
