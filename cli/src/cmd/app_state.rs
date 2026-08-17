//! Private debuggable-app state transfer and transactional snapshot/restore.
//!
//! File contents only travel through byte-preserving ADB streams and protected
//! host/device staging files. Stdout contains metadata, hashes, and warnings —
//! never preferences, tokens, databases, or session bodies.

use crate::cmd::artifact;
use crate::config::validate_android_package;
use crate::device::adb;
use crate::events::emit_action;
use crate::ids::Serial;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SNAPSHOT_TYPE: &str = "app_state_snapshot";
const FORMAT_VERSION: u32 = 1;
const PRIVATE_CONTROL_DIR: &str = ".shadowdroid_state";
const PENDING_PATH: &str = ".shadowdroid_state/pending";
const COMMIT_OK: &str = "__shadowdroid_state_commit_ok__";
const FINALIZE_OK: &str = "__shadowdroid_state_finalize_ok__";
const ROLLBACK_OK: &str = "__shadowdroid_state_rollback_ok__";
const PENDING_CLAIM_OK: &str = "__shadowdroid_state_pending_claim_ok__";
const PHASE_WRITE_OK: &str = "__shadowdroid_state_phase_write_ok__";
const COPY_OK: &str = "__shadowdroid_private_copy_ok__";
const PHASE_PREPARED: &str = "prepared";
const PHASE_VERIFIED: &str = "verified";
const TEST_FAILPOINT_ENV: &str = "SHADOWDROID_TEST_APP_STATE_FAILPOINT";
const TEST_FAILPOINT_MARKER: &str = "__shadowdroid_app_state_failpoint__:";
static TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Args)]
pub struct StateArgs {
    #[command(subcommand)]
    pub cmd: StateCmd,
}

#[derive(Debug, Subcommand)]
pub enum StateCmd {
    /// Snapshot selected private debuggable-app paths into a protected host directory.
    Snapshot {
        /// App package or configured app alias. Defaults through normal app resolution.
        #[arg(long)]
        app: Option<String>,
        /// New snapshot directory. Existing paths are refused.
        #[arg(long)]
        out: PathBuf,
        /// Private relative file/directory to include; repeat for multiple roots.
        #[arg(long = "include", required = true)]
        include: Vec<String>,
    },
    /// Restore a snapshot transactionally, retaining private rollback data until verification.
    Restore {
        /// Target app package or configured app alias. Defaults to the manifest package.
        #[arg(long)]
        app: Option<String>,
        /// Snapshot directory containing manifest.json and data/.
        #[arg(long = "from")]
        from: PathBuf,
        /// Override package/signature compatibility refusal. This is unsafe and explicit.
        #[arg(long)]
        allow_incompatible: bool,
    },
    /// Resolve an interrupted restore by rolling back unverified state or finalizing verified state.
    Recover {
        /// App package or configured app alias. Defaults through normal app resolution.
        #[arg(long)]
        app: Option<String>,
    },
    /// Best-effort overwrite and delete of a protected host snapshot directory.
    Cleanup {
        /// Snapshot directory to securely clean up.
        #[arg(long = "from")]
        from: PathBuf,
    },
}

impl StateArgs {
    pub fn requested_app(&self) -> Option<&str> {
        match &self.cmd {
            StateCmd::Snapshot { app, .. }
            | StateCmd::Restore { app, .. }
            | StateCmd::Recover { app } => app.as_deref(),
            StateCmd::Cleanup { .. } => None,
        }
    }

    pub fn needs_device(&self) -> bool {
        !matches!(self.cmd, StateCmd::Cleanup { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotManifest {
    #[serde(rename = "type")]
    artifact_type: String,
    format_version: u32,
    created_at_ms: u64,
    package: String,
    version_code: u64,
    version_name: Option<String>,
    signature_digest: String,
    signature_digest_source: String,
    contains_sensitive_data: bool,
    encrypted: bool,
    privacy: SnapshotPrivacy,
    roots: Vec<SnapshotRoot>,
    directories: Vec<StateDirectory>,
    files: Vec<StateFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotPrivacy {
    host_directory_mode: String,
    host_file_mode: String,
    warning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotRoot {
    path: String,
    kind: StatePathKind,
    #[serde(default)]
    implicit_sqlite_sidecar: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StatePathKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateDirectory {
    path: String,
    mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    path: String,
    bytes: u64,
    sha256: String,
    mode: u32,
}

#[derive(Debug)]
struct PackageMetadata {
    version_code: u64,
    version_name: Option<String>,
    signature_digest: String,
    debuggable: bool,
}

#[derive(Debug, Clone, Copy)]
struct PrivateStat {
    kind: StatePathKind,
    bytes: u64,
    mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransactionRoot {
    path: String,
    existed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionPhase {
    Legacy,
    Prepared,
    Verified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingTerminalState {
    Pending(String),
    Terminal,
    Missing,
}

pub async fn run(
    args: &StateArgs,
    serial: Option<&Serial>,
    resolved_package: Option<String>,
) -> Result<()> {
    match &args.cmd {
        StateCmd::Snapshot { out, include, .. } => {
            let serial = require_serial(serial)?;
            let package = require_package(resolved_package, "app state snapshot")?;
            snapshot(serial, &package, out, include).await
        }
        StateCmd::Restore {
            from,
            allow_incompatible,
            ..
        } => {
            let serial = require_serial(serial)?;
            restore(serial, resolved_package, from, *allow_incompatible).await
        }
        StateCmd::Recover { .. } => {
            let serial = require_serial(serial)?;
            let package = require_package(resolved_package, "app state recover")?;
            recover(serial, &package).await
        }
        StateCmd::Cleanup { from } => cleanup(from),
    }
}

fn require_serial(serial: Option<&Serial>) -> Result<&Serial> {
    serial.context("app state command requires an attached device")
}

fn require_package(package: Option<String>, command: &str) -> Result<String> {
    package.ok_or_else(|| {
        crate::diagnostic::DiagnosticError::new(
            "app_required",
            "app_state",
            format!("shadowdroid {command} needs --app or a configured/default app"),
        )
        .next_actions([format!(
            "rerun with `shadowdroid {command} --app <package>`"
        )])
        .into()
    })
}

pub async fn private_pull(
    serial: &Serial,
    package: &str,
    remote: &str,
    local: &Path,
) -> Result<()> {
    ensure_run_as(serial, package).await?;
    let remote = normalize_private_path(remote)?;
    let stat = private_stat(serial, package, &remote).await?;
    if stat.kind != StatePathKind::File {
        return Err(crate::diagnostic::DiagnosticError::new(
            "private_path_not_file",
            "files",
            "run-as pull currently requires a regular private file",
        )
        .detail(json!({"remote": remote, "kind": stat.kind}))
        .next_actions([
            "select a regular private file, or use `app state snapshot --include <directory>`",
        ])
        .into());
    }
    let bytes = read_private_file(serial, package, &remote, stat.bytes).await?;
    artifact::write_bytes(local, &bytes)?;
    protect_host_file(local)?;
    emit_action(
        "private_pull",
        &json!({
            "app": package,
            "remote": remote,
            "local": local.display().to_string(),
            "bytes": bytes.len(),
            "sha256": sha256_bytes(&bytes),
            "mode": stat.mode,
            "via": "run-as",
            "contains_sensitive_data": true,
            "contents_printed": false,
        }),
    );
    Ok(())
}

pub async fn private_push(
    serial: &Serial,
    package: &str,
    local: &Path,
    remote: &str,
    requested_mode: Option<u32>,
) -> Result<()> {
    ensure_run_as(serial, package).await?;
    let metadata = std::fs::metadata(local)
        .with_context(|| format!("reading private push source {}", local.display()))?;
    if !metadata.is_file() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "local_path_not_file",
            "files",
            "run-as push source must be a regular host file",
        )
        .detail(json!({"local": local.display().to_string()}))
        .into());
    }
    let remote = normalize_private_path(remote)?;
    let existing_mode = match private_stat(serial, package, &remote).await {
        Ok(stat) if stat.kind == StatePathKind::File => Some(stat.mode),
        _ => None,
    };
    let mode = requested_mode.or(existing_mode).unwrap_or(0o600);
    validate_mode(mode)?;
    copy_host_file_to_private(serial, package, local, &remote, mode).await?;
    let expected = std::fs::read(local)
        .with_context(|| format!("verifying private push source {}", local.display()))?;
    let observed = read_private_file(serial, package, &remote, expected.len() as u64).await?;
    if sha256_bytes(&observed) != sha256_bytes(&expected) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "private_push_verification_failed",
            "files",
            "private push hash verification failed after the atomic move",
        )
        .detail(json!({
            "app": package,
            "remote": remote,
            "expected_bytes": expected.len(),
            "actual_bytes": observed.len(),
        }))
        .next_actions(["force-stop the app if it may be rewriting the file, then retry"])
        .into());
    }
    let stat = private_stat(serial, package, &remote).await?;
    if stat.mode != mode {
        return Err(crate::diagnostic::DiagnosticError::new(
            "private_push_mode_mismatch",
            "files",
            "private push mode verification failed",
        )
        .detail(json!({"remote": remote, "expected_mode": mode, "actual_mode": stat.mode}))
        .into());
    }
    emit_action(
        "private_push",
        &json!({
            "app": package,
            "local": local.display().to_string(),
            "remote": remote,
            "bytes": expected.len(),
            "sha256": sha256_bytes(&expected),
            "mode": mode,
            "via": "run-as",
            "contains_sensitive_data": true,
            "contents_printed": false,
        }),
    );
    Ok(())
}

pub async fn private_list(serial: &Serial, package: &str, remote: &str) -> Result<()> {
    ensure_run_as(serial, package).await?;
    let remote = normalize_private_path(remote)?;
    let stat = private_stat(serial, package, &remote).await?;
    if stat.kind != StatePathKind::Directory {
        return Err(crate::diagnostic::DiagnosticError::new(
            "private_path_not_directory",
            "files",
            "run-as ls requires a private directory",
        )
        .detail(json!({"remote": remote}))
        .into());
    }
    let bytes = run_as_bytes(
        serial,
        package,
        &format!(
            "find {} -mindepth 1 -maxdepth 1 -print0",
            shell_quote(&remote)
        ),
    )
    .await?;
    let mut entries = nul_paths(&bytes)?;
    entries.sort();
    emit_action(
        "private_ls",
        &json!({"app": package, "remote": remote, "entries": entries, "via": "run-as"}),
    );
    Ok(())
}

async fn ensure_run_as(serial: &Serial, package: &str) -> Result<PackageMetadata> {
    validate_android_package(package)?;
    let metadata = package_metadata(serial, package).await?;
    if !metadata.debuggable {
        return Err(crate::diagnostic::DiagnosticError::new(
            "package_not_debuggable",
            "app_state",
            "private app state access requires an installed debuggable package",
        )
        .detail(json!({"app": package}))
        .next_actions(["install a debuggable APK signed compatibly with the state snapshot"])
        .into());
    }
    let output = run_as_text(
        serial,
        package,
        "id >/dev/null && echo __shadowdroid_run_as_ok__",
    )
    .await?;
    if !output.contains("__shadowdroid_run_as_ok__") {
        return Err(crate::diagnostic::DiagnosticError::new(
            "run_as_unavailable",
            "app_state",
            "Android run-as did not accept the package",
        )
        .detail(json!({"app": package}))
        .next_actions([
            "verify the installed package is debuggable and belongs to the current Android user",
            "reinstall a debug build, then retry",
        ])
        .into());
    }
    Ok(metadata)
}

async fn package_metadata(serial: &Serial, package: &str) -> Result<PackageMetadata> {
    validate_android_package(package)?;
    let dump = adb::shell(serial, format!("dumpsys package {}", shell_quote(package))).await?;
    let version_code = dump
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("versionCode=")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        })
        .ok_or_else(|| {
            crate::diagnostic::DiagnosticError::new(
                "package_not_found",
                "app_state",
                "could not read installed package metadata",
            )
            .detail(json!({"app": package}))
        })?;
    let version_name = dump.lines().find_map(|line| {
        line.trim()
            .strip_prefix("versionName=")
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "null")
            .map(str::to_string)
    });
    let signature_identity = dump
        .lines()
        .find(|line| {
            line.trim_start()
                .starts_with("signatures=PackageSignatures")
        })
        .and_then(signature_identity)
        .context("installed package metadata has no signing identity")?;
    Ok(PackageMetadata {
        version_code,
        version_name,
        signature_digest: sha256_bytes(signature_identity.as_bytes()),
        debuggable: dump.lines().any(|line| {
            let line = line.trim_start();
            (line.starts_with("flags=[") || line.starts_with("pkgFlags=["))
                && line.contains("DEBUGGABLE")
        }),
    })
}

fn signature_identity(line: &str) -> Option<&str> {
    line.split_once("signatures:[")?
        .1
        .split_once(']')
        .map(|(identity, _)| identity.trim())
        .filter(|identity| !identity.is_empty())
}

async fn private_stat(serial: &Serial, package: &str, path: &str) -> Result<PrivateStat> {
    let output = run_as_text(
        serial,
        package,
        &format!("stat -c '%F|%s|%a' {} 2>/dev/null", shell_quote(path)),
    )
    .await?;
    parse_private_stat(output.trim()).ok_or_else(|| {
        crate::diagnostic::DiagnosticError::new(
            "private_path_not_found",
            "app_state",
            "private path is missing or cannot be inspected through run-as",
        )
        .detail(json!({"app": package, "path": path}))
        .next_actions(["check the relative path with `shadowdroid files ls --run-as --app <package> <directory>`"])
        .into()
    })
}

fn parse_private_stat(value: &str) -> Option<PrivateStat> {
    let mut fields = value.lines().last()?.split('|');
    let kind = match fields.next()?.trim() {
        "regular file" => StatePathKind::File,
        "directory" => StatePathKind::Directory,
        _ => return None,
    };
    let bytes = fields.next()?.trim().parse().ok()?;
    let mode = u32::from_str_radix(fields.next()?.trim(), 8).ok()?;
    (fields.next().is_none()).then_some(PrivateStat { kind, bytes, mode })
}

async fn private_exists(serial: &Serial, package: &str, path: &str) -> Result<bool> {
    let output = run_as_text(
        serial,
        package,
        &format!(
            "if [ -e {} ]; then echo 1; else echo 0; fi",
            shell_quote(path)
        ),
    )
    .await?;
    Ok(output.lines().last().is_some_and(|line| line.trim() == "1"))
}

async fn read_private_file(
    serial: &Serial,
    package: &str,
    path: &str,
    expected_bytes: u64,
) -> Result<Vec<u8>> {
    let bytes = run_as_bytes(serial, package, &format!("cat {}", shell_quote(path))).await?;
    if bytes.len() as u64 != expected_bytes {
        return Err(crate::diagnostic::DiagnosticError::new(
            "private_file_truncated",
            "app_state",
            "private file byte count changed during transfer",
        )
        .retryable(true)
        .detail(json!({
            "path": path,
            "expected_bytes": expected_bytes,
            "actual_bytes": bytes.len(),
        }))
        .next_actions(["force-stop the app and retry the transfer"])
        .into());
    }
    Ok(bytes)
}

async fn copy_host_file_to_private(
    serial: &Serial,
    package: &str,
    local: &Path,
    destination: &str,
    mode: u32,
) -> Result<()> {
    validate_mode(mode)?;
    let id = TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
    let remote_stage = format!(
        "/data/local/tmp/shadowdroid-private-{}-{}-{}",
        std::process::id(),
        now_ms(),
        id
    );
    adb::push(serial, local.to_path_buf(), remote_stage.clone()).await?;
    let chmod = adb::shell_mutating(
        serial,
        format!(
            "chmod 0644 {} && echo {COPY_OK}",
            shell_quote(&remote_stage)
        ),
    )
    .await?;
    if !chmod.contains(COPY_OK) {
        let _ = cleanup_remote_stage(serial, &remote_stage).await;
        return Err(crate::diagnostic::DiagnosticError::new(
            "private_stage_failed",
            "app_state",
            "could not make the protected ADB staging file readable to run-as",
        )
        .detail(json!({"app": package, "destination": destination}))
        .into());
    }
    let parent = private_parent(destination);
    let private_temp = format!("{destination}.shadowdroid-tmp-{id}");
    let script = format!(
        "mkdir -p {parent} && cp {source} {temp} && chmod {mode:o} {temp} && mv {temp} {destination} && echo {COPY_OK}",
        parent = shell_quote(parent),
        source = shell_quote(&remote_stage),
        temp = shell_quote(&private_temp),
        destination = shell_quote(destination),
    );
    let copy_result = run_as_mutating(serial, package, &script).await;
    let cleanup_result = cleanup_remote_stage(serial, &remote_stage).await;
    let output = copy_result?;
    cleanup_result?;
    if !output.contains(COPY_OK) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "private_copy_failed",
            "app_state",
            "run-as could not atomically publish the private file",
        )
        .detail(json!({"app": package, "destination": destination}))
        .next_actions(["verify the private parent path is writable by the debuggable app"])
        .into());
    }
    Ok(())
}

