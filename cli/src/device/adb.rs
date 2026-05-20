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
