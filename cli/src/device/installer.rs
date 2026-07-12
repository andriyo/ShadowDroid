//! APK lifecycle manager — resolve, install, and verify the on-device server.
//!
//! Source-precedence chain (first hit wins):
//!
//! 1. `--apk PATH` flag (explicit, highest priority)
//! 2. `SHADOWDROID_APK` env var (same semantics as `--apk`)
//! 3. Repo auto-discovery in `$CWD` or any ancestor. The test APK is under
//!    `server/app/build/outputs/apk/androidTest/debug/`, with its sibling main
//!    APK under `server/app/build/outputs/apk/debug/`.
//! 4. Dev drop-in: `~/.shadowdroid/apks/local/{main,test}.apk`
//! 5. Versioned cache: `~/.shadowdroid/apks/<EXPECTED_APK_VERSION>/{main,test}.apk`
//! 6. Download from GitHub releases.
//!
//! Sources 1-4 are *developer* sources: we install them as-is, identifying
//! re-install need by APK SHA-256 instead of versionName (so a `gradlew
//! assembleDebug` followed by `shadowdroid connect` reinstalls if and only
//! if bytes changed). Sources 5-6 are *user* sources; versionName must match
//! the CLI's baked-in `EXPECTED_APK_VERSION`.

use crate::ids::{Serial, stable_file_component};
use anyhow::{Context, Result, anyhow, bail};
use std::fs::{self, File, TryLockError};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::device::{adb, client::ServerClient, portmap};
use crate::hostenv::{env_truthy, shadowdroid_home};
use crate::release::{
    CHECKSUMS_ASSET, checksum_for, download_text, download_verified_file, expected_sha,
    release_asset_url, release_base_url, release_client, sha256_file, verify_sha256,
};

pub const EXPECTED_APK_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const APP_PACKAGE: &str = "io.github.andriyo.shadowdroid";
pub const TEST_PACKAGE: &str = "io.github.andriyo.shadowdroid.test";
/// [`portmap`] channel for the on-device UI server's host-side `adb forward`.
pub const UI_CHANNEL: &str = "ui";
/// Standard AndroidJUnitRunner — we run a normal @Test method in
/// `SERVER_TEST_CLASS` that holds the process open. See ShadowDroidServerTest.kt
/// for why this is the proven pattern over a custom runner subclass.
pub const RUNNER_CLASS: &str = "androidx.test.runner.AndroidJUnitRunner";
pub const SERVER_TEST_CLASS: &str = "io.github.andriyo.shadowdroid.ShadowDroidServerTest";
pub const DEFAULT_PORT: u16 = 7912;
const RELEASE_MAIN_APK_ASSET: &str = "shadowdroid-server-main.apk";
const RELEASE_TEST_APK_ASSET: &str = "shadowdroid-server-test.apk";
const CACHE_MAIN_APK: &str = "main.apk";
const CACHE_TEST_APK: &str = "test.apk";
const CACHE_COMPLETE_FILE: &str = ".complete.sha256";
pub const INSTRUMENT_LOG_PATH: &str = "/sdcard/shadowdroid-instr.log";

/// Cross-process ownership guard for lifecycle mutations on one device. The
/// OS releases the advisory lock when this file handle closes, including when
/// the owning process exits unexpectedly. The lock file itself is persistent:
/// unlinking a held lock can let another process lock a different inode at the
/// same path and enter the critical section concurrently.
#[derive(Debug)]
pub struct DeviceLifecycleGuard {
    file: File,
}

impl Drop for DeviceLifecycleGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn acquire_lifecycle_lock(serial: &Serial) -> Result<DeviceLifecycleGuard> {
    let dir = shadowdroid_home()?.join("locks");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let component = stable_file_component(serial.as_str());
    acquire_lifecycle_lock_at(serial, &dir.join(format!("device-{component}.lock")))
}

fn acquire_lifecycle_lock_at(serial: &Serial, path: &Path) -> Result<DeviceLifecycleGuard> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let owner = std::fs::read_to_string(path)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            return Err(crate::diagnostic::DiagnosticError::new(
                "device_lifecycle_busy",
                "device",
                format!("another ShadowDroid process is changing device {serial}"),
            )
            .retryable(true)
            .detail(serde_json::json!({
                "device": serial.as_str(),
                "owner_pid": owner,
                "lock": path.display().to_string(),
            }))
            .next_actions([
                "wait for the active ShadowDroid lifecycle command to finish, then retry",
            ])
            .into());
        }
        Err(TryLockError::Error(error)) => {
            return Err(error).with_context(|| format!("lock {}", path.display()));
        }
    }

    let write_result = (|| -> std::io::Result<()> {
        file.set_len(0)?;
        file.rewind()?;
        write_lock_pid(&mut file, std::process::id())?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = file.unlock();
        return Err(crate::diagnostic::DiagnosticError::new(
            "device_lifecycle_lock_write_failed",
            "device",
            format!("could not record ownership of the lifecycle lock for {serial}"),
        )
        .retryable(true)
        .detail(serde_json::json!({
            "device": serial.as_str(),
            "lock": path.display().to_string(),
            "error": error.to_string(),
        }))
        .next_actions(["retry the lifecycle command"])
        .into());
    }

    Ok(DeviceLifecycleGuard { file })
}

