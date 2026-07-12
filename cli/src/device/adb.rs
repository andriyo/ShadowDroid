//! Thin wrapper over the `adb_client` crate. Talks to the host `adbd` over
//! the ADB wire protocol (port 5037) — no shelling out to the `adb` binary,
//! so a single static Rust binary works on any machine with a running adbd
//! (no Android SDK required).
//!
//! All `adb_client` calls are synchronous. Public functions wrap each call
//! in `tokio::task::spawn_blocking` so they're safe to .await from the async
//! CLI dispatch without stalling the runtime. Every wrapper also has a host-side
//! deadline: ADB is an external service and can hang when a transport wedges.

use adb_client::ADBDeviceExt;
use adb_client::server::ADBServer;
use adb_client::server_device::ADBServerDevice;
use anyhow::{Context, Result, anyhow, bail};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc as std_mpsc};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::spawn_blocking;
use tracing::debug;

const ADB_TIMEOUT: Duration = Duration::from_secs(20);
const ADB_TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
const ADB_BLOCKING_CONCURRENCY: usize = 8;
const MOVE_OK_MARKER: &str = "__shadowdroid_move_ok__";
const DESTINATION_UNSAFE_MARKER: &str = "__shadowdroid_destination_unsafe__";
static TRANSFER_ID: AtomicU64 = AtomicU64::new(1);
static ADB_BLOCKING_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
static REMOTE_CLEANUP: OnceLock<std_mpsc::SyncSender<RemoteCleanup>> = OnceLock::new();

#[derive(Debug)]
struct RemoteCleanup {
    serial: String,
    path: String,
}

fn enqueue_remote_cleanup(serial: String, path: String) {
    let sender = REMOTE_CLEANUP.get_or_init(|| {
        let (sender, receiver) = std_mpsc::sync_channel::<RemoteCleanup>(64);
        let _ = std::thread::Builder::new()
            .name("shadowdroid-adb-cleanup".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    if let Ok(mut device) = get_device_sync(&job.serial) {
                        remove_remote_temp(&mut device, &job.path);
                    }
                }
            });
        sender
    });
    let _ = sender.try_send(RemoteCleanup { serial, path });
}

async fn bounded_blocking<T, F>(label: &'static str, timeout: Duration, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let slots = ADB_BLOCKING_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(ADB_BLOCKING_CONCURRENCY)))
        .clone();
    bounded_blocking_with_slots(label, timeout, slots, f).await
}

async fn bounded_blocking_with_slots<T, F>(
    label: &'static str,
    timeout: Duration,
    slots: Arc<Semaphore>,
    f: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let permit = match tokio::time::timeout_at(deadline, slots.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err(anyhow!("ADB blocking worker pool is closed")),
        Err(_) => return Err(adb_timeout_error(label, timeout, "waiting_for_worker")),
    };
    let task = spawn_blocking(move || {
        // A timed-out or cancelled caller drops only its JoinHandle. Keeping
        // the permit inside the closure ensures wedged native ADB calls still
        // count against the cap until they actually return.
        let _permit = permit;
        f()
    });
    match tokio::time::timeout_at(deadline, task).await {
        Ok(joined) => joined.with_context(|| format!("{label} task panicked"))?,
        Err(_) => Err(adb_timeout_error(label, timeout, "running")),
    }
}

/// Run a small atomic publication step after staging has succeeded. Queueing is
/// bounded, but once publication starts we await its definitive result: a
/// caller must never receive `adb_timeout` and then have the destination change
/// later when a detached blocking task finally resumes.
async fn blocking_publication<T, F>(label: &'static str, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let slots = ADB_BLOCKING_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(ADB_BLOCKING_CONCURRENCY)))
        .clone();
    let permit = tokio::time::timeout(ADB_TIMEOUT, slots.acquire_owned())
        .await
        .map_err(|_| adb_timeout_error(label, ADB_TIMEOUT, "waiting_for_worker"))?
        .map_err(|_| anyhow!("ADB blocking worker pool is closed"))?;
    spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .with_context(|| format!("{label} task panicked"))?
}

fn adb_timeout_error(label: &'static str, timeout: Duration, stage: &'static str) -> anyhow::Error {
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    crate::diagnostic::DiagnosticError::new(
        "adb_timeout",
        "adb",
        format!("{label} did not complete within {}ms", timeout.as_millis()),
    )
    .retryable(true)
    .detail(serde_json::json!({
        "operation": label,
        "timeout_ms": timeout_ms,
        "stage": stage,
    }))
    .next_actions(["shadowdroid devices", "shadowdroid doctor --json"])
    .into()
}