async fn cleanup_remote_stage(serial: &Serial, remote_stage: &str) -> Result<()> {
    adb::shell_mutating(serial, format!("rm -f {}", shell_quote(remote_stage)))
        .await
        .map(|_| ())
}

async fn run_as_text(serial: &Serial, package: &str, script: &str) -> Result<String> {
    adb::shell(
        serial,
        format!(
            "run-as {} sh -c {}",
            shell_quote(package),
            shell_quote(script)
        ),
    )
    .await
}

async fn run_as_mutating(serial: &Serial, package: &str, script: &str) -> Result<String> {
    adb::shell_mutating(
        serial,
        format!(
            "run-as {} sh -c {}",
            shell_quote(package),
            shell_quote(script)
        ),
    )
    .await
}

async fn run_as_bytes(serial: &Serial, package: &str, script: &str) -> Result<Vec<u8>> {
    adb::shell_bytes(
        serial,
        format!(
            "run-as {} sh -c {}",
            shell_quote(package),
            shell_quote(script)
        ),
    )
    .await
}

fn normalize_private_path(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\0')
        || value.contains('\n')
        || value.contains('\r')
        || value.contains('\t')
        || value.contains('\\')
    {
        return Err(unsafe_private_path(value));
    }
    let mut parts = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| unsafe_private_path(value))?;
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_private_path(value));
            }
        }
    }
    if parts.is_empty() || parts[0] == PRIVATE_CONTROL_DIR {
        return Err(unsafe_private_path(value));
    }
    Ok(parts.join("/"))
}

fn unsafe_private_path(value: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "unsafe_private_path",
        "input",
        "private paths must be relative, normalized, and outside ShadowDroid transaction storage",
    )
    .detail(json!({"path": value}))
    .next_actions(["use a relative app-data path such as `shared_prefs` or `files/state.json`"])
    .into()
}

fn private_parent(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or(".")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn validate_mode(mode: u32) -> Result<()> {
    if mode <= 0o777 {
        Ok(())
    } else {
        Err(crate::diagnostic::DiagnosticError::new(
            "invalid_mode",
            "input",
            "private file mode must be in the 000..777 range",
        )
        .detail(json!({"mode": mode}))
        .into())
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

async fn snapshot(serial: &Serial, package: &str, out: &Path, includes: &[String]) -> Result<()> {
    if out.exists() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "snapshot_destination_exists",
            "app_state",
            "app state snapshot refuses to replace an existing path",
        )
        .detail(json!({"out": out.display().to_string()}))
        .next_actions([
            "choose a new --out path or run `app state cleanup --from <snapshot>` first",
        ])
        .into());
    }
    let metadata = ensure_run_as(serial, package).await?;
    reject_pending_transaction(serial, package).await?;
    force_stop_private_app(serial, package).await?;

    let roots = snapshot_roots(serial, package, includes).await?;
    let (directories, files) = enumerate_snapshot(serial, package, &roots).await?;
    let parent = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating snapshot parent {}", parent.display()))?;
    let temp = tempfile::Builder::new()
        .prefix(".shadowdroid-state-")
        .tempdir_in(parent)
        .with_context(|| format!("creating temporary snapshot beside {}", out.display()))?;
    protect_host_directory(temp.path())?;
    let data_root = temp.path().join("data");
    std::fs::create_dir_all(&data_root).context("creating snapshot data directory")?;
    protect_host_directory(&data_root)?;

    let mut manifest_files = Vec::with_capacity(files.len());
    for (path, stat) in files {
        let bytes = read_private_file(serial, package, &path, stat.bytes).await?;
        let host_path = data_root.join(&path);
        create_protected_parent(&host_path, temp.path())?;
        artifact::write_bytes(&host_path, &bytes)?;
        protect_host_file(&host_path)?;
        manifest_files.push(StateFile {
            path,
            bytes: bytes.len() as u64,
            sha256: sha256_bytes(&bytes),
            mode: stat.mode,
        });
    }
    let manifest = SnapshotManifest {
        artifact_type: SNAPSHOT_TYPE.into(),
        format_version: FORMAT_VERSION,
        created_at_ms: now_ms(),
        package: package.into(),
        version_code: metadata.version_code,
        version_name: metadata.version_name,
        signature_digest: metadata.signature_digest,
        signature_digest_source: "sha256_of_android_dumpsys_signature_identity".into(),
        contains_sensitive_data: true,
        encrypted: false,
        privacy: SnapshotPrivacy {
            host_directory_mode: "0700".into(),
            host_file_mode: "0600".into(),
            warning: "Snapshot contains private app state and is not encrypted; keep it local or encrypt it externally".into(),
        },
        roots,
        directories,
        files: manifest_files,
    };
    let manifest_path = temp.path().join("manifest.json");
    artifact::write_json(&manifest_path, &serde_json::to_value(&manifest)?)?;
    protect_host_file(&manifest_path)?;
    let kept = temp.keep();
    if let Err(error) = std::fs::rename(&kept, out) {
        let _ = std::fs::remove_dir_all(&kept);
        return Err(error).with_context(|| format!("publishing snapshot {}", out.display()));
    }
    let total_bytes = manifest.files.iter().map(|file| file.bytes).sum::<u64>();
    emit_action(
        "app_state_snapshot",
        &json!({
            "app": package,
            "snapshot": out.display().to_string(),
            "manifest": out.join("manifest.json").display().to_string(),
            "version_code": manifest.version_code,
            "version_name": manifest.version_name,
            "signature_digest": manifest.signature_digest,
            "roots": manifest.roots,
            "file_count": manifest.files.len(),
            "directory_count": manifest.directories.len(),
            "bytes": total_bytes,
            "contains_sensitive_data": true,
            "encrypted": false,
            "contents_printed": false,
            "host_permissions": {"directory": "0700", "files": "0600"},
            "next_actions": [
                format!("shadowdroid app state restore --from {}", crate::events::shell_token(&out.display().to_string())),
                format!("shadowdroid app state cleanup --from {}", crate::events::shell_token(&out.display().to_string()))
            ]
        }),
    );
    Ok(())
}