fn write_lock_pid(writer: &mut impl Write, pid: u32) -> std::io::Result<()> {
    writeln!(writer, "{pid}")
}

/// Where each APK pair came from. Used for logging + to decide whether to
/// version-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApkSource {
    Explicit,
    RepoBuild,
    LocalDropIn,
    VersionedCache,
    GithubRelease,
}

impl ApkSource {
    /// Sources 1-4 are dev mode: trust the bytes, version-check is by hash.
    pub fn is_dev(self) -> bool {
        matches!(self, Self::Explicit | Self::RepoBuild | Self::LocalDropIn)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Explicit => "--apk / SHADOWDROID_APK",
            Self::RepoBuild => "repo auto-discovery",
            Self::LocalDropIn => "~/.shadowdroid/apks/local/",
            Self::VersionedCache => "~/.shadowdroid/apks/<version>/",
            Self::GithubRelease => "GitHub release",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApkPair {
    pub main: PathBuf,
    pub test: PathBuf,
    pub source: ApkSource,
}

/// Walk the precedence chain and return the APK pair to install.
pub async fn resolve_apk(explicit: Option<&Path>) -> Result<ApkPair> {
    // 1. Explicit override (--apk / SHADOWDROID_APK)
    if let Some(p) = explicit {
        return resolve_explicit(p);
    }
    let disable_dev_sources = env_truthy("SHADOWDROID_DISABLE_DEV_SOURCES");
    if !disable_dev_sources {
        // 2. Repo auto-discovery
        if let Some(pair) = resolve_repo_build()? {
            info!(
                "using local APK at {} (dev mode, source: {})",
                pair.test.display(),
                pair.source.label()
            );
            return Ok(pair);
        }
        // 3. Local drop-in
        if let Some(pair) = resolve_local_dropin()? {
            info!(
                "using local APK at {} (dev mode, source: {})",
                pair.test.display(),
                pair.source.label()
            );
            return Ok(pair);
        }
    } else {
        info!("skipping repo/local APK discovery because SHADOWDROID_DISABLE_DEV_SOURCES is set");
    }
    // 4. Versioned cache
    if let Some(pair) = resolve_versioned_cache()? {
        return Ok(pair);
    }
    // 5. GitHub release
    download_github_release().await
}

fn resolve_explicit(p: &Path) -> Result<ApkPair> {
    let (main, test) = pair_from_path(p)?;
    info!(
        "using local APK at {} (dev mode, source: --apk explicit)",
        test.display()
    );
    Ok(ApkPair {
        main,
        test,
        source: ApkSource::Explicit,
    })
}

fn resolve_repo_build() -> Result<Option<ApkPair>> {
    let cwd = std::env::current_dir().context("cannot read $CWD")?;
    let mut dir: &Path = &cwd;
    loop {
        let test_glob = dir.join("server/app/build/outputs/apk/androidTest/debug");
        if test_glob.is_dir()
            && let Some(test) = first_apk(&test_glob)?
        {
            let main_glob = dir.join("server/app/build/outputs/apk/debug");
            if let Some(main) = first_apk(&main_glob)? {
                return Ok(Some(ApkPair {
                    main,
                    test,
                    source: ApkSource::RepoBuild,
                }));
            }
            warn!(
                "found test APK at {} but no main APK at {} — building both: \
                     `./gradlew :app:assembleDebug :app:assembleDebugAndroidTest`",
                test_glob.display(),
                main_glob.display()
            );
            return Ok(None);
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Ok(None),
        }
    }
}

fn resolve_local_dropin() -> Result<Option<ApkPair>> {
    let local = shadowdroid_home()?.join("apks/local");
    let main = local.join("main.apk");
    let test = local.join("test.apk");
    if main.is_file() && test.is_file() {
        Ok(Some(ApkPair {
            main,
            test,
            source: ApkSource::LocalDropIn,
        }))
    } else {
        Ok(None)
    }
}

fn resolve_versioned_cache() -> Result<Option<ApkPair>> {
    let dir = versioned_cache_dir()?;
    let main = dir.join(CACHE_MAIN_APK);
    let test = dir.join(CACHE_TEST_APK);
    if apk_cache_is_complete(&dir) {
        info!(
            "using cached APK at {} (version {EXPECTED_APK_VERSION})",
            dir.display()
        );
        Ok(Some(ApkPair {
            main,
            test,
            source: ApkSource::VersionedCache,
        }))
    } else {
        Ok(None)
    }
}

async fn download_github_release() -> Result<ApkPair> {
    let cache_dir = versioned_cache_dir()?;
    let cache_parent = cache_dir
        .parent()
        .ok_or_else(|| anyhow!("invalid APK cache path {}", cache_dir.display()))?;
    fs::create_dir_all(cache_parent).context(format!(
        "create APK cache parent {}",
        cache_parent.display()
    ))?;
    let staging_dir = tempfile::Builder::new()
        .prefix(&format!(".download-{EXPECTED_APK_VERSION}-"))
        .tempdir_in(cache_parent)
        .context("create APK download staging directory")?;

    let base = release_base_url(EXPECTED_APK_VERSION);
    let main_url = release_asset_url(&base, RELEASE_MAIN_APK_ASSET);
    let test_url = release_asset_url(&base, RELEASE_TEST_APK_ASSET);
    let sums_url = release_asset_url(&base, CHECKSUMS_ASSET);
    info!("downloading ShadowDroid server APKs from {base}");

    let client = release_client()?;
    let checksums = download_text(&client, &sums_url).await.with_context(|| {
        format!("download {CHECKSUMS_ASSET} from GitHub release v{EXPECTED_APK_VERSION}")
    })?;
    let main_sha = expected_sha(
        option_env!("SHADOWDROID_RELEASE_MAIN_APK_SHA256"),
        &checksums,
        RELEASE_MAIN_APK_ASSET,
    )?;
    let test_sha = expected_sha(
        option_env!("SHADOWDROID_RELEASE_TEST_APK_SHA256"),
        &checksums,
        RELEASE_TEST_APK_ASSET,
    )?;

    let staged_main = staging_dir.path().join(CACHE_MAIN_APK);
    let staged_test = staging_dir.path().join(CACHE_TEST_APK);
    let main_receipt = download_verified_file(&client, &main_url, &staged_main, &main_sha)
        .await
        .with_context(|| format!("download and verify {RELEASE_MAIN_APK_ASSET}"))?;
    let test_receipt = download_verified_file(&client, &test_url, &staged_test, &test_sha)
        .await
        .with_context(|| format!("download and verify {RELEASE_TEST_APK_ASSET}"))?;
    publish_apk_pair(
        staging_dir.path(),
        &cache_dir,
        &main_receipt.sha256,
        &test_receipt.sha256,
    )?;
    info!(
        "cached ShadowDroid server APKs at {} (version {EXPECTED_APK_VERSION})",
        cache_dir.display()
    );
    Ok(ApkPair {
        main: cache_dir.join(CACHE_MAIN_APK),
        test: cache_dir.join(CACHE_TEST_APK),
        source: ApkSource::GithubRelease,
    })
}

fn publish_apk_pair(
    staging_dir: &Path,
    cache_dir: &Path,
    main_sha: &str,
    test_sha: &str,
) -> Result<()> {
    let staged_main = staging_dir.join(CACHE_MAIN_APK);
    let staged_test = staging_dir.join(CACHE_TEST_APK);
    if !staged_main.is_file() || !staged_test.is_file() {
        bail!("cannot publish incomplete staged APK pair")
    }
    fs::create_dir_all(cache_dir).context(format!("create {}", cache_dir.display()))?;

    // Both downloads are complete and verified before the old cache becomes
    // unavailable. With the marker withdrawn, readers reject the brief mixed
    // generation while the two atomic file replacements are published.
    let marker = cache_dir.join(CACHE_COMPLETE_FILE);
    match fs::remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("invalidate {}", marker.display()));
        }
    }

    tempfile::TempPath::try_from_path(staged_main)
        .context("adopt staged main APK")?
        .persist(cache_dir.join(CACHE_MAIN_APK))
        .map_err(|error| error.error)
        .context("atomically publish main APK")?;
    tempfile::TempPath::try_from_path(staged_test)
        .context("adopt staged test APK")?
        .persist(cache_dir.join(CACHE_TEST_APK))
        .map_err(|error| error.error)
        .context("atomically publish test APK")?;
    write_apk_cache_complete(cache_dir, main_sha, test_sha)
}