/// Return serials of devices currently in "device" state. Skips offline /
/// unauthorized / no-permissions devices — those are not actionable.
pub async fn list_devices() -> Result<Vec<String>> {
    bounded_blocking("list devices", ADB_TIMEOUT, || {
        let mut server = ADBServer::default();
        let devices = server.devices().map_err(|e| anyhow!("adb devices: {e}"))?;
        // DeviceShort stringifies as `<serial> <state>`; we want only "device"
        Ok(devices
            .into_iter()
            .filter(|d| format!("{}", d.state) == "device")
            .map(|d| d.identifier)
            .collect())
    })
    .await
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
    bounded_blocking("device shell", ADB_TIMEOUT, move || {
        shell_sync(&serial, &cmd)
    })
    .await
}

/// Run a shell command whose effects mutate device state. Once native ADB work
/// starts, await its definitive result so a host timeout can never be followed
/// by a late settings/package/process change after caller rollback.
pub async fn shell_mutating(serial: impl Into<String>, cmd: impl Into<String>) -> Result<String> {
    let serial = serial.into();
    let cmd = cmd.into();
    blocking_publication("mutating device shell", move || shell_sync(&serial, &cmd)).await
}

fn shell_sync(serial: &str, cmd: &str) -> Result<String> {
    let mut device = get_device_sync(serial)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    device
        .shell_command(&cmd, Some(&mut stdout), Some(&mut stderr))
        .map_err(|e| anyhow!("adb shell {cmd:?}: {e}"))?;
    if !stderr.is_empty() {
        debug!(
            "adb shell stderr ({serial}, {cmd:?}): {}",
            String::from_utf8_lossy(&stderr)
        );
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Install an APK on the device. Uses the ADB streaming `exec:install` path
/// under the hood (faster than `adb push` + `pm install`).
pub async fn install(serial: impl Into<String>, apk_path: impl Into<PathBuf>) -> Result<()> {
    let serial = serial.into();
    let apk_path = apk_path.into();
    blocking_publication("install APK", move || {
        let mut device = get_device_sync(&serial)?;
        device
            .install(&apk_path, None)
            .map_err(|e| anyhow!("adb install {}: {e}", apk_path.display()))
    })
    .await
}

/// Uninstall a package by name. Idempotent-ish: errors (e.g. "not installed")
/// are surfaced to the caller, which usually treats them as best-effort.
pub async fn uninstall(serial: impl Into<String>, package: impl Into<String>) -> Result<()> {
    let serial = serial.into();
    let package = package.into();
    blocking_publication("uninstall package", move || {
        let mut device = get_device_sync(&serial)?;
        device
            .uninstall(package.as_str(), None)
            .map_err(|e| anyhow!("adb uninstall {package}: {e}"))
    })
    .await
}

/// Push a local file to the device over the ADB protocol. Used as a fallback
/// when the on-device server can't reach the target path (e.g. `/sdcard` under
/// Android's scoped storage returns EPERM to the instrumentation uid).
pub async fn push(
    serial: impl Into<String>,
    local: impl Into<PathBuf>,
    remote: impl Into<String>,
) -> Result<u64> {
    let serial = serial.into();
    let local = local.into();
    let remote = remote.into();
    let staged = bounded_blocking("stage pushed file", ADB_TRANSFER_TIMEOUT, move || {
        let mut device = get_device_sync(&serial)?;
        let mut file =
            std::fs::File::open(&local).with_context(|| format!("open {}", local.display()))?;
        let bytes = file
            .metadata()
            .with_context(|| format!("stat {}", local.display()))?
            .len();
        let temp_remote = remote_temp_path(&remote)?;
        if let Err(error) = device.push(&mut file, temp_remote.as_str()) {
            remove_remote_temp(&mut device, &temp_remote);
            return Err(anyhow!("adb push {} -> {remote}: {error}", local.display()));
        }
        Ok(StagedRemotePush {
            device: Some(device),
            serial,
            temp_remote,
            remote,
            bytes,
        })
    })
    .await?;
    blocking_publication("commit pushed file", move || staged.commit()).await
}

struct StagedRemotePush {
    device: Option<ADBServerDevice>,
    serial: String,
    temp_remote: String,
    remote: String,
    bytes: u64,
}

impl StagedRemotePush {
    fn commit(mut self) -> Result<u64> {
        let device = self
            .device
            .as_mut()
            .ok_or_else(|| anyhow!("staged ADB push lost its device connection"))?;
        if let Err(error) = commit_remote_temp(device, &self.temp_remote, &self.remote) {
            remove_remote_temp(device, &self.temp_remote);
            self.device = None;
            return Err(error);
        }
        self.device = None;
        Ok(self.bytes)
    }
}

impl Drop for StagedRemotePush {
    fn drop(&mut self) {
        if self.device.is_some() {
            enqueue_remote_cleanup(self.serial.clone(), self.temp_remote.clone());
        }
    }
}

/// Pull a small device artifact to memory over the ADB protocol.
///
/// End-user file transfers must use [`pull_to_path`] so their size is bounded
/// by disk rather than process memory. This helper remains for tiny protocol
/// artifacts such as the proxy CA certificate.
pub async fn pull(serial: impl Into<String>, remote: impl Into<String>) -> Result<Vec<u8>> {
    let serial = serial.into();
    let remote = remote.into();
    bounded_blocking("pull file", ADB_TRANSFER_TIMEOUT, move || {
        let mut device = get_device_sync(&serial)?;
        let mut buf: Vec<u8> = Vec::new();
        device
            .pull(&remote.as_str(), &mut buf)
            .map_err(|e| anyhow!("adb pull {remote}: {e}"))?;
        Ok(buf)
    })
    .await
}

/// Pull a device file into a same-directory temporary file, then atomically
/// replace `local` only after the ADB sync transfer and file sync both finish.
pub async fn pull_to_path(
    serial: impl Into<String>,
    remote: impl Into<String>,
    local: impl Into<PathBuf>,
) -> Result<u64> {
    let serial = serial.into();
    let remote = remote.into();
    let local = local.into();
    let publish_path = local.clone();
    let staged = bounded_blocking("stage pulled file", ADB_TRANSFER_TIMEOUT, move || {
        let (mut temp, existing_permissions) =
            crate::transfer::atomic_temp_for_destination(&local)?;
        let mut device = get_device_sync(&serial)?;
        device
            .pull(&remote.as_str(), temp.as_file_mut())
            .map_err(|error| anyhow!("adb pull {remote}: {error}"))?;
        temp.as_file_mut()
            .flush()
            .with_context(|| format!("flush temporary file for {}", local.display()))?;
        if let Some(permissions) = existing_permissions {
            temp.as_file()
                .set_permissions(permissions)
                .with_context(|| format!("preserve permissions for {}", local.display()))?;
        }
        temp.as_file()
            .sync_all()
            .with_context(|| format!("sync temporary file for {}", local.display()))?;
        let bytes = temp
            .as_file()
            .metadata()
            .with_context(|| format!("stat temporary file for {}", local.display()))?
            .len();
        Ok(StagedLocalPull {
            temp: temp.into_temp_path(),
            bytes,
        })
    })
    .await?;
    blocking_publication("publish pulled file", move || staged.publish(&publish_path)).await
}

#[derive(Debug)]
struct StagedLocalPull {
    temp: tempfile::TempPath,
    bytes: u64,
}

impl StagedLocalPull {
    fn publish(self, destination: &Path) -> Result<u64> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        self.temp
            .persist(destination)
            .map_err(|error| error.error)
            .with_context(|| format!("atomically replace {}", destination.display()))?;
        sync_parent_best_effort(parent);
        Ok(self.bytes)
    }
}

fn remote_temp_path(remote: &str) -> Result<String> {
    let remote = remote.trim_end_matches('/');
    let (parent, name) = remote.rsplit_once('/').unwrap_or(("", remote));
    if name.is_empty() {
        bail!("remote file path is empty");
    }
    let id = TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
    let component = crate::ids::stable_file_component(name);
    let temp_name = format!(".shadowdroid-{component}-{}-{id}.tmp", std::process::id());
    Ok(match parent {
        "" => temp_name,
        "/" => format!("/{temp_name}"),
        _ => format!("{parent}/{temp_name}"),
    })
}

fn commit_remote_temp(device: &mut ADBServerDevice, temp_remote: &str, remote: &str) -> Result<()> {
    let command = commit_remote_command(temp_remote, remote);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    device
        .shell_command(&command.as_str(), Some(&mut stdout), Some(&mut stderr))
        .map_err(|error| anyhow!("commit adb push to {remote}: {error}"))?;
    let stdout = String::from_utf8_lossy(&stdout);
    if stdout.contains(DESTINATION_UNSAFE_MARKER) {
        bail!("refusing to replace non-regular remote destination {remote}");
    }
    if !stdout.contains(MOVE_OK_MARKER) {
        bail!(
            "commit adb push to {remote} failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(())
}

fn commit_remote_command(temp_remote: &str, remote: &str) -> String {
    let temp = crate::config::quote_device_shell_arg(temp_remote);
    let destination = crate::config::quote_device_shell_arg(remote);
    format!(
        "if [ -L {destination} ]; then echo {DESTINATION_UNSAFE_MARKER}; \
         elif [ -e {destination} ] && [ ! -f {destination} ]; then echo {DESTINATION_UNSAFE_MARKER}; \
         else mv -f -- {temp} {destination} && echo {MOVE_OK_MARKER}; fi"
    )
}

fn remove_remote_temp(device: &mut ADBServerDevice, temp_remote: &str) {
    let command = format!(
        "rm -f -- {}",
        crate::config::quote_device_shell_arg(temp_remote)
    );
    let _ = device.shell_command(&command.as_str(), None, None);
}

#[cfg(unix)]
fn sync_parent_best_effort(parent: &Path) {
    let result = std::fs::File::open(parent)
        .with_context(|| format!("open {} for sync", parent.display()))
        .and_then(|directory| {
            directory
                .sync_all()
                .with_context(|| format!("sync {}", parent.display()))
        });
    if let Err(error) = result {
        tracing::warn!(directory = %parent.display(), error = %error, "pulled file committed but directory sync failed");
    }
}

#[cfg(not(unix))]
fn sync_parent_best_effort(_parent: &Path) {}

/// Set up `adb forward tcp:<host_port> tcp:<device_port>`.
/// A laptop-side connect to host_port is proxied to device_port.
pub async fn forward(serial: impl Into<String>, host_port: u16, device_port: u16) -> Result<()> {
    let serial = serial.into();
    bounded_blocking("create forward", ADB_TIMEOUT, move || {
        let mut stream = adb_server_transport(&serial)?;
        let command = format!("host:forward:norebind:tcp:{host_port};tcp:{device_port}");
        match adb_server_request(&mut stream, &command) {
            Ok(()) => verify_forward_mapping(&serial, host_port, device_port),
            Err(_error) if forward_mapping_matches(&serial, host_port, device_port)? => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!("adb forward --no-rebind tcp:{host_port} tcp:{device_port}")
            }),
        }
    })
    .await
}

/// Remove a previously-set forward rule by host port.
pub async fn forward_remove(serial: impl Into<String>, host_port: u16) -> Result<()> {
    let serial = serial.into();
    bounded_blocking("remove forward", ADB_TIMEOUT, move || {
        let mut stream = adb_server_transport(&serial)?;
        adb_server_request(&mut stream, &format!("host:killforward:tcp:{host_port}"))
            .with_context(|| format!("adb forward --remove tcp:{host_port}"))
    })
    .await
}

/// Compare-and-swap one reverse mapping. The caller supplies the mapping it
/// observed before its lifecycle transition; a different current owner is
/// preserved. Replacement uses `norebind`, and a failed publish restores the
/// expected mapping before returning whenever possible.
pub async fn reverse_replace(
    serial: impl Into<String>,
    device_port: u16,
    expected_host_port: Option<u16>,
    replacement_host_port: Option<u16>,
) -> Result<()> {
    let serial = serial.into();
    bounded_blocking("replace reverse", ADB_TIMEOUT, move || {
        let current = current_reverse_host_sync(&serial, device_port)?;
        if !reverse_transition_required(current, expected_host_port, replacement_host_port)
            .with_context(|| format!("compare adb reverse tcp:{device_port}"))?
        {
            return Ok(());
        }

        if current.is_some() {
            reverse_remove_sync(&serial, device_port)?;
        }
        if let Some(replacement) = replacement_host_port
            && let Err(error) = reverse_create_norebind_sync(&serial, device_port, replacement)
        {
            let rollback = match expected_host_port {
                Some(expected) => reverse_create_norebind_sync(&serial, device_port, expected),
                None => Ok(()),
            };
            return match rollback {
                Ok(()) => Err(error).context("publish replacement adb reverse"),
                Err(rollback_error) => Err(anyhow!(
                    "publish replacement adb reverse failed: {error:#}; restoring the prior reverse also failed: {rollback_error:#}"
                )),
            };
        }
        let observed = current_reverse_host_sync(&serial, device_port)?;
        if observed != replacement_host_port {
            bail!(
                "adb reverse compare-and-swap postcondition failed for tcp:{device_port}: expected {replacement_host_port:?}, found {observed:?}"
            );
        }
        Ok(())
    })
    .await
}

fn reverse_transition_required(
    current: Option<u16>,
    expected: Option<u16>,
    replacement: Option<u16>,
) -> Result<bool> {
    if current == replacement {
        Ok(false)
    } else if current == expected {
        Ok(true)
    } else {
        bail!("refusing to replace adb reverse: expected host {expected:?}, found {current:?}")
    }
}

/// Remove a previously-set reverse rule by the device-side port.
pub async fn reverse_remove(serial: impl Into<String>, device_port: u16) -> Result<()> {
    let serial = serial.into();
    bounded_blocking("remove reverse", ADB_TIMEOUT, move || {
        let mut stream = adb_server_transport(&serial)?;
        adb_server_request(
            &mut stream,
            &format!("reverse:killforward:tcp:{device_port}"),
        )
        .with_context(|| format!("adb reverse --remove tcp:{device_port}"))
    })
    .await
}

/// One `adb reverse --list` entry. The ADB server prefixes each line with an
/// internal transport name; callers only care about the device and host socket
/// endpoints that follow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseMapping {
    pub device: String,
    pub host: String,
}