async fn snapshot_roots(
    serial: &Serial,
    package: &str,
    includes: &[String],
) -> Result<Vec<SnapshotRoot>> {
    let mut requested = includes
        .iter()
        .map(|path| normalize_private_path(path))
        .collect::<Result<Vec<_>>>()?;
    requested.sort();
    requested.dedup();
    let mut collapsed = Vec::<String>::new();
    for path in requested {
        if collapsed
            .iter()
            .any(|root| is_same_or_descendant(&path, root))
        {
            continue;
        }
        collapsed.push(path);
    }

    let mut roots = Vec::new();
    for path in collapsed {
        let stat = private_stat(serial, package, &path).await?;
        roots.push(SnapshotRoot {
            path: path.clone(),
            kind: stat.kind,
            implicit_sqlite_sidecar: false,
        });
        if stat.kind == StatePathKind::File
            && path.starts_with("databases/")
            && !is_sqlite_sidecar(&path)
        {
            for sidecar in sqlite_sidecar_candidates(&path) {
                if private_exists(serial, package, &sidecar).await? {
                    roots.push(SnapshotRoot {
                        path: sidecar,
                        kind: StatePathKind::File,
                        implicit_sqlite_sidecar: true,
                    });
                }
            }
        }
    }
    roots.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(roots)
}

async fn enumerate_snapshot(
    serial: &Serial,
    package: &str,
    roots: &[SnapshotRoot],
) -> Result<(Vec<StateDirectory>, BTreeMap<String, PrivateStat>)> {
    let mut directories = BTreeMap::<String, u32>::new();
    let mut files = BTreeMap::<String, PrivateStat>::new();
    for root in roots {
        match root.kind {
            StatePathKind::File => {
                files.insert(
                    root.path.clone(),
                    private_stat(serial, package, &root.path).await?,
                );
            }
            StatePathKind::Directory => {
                let links = run_as_bytes(
                    serial,
                    package,
                    &format!("find {} -type l -print0", shell_quote(&root.path)),
                )
                .await?;
                if !nul_paths(&links)?.is_empty() {
                    return Err(crate::diagnostic::DiagnosticError::new(
                        "private_symlink_unsupported",
                        "app_state",
                        "snapshot roots containing symbolic links are refused",
                    )
                    .detail(json!({"root": root.path}))
                    .next_actions(["select explicit regular files/directories without symlinks"])
                    .into());
                }
                let dir_bytes = run_as_bytes(
                    serial,
                    package,
                    &format!("find {} -type d -print0", shell_quote(&root.path)),
                )
                .await?;
                for path in nul_paths(&dir_bytes)? {
                    let path = normalize_private_path(&path)?;
                    let stat = private_stat(serial, package, &path).await?;
                    directories.insert(path, stat.mode);
                }
                let file_bytes = run_as_bytes(
                    serial,
                    package,
                    &format!("find {} -type f -print0", shell_quote(&root.path)),
                )
                .await?;
                for path in nul_paths(&file_bytes)? {
                    let path = normalize_private_path(&path)?;
                    files.insert(path.clone(), private_stat(serial, package, &path).await?);
                }
            }
        }
    }
    let directories = directories
        .into_iter()
        .map(|(path, mode)| StateDirectory { path, mode })
        .collect();
    Ok((directories, files))
}

fn is_same_or_descendant(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn is_sqlite_sidecar(path: &str) -> bool {
    ["-wal", "-shm", "-journal"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
}

fn sqlite_sidecar_candidates(path: &str) -> [String; 3] {
    [
        format!("{path}-wal"),
        format!("{path}-shm"),
        format!("{path}-journal"),
    ]
}

fn nul_paths(bytes: &[u8]) -> Result<Vec<String>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| {
            std::str::from_utf8(part)
                .context("private path is not valid UTF-8")
                .map(str::to_string)
        })
        .collect()
}

async fn restore(
    serial: &Serial,
    resolved_package: Option<String>,
    from: &Path,
    allow_incompatible: bool,
) -> Result<()> {
    let manifest = read_manifest(from)?;
    validate_manifest(&manifest, from)?;
    let package = resolved_package.unwrap_or_else(|| manifest.package.clone());
    validate_android_package(&package)?;
    let current = ensure_run_as(serial, &package).await?;
    reject_pending_transaction(serial, &package).await?;
    let package_matches = package == manifest.package;
    let signature_matches = current.signature_digest == manifest.signature_digest;
    if (!package_matches || !signature_matches) && !allow_incompatible {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_snapshot_incompatible",
            "app_state",
            "snapshot package/signature compatibility check failed",
        )
        .detail(json!({
            "snapshot_package": manifest.package,
            "target_package": package,
            "package_matches": package_matches,
            "signature_matches": signature_matches,
            "snapshot_signature_digest": manifest.signature_digest,
            "target_signature_digest": current.signature_digest,
            "snapshot_version_code": manifest.version_code,
            "target_version_code": current.version_code,
        }))
        .next_actions([
            "install an APK signed with the snapshot signer and retry",
            "only when intentionally testing incompatible state, rerun with --allow-incompatible",
        ])
        .into());
    }
    force_stop_private_app(serial, &package).await?;
    let transaction = format!(
        "txn-{}-{}-{}",
        now_ms(),
        std::process::id(),
        TRANSFER_ID.fetch_add(1, Ordering::Relaxed)
    );
    let transaction_root = format!("{PRIVATE_CONTROL_DIR}/{transaction}");
    let stage_root = format!("{transaction_root}/stage");
    let backup_root = format!("{transaction_root}/backup");
    let init = format!(
        "mkdir -p {stage} {backup} && echo {COPY_OK}",
        stage = shell_quote(&stage_root),
        backup = shell_quote(&backup_root),
    );
    require_marker(
        run_as_mutating(serial, &package, &init).await?,
        COPY_OK,
        "state_restore_stage_failed",
        "could not create private restore staging directories",
    )?;

    let stage_result = stage_snapshot(serial, &package, from, &manifest, &stage_root).await;
    if let Err(error) = stage_result {
        let _ = remove_inactive_transaction(serial, &package, &transaction_root).await;
        return Err(error);
    }
    maybe_test_failpoint("after_stage")?;

    let mut roots = Vec::with_capacity(manifest.roots.len());
    for root in &manifest.roots {
        roots.push(TransactionRoot {
            path: root.path.clone(),
            existed: private_exists(serial, &package, &root.path).await?,
        });
    }
    write_transaction_roots(serial, &package, &transaction_root, &roots).await?;
    maybe_test_failpoint("after_roots")?;
    write_transaction_phase(
        serial,
        &package,
        &transaction_root,
        TransactionPhase::Prepared,
    )
    .await?;
    maybe_test_failpoint("after_prepared_phase")?;
    if let Err(error) = claim_pending_transaction(serial, &package, &transaction).await {
        let _ = remove_inactive_transaction(serial, &package, &transaction_root).await;
        return Err(error);
    }
    maybe_test_failpoint("after_pending_claim")?;

    let failpoint = test_failpoint();
    let commit = commit_script(&roots, &stage_root, &backup_root, failpoint.as_deref());
    let commit_output = match run_as_mutating(serial, &package, &commit).await {
        Ok(output) => output,
        Err(error) => {
            rollback_or_warn(serial, &package, &transaction, &roots)
                .await
                .context("restore commit transport failed and rollback did not complete")?;
            return Err(error.context("restore commit transport failed; transaction rolled back"));
        }
    };
    propagate_remote_test_failpoint(&commit_output)?;
    if !has_output_marker(&commit_output, COMMIT_OK) {
        rollback_or_warn(serial, &package, &transaction, &roots).await?;
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_commit_failed",
            "app_state",
            "restore commit failed and was rolled back",
        )
        .detail(json!({"app": package, "snapshot": from.display().to_string()}))
        .next_actions(["inspect app storage availability and retry the restore"])
        .into());
    }
    maybe_test_failpoint("after_commit")?;

    if let Err(error) = verify_restored_state(serial, &package, &manifest).await {
        rollback_or_warn(serial, &package, &transaction, &roots).await?;
        return Err(error.context("restored state verification failed; transaction rolled back"));
    }
    maybe_test_failpoint("after_verify_before_verified")?;
    write_transaction_phase(
        serial,
        &package,
        &transaction_root,
        TransactionPhase::Verified,
    )
    .await?;
    maybe_test_failpoint("after_verified_phase")?;
    let cleanup_complete = finalize_transaction(serial, &package, &transaction).await?;
    emit_action(
        "app_state_restore",
        &json!({
            "app": package,
            "snapshot": from.display().to_string(),
            "snapshot_version_code": manifest.version_code,
            "target_version_code": current.version_code,
            "version_changed": manifest.version_code != current.version_code,
            "package_matches": package_matches,
            "signature_matches": signature_matches,
            "compatibility_overridden": allow_incompatible && (!package_matches || !signature_matches),
            "root_count": manifest.roots.len(),
            "file_count": manifest.files.len(),
            "directory_count": manifest.directories.len(),
            "verified": true,
            "transaction_complete": true,
            "transaction_cleanup_complete": cleanup_complete,
            "contents_printed": false,
            "app_state": "stopped",
            "next_actions": [
                format!("shadowdroid app start {package}"),
                format!("shadowdroid app wait {package} --front")
            ]
        }),
    );
    Ok(())
}