fn apk_cache_is_complete(cache_dir: &Path) -> bool {
    let manifest = match fs::read_to_string(cache_dir.join(CACHE_COMPLETE_FILE)) {
        Ok(manifest) => manifest,
        Err(_) => return false,
    };
    let Some(main_sha) = checksum_for(&manifest, CACHE_MAIN_APK) else {
        return false;
    };
    let Some(test_sha) = checksum_for(&manifest, CACHE_TEST_APK) else {
        return false;
    };
    verify_sha256(&cache_dir.join(CACHE_MAIN_APK), &main_sha).is_ok()
        && verify_sha256(&cache_dir.join(CACHE_TEST_APK), &test_sha).is_ok()
}

fn write_apk_cache_complete(cache_dir: &Path, main_sha: &str, test_sha: &str) -> Result<()> {
    let destination = cache_dir.join(CACHE_COMPLETE_FILE);
    let mut temporary = tempfile::NamedTempFile::new_in(cache_dir)
        .with_context(|| format!("create completion marker in {}", cache_dir.display()))?;
    writeln!(temporary, "{main_sha}  {CACHE_MAIN_APK}")?;
    writeln!(temporary, "{test_sha}  {CACHE_TEST_APK}")?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically publish {}", destination.display()))?;
    #[cfg(unix)]
    fs::File::open(cache_dir)
        .with_context(|| format!("open {} for sync", cache_dir.display()))?
        .sync_all()
        .with_context(|| format!("sync {}", cache_dir.display()))?;
    Ok(())
}