/// List reverse socket mappings for one device without shelling out to `adb`.
/// `adb_client` 3.2 supports adding/removing reverse mappings but does not
/// expose the protocol's `reverse:list-forward` command, so this read-only call
/// uses the same local ADB server wire protocol directly.
pub async fn reverse_list(serial: impl Into<String>) -> Result<Vec<ReverseMapping>> {
    let serial = serial.into();
    bounded_blocking("list reverse mappings", ADB_TIMEOUT, move || {
        reverse_list_sync(&serial)
    })
    .await
}

fn adb_server_connection() -> Result<TcpStream> {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], 5037));
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .context("connect to local ADB server")?;
    let timeout = Some(Duration::from_secs(2));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    Ok(stream)
}

fn adb_server_transport(serial: &str) -> Result<TcpStream> {
    let mut stream = adb_server_connection()?;
    adb_server_request(&mut stream, &format!("host:transport:{serial}"))?;
    Ok(stream)
}

fn forward_list_sync() -> Result<String> {
    let mut stream = adb_server_connection()?;
    adb_server_request(&mut stream, "host:list-forward")?;
    String::from_utf8(adb_server_read_hex_body(&mut stream)?)
        .context("ADB forward list was not UTF-8")
}

fn forward_mapping_matches(serial: &str, host_port: u16, device_port: u16) -> Result<bool> {
    let local = format!("tcp:{host_port}");
    let remote = format!("tcp:{device_port}");
    Ok(forward_list_sync()?.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(serial)
            && fields.next() == Some(local.as_str())
            && fields.next() == Some(remote.as_str())
    }))
}