async fn stage_snapshot(
    serial: &Serial,
    package: &str,
    from: &Path,
    manifest: &SnapshotManifest,
    stage_root: &str,
) -> Result<()> {
    for directory in &manifest.directories {
        let destination = format!("{stage_root}/{}", directory.path);
        let script = format!(
            "mkdir -p {path} && chmod {mode:o} {path} && echo {COPY_OK}",
            path = shell_quote(&destination),
            mode = directory.mode,
        );
        require_marker(
            run_as_mutating(serial, package, &script).await?,
            COPY_OK,
            "state_restore_stage_failed",
            "could not stage a private directory",
        )?;
    }
    for file in &manifest.files {
        let source = snapshot_data_path(from, &file.path)?;
        validate_snapshot_source(from, &source)?;
        let metadata = std::fs::metadata(&source)
            .with_context(|| format!("reading snapshot file {}", source.display()))?;
        if !metadata.is_file() || metadata.len() != file.bytes {
            return Err(crate::diagnostic::DiagnosticError::new(
                "snapshot_file_mismatch",
                "app_state",
                "snapshot file is missing or has an unexpected byte count",
            )
            .detail(json!({
                "path": file.path,
                "expected_bytes": file.bytes,
                "actual_bytes": metadata.len(),
            }))
            .into());
        }
        let bytes = std::fs::read(&source)
            .with_context(|| format!("hashing snapshot file {}", source.display()))?;
        if sha256_bytes(&bytes) != file.sha256 {
            return Err(crate::diagnostic::DiagnosticError::new(
                "snapshot_hash_mismatch",
                "app_state",
                "snapshot file hash verification failed before device staging",
            )
            .detail(json!({"path": file.path, "expected_sha256": file.sha256}))
            .into());
        }
        let destination = format!("{stage_root}/{}", file.path);
        copy_host_file_to_private(serial, package, &source, &destination, file.mode).await?;
    }
    Ok(())
}

async fn write_transaction_roots(
    serial: &Serial,
    package: &str,
    transaction_root: &str,
    roots: &[TransactionRoot],
) -> Result<()> {
    let mut body = String::new();
    for root in roots {
        body.push_str(&root.path);
        body.push('\t');
        body.push(if root.existed { '1' } else { '0' });
        body.push('\n');
    }
    let temp = tempfile::NamedTempFile::new().context("creating transaction root metadata")?;
    artifact::write_bytes(temp.path(), body.as_bytes())?;
    copy_host_file_to_private(
        serial,
        package,
        temp.path(),
        &format!("{transaction_root}/roots.tsv"),
        0o600,
    )
    .await
}

async fn write_transaction_phase(
    serial: &Serial,
    package: &str,
    transaction_root: &str,
    phase: TransactionPhase,
) -> Result<()> {
    let value = match phase {
        TransactionPhase::Prepared => PHASE_PREPARED,
        TransactionPhase::Verified => PHASE_VERIFIED,
        TransactionPhase::Legacy => {
            return Err(anyhow::anyhow!(
                "legacy transaction phases cannot be persisted"
            ));
        }
    };
    let phase_path = format!("{transaction_root}/phase");
    let phase_temp = format!("{phase_path}.tmp");
    let guard = match phase {
        TransactionPhase::Prepared => format!(
            "[ ! -f {path} ] || [ \"$(cat {path} 2>/dev/null)\" = {prepared} ]",
            path = shell_quote(&phase_path),
            prepared = shell_quote(PHASE_PREPARED),
        ),
        TransactionPhase::Verified => format!(
            "[ \"$(cat {path} 2>/dev/null)\" = {prepared} ] || [ \"$(cat {path} 2>/dev/null)\" = {verified} ]",
            path = shell_quote(&phase_path),
            prepared = shell_quote(PHASE_PREPARED),
            verified = shell_quote(PHASE_VERIFIED),
        ),
        TransactionPhase::Legacy => unreachable!(),
    };
    let script = format!(
        "if {guard}; then printf '%s\\n' {value} > {temp} && mv {temp} {path} && echo {PHASE_WRITE_OK}; fi",
        guard = guard,
        value = shell_quote(value),
        temp = shell_quote(&phase_temp),
        path = shell_quote(&phase_path),
    );
    let output = run_as_mutating(serial, package, &script).await;
    if output
        .as_ref()
        .is_ok_and(|value| has_output_marker(value, PHASE_WRITE_OK))
    {
        return Ok(());
    }
    if read_transaction_phase(serial, package, transaction_root).await? == phase {
        return Ok(());
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "state_restore_phase_write_failed",
        "app_state",
        "could not atomically persist the private restore phase",
    )
    .retryable(true)
    .detail(json!({"app": package, "pending_transaction": phase == TransactionPhase::Verified}))
    .next_actions([format!("shadowdroid app state recover --app {package}")])
    .into())
}

async fn claim_pending_transaction(
    serial: &Serial,
    package: &str,
    transaction: &str,
) -> Result<()> {
    let script = claim_pending_script(transaction);
    let output = run_as_mutating(serial, package, &script).await;
    if output
        .as_ref()
        .is_ok_and(|value| has_output_marker(value, PENDING_CLAIM_OK))
    {
        return Ok(());
    }
    match pending_transaction(serial, package).await? {
        Some(owner) if owner == transaction => Ok(()),
        Some(_) => Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_concurrent_transaction",
            "app_state",
            "another restore atomically claimed the private pending marker",
        )
        .retryable(true)
        .detail(json!({
            "app": package,
            "pending_transaction": true,
            "owned_by_this_restore": false,
            "contents_printed": false,
        }))
        .next_actions([format!("shadowdroid app state recover --app {package}")])
        .into()),
        None => Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_marker_failed",
            "app_state",
            "could not atomically claim the private restore recovery marker",
        )
        .retryable(true)
        .detail(json!({"app": package, "pending_transaction": false}))
        .next_actions(["retry the restore after checking app storage availability"])
        .into()),
    }
}

fn claim_pending_script(transaction: &str) -> String {
    let transaction_root = format!("{PRIVATE_CONTROL_DIR}/{transaction}");
    let claim = format!("{transaction_root}/claim");
    let claim_temp = format!("{claim}.tmp");
    format!(
        "printf '%s\\n' {transaction} > {claim_temp} && mv {claim_temp} {claim} && mv -nT {claim} {pending} 2>/dev/null; if [ ! -e {claim} ] && [ \"$(cat {pending} 2>/dev/null)\" = {transaction} ]; then echo {PENDING_CLAIM_OK}; fi",
        transaction = shell_quote(transaction),
        claim_temp = shell_quote(&claim_temp),
        claim = shell_quote(&claim),
        pending = shell_quote(PENDING_PATH),
    )
}

fn test_failpoint() -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(TEST_FAILPOINT_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn test_failpoint_error(name: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "app_state_test_failpoint",
        "app_state",
        "app-state fail-stop test interrupted the transaction",
    )
    .retryable(true)
    .detail(json!({"failpoint": name, "test_only": true}))
    .next_actions(["run `shadowdroid app state recover` when a pending transaction exists"])
    .process_exit_code(86)
    .into()
}

fn maybe_test_failpoint(name: &str) -> Result<()> {
    if test_failpoint().as_deref() == Some(name) {
        Err(test_failpoint_error(name))
    } else {
        Ok(())
    }
}

fn append_remote_test_failpoint(commands: &mut Vec<String>, active: Option<&str>, name: &str) {
    if active == Some(name) {
        commands.push(format!(
            "echo {} && exit 86",
            shell_quote(&format!("{TEST_FAILPOINT_MARKER}{name}"))
        ));
    }
}

fn propagate_remote_test_failpoint(output: &str) -> Result<()> {
    if let Some(start) = output.find(TEST_FAILPOINT_MARKER) {
        let name = output[start + TEST_FAILPOINT_MARKER.len()..]
            .split_whitespace()
            .next()
            .unwrap_or("remote");
        return Err(test_failpoint_error(name));
    }
    Ok(())
}

fn commit_script(
    roots: &[TransactionRoot],
    stage_root: &str,
    backup_root: &str,
    failpoint: Option<&str>,
) -> String {
    let mut commands = vec!["set -e".to_string()];
    for (index, root) in roots.iter().enumerate() {
        let target = shell_quote(&root.path);
        let backup = shell_quote(&format!("{backup_root}/{}", root.path));
        let backup_parent = shell_quote(&format!("{backup_root}/{}", private_parent(&root.path)));
        if root.existed {
            commands.push(format!("mkdir -p {backup_parent}"));
            commands.push(format!("mv {target} {backup}"));
            append_remote_test_failpoint(
                &mut commands,
                failpoint,
                &format!("commit_after_backup_{index}"),
            );
        }
        commands.push(format!(
            "mkdir -p {}",
            shell_quote(private_parent(&root.path))
        ));
        commands.push(format!(
            "mv {} {target}",
            shell_quote(&format!("{stage_root}/{}", root.path))
        ));
        append_remote_test_failpoint(
            &mut commands,
            failpoint,
            &format!("commit_after_publish_{index}"),
        );
    }
    commands.push(format!("echo {COMMIT_OK}"));
    commands.join(" && ")
}

async fn verify_restored_state(
    serial: &Serial,
    package: &str,
    manifest: &SnapshotManifest,
) -> Result<()> {
    for directory in &manifest.directories {
        let stat = private_stat(serial, package, &directory.path).await?;
        if stat.kind != StatePathKind::Directory || stat.mode != directory.mode {
            return Err(crate::diagnostic::DiagnosticError::new(
                "state_restore_directory_mismatch",
                "app_state",
                "restored directory type/mode does not match the manifest",
            )
            .detail(json!({
                "path": directory.path,
                "expected_mode": directory.mode,
                "actual_mode": stat.mode,
            }))
            .into());
        }
    }
    for file in &manifest.files {
        let stat = private_stat(serial, package, &file.path).await?;
        if stat.kind != StatePathKind::File || stat.bytes != file.bytes || stat.mode != file.mode {
            return Err(crate::diagnostic::DiagnosticError::new(
                "state_restore_file_mismatch",
                "app_state",
                "restored file metadata does not match the manifest",
            )
            .detail(json!({
                "path": file.path,
                "expected_bytes": file.bytes,
                "actual_bytes": stat.bytes,
                "expected_mode": file.mode,
                "actual_mode": stat.mode,
            }))
            .into());
        }
        let bytes = read_private_file(serial, package, &file.path, file.bytes).await?;
        if sha256_bytes(&bytes) != file.sha256 {
            return Err(crate::diagnostic::DiagnosticError::new(
                "state_restore_hash_mismatch",
                "app_state",
                "restored private file hash does not match the manifest",
            )
            .detail(json!({"path": file.path, "expected_sha256": file.sha256}))
            .into());
        }
    }
    Ok(())
}