fn versioned_cache_dir() -> Result<PathBuf> {
    Ok(shadowdroid_home()?.join("apks").join(EXPECTED_APK_VERSION))
}

/// Given a path that's either the test APK or a directory containing both,
/// return (main_apk_path, test_apk_path).
fn pair_from_path(p: &Path) -> Result<(PathBuf, PathBuf)> {
    if p.is_dir() {
        let main = first_apk_matching(p, "app-debug.apk")?
            .or(first_apk_matching(p, "main.apk")?)
            .ok_or_else(|| anyhow!("no main APK found in {}", p.display()))?;
        let test = first_apk_matching(p, "-androidTest.apk")?
            .or(first_apk_matching(p, "test.apk")?)
            .ok_or_else(|| anyhow!("no test/androidTest APK found in {}", p.display()))?;
        return Ok((main, test));
    }
    if !p.is_file() {
        bail!("--apk path does not exist: {}", p.display());
    }
    // It's a single file. Assume it's the test APK; find the sibling main.
    let parent = p
        .parent()
        .ok_or_else(|| anyhow!("--apk has no parent dir: {}", p.display()))?;
    // The main APK lives in androidTest/debug → ../../debug (Gradle layout)
    // or in the same dir (user-staged).
    let candidates = [parent.join("../../debug"), parent.to_path_buf()];
    for cand in &candidates {
        if cand.is_dir()
            && let Some(main) =
                first_apk_matching(cand, "app-debug.apk")?.or(first_apk_matching(cand, "main.apk")?)
            && main != *p
        {
            return Ok((main, p.to_path_buf()));
        }
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "apk_pair_incomplete",
        "install",
        format!(
            "could not resolve a main/test APK pair from {}",
            p.display()
        ),
    )
    .detail(serde_json::json!({
        "provided": p.display().to_string(),
        "expected": "a test/androidTest APK path, or a directory containing both main and test APKs",
        "parent": parent.display().to_string(),
        "searched": candidates.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
    }))
    .next_actions([
        format!(
            "shadowdroid --apk {} connect",
            crate::events::shell_token(&parent.display().to_string())
        ),
        format!(
            "find {} -maxdepth 3 -name '*.apk' -print",
            crate::events::shell_token(&parent.display().to_string())
        ),
        "pass the test/androidTest APK file, or stage both main and test APKs in one directory"
            .to_string(),
    ])
    .into())
}

fn first_apk(dir: &Path) -> Result<Option<PathBuf>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut found: Option<PathBuf> = None;
    for entry in fs::read_dir(dir).context(format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("apk") {
            found = Some(path);
            break;
        }
    }
    Ok(found)
}