fn verify_forward_mapping(serial: &str, host_port: u16, device_port: u16) -> Result<()> {
    if forward_mapping_matches(serial, host_port, device_port)? {
        Ok(())
    } else {
        bail!(
            "ADB acknowledged forward tcp:{host_port} -> tcp:{device_port}, but ownership verification failed"
        )
    }
}

fn reverse_list_sync(serial: &str) -> Result<Vec<ReverseMapping>> {
    let mut stream = adb_server_transport(serial)?;
    adb_server_request(&mut stream, "reverse:list-forward")?;
    let body = adb_server_read_hex_body(&mut stream)?;
    let output = String::from_utf8(body).context("ADB reverse list was not UTF-8")?;
    Ok(parse_reverse_list(&output))
}

fn current_reverse_host_sync(serial: &str, device_port: u16) -> Result<Option<u16>> {
    let device = format!("tcp:{device_port}");
    let mut matches = reverse_list_sync(serial)?
        .into_iter()
        .filter(|mapping| mapping.device == device);
    let Some(mapping) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        bail!("multiple adb reverse mappings exist for tcp:{device_port}");
    }
    let host = mapping
        .host
        .strip_prefix("tcp:")
        .ok_or_else(|| anyhow!("unexpected adb reverse host endpoint {}", mapping.host))?;
    let port = host
        .parse::<u16>()
        .with_context(|| format!("parse adb reverse host endpoint {}", mapping.host))?;
    if port == 0 {
        bail!("adb reverse host port cannot be zero");
    }
    Ok(Some(port))
}