async fn recover(serial: &Serial, package: &str) -> Result<()> {
    ensure_run_as(serial, package).await?;
    let Some(transaction) = pending_transaction(serial, package).await? else {
        let terminal_transactions_cleaned = cleanup_terminal_transactions(serial, package).await?;
        emit_action(
            "app_state_recover",
            &json!({
                "app": package,
                "recovered": false,
                "pending_transaction": false,
                "terminal_transactions_cleaned": terminal_transactions_cleaned,
                "app_state": "unchanged",
            }),
        );
        return Ok(());
    };
    force_stop_private_app(serial, package).await?;
    let transaction_root = format!("{PRIVATE_CONTROL_DIR}/{transaction}");
    if !control_path_exists(serial, package, &transaction_root).await? {
        terminalize_pending(
            serial,
            package,
            &transaction,
            "legacy_terminal",
            FINALIZE_OK,
            "state_restore_legacy_terminal_incomplete",
            "could not retire a legacy terminal restore marker",
        )
        .await?;
        let cleanup_complete = remove_inactive_transaction(
            serial,
            package,
            &format!("{PRIVATE_CONTROL_DIR}/{transaction}"),
        )
        .await
        .is_ok();
        emit_action(
            "app_state_recover",
            &json!({
                "app": package,
                "recovered": true,
                "recovery_action": "retired_legacy_terminal_marker",
                "restored_state_outcome": "unknown_but_terminal",
                "pending_transaction": false,
                "transaction_cleanup_complete": cleanup_complete,
                "app_state": "stopped",
                "contents_printed": false,
            }),
        );
        return Ok(());
    }

    let phase = read_transaction_phase(serial, package, &transaction_root).await?;
    let (recovery_action, root_count, cleanup_complete) = match phase {
        TransactionPhase::Verified => (
            "finalized_verified_restore",
            None,
            finalize_transaction(serial, package, &transaction).await?,
        ),
        TransactionPhase::Legacy | TransactionPhase::Prepared => {
            let roots = read_transaction_roots(serial, package, &transaction_root).await?;
            let root_count = roots.len();
            let cleanup_complete = rollback_or_warn(serial, package, &transaction, &roots).await?;
            (
                "rolled_back_unverified_restore",
                Some(root_count),
                cleanup_complete,
            )
        }
    };
    emit_action(
        "app_state_recover",
        &json!({
            "app": package,
            "recovered": true,
            "recovery_action": recovery_action,
            "transaction_phase": match phase {
                TransactionPhase::Legacy => "legacy",
                TransactionPhase::Prepared => PHASE_PREPARED,
                TransactionPhase::Verified => PHASE_VERIFIED,
            },
            "pending_transaction": false,
            "root_count": root_count,
            "transaction_cleanup_complete": cleanup_complete,
            "app_state": "stopped",
            "contents_printed": false,
        }),
    );
    Ok(())
}

async fn rollback_or_warn(
    serial: &Serial,
    package: &str,
    transaction: &str,
    roots: &[TransactionRoot],
) -> Result<bool> {
    let transaction_root = format!("{PRIVATE_CONTROL_DIR}/{transaction}");
    let backup_root = format!("{transaction_root}/backup");
    let failpoint = test_failpoint();
    let script = rollback_roots_script(roots, &backup_root, failpoint.as_deref());
    let output = run_as_mutating(serial, package, &script).await?;
    propagate_remote_test_failpoint(&output)?;
    if !has_output_marker(&output, ROLLBACK_OK) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_rollback_incomplete",
            "app_state",
            "restore rollback could not be verified; the pending marker remains and app state may be partial",
        )
        .detail(json!({"app": package, "pending_transaction": true}))
        .next_actions([format!("shadowdroid app state recover --app {package}")])
        .into());
    }
    terminalize_pending(
        serial,
        package,
        transaction,
        "rolled_back",
        ROLLBACK_OK,
        "state_restore_rollback_incomplete",
        "restore roots were rolled back, but the pending marker could not be retired",
    )
    .await?;
    maybe_test_failpoint("rollback_after_commit_point")?;
    Ok(
        remove_inactive_transaction(serial, package, &transaction_root)
            .await
            .is_ok(),
    )
}

fn rollback_roots_script(
    roots: &[TransactionRoot],
    backup_root: &str,
    failpoint: Option<&str>,
) -> String {
    let mut commands = vec!["set -e".to_string()];
    for (index, root) in roots.iter().rev().enumerate() {
        let target = shell_quote(&root.path);
        let backup = shell_quote(&format!("{backup_root}/{}", root.path));
        let parent = shell_quote(private_parent(&root.path));
        if root.existed {
            commands.push(format!(
                "if [ -e {backup} ]; then rm -rf {target}; mkdir -p {parent}; mv {backup} {target}; fi; [ -e {target} ]"
            ));
        } else {
            commands.push(format!("rm -rf {target}; [ ! -e {target} ]"));
        }
        append_remote_test_failpoint(
            &mut commands,
            failpoint,
            &format!("rollback_after_root_{index}"),
        );
    }
    commands.push(format!("echo {ROLLBACK_OK}"));
    commands.join(" && ")
}

async fn finalize_transaction(serial: &Serial, package: &str, transaction: &str) -> Result<bool> {
    terminalize_pending(
        serial,
        package,
        transaction,
        "finalized",
        FINALIZE_OK,
        "state_restore_finalize_failed",
        "verified state could not atomically retire the pending restore marker",
    )
    .await?;
    maybe_test_failpoint("finalize_after_commit_point")?;
    let transaction_root = format!("{PRIVATE_CONTROL_DIR}/{transaction}");
    Ok(
        remove_inactive_transaction(serial, package, &transaction_root)
            .await
            .is_ok(),
    )
}

async fn read_transaction_roots(
    serial: &Serial,
    package: &str,
    transaction_root: &str,
) -> Result<Vec<TransactionRoot>> {
    let roots_output = run_as_text(
        serial,
        package,
        &format!(
            "cat {}",
            shell_quote(&format!("{transaction_root}/roots.tsv"))
        ),
    )
    .await?;
    parse_transaction_roots(&roots_output)
}

async fn read_transaction_phase(
    serial: &Serial,
    package: &str,
    transaction_root: &str,
) -> Result<TransactionPhase> {
    let phase_path = format!("{transaction_root}/phase");
    let output = run_as_text(
        serial,
        package,
        &format!(
            "if [ -f {phase} ]; then echo __shadowdroid_phase_present__; cat {phase}; else echo __shadowdroid_phase_missing__; fi",
            phase = shell_quote(&phase_path),
        ),
    )
    .await?;
    parse_transaction_phase(&output)
}

fn parse_transaction_phase(output: &str) -> Result<TransactionPhase> {
    let lines = output.lines().map(str::trim).collect::<Vec<_>>();
    match lines.as_slice() {
        ["__shadowdroid_phase_missing__"] => Ok(TransactionPhase::Legacy),
        ["__shadowdroid_phase_present__", PHASE_PREPARED] => Ok(TransactionPhase::Prepared),
        ["__shadowdroid_phase_present__", PHASE_VERIFIED] => Ok(TransactionPhase::Verified),
        _ => Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_phase_invalid",
            "app_state",
            "private restore phase is malformed; refusing to guess between rollback and finalize",
        )
        .detail(json!({"pending_transaction": true, "contents_printed": false}))
        .next_actions(["inspect .shadowdroid_state transaction metadata before manual recovery"])
        .into()),
    }
}

async fn terminalize_pending(
    serial: &Serial,
    package: &str,
    transaction: &str,
    terminal_name: &str,
    marker: &str,
    error_code: &'static str,
    error_message: &'static str,
) -> Result<()> {
    let script = terminalize_pending_script(transaction, terminal_name, marker);
    let output = run_as_mutating(serial, package, &script).await;
    if output
        .as_ref()
        .is_ok_and(|value| has_output_marker(value, marker))
    {
        return Ok(());
    }

    match pending_terminal_state(serial, package, transaction, terminal_name).await? {
        PendingTerminalState::Terminal | PendingTerminalState::Missing => Ok(()),
        PendingTerminalState::Pending(owner) if owner == transaction => Err(
            crate::diagnostic::DiagnosticError::new(error_code, "app_state", error_message)
                .retryable(true)
                .detail(json!({
                    "app": package,
                    "pending_transaction": true,
                    "owned_by_this_restore": true,
                    "contents_printed": false,
                }))
                .next_actions([format!("shadowdroid app state recover --app {package}")])
                .into(),
        ),
        PendingTerminalState::Pending(_) => Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_transaction_ownership_lost",
            "app_state",
            "the private pending marker belongs to another transaction; refusing to move it",
        )
        .detail(json!({
            "app": package,
            "pending_transaction": true,
            "owned_by_this_restore": false,
            "contents_printed": false,
        }))
        .next_actions(["inspect the private transaction metadata before manual recovery"])
        .into()),
    }
}

fn terminalize_pending_script(transaction: &str, terminal_name: &str, marker: &str) -> String {
    let transaction_root = format!("{PRIVATE_CONTROL_DIR}/{transaction}");
    let terminal = format!("{transaction_root}/{terminal_name}");
    format!(
        "mkdir -p {transaction_root} && if [ -f {terminal} ] && [ \"$(cat {terminal} 2>/dev/null)\" = {transaction} ]; then echo {marker}; elif [ -f {pending} ] && [ \"$(cat {pending} 2>/dev/null)\" = {transaction} ]; then mv -nT {pending} {terminal}; if [ -f {terminal} ] && [ \"$(cat {terminal} 2>/dev/null)\" = {transaction} ]; then echo {marker}; fi; fi",
        transaction_root = shell_quote(&transaction_root),
        terminal = shell_quote(&terminal),
        transaction = shell_quote(transaction),
        pending = shell_quote(PENDING_PATH),
    )
}

async fn pending_terminal_state(
    serial: &Serial,
    package: &str,
    transaction: &str,
    terminal_name: &str,
) -> Result<PendingTerminalState> {
    let terminal = format!("{PRIVATE_CONTROL_DIR}/{transaction}/{terminal_name}");
    let output = run_as_text(
        serial,
        package,
        &format!(
            "if [ -f {terminal} ]; then printf '__shadowdroid_terminal__'; cat {terminal}; elif [ -f {pending} ]; then printf '__shadowdroid_pending__'; cat {pending}; else echo __shadowdroid_terminal_missing__; fi",
            terminal = shell_quote(&terminal),
            pending = shell_quote(PENDING_PATH),
        ),
    )
    .await?;
    parse_pending_terminal_state(&output, transaction)
}

fn parse_pending_terminal_state(output: &str, transaction: &str) -> Result<PendingTerminalState> {
    let output = output.trim();
    if output == "__shadowdroid_terminal_missing__" {
        return Ok(PendingTerminalState::Missing);
    }
    if let Some(value) = output.strip_prefix("__shadowdroid_terminal__") {
        validate_transaction_id(value.trim())?;
        if value.trim() == transaction {
            return Ok(PendingTerminalState::Terminal);
        }
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_terminal_invalid",
            "app_state",
            "private terminal transaction marker has unexpected ownership",
        )
        .detail(json!({"pending_transaction": true, "contents_printed": false}))
        .into());
    }
    if let Some(value) = output.strip_prefix("__shadowdroid_pending__") {
        let value = value.trim();
        validate_transaction_id(value)?;
        return Ok(PendingTerminalState::Pending(value.to_string()));
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "state_restore_terminal_invalid",
        "app_state",
        "private terminal transaction state is malformed",
    )
    .detail(json!({"pending_transaction": true, "contents_printed": false}))
    .into())
}