fn first_apk_matching(dir: &Path, suffix: &str) -> Result<Option<PathBuf>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    for entry in fs::read_dir(dir).context(format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.ends_with(suffix) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

// ── full lifecycle: resolve → install → forward → instrument → poll ──────

/// Set up `adb forward <host_port> -> tcp:7912` for `serial`, returning the
/// per-serial host port. Reuses the persisted port (idempotent re-assert); if
/// that port is now held by an unrelated process, reallocates once. Scoping the
/// host port to the serial is what lets concurrent sessions drive different
/// devices without rebinding each other's forward.
pub async fn ensure_forward(serial: &Serial) -> Result<u16> {
    portmap::publish_forward(serial, UI_CHANNEL, DEFAULT_PORT).await
}

/// Return a client only when the already-established session is reachable.
/// This deliberately does not allocate/reassert an adb forward, install APKs,
/// start instrumentation, or clean up processes. Diagnostic commands use it to
/// preserve their read-only contract.
pub async fn probe_existing(
    serial: &Serial,
    any_apk_version: bool,
) -> Result<Option<ServerClient>> {
    let Some(host_port) = portmap::peek(serial, UI_CHANNEL) else {
        return Ok(None);
    };
    let client = ServerClient::new(host_port)?;
    match probe(&client, any_apk_version).await {
        ProbeResult::Ready => Ok(Some(client)),
        ProbeResult::VersionMismatch { .. } | ProbeResult::Down => Ok(None),
    }
}

/// Make sure the device has our server running and reachable, then return
/// a connected ServerClient ready to use.
pub async fn ensure_ready(
    serial: &Serial,
    explicit_apk: Option<&Path>,
    any_apk_version: bool,
) -> Result<ServerClient> {
    let _guard = acquire_lifecycle_lock(serial)?;
    ensure_ready_locked(serial, explicit_apk, any_apk_version).await
}

async fn ensure_ready_locked(
    serial: &Serial,
    explicit_apk: Option<&Path>,
    any_apk_version: bool,
) -> Result<ServerClient> {
    // Probe early: if the server is already up from a previous connect, we can
    // skip APK resolution, install checks, and any cooldowns. Warm path stays
    // <100ms. The host port is per-serial so two sessions on different devices
    // each connect to their own loopback port.
    let host_port = ensure_forward(serial).await?;
    let client = ServerClient::new(host_port)?;
    match probe(&client, any_apk_version).await {
        ProbeResult::Ready => {
            info!("server already up — reusing");
            return Ok(client);
        }
        ProbeResult::VersionMismatch { found } => {
            warn!(
                "server version mismatch: expected {EXPECTED_APK_VERSION}, got {found}; \
                 stopping stale server before reconnect"
            );
            cleanup_stale_server(serial, host_port).await?;
        }
        ProbeResult::Down => {}
    }
    // Cold path: resolve + install + start. May need a retry with longer
    // cooldown if Android's system_server hasn't released the UiAutomation
    // slot from a prior dev cycle, or another UI automation process is still
    // claiming it.
    let pair = resolve_apk(explicit_apk).await?;
    install_if_needed(serial, &pair, any_apk_version).await?;
    adb::forward(serial, host_port, DEFAULT_PORT).await?;
    start_instrumentation(serial).await?;
    if wait_for_server(serial, &client, any_apk_version)
        .await
        .is_ok()
    {
        return Ok(client);
    }
    // First start failed — most likely the UiAutomation slot is still owned
    // by a prior instrumentation/app_process. Heavier cleanup + retry.
    warn!(
        "first start attempt failed; cooling down 10s and retrying (UiAutomation slot may need time to release)"
    );
    long_cooldown(serial).await?;
    start_instrumentation(serial).await?;
    wait_for_server(serial, &client, any_apk_version).await?;
    Ok(client)
}

/// Like [`ensure_ready`], but refuses to repair a live server that is merely
/// stale. This keeps an unrelated UI/read command from unexpectedly spending
/// seconds reinstalling the server after a CLI upgrade; `connect` and
/// `doctor --fix` remain the explicit reconciliation paths.
pub async fn ensure_ready_for_command(
    serial: &Serial,
    explicit_apk: Option<&Path>,
    any_apk_version: bool,
) -> Result<ServerClient> {
    // The common path is a direct HTTP probe through the already-established
    // forward. Avoid an ADB round-trip and lifecycle lock unless the mapping is
    // actually absent or broken.
    if let Some(host_port) = portmap::peek(serial, UI_CHANNEL) {
        let client = ServerClient::new(host_port)?;
        match probe(&client, any_apk_version).await {
            ProbeResult::Ready => return Ok(client),
            ProbeResult::VersionMismatch { found } => {
                return Err(running_version_mismatch_error(&found));
            }
            ProbeResult::Down => {}
        }
    }

    let _guard = acquire_lifecycle_lock(serial)?;
    let host_port = ensure_forward(serial).await?;
    let client = ServerClient::new(host_port)?;
    match probe(&client, any_apk_version).await {
        ProbeResult::Ready => Ok(client),
        ProbeResult::VersionMismatch { found } => Err(running_version_mismatch_error(&found)),
        ProbeResult::Down => ensure_ready_locked(serial, explicit_apk, any_apk_version).await,
    }
}

fn running_version_mismatch_error(found: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "server_version_mismatch",
        "connect",
        format!(
            "ShadowDroid server is running version {found}, but this CLI expects {EXPECTED_APK_VERSION}"
        ),
    )
    .detail(serde_json::json!({
        "found": found,
        "expected": EXPECTED_APK_VERSION,
    }))
    .next_actions(["shadowdroid connect", "shadowdroid doctor --fix --json"])
    .into()
}

/// Best-effort version probe used by `connect` to report when it reconciled a
/// stale live server. Returns `Ok(None)` when no server is answering.
pub async fn running_server_version(serial: &Serial) -> Result<Option<String>> {
    let Some(client) = probe_existing(serial, true).await? else {
        return Ok(None);
    };
    Ok(client.state().await.ok().map(|state| state.server_version))
}

/// Heavy-handed cleanup for the case where system_server is holding a stale
/// `UiAutomationService` registration. Force-stop everything, then wait long
/// enough for system_server to actually release the slot (~5-10s observed).
async fn long_cooldown(serial: &Serial) -> Result<()> {
    adb::am_force_stop(serial, TEST_PACKAGE).await?;
    adb::am_force_stop(serial, APP_PACKAGE).await?;
    adb::kill_instrument_zombies(serial).await?;
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    Ok(())
}