fn reverse_create_norebind_sync(serial: &str, device_port: u16, host_port: u16) -> Result<()> {
    let mut stream = adb_server_transport(serial)?;
    let command = format!("reverse:forward:norebind:tcp:{device_port};tcp:{host_port}");
    match adb_server_request(&mut stream, &command) {
        Ok(()) => verify_reverse_mapping_sync(serial, device_port, host_port),
        Err(_error) if reverse_mapping_matches_sync(serial, device_port, host_port)? => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("adb reverse --no-rebind tcp:{device_port} tcp:{host_port}")),
    }
}

fn reverse_remove_sync(serial: &str, device_port: u16) -> Result<()> {
    let mut stream = adb_server_transport(serial)?;
    adb_server_request(
        &mut stream,
        &format!("reverse:killforward:tcp:{device_port}"),
    )
    .with_context(|| format!("adb reverse --remove tcp:{device_port}"))
}

fn reverse_mapping_matches_sync(serial: &str, device_port: u16, host_port: u16) -> Result<bool> {
    let device = format!("tcp:{device_port}");
    let host = format!("tcp:{host_port}");
    Ok(reverse_list_sync(serial)?
        .iter()
        .any(|mapping| mapping.device == device && mapping.host == host))
}

fn verify_reverse_mapping_sync(serial: &str, device_port: u16, host_port: u16) -> Result<()> {
    if reverse_mapping_matches_sync(serial, device_port, host_port)? {
        Ok(())
    } else {
        bail!(
            "ADB acknowledged reverse tcp:{device_port} -> tcp:{host_port}, but ownership verification failed"
        )
    }
}