fn has_output_marker(output: &str, marker: &str) -> bool {
    output.lines().any(|line| line.trim() == marker)
}

async fn reject_pending_transaction(serial: &Serial, package: &str) -> Result<()> {
    if pending_transaction(serial, package).await?.is_some() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_interrupted",
            "app_state",
            "a previous restore left a private pending marker; app state may be partial",
        )
        .detail(json!({"app": package, "pending_transaction": true, "contents_printed": false}))
        .next_actions([format!("shadowdroid app state recover --app {package}")])
        .into());
    }
    Ok(())
}

async fn force_stop_private_app(serial: &Serial, package: &str) -> Result<()> {
    adb::am_force_stop(serial, package).await?;
    let pid = adb::shell(serial, format!("pidof {}", shell_quote(package))).await?;
    if !pid.trim().is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "app_force_stop_unverified",
            "app_state",
            "package still has a running process after force-stop",
        )
        .retryable(true)
        .detail(json!({"app": package, "process_still_running": true}))
        .next_actions([format!("shadowdroid app stop {package}")])
        .into());
    }
    Ok(())
}

async fn pending_transaction(serial: &Serial, package: &str) -> Result<Option<String>> {
    let output = run_as_text(
        serial,
        package,
        &format!(
            "if [ -L {pending} ]; then echo __shadowdroid_pending_invalid_type__; elif [ -f {pending} ]; then printf '__shadowdroid_pending_present__'; cat {pending}; elif [ -e {pending} ]; then echo __shadowdroid_pending_invalid_type__; else echo __shadowdroid_pending_missing__; fi",
            pending = shell_quote(PENDING_PATH)
        ),
    )
    .await?;
    parse_pending_transaction_output(&output, package)
}

fn parse_pending_transaction_output(output: &str, package: &str) -> Result<Option<String>> {
    let output = output.trim();
    if output == "__shadowdroid_pending_missing__" {
        return Ok(None);
    }
    let transaction = output
        .strip_prefix("__shadowdroid_pending_present__")
        .unwrap_or("")
        .trim();
    validate_transaction_id(transaction).map_err(|_| {
        crate::diagnostic::DiagnosticError::new(
            "state_restore_marker_invalid",
            "app_state",
            "private restore marker is malformed; refusing automated recovery",
        )
        .detail(json!({"app": package, "pending_transaction": true, "contents_printed": false}))
        .next_actions([
            "inspect the app data directory manually before removing .shadowdroid_state",
        ])
    })?;
    Ok(Some(transaction.to_string()))
}

async fn cleanup_terminal_transactions(serial: &Serial, package: &str) -> Result<u64> {
    let script = terminal_cleanup_script();
    let output = run_as_mutating(serial, package, &script).await?;
    let count = output
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("__shadowdroid_terminal_cleanup_ok__:")
                .and_then(|value| value.parse::<u64>().ok())
        })
        .ok_or_else(|| {
            crate::diagnostic::DiagnosticError::new(
                "state_restore_terminal_cleanup_failed",
                "app_state",
                "could not verify cleanup of terminal private restore transactions",
            )
        })?;
    Ok(count)
}

fn terminal_cleanup_script() -> String {
    format!(
        "count=0; for marker in {control}/txn-*/finalized {control}/txn-*/rolled_back {control}/txn-*/legacy_terminal; do [ -f \"$marker\" ] || continue; directory=${{marker%/*}}; transaction=${{directory##*/}}; case \"$transaction\" in txn-*) ;; *) continue ;; esac; [ \"$(cat \"$marker\" 2>/dev/null)\" = \"$transaction\" ] || continue; rm -rf \"$directory\" && count=$((count + 1)); done; echo __shadowdroid_terminal_cleanup_ok__:$count",
        control = shell_quote(PRIVATE_CONTROL_DIR),
    )
}

async fn control_path_exists(serial: &Serial, package: &str, path: &str) -> Result<bool> {
    debug_assert!(path.starts_with(&format!("{PRIVATE_CONTROL_DIR}/txn-")));
    let output = run_as_text(
        serial,
        package,
        &format!(
            "if [ -e {} ]; then echo 1; else echo 0; fi",
            shell_quote(path)
        ),
    )
    .await?;
    Ok(has_output_marker(&output, "1"))
}

fn validate_transaction_id(transaction: &str) -> Result<()> {
    if transaction.starts_with("txn-")
        && transaction
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        Ok(())
    } else {
        Err(anyhow::anyhow!("invalid private transaction identifier"))
    }
}

fn parse_transaction_roots(value: &str) -> Result<Vec<TransactionRoot>> {
    let roots = value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (path, existed) = line.split_once('\t').ok_or_else(|| {
                crate::diagnostic::DiagnosticError::new(
                    "state_restore_metadata_invalid",
                    "app_state",
                    "private restore root metadata is malformed",
                )
            })?;
            let normalized = normalize_private_path(path)?;
            if normalized != path {
                return Err(crate::diagnostic::DiagnosticError::new(
                    "state_restore_metadata_invalid",
                    "app_state",
                    "private restore root metadata contains a non-normalized path",
                )
                .into());
            }
            Ok(TransactionRoot {
                path: normalized,
                existed: match existed {
                    "1" => true,
                    "0" => false,
                    _ => {
                        return Err(crate::diagnostic::DiagnosticError::new(
                            "state_restore_metadata_invalid",
                            "app_state",
                            "private restore existence metadata is malformed",
                        )
                        .into());
                    }
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if roots.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_metadata_invalid",
            "app_state",
            "private restore root metadata is empty; refusing unsafe recovery",
        )
        .into());
    }
    if roots_have_duplicates_or_overlap(roots.iter().map(|root| root.path.as_str())) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_restore_metadata_invalid",
            "app_state",
            "private restore roots are duplicate or overlap; refusing unsafe recovery",
        )
        .into());
    }
    Ok(roots)
}

fn roots_have_duplicates_or_overlap<'a>(paths: impl IntoIterator<Item = &'a str>) -> bool {
    let paths = paths.into_iter().collect::<Vec<_>>();
    paths.iter().enumerate().any(|(index, left)| {
        paths
            .iter()
            .skip(index + 1)
            .any(|right| is_same_or_descendant(left, right) || is_same_or_descendant(right, left))
    })
}

async fn remove_inactive_transaction(
    serial: &Serial,
    package: &str,
    transaction_root: &str,
) -> Result<()> {
    run_as_mutating(
        serial,
        package,
        &format!("rm -rf {}", shell_quote(transaction_root)),
    )
    .await
    .map(|_| ())
}

fn require_marker(
    output: String,
    marker: &str,
    code: &'static str,
    message: &'static str,
) -> Result<()> {
    if has_output_marker(&output, marker) {
        Ok(())
    } else {
        Err(crate::diagnostic::DiagnosticError::new(code, "app_state", message).into())
    }
}

fn read_manifest(from: &Path) -> Result<SnapshotManifest> {
    let path = from.join("manifest.json");
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading app state manifest {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing app state manifest {}", path.display()))
}

fn validate_manifest(manifest: &SnapshotManifest, from: &Path) -> Result<()> {
    if manifest.artifact_type != SNAPSHOT_TYPE || manifest.format_version != FORMAT_VERSION {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_snapshot_format_unsupported",
            "app_state",
            "snapshot manifest type/version is unsupported",
        )
        .detail(json!({
            "type": manifest.artifact_type,
            "format_version": manifest.format_version,
            "supported_format_version": FORMAT_VERSION,
        }))
        .into());
    }
    validate_android_package(&manifest.package)?;
    if !manifest.contains_sensitive_data || manifest.encrypted {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_snapshot_privacy_metadata_invalid",
            "app_state",
            "snapshot privacy metadata is inconsistent with this unencrypted format",
        )
        .into());
    }
    let mut file_paths = BTreeSet::new();
    for file in &manifest.files {
        let normalized = normalize_private_path(&file.path)?;
        validate_mode(file.mode)?;
        if normalized != file.path || !file_paths.insert(normalized) {
            return Err(crate::diagnostic::DiagnosticError::new(
                "state_snapshot_manifest_invalid",
                "app_state",
                "snapshot manifest contains duplicate or non-normalized file paths",
            )
            .into());
        }
        snapshot_data_path(from, &file.path)?;
    }
    let mut directory_paths = BTreeSet::new();
    for directory in &manifest.directories {
        let normalized = normalize_private_path(&directory.path)?;
        validate_mode(directory.mode)?;
        if normalized != directory.path || !directory_paths.insert(normalized) {
            return Err(crate::diagnostic::DiagnosticError::new(
                "state_snapshot_manifest_invalid",
                "app_state",
                "snapshot manifest contains duplicate or non-normalized directory paths",
            )
            .into());
        }
    }
    if file_paths.iter().any(|path| directory_paths.contains(path)) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_snapshot_manifest_invalid",
            "app_state",
            "snapshot manifest describes the same path as both a file and directory",
        )
        .into());
    }
    if manifest.roots.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_snapshot_manifest_invalid",
            "app_state",
            "snapshot manifest must contain at least one restore root",
        )
        .into());
    }
    let mut root_paths = Vec::with_capacity(manifest.roots.len());
    for root in &manifest.roots {
        let normalized = normalize_private_path(&root.path)?;
        if normalized != root.path {
            return Err(crate::diagnostic::DiagnosticError::new(
                "state_snapshot_manifest_invalid",
                "app_state",
                "snapshot manifest contains a non-normalized root",
            )
            .into());
        }
        let kind_matches = match root.kind {
            StatePathKind::File => file_paths.contains(&root.path),
            StatePathKind::Directory => directory_paths.contains(&root.path),
        };
        if !kind_matches {
            return Err(crate::diagnostic::DiagnosticError::new(
                "state_snapshot_manifest_invalid",
                "app_state",
                "snapshot root kind does not match its staged manifest entry",
            )
            .detail(json!({"root": root.path, "kind": root.kind}))
            .into());
        }
        root_paths.push(root.path.as_str());
    }
    if roots_have_duplicates_or_overlap(root_paths.iter().copied()) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_snapshot_manifest_invalid",
            "app_state",
            "snapshot manifest roots are duplicate or overlap",
        )
        .into());
    }
    let belongs_to_root = |path: &str| {
        manifest.roots.iter().any(|root| match root.kind {
            StatePathKind::File => path == root.path,
            StatePathKind::Directory => is_same_or_descendant(path, &root.path),
        })
    };
    if file_paths
        .iter()
        .chain(directory_paths.iter())
        .any(|path| !belongs_to_root(path))
    {
        return Err(crate::diagnostic::DiagnosticError::new(
            "state_snapshot_manifest_invalid",
            "app_state",
            "snapshot manifest contains staged paths outside its declared restore roots",
        )
        .into());
    }
    Ok(())
}