async fn cleanup_stale_server(serial: &Serial, host_port: u16) -> Result<()> {
    adb::forward_remove(serial, host_port).await.ok();
    adb::kill_instrument_zombies(serial).await?;
    adb::am_force_stop(serial, TEST_PACKAGE).await.ok();
    adb::am_force_stop(serial, APP_PACKAGE).await.ok();
    adb::forward(serial, host_port, DEFAULT_PORT).await.ok();
    Ok(())
}

async fn install_if_needed(serial: &Serial, pair: &ApkPair, any_apk_version: bool) -> Result<()> {
    let installed_main = adb::pm_path(serial, APP_PACKAGE).await?.is_some();
    let installed_test = adb::pm_path(serial, TEST_PACKAGE).await?.is_some();
    if !installed_main || !installed_test {
        info!("installing main + test APKs (cold install)");
        install_pair(serial, pair).await?;
        return verify_installed_version(serial, pair.source, any_apk_version).await;
    }
    if pair.source.is_dev() {
        // Explicit --apk: the user pointed us at specific bytes, always reinstall.
        if pair.source == ApkSource::Explicit {
            info!("reinstalling APKs (dev mode, explicit --apk)");
            return install_pair(serial, pair).await;
        }
        // Repo build / local drop-in: reinstall iff the bytes changed since the
        // last install, so `gradlew assembleDebug` + `connect` reliably picks up
        // server edits without a manual uninstall. The androidTest APK carries
        // the server code (and has no versionName), so we key the decision off
        // its SHA-256. Hashing is best-effort: if we can't read the installed
        // APK's hash, reinstall rather than risk silently running stale code.
        return match test_apk_changed(serial, &pair.test).await {
            Ok(false) => Ok(()),
            Ok(true) => {
                info!("reinstalling APKs (dev mode, test APK bytes changed)");
                install_pair(serial, pair).await
            }
            Err(e) => {
                warn!("could not compare installed test-APK hash ({e}); reinstalling to be safe");
                install_pair(serial, pair).await
            }
        };
    }
    if any_apk_version {
        return Ok(());
    }
    // The androidTest package has no versionName (always null), so keying the
    // decision off it would reinstall on every connect. The main package's
    // version is authoritative and the two APKs are always built together.
    let main_version = adb::pm_version(serial, APP_PACKAGE).await?;
    if main_version.as_deref() != Some(EXPECTED_APK_VERSION) {
        info!(
            "reinstalling APKs (expected version {EXPECTED_APK_VERSION}, found main={:?})",
            main_version
        );
        install_pair(serial, pair).await?;
        return verify_installed_version(serial, pair.source, any_apk_version).await;
    }
    Ok(())
}

/// After installing a *user* APK (versioned cache / GitHub release), confirm it
/// actually self-reports the version this CLI expects. Dev sources are matched by
/// bytes (not versionName) and `--any-apk-version` opts out of the gate, so both
/// short-circuit. A mismatch here means a *mislabeled* artifact — e.g. a release
/// APK packaged with a stale `versionName` — which reinstalling can never fix.
/// Failing fast with actionable guidance beats letting the version gate retry the
/// installer and stall on every subsequent command.
async fn verify_installed_version(
    serial: &Serial,
    source: ApkSource,
    any_apk_version: bool,
) -> Result<()> {
    if source.is_dev() || any_apk_version {
        return Ok(());
    }
    let found = adb::pm_version(serial, APP_PACKAGE).await?;
    if found.as_deref() != Some(EXPECTED_APK_VERSION) {
        return Err(mislabeled_apk_error(source, found.as_deref()));
    }
    Ok(())
}

/// Actionable error for a freshly-installed user APK whose `versionName` doesn't
/// match this CLI. Names both the bypass flag and the env var, and suggests a
/// known-good local build — the recovery paths that actually work for a mislabeled
/// cached/release artifact (plain `disconnect` + `connect` does not).
fn mislabeled_apk_error(source: ApkSource, found: Option<&str>) -> anyhow::Error {
    let found = found.unwrap_or("(none)");
    crate::diagnostic::DiagnosticError::new(
        "apk_version_mismatch",
        "connect",
        format!(
            "installed {APP_PACKAGE} reports versionName {found}, but this CLI expects {EXPECTED_APK_VERSION}"
        ),
    )
    .detail(serde_json::json!({
        "source": source.label(),
        "found": found,
        "expected": EXPECTED_APK_VERSION,
        "package": APP_PACKAGE,
    }))
    .next_actions([
        "shadowdroid connect --apk <path-to-known-good-test-apk>",
        "use --any-apk-version only after verifying that the local APK is intentionally compatible",
    ])
    .into()
}