fn adb_server_request(stream: &mut TcpStream, command: &str) -> Result<()> {
    let request = format!("{:04x}{command}", command.len());
    stream.write_all(request.as_bytes())?;

    let mut status = [0_u8; 4];
    stream.read_exact(&mut status)?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let message = String::from_utf8_lossy(&adb_server_read_hex_body(stream)?).into_owned();
            bail!("ADB server rejected {command:?}: {message}")
        }
        other => bail!(
            "unexpected ADB server response to {command:?}: {:?}",
            String::from_utf8_lossy(other)
        ),
    }
}

fn adb_server_read_hex_body(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = usize::from_str_radix(std::str::from_utf8(&length)?, 16)
        .context("parse ADB response length")?;
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn parse_reverse_list(output: &str) -> Vec<ReverseMapping> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().rev();
            let host = fields.next()?;
            let device = fields.next()?;
            Some(ReverseMapping {
                device: device.to_string(),
                host: host.to_string(),
            })
        })
        .collect()
}

/// Force-stop a package via `am force-stop`. Idempotent — safe to call when
/// the package isn't running.
pub async fn am_force_stop(serial: impl Into<String>, package: impl AsRef<str>) -> Result<()> {
    shell_mutating(serial, format!("am force-stop {}", package.as_ref())).await?;
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
    shell_mutating(serial, cmd).await?;
    Ok(())
}

/// Kill only lingering `app_process` wrappers owned by ShadowDroid. Other tools
/// such as uiautomator2 may legitimately own the single UiAutomation slot; an
/// implicit connect/disconnect must never destroy them.
pub async fn kill_instrument_zombies(serial: impl Into<String>) -> Result<()> {
    let serial = serial.into();
    // The `am instrument` wrapper command line contains our test package. Match
    // that ownership marker before killing instead of selecting every
    // `app_process` on the device.
    let _ = shell_mutating(
        &serial,
        "ps -A -o PID,ARGS | grep app_process | grep 'io.github.andriyo.shadowdroid.test' | grep -v grep | awk '{print $1}' | xargs -r kill -9 2>/dev/null",
    )
    .await;
    // Then: nuke the actual test process by package. force-stop the app under
    // test too — its UiAutomation registration leaks into the system until the
    // process dies completely.
    let _ = shell_mutating(&serial, "am force-stop io.github.andriyo.shadowdroid.test").await;
    let _ = shell_mutating(&serial, "am force-stop io.github.andriyo.shadowdroid").await;
    // Give system_server a beat to actually release the UiAutomation slot.
    // Without this, the very next `am instrument` races and hits
    // "UiAutomationService already registered!".
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    Ok(())
}