fn snapshot_data_path(from: &Path, relative: &str) -> Result<PathBuf> {
    let relative = normalize_private_path(relative)?;
    let path = from.join("data").join(relative);
    if !path.starts_with(from.join("data")) {
        return Err(unsafe_private_path(path.to_string_lossy().as_ref()));
    }
    Ok(path)
}

fn validate_snapshot_source(from: &Path, source: &Path) -> Result<()> {
    let data = from
        .join("data")
        .canonicalize()
        .with_context(|| format!("canonicalizing snapshot data directory {}", from.display()))?;
    let source = source
        .canonicalize()
        .with_context(|| format!("canonicalizing snapshot source {}", source.display()))?;
    if !source.starts_with(&data) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "snapshot_symlink_escape",
            "app_state",
            "snapshot data path resolves outside the protected data directory",
        )
        .detail(json!({"source": source.display().to_string()}))
        .into());
    }
    Ok(())
}

fn cleanup(from: &Path) -> Result<()> {
    let manifest = read_manifest(from)?;
    validate_manifest(&manifest, from)?;
    let mut files = Vec::new();
    collect_cleanup_files(from, &mut files)?;
    let mut overwritten_bytes = 0u64;
    for path in &files {
        overwritten_bytes = overwritten_bytes.saturating_add(overwrite_file(path)?);
    }
    std::fs::remove_dir_all(from)
        .with_context(|| format!("removing snapshot directory {}", from.display()))?;
    emit_action(
        "app_state_cleanup",
        &json!({
            "snapshot": from.display().to_string(),
            "deleted": true,
            "files_overwritten": files.len(),
            "bytes_overwritten": overwritten_bytes,
            "best_effort_secure_delete": true,
            "warning": "Overwrite-before-delete cannot guarantee physical erasure on SSD, copy-on-write, journaled, or backed-up storage",
        }),
    );
    Ok(())
}

fn collect_cleanup_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("reading snapshot directory {}", path.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(crate::diagnostic::DiagnosticError::new(
                "snapshot_cleanup_symlink_refused",
                "app_state",
                "secure cleanup refuses snapshot directories containing symbolic links",
            )
            .detail(json!({"path": entry.path().display().to_string()}))
            .into());
        }
        if file_type.is_dir() {
            collect_cleanup_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn overwrite_file(path: &Path) -> Result<u64> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("stat cleanup file {}", path.display()))?
        .len();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening cleanup file {}", path.display()))?;
    file.seek(SeekFrom::Start(0))?;
    let zeros = [0u8; 64 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(zeros.len() as u64) as usize;
        file.write_all(&zeros[..chunk])?;
        remaining -= chunk as u64;
    }
    file.sync_all()?;
    Ok(len)
}

fn create_protected_parent(path: &Path, snapshot_root: &Path) -> Result<()> {
    let parent = path.parent().context("snapshot file has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating protected snapshot path {}", parent.display()))?;
    let mut current = parent;
    loop {
        protect_host_directory(current)?;
        if current == snapshot_root {
            break;
        }
        current = current
            .parent()
            .filter(|ancestor| ancestor.starts_with(snapshot_root))
            .context("snapshot parent escaped the protected snapshot directory")?;
    }
    Ok(())
}

#[cfg(unix)]
fn protect_host_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protecting snapshot directory {}", path.display()))
}

#[cfg(not(unix))]
fn protect_host_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn protect_host_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protecting snapshot file {}", path.display()))
}