/// Install the main + test APKs, recovering from a signature mismatch. When the
/// installed build was signed with a different key (e.g. a release APK over a
/// locally dev-signed one), `adb install` fails with
/// `INSTALL_FAILED_UPDATE_INCOMPATIBLE`. Our server packages hold no user data,
/// so on that error we uninstall both and install fresh — this is what makes
/// `doctor --fix` (and `connect`) able to recover a cross-signed device instead
/// of dead-ending.
async fn install_pair(serial: &Serial, pair: &ApkPair) -> Result<()> {
    match try_install_pair(serial, pair).await {
        Ok(()) => Ok(()),
        Err(e) if is_signature_mismatch(&e) => {
            warn!(
                "APK signature differs from the installed build; uninstalling ShadowDroid \
                 server packages and reinstalling fresh (they hold no user data)"
            );
            // Uninstall the test package before the app it instruments.
            adb::uninstall(serial, TEST_PACKAGE).await.ok();
            adb::uninstall(serial, APP_PACKAGE).await.ok();
            try_install_pair(serial, pair).await
        }
        Err(e) => Err(e),
    }
}

async fn try_install_pair(serial: &Serial, pair: &ApkPair) -> Result<()> {
    adb::install(serial, pair.main.clone()).await?;
    adb::install(serial, pair.test.clone()).await?;
    Ok(())
}

/// True when the locally-built test APK differs (by SHA-256) from the copy
/// installed on the device. Lets the dev loop honour "reinstall iff bytes
/// changed" for repo/drop-in builds without a manual uninstall.
async fn test_apk_changed(serial: &Serial, local_test: &Path) -> Result<bool> {
    let device_path = adb::pm_path(serial, TEST_PACKAGE)
        .await?
        .ok_or_else(|| anyhow!("{TEST_PACKAGE} not installed"))?;
    let device_path_arg = crate::config::quote_device_shell_arg(&device_path);
    let out = adb::shell(serial, format!("sha256sum {device_path_arg}")).await?;
    let device_hash = out
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("empty sha256sum output for {device_path}"))?
        .to_lowercase();
    let local_hash = sha256_file(local_test)?;
    Ok(device_hash != local_hash)
}

fn is_signature_mismatch(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") || s.contains("signatures do not match")
}