/// Explicit takeover used only by `doctor --fix --force`. Unlike normal
/// lifecycle cleanup this may stop foreign shell-hosted UiAutomation tooling.
pub async fn kill_all_ui_automation_owners(serial: impl Into<String>) -> Result<()> {
    let serial = serial.into();
    let _ = shell_mutating(
        &serial,
        "ps -A -o PID,ARGS | grep -E 'app_process|uiautomator|com.wetest.uia2.Main|atx' | grep -v grep | awk '{print $1}' | xargs -r kill -9 2>/dev/null",
    )
    .await;
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

/// List a directory via `adb shell ls` — the host-side fallback for `files ls`
/// when the on-device server can only reach its scoped-storage sandbox. `-L`
/// dereferences symlinks (so `/sdcard` resolves to its target), and the long
/// format is parsed into the same `{name, size, is_dir}` shape the server
/// returns. `ls` reports failures as `ls: <path>: <reason>`; we merge stderr
/// (`2>&1`, honoured because `adb shell` runs through the device's real sh) so
/// those surface as an error instead of an empty listing.
pub async fn list_dir(
    serial: impl Into<String>,
    remote: impl AsRef<str>,
) -> Result<Vec<crate::proto::FileEntry>> {
    let remote = remote.as_ref();
    let out = shell(serial, format!("ls -lLA {} 2>&1", sh_single_quote(remote))).await?;
    if let Some(err) = out.lines().find(|l| l.trim_start().starts_with("ls:")) {
        bail!("{}", err.trim());
    }
    Ok(parse_ls_long(&out))
}

/// Parse `ls -l` long-format output into `{name, size, is_dir}` entries.
/// Pure (no I/O) so it can be unit-tested against toybox sample output.
fn parse_ls_long(out: &str) -> Vec<crate::proto::FileEntry> {
    let mut entries = Vec::new();
    for line in out.lines() {
        let line = line.trim_end();
        // Skip the `total N` header and blank lines.
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        // perms links owner group size date time name…
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 8 {
            continue;
        }
        let is_dir = line.starts_with('d');
        let size = cols[4].parse::<u64>().unwrap_or(0);
        let mut name = cols[7..].join(" ");
        // Symlinks render as `name -> target`; keep just the name.
        if let Some(idx) = name.find(" -> ") {
            name.truncate(idx);
        }
        if name.is_empty() {
            continue;
        }
        entries.push(crate::proto::FileEntry { name, size, is_dir });
    }
    entries
}

/// Single-quote a string for the device shell, escaping embedded quotes.
fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
    bounded_blocking("list devices with state", ADB_TIMEOUT, || {
        let mut server = ADBServer::default();
        let devices = server.devices().map_err(|e| anyhow!("adb devices: {e}"))?;
        Ok(devices
            .into_iter()
            .map(|d| (d.identifier, format!("{}", d.state)))
            .collect())
    })
    .await
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
/// `device_model`, `device_manufacturer`, plus emulator/form-factor identity)
/// parsed from `getprop`. Shared by
/// crash events ([crate::watch]) and `collect`. Best-effort: missing props are
/// simply omitted.
pub async fn device_info(serial: impl Into<String>) -> serde_json::Value {
    let out = shell(serial, "getprop").await.unwrap_or_default();
    let wanted = [
        ("ro.build.version.release", "android_release"),
        ("ro.build.version.sdk", "android_sdk"),
        ("ro.product.model", "device_model"),
        ("ro.product.manufacturer", "device_manufacturer"),
        ("ro.boot.qemu.avd_name", "avd"),
        ("ro.build.characteristics", "build_characteristics"),
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
    use super::{
        ReverseMapping, StagedLocalPull, bounded_blocking_with_slots, commit_remote_command,
        parse_getprop_line, parse_ls_long, parse_reverse_list, remote_temp_path,
    };
    use std::io::Write as _;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn timed_out_blocking_calls_keep_their_worker_slot_until_exit() {
        let slots = Arc::new(Semaphore::new(1));
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let running_error = bounded_blocking_with_slots(
            "wedged operation",
            Duration::from_millis(20),
            slots.clone(),
            move || {
                let _ = release_rx.recv();
                Ok(())
            },
        )
        .await
        .unwrap_err();
        let running = running_error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(running.code, "adb_timeout");
        assert_eq!(running.detail["stage"], "running");

        let waiting_error = bounded_blocking_with_slots(
            "queued operation",
            Duration::from_millis(20),
            slots.clone(),
            || Ok(()),
        )
        .await
        .unwrap_err();
        let waiting = waiting_error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(waiting.code, "adb_timeout");
        assert_eq!(waiting.detail["stage"], "waiting_for_worker");

        release_tx.send(()).unwrap();
        bounded_blocking_with_slots("recovered operation", Duration::from_secs(1), slots, || {
            Ok(())
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn timed_out_staging_cannot_publish_a_local_destination_later() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("pull.bin");
        std::fs::write(&destination, b"old").unwrap();
        let stage_destination = destination.clone();
        let slots = Arc::new(Semaphore::new(1));
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let error = bounded_blocking_with_slots(
            "stage test pull",
            Duration::from_millis(20),
            slots,
            move || {
                let (mut temp, _) =
                    crate::transfer::atomic_temp_for_destination(&stage_destination)?;
                temp.write_all(b"new")?;
                temp.as_file().sync_all()?;
                let _ = release_rx.recv();
                Ok(StagedLocalPull {
                    temp: temp.into_temp_path(),
                    bytes: 3,
                })
            },
        )
        .await
        .unwrap_err();
        assert_eq!(crate::cli::error_code_of(&error), "adb_timeout");
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");

        release_tx.send(()).unwrap();
        for _ in 0..100 {
            if std::fs::read_dir(dir.path()).unwrap().count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn remote_transfer_temps_are_unique_siblings() {
        let first = remote_temp_path("/sdcard/My File.bin").unwrap();
        let second = remote_temp_path("/sdcard/My File.bin").unwrap();
        assert!(first.starts_with("/sdcard/.shadowdroid-My_File.bin-"));
        assert!(first.ends_with(".tmp"));
        assert_ne!(first, second);

        assert!(
            remote_temp_path("relative.bin")
                .unwrap()
                .starts_with(".shadowdroid-")
        );
        assert!(remote_temp_path("/").is_err());

        let long_name = "x".repeat(240);
        let temp = remote_temp_path(&format!("/sdcard/{long_name}")).unwrap();
        let component = temp.rsplit('/').next().unwrap();
        assert!(component.len() <= 255, "{component}");
    }

    #[test]
    fn remote_commit_rejects_symlinks_and_special_nodes_before_mv() {
        let command = commit_remote_command(
            "/sdcard/.shadowdroid-temp",
            "/sdcard/destination with space",
        );
        assert!(command.contains("[ -L '/sdcard/destination with space' ]"));
        assert!(command.contains("[ ! -f '/sdcard/destination with space' ]"));
        assert!(command.contains("__shadowdroid_destination_unsafe__"));
        assert!(command.contains("mv -f -- '/sdcard/.shadowdroid-temp'"));
    }

    #[test]
    fn parses_reverse_list_endpoints() {
        let mappings = parse_reverse_list(
            "host-16 tcp:8080 tcp:43127\ntransport-id-3 localabstract:debug tcp:9000\n",
        );
        assert_eq!(
            mappings,
            vec![
                ReverseMapping {
                    device: "tcp:8080".into(),
                    host: "tcp:43127".into(),
                },
                ReverseMapping {
                    device: "localabstract:debug".into(),
                    host: "tcp:9000".into(),
                },
            ]
        );
        assert!(parse_reverse_list("\n").is_empty());
    }

    #[test]
    fn reverse_compare_and_swap_preserves_unexpected_owners() {
        assert!(!super::reverse_transition_required(Some(42), None, Some(42)).unwrap());
        assert!(super::reverse_transition_required(Some(41), Some(41), Some(42)).unwrap());
        assert!(super::reverse_transition_required(None, None, Some(42)).unwrap());
        assert!(super::reverse_transition_required(Some(99), Some(41), Some(42)).is_err());
    }

    #[test]
    fn parses_ls_long_format() {
        // Real toybox `ls -lLA /sdcard/` sample: total header, dirs, a file,
        // a name with spaces, and a symlink with a ` -> target` suffix.
        let out = "total 136\n\
            drwxrws--- 2 u0_a205  media_rw 4096 2026-05-29 15:53 Alarms\n\
            drwxrws--x 5 media_rw media_rw 4096 2026-05-29 15:53 Android\n\
            -rw-rw---- 1 u0_a205  media_rw   33 2026-06-13 00:31 sd_push_test.txt\n\
            -rw-rw---- 1 u0_a205  media_rw   12 2026-06-13 00:31 My Notes.txt\n\
            lrwxrwxrwx 1 root     root        7 2026-06-13 00:31 link -> Android\n";
        let entries = parse_ls_long(out);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].name, "Alarms");
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].size, 4096);
        assert_eq!(entries[2].name, "sd_push_test.txt");
        assert!(!entries[2].is_dir);
        assert_eq!(entries[2].size, 33);
        // name with spaces is preserved (everything after the time column)
        assert_eq!(entries[3].name, "My Notes.txt");
        // symlink keeps just the name, drops ` -> target`
        assert_eq!(entries[4].name, "link");
        assert!(!entries[4].is_dir);
    }

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