#[cfg(not(unix))]
fn protect_host_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn property_config() -> ProptestConfig {
        ProptestConfig {
            cases: 128,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x5344_4150_5053_5441),
            ..ProptestConfig::default()
        }
    }

    proptest! {
        #![proptest_config(property_config())]

        #[cfg(unix)]
        #[test]
        fn shell_arguments_round_trip_through_a_real_shell_without_value_loss(
            chars in prop::collection::vec(
                prop_oneof![
                    3 => prop::sample::select(vec![
                        '\'', '\\', ' ', '\t', '\n', '"', '$', '`', ';', '&', '|', '<', '>', '*', '?',
                    ]),
                    1 => any::<char>().prop_filter(
                        "NUL cannot be transported in a process argument",
                        |ch| *ch != '\0',
                    ),
                ],
                0..128,
            ),
        ) {
            let value = chars.into_iter().collect::<String>();
            let script = format!("printf '%s' {}", shell_quote(&value));
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .output()
                .map_err(|error| TestCaseError::fail(format!("start shell oracle: {error}")))?;
            prop_assert!(
                output.status.success(),
                "shell oracle failed: {}",
                String::from_utf8_lossy(&output.stderr),
            );
            prop_assert_eq!(output.stdout.as_slice(), value.as_bytes());
        }

        #[test]
        fn transaction_root_metadata_round_trips_generated_safe_roots(
            entries in prop::collection::btree_map("[a-z][a-z0-9_]{0,12}", any::<bool>(), 1..16),
        ) {
            let expected = entries
                .into_iter()
                .map(|(name, existed)| TransactionRoot {
                    path: format!("files/{name}"),
                    existed,
                })
                .collect::<Vec<_>>();
            let encoded = expected
                .iter()
                .map(|root| format!("{}\t{}\n", root.path, u8::from(root.existed)))
                .collect::<String>();
            prop_assert_eq!(parse_transaction_roots(&encoded).unwrap(), expected);
        }

        #[test]
        fn every_valid_pending_transaction_has_one_terminal_interpretation(
            name in "[a-z0-9]{1,16}",
            pid in 1u32..u32::MAX,
            nonce in 1u32..u32::MAX,
        ) {
            let transaction = format!("txn-{name}-{pid}-{nonce}");
            let pending = format!("__shadowdroid_pending__{transaction}\n");
            let terminal = format!("__shadowdroid_terminal__{transaction}\n");
            prop_assert_eq!(
                parse_pending_terminal_state(&pending, &transaction).unwrap(),
                PendingTerminalState::Pending(transaction.clone()),
            );
            prop_assert_eq!(
                parse_pending_terminal_state(&terminal, &transaction).unwrap(),
                PendingTerminalState::Terminal,
            );
        }
    }

    #[test]
    fn private_paths_are_normalized_and_reserved_storage_is_refused() {
        assert_eq!(
            normalize_private_path("./files/state.json").unwrap(),
            "files/state.json"
        );
        for invalid in [
            "",
            "/data/user/0/app/files/state",
            "../files/state",
            "files/../../escape",
            ".shadowdroid_state/pending",
            "files\\state",
            "files/state\nother",
        ] {
            assert!(normalize_private_path(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn private_stat_parses_files_directories_modes_and_rejects_other_types() {
        let file = parse_private_stat("regular file|4210|600").unwrap();
        assert_eq!(file.kind, StatePathKind::File);
        assert_eq!(file.bytes, 4210);
        assert_eq!(file.mode, 0o600);
        let dir = parse_private_stat("directory|4096|771").unwrap();
        assert_eq!(dir.kind, StatePathKind::Directory);
        assert_eq!(dir.mode, 0o771);
        assert!(parse_private_stat("symbolic link|12|777").is_none());
    }

    #[test]
    fn signing_digest_uses_stable_signer_identity_not_package_object_id() {
        let first = "signatures=PackageSignatures{6f3a1b2 version:2, signatures:[c253d2d5], past signatures:[]}";
        let second = "signatures=PackageSignatures{abcdef0 version:2, signatures:[c253d2d5], past signatures:[]}";
        assert_eq!(signature_identity(first), Some("c253d2d5"));
        assert_eq!(signature_identity(first), signature_identity(second));
    }

    #[test]
    fn include_roots_collapse_descendants_and_sidecars_are_recognized() {
        assert!(is_same_or_descendant("shared_prefs/a.xml", "shared_prefs"));
        assert!(!is_same_or_descendant(
            "shared_prefs2/a.xml",
            "shared_prefs"
        ));
        assert!(is_sqlite_sidecar("databases/app.db-wal"));
        assert!(is_sqlite_sidecar("databases/app.db-shm"));
        assert!(!is_sqlite_sidecar("databases/app.db"));
        assert_eq!(
            sqlite_sidecar_candidates("databases/app.db"),
            [
                "databases/app.db-wal",
                "databases/app.db-shm",
                "databases/app.db-journal",
            ]
        );
    }

    #[test]
    fn transaction_root_metadata_round_trips_and_rejects_unsafe_paths() {
        let roots = parse_transaction_roots("shared_prefs\t1\nfiles/state.json\t0\n").unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots[0].existed);
        assert!(!roots[1].existed);
        assert!(parse_transaction_roots("../escape\t1\n").is_err());
        assert!(parse_transaction_roots("./files/state\t1\n").is_err());
        assert!(parse_transaction_roots("files/state\tmaybe\n").is_err());
        assert!(parse_transaction_roots("files\t1\nfiles/state\t1\n").is_err());
        assert!(parse_transaction_roots("files/state\t1\nfiles/state\t0\n").is_err());
        assert!(parse_transaction_roots("").is_err());
    }

    #[test]
    fn transaction_phases_and_terminal_ownership_parse_strictly() {
        assert_eq!(
            parse_transaction_phase("__shadowdroid_phase_missing__\n").unwrap(),
            TransactionPhase::Legacy
        );
        assert_eq!(
            parse_transaction_phase("__shadowdroid_phase_present__\nprepared\n").unwrap(),
            TransactionPhase::Prepared
        );
        assert_eq!(
            parse_transaction_phase("__shadowdroid_phase_present__\nverified\n").unwrap(),
            TransactionPhase::Verified
        );
        assert!(parse_transaction_phase("__shadowdroid_phase_present__\nunknown\n").is_err());

        let transaction = "txn-123-4-5";
        assert_eq!(
            parse_pending_terminal_state("__shadowdroid_pending__txn-123-4-5\n", transaction)
                .unwrap(),
            PendingTerminalState::Pending(transaction.into())
        );
        assert_eq!(
            parse_pending_terminal_state("__shadowdroid_terminal__txn-123-4-5\n", transaction)
                .unwrap(),
            PendingTerminalState::Terminal
        );
        assert_eq!(
            parse_pending_terminal_state("__shadowdroid_terminal_missing__\n", transaction)
                .unwrap(),
            PendingTerminalState::Missing
        );
        assert!(
            parse_pending_terminal_state("__shadowdroid_terminal__txn-other-1\n", transaction)
                .is_err()
        );
        assert_eq!(
            parse_pending_transaction_output(
                "__shadowdroid_pending_missing__\n",
                "com.example.app"
            )
            .unwrap(),
            None
        );
        assert_eq!(
            parse_pending_transaction_output(
                "__shadowdroid_pending_present__txn-123-4-5\n",
                "com.example.app"
            )
            .unwrap(),
            Some(transaction.into())
        );
        assert!(
            parse_pending_transaction_output("__shadowdroid_pending_present__", "com.example.app")
                .is_err()
        );
        assert!(
            parse_pending_transaction_output(
                "__shadowdroid_pending_invalid_type__\n",
                "com.example.app"
            )
            .is_err()
        );
    }

    #[test]
    fn transaction_scripts_put_failpoints_and_terminal_commit_points_in_order() {
        let roots = vec![
            TransactionRoot {
                path: "files/existing".into(),
                existed: true,
            },
            TransactionRoot {
                path: "files/new".into(),
                existed: false,
            },
        ];
        let commit = commit_script(
            &roots,
            ".shadowdroid_state/txn-1/stage",
            ".shadowdroid_state/txn-1/backup",
            Some("commit_after_backup_0"),
        );
        let backup = commit.find("mv 'files/existing'").unwrap();
        let failpoint = commit
            .find("__shadowdroid_app_state_failpoint__:commit_after_backup_0")
            .unwrap();
        let publish = commit
            .find("mv '.shadowdroid_state/txn-1/stage/files/existing'")
            .unwrap();
        assert!(backup < failpoint && failpoint < publish);

        let claim = claim_pending_script("txn-1");
        let no_replace_move = claim
            .find("mv -nT '.shadowdroid_state/txn-1/claim' '.shadowdroid_state/pending'")
            .unwrap();
        let source_absence_proof = claim
            .find("[ ! -e '.shadowdroid_state/txn-1/claim' ]")
            .unwrap();
        let ownership_proof = claim
            .find("$(cat '.shadowdroid_state/pending' 2>/dev/null)")
            .unwrap();
        assert!(no_replace_move < source_absence_proof && source_absence_proof < ownership_proof);
        assert!(!claim.contains("> '.shadowdroid_state/pending'"));

        let rollback = rollback_roots_script(
            &roots,
            ".shadowdroid_state/txn-1/backup",
            Some("rollback_after_root_0"),
        );
        let remove_new = rollback.find("rm -rf 'files/new'").unwrap();
        let rollback_failpoint = rollback
            .find("__shadowdroid_app_state_failpoint__:rollback_after_root_0")
            .unwrap();
        let restore_existing = rollback
            .find(".shadowdroid_state/txn-1/backup/files/existing")
            .unwrap();
        assert!(remove_new < rollback_failpoint && rollback_failpoint < restore_existing);

        let terminal = terminalize_pending_script("txn-1", "finalized", FINALIZE_OK);
        let move_pending = terminal
            .find("mv -nT '.shadowdroid_state/pending' '.shadowdroid_state/txn-1/finalized'")
            .unwrap();
        let proof = terminal.rfind(FINALIZE_OK).unwrap();
        assert!(move_pending < proof);
        assert!(!terminal.contains("rm -rf"));
    }

    #[test]
    fn commit_failpoints_recover_old_state_after_backup_or_publish() {
        for failpoint in ["commit_after_backup_0", "commit_after_publish_0"] {
            let temp = tempfile::tempdir().unwrap();
            let stage = ".shadowdroid_state/txn-1/stage";
            let backup = ".shadowdroid_state/txn-1/backup";
            std::fs::create_dir_all(temp.path().join(stage).join("files")).unwrap();
            std::fs::create_dir_all(temp.path().join("files")).unwrap();
            std::fs::write(temp.path().join("files/state"), b"old").unwrap();
            std::fs::write(temp.path().join(stage).join("files/state"), b"new").unwrap();
            let roots = vec![TransactionRoot {
                path: "files/state".into(),
                existed: true,
            }];

            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(commit_script(&roots, stage, backup, Some(failpoint)))
                .current_dir(temp.path())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(86), "{failpoint}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.contains(&format!("{TEST_FAILPOINT_MARKER}{failpoint}")));
            assert!(!has_output_marker(&stdout, COMMIT_OK));
            assert_eq!(
                std::fs::read(temp.path().join(backup).join("files/state")).unwrap(),
                b"old"
            );

            let rollback = rollback_roots_script(&roots, backup, None);
            for _ in 0..2 {
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&rollback)
                    .current_dir(temp.path())
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{failpoint}: {output:?}");
                assert!(has_output_marker(
                    &String::from_utf8(output.stdout).unwrap(),
                    ROLLBACK_OK
                ));
                assert_eq!(
                    std::fs::read(temp.path().join("files/state")).unwrap(),
                    b"old"
                );
            }
        }
    }

    #[test]
    fn rollback_failpoint_is_idempotent_and_terminal_cleanup_is_owned() {
        let temp = tempfile::tempdir().unwrap();
        let stage = ".shadowdroid_state/txn-1/stage";
        let backup = ".shadowdroid_state/txn-1/backup";
        std::fs::create_dir_all(temp.path().join(stage).join("files")).unwrap();
        std::fs::create_dir_all(temp.path().join("files")).unwrap();
        for name in ["a", "b"] {
            std::fs::write(temp.path().join("files").join(name), format!("old-{name}")).unwrap();
            std::fs::write(
                temp.path().join(stage).join("files").join(name),
                format!("new-{name}"),
            )
            .unwrap();
        }
        let roots = vec![
            TransactionRoot {
                path: "files/a".into(),
                existed: true,
            },
            TransactionRoot {
                path: "files/b".into(),
                existed: true,
            },
        ];
        let commit = std::process::Command::new("sh")
            .arg("-c")
            .arg(commit_script(&roots, stage, backup, None))
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(commit.status.success());

        let interrupted = std::process::Command::new("sh")
            .arg("-c")
            .arg(rollback_roots_script(
                &roots,
                backup,
                Some("rollback_after_root_0"),
            ))
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert_eq!(interrupted.status.code(), Some(86));
        let completed = std::process::Command::new("sh")
            .arg("-c")
            .arg(rollback_roots_script(&roots, backup, None))
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(completed.status.success());
        assert_eq!(
            std::fs::read(temp.path().join("files/a")).unwrap(),
            b"old-a"
        );
        assert_eq!(
            std::fs::read(temp.path().join("files/b")).unwrap(),
            b"old-b"
        );

        let valid = temp.path().join(".shadowdroid_state/txn-2");
        let invalid = temp.path().join(".shadowdroid_state/txn-3");
        std::fs::create_dir_all(&valid).unwrap();
        std::fs::create_dir_all(&invalid).unwrap();
        std::fs::write(valid.join("finalized"), b"txn-2\n").unwrap();
        std::fs::write(invalid.join("rolled_back"), b"txn-other\n").unwrap();
        let cleanup = std::process::Command::new("sh")
            .arg("-c")
            .arg(terminal_cleanup_script())
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(cleanup.status.success());
        assert!(has_output_marker(
            &String::from_utf8(cleanup.stdout).unwrap(),
            "__shadowdroid_terminal_cleanup_ok__:1"
        ));
        assert!(!valid.exists());
        assert!(invalid.exists());
    }

    #[test]
    fn manifest_validation_rejects_traversal_and_duplicate_files() {
        let temp = tempfile::tempdir().unwrap();
        let base = SnapshotManifest {
            artifact_type: SNAPSHOT_TYPE.into(),
            format_version: FORMAT_VERSION,
            created_at_ms: 1,
            package: "com.example.app".into(),
            version_code: 1,
            version_name: Some("1".into()),
            signature_digest: "abc".into(),
            signature_digest_source: "test".into(),
            contains_sensitive_data: true,
            encrypted: false,
            privacy: SnapshotPrivacy {
                host_directory_mode: "0700".into(),
                host_file_mode: "0600".into(),
                warning: "sensitive".into(),
            },
            roots: vec![SnapshotRoot {
                path: "files/state".into(),
                kind: StatePathKind::File,
                implicit_sqlite_sidecar: false,
            }],
            directories: vec![],
            files: vec![StateFile {
                path: "files/state".into(),
                bytes: 0,
                sha256: sha256_bytes(b""),
                mode: 0o600,
            }],
        };
        assert!(validate_manifest(&base, temp.path()).is_ok());
        let mut duplicate = base.clone();
        duplicate.files.push(duplicate.files[0].clone());
        assert!(validate_manifest(&duplicate, temp.path()).is_err());
        let mut traversal = base.clone();
        traversal.files[0].path = "../secret".into();
        assert!(validate_manifest(&traversal, temp.path()).is_err());

        let mut no_roots = base.clone();
        no_roots.roots.clear();
        assert!(validate_manifest(&no_roots, temp.path()).is_err());

        let mut duplicate_roots = base.clone();
        duplicate_roots.roots.push(duplicate_roots.roots[0].clone());
        assert!(validate_manifest(&duplicate_roots, temp.path()).is_err());

        let mut wrong_kind = base.clone();
        wrong_kind.roots[0].kind = StatePathKind::Directory;
        assert!(validate_manifest(&wrong_kind, temp.path()).is_err());

        let mut file_directory_collision = base.clone();
        file_directory_collision.directories.push(StateDirectory {
            path: "files/state".into(),
            mode: 0o700,
        });
        assert!(validate_manifest(&file_directory_collision, temp.path()).is_err());

        let mut overlap = base.clone();
        overlap.directories.push(StateDirectory {
            path: "files".into(),
            mode: 0o700,
        });
        overlap.roots.insert(
            0,
            SnapshotRoot {
                path: "files".into(),
                kind: StatePathKind::Directory,
                implicit_sqlite_sidecar: false,
            },
        );
        assert!(validate_manifest(&overlap, temp.path()).is_err());

        let mut outside_root = base;
        outside_root.files.push(StateFile {
            path: "shared_prefs/unclaimed.xml".into(),
            bytes: 0,
            sha256: sha256_bytes(b""),
            mode: 0o600,
        });
        assert!(validate_manifest(&outside_root, temp.path()).is_err());
    }

    #[test]
    fn cleanup_overwrites_and_removes_only_a_valid_snapshot() {
        let parent = tempfile::tempdir().unwrap();
        let snapshot = parent.path().join("state");
        std::fs::create_dir_all(snapshot.join("data/files")).unwrap();
        std::fs::write(snapshot.join("data/files/token"), b"secret-token").unwrap();
        let manifest = SnapshotManifest {
            artifact_type: SNAPSHOT_TYPE.into(),
            format_version: FORMAT_VERSION,
            created_at_ms: 1,
            package: "com.example.app".into(),
            version_code: 1,
            version_name: None,
            signature_digest: "abc".into(),
            signature_digest_source: "test".into(),
            contains_sensitive_data: true,
            encrypted: false,
            privacy: SnapshotPrivacy {
                host_directory_mode: "0700".into(),
                host_file_mode: "0600".into(),
                warning: "sensitive".into(),
            },
            roots: vec![SnapshotRoot {
                path: "files/token".into(),
                kind: StatePathKind::File,
                implicit_sqlite_sidecar: false,
            }],
            directories: vec![],
            files: vec![StateFile {
                path: "files/token".into(),
                bytes: 12,
                sha256: sha256_bytes(b"secret-token"),
                mode: 0o600,
            }],
        };
        artifact::write_json(
            &snapshot.join("manifest.json"),
            &serde_json::to_value(manifest).unwrap(),
        )
        .unwrap();
        cleanup(&snapshot).unwrap();
        assert!(!snapshot.exists());
    }
}