async fn start_instrumentation(serial: &Serial) -> Result<()> {
    // Kill any zombie app_process from previous runs — they hold the
    // UiAutomation slot and would cause "already registered" errors.
    adb::kill_instrument_zombies(serial).await?;
    adb::am_force_stop(serial, TEST_PACKAGE).await?;
    adb::am_force_stop(serial, APP_PACKAGE).await?;
    let runner = format!("{TEST_PACKAGE}/{RUNNER_CLASS}");
    adb::am_instrument(serial, runner, Some(SERVER_TEST_CLASS), INSTRUMENT_LOG_PATH).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeResult {
    Ready,
    VersionMismatch { found: String },
    Down,
}

async fn probe(client: &ServerClient, any_apk_version: bool) -> ProbeResult {
    match client.state().await {
        Ok(state) if any_apk_version || state.server_version == EXPECTED_APK_VERSION => {
            ProbeResult::Ready
        }
        Ok(state) => ProbeResult::VersionMismatch {
            found: state.server_version,
        },
        Err(_) => ProbeResult::Down,
    }
}

async fn wait_for_server(
    serial: &Serial,
    client: &ServerClient,
    any_apk_version: bool,
) -> Result<()> {
    // 10s gives Ktor + UiAutomation + JUnit setup enough headroom on slower
    // emulators (Android 16 emulator with fresh APK install can take ~3s
    // for the test framework to fully initialise before our @Before runs).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut last_mismatch: Option<String> = None;
    while std::time::Instant::now() < deadline {
        match probe(client, any_apk_version).await {
            ProbeResult::Ready => return Ok(()),
            ProbeResult::VersionMismatch { found } => {
                if last_mismatch.as_deref() != Some(found.as_str()) {
                    warn!(
                        "waiting for server version {EXPECTED_APK_VERSION}; \
                         still seeing stale version {found}"
                    );
                    last_mismatch = Some(found);
                }
            }
            ProbeResult::Down => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    if let Some(found) = last_mismatch {
        return Err(running_version_mismatch_error(&found));
    }
    if let Some(hint) = ui_automation_failure_hint(serial).await {
        bail!("server did not become ready within 10s after `am instrument`.\n{hint}")
    }
    Err(crate::diagnostic::DiagnosticError::new(
        "server_unavailable",
        "connect",
        "server did not become ready within 10s after am instrument",
    )
    .retryable(true)
    .detail(serde_json::json!({"instrument_log": INSTRUMENT_LOG_PATH}))
    .next_actions([
        format!("adb shell cat {INSTRUMENT_LOG_PATH}"),
        "shadowdroid doctor --fix --json".to_string(),
    ])
    .into())
}

async fn ui_automation_failure_hint(serial: &Serial) -> Option<String> {
    let log = adb::shell(serial, format!("cat {INSTRUMENT_LOG_PATH} 2>/dev/null"))
        .await
        .ok()?;
    if !log.contains("already registered") {
        return None;
    }

    let owners = adb::ps_ui_automation_owners(serial)
        .await
        .unwrap_or_default();

    let mut hint = String::from(
        "Android reports the UiAutomation slot is already registered. Only one \
         UiAutomation owner can run at a time.",
    );
    if owners.contains("com.wetest.uia2.Main") {
        hint.push_str(
            "\nDetected openatx/uiautomator2 (`com.wetest.uia2.Main`) on the device. \
             Stop any host-side uiautomator2 watcher that may be respawning it, \
             then kill the device process and retry.",
        );
    } else if !owners.trim().is_empty() {
        hint.push_str("\nPotential on-device owners:\n");
        hint.push_str(owners.trim());
    }
    hint.push_str(&format!(
        "\nInstrumentation log: `adb shell cat {INSTRUMENT_LOG_PATH}`. If no owner \
         remains visible after cleanup, reset the AVD with `emulator -wipe-data`."
    ));
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_cache_is_visible_only_after_a_verified_pair_is_published() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let staging = root.path().join("staging");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&staging).unwrap();
        let main = cache.join(CACHE_MAIN_APK);
        let test = cache.join(CACHE_TEST_APK);
        fs::write(&main, b"complete-main").unwrap();
        fs::write(&test, b"complete-test").unwrap();
        let main_sha = sha256_file(&main).unwrap();
        let test_sha = sha256_file(&test).unwrap();
        write_apk_cache_complete(&cache, &main_sha, &test_sha).unwrap();
        assert!(apk_cache_is_complete(&cache));

        let staged_main = staging.join(CACHE_MAIN_APK);
        fs::write(&staged_main, b"replacement-main").unwrap();
        let replacement_main_sha = sha256_file(&staged_main).unwrap();
        assert!(
            publish_apk_pair(&staging, &cache, &replacement_main_sha, &test_sha).is_err(),
            "an incomplete staging pair must not begin publication"
        );
        assert!(apk_cache_is_complete(&cache));
        assert_eq!(fs::read(&main).unwrap(), b"complete-main");
        assert_eq!(fs::read(&test).unwrap(), b"complete-test");

        let staged_test = staging.join(CACHE_TEST_APK);
        fs::write(&staged_test, b"replacement-test").unwrap();
        let replacement_test_sha = sha256_file(&staged_test).unwrap();
        publish_apk_pair(
            &staging,
            &cache,
            &replacement_main_sha,
            &replacement_test_sha,
        )
        .unwrap();
        assert!(apk_cache_is_complete(&cache));
        assert_eq!(fs::read(&main).unwrap(), b"replacement-main");
        assert_eq!(fs::read(&test).unwrap(), b"replacement-test");
    }

    #[test]
    fn mislabeled_apk_error_names_the_bypass() {
        let error = mislabeled_apk_error(ApkSource::GithubRelease, Some("0.3.1"));
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .expect("version mismatch should stay machine-actionable");
        assert_eq!(diagnostic.code, "apk_version_mismatch");
        assert_eq!(diagnostic.detail["found"], "0.3.1");
        assert_eq!(diagnostic.detail["expected"], EXPECTED_APK_VERSION);
        assert!(
            diagnostic
                .next_actions
                .iter()
                .any(|action| action.contains("--any-apk-version"))
        );
        assert!(
            diagnostic
                .next_actions
                .iter()
                .any(|action| action.contains("--apk"))
        );
    }

    #[test]
    fn lifecycle_lock_contends_and_releases_without_unlinking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.lock");
        let serial = Serial::new("device:5555");

        let first = acquire_lifecycle_lock_at(&serial, &path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );

        let error = acquire_lifecycle_lock_at(&serial, &path).unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .expect("contention should stay machine-actionable");
        assert_eq!(diagnostic.code, "device_lifecycle_busy");

        drop(first);
        assert!(path.exists(), "persistent lock inode must not be unlinked");
        acquire_lifecycle_lock_at(&serial, &path).unwrap();
    }

    #[test]
    fn lifecycle_pid_write_failures_are_not_ignored() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let error = write_lock_pid(&mut FailingWriter, 42).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(error.to_string(), "disk full");
    }

    #[test]
    fn main_apk_passed_as_single_file_points_to_the_pair_directory() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("shadowdroid-server-main.apk");
        std::fs::write(&main, b"main").unwrap();

        let error = pair_from_path(&main).unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .expect("incomplete pair should be typed");
        assert_eq!(diagnostic.code, "apk_pair_incomplete");
        assert!(diagnostic.next_actions.iter().any(|action| {
            action == &format!("shadowdroid --apk {} connect", dir.path().display())
        }));
    }
}
