//! APK lifecycle manager — resolve, install, and verify the on-device server.
//!
//! Source-precedence chain (first hit wins; see architecture.md §4):
//!
//!   1. `--apk PATH` flag                  (explicit, highest priority)
//!   2. `SHADOWDROID_APK` env var          (same semantics as --apk)
//!   3. Repo auto-discovery in $CWD or any ancestor:
//!         server/app/build/outputs/apk/androidTest/debug/*-androidTest.apk
//!         + sibling main APK at server/app/build/outputs/apk/debug/*.apk
//!   4. Dev drop-in:  ~/.shadowdroid/apks/local/{main,test}.apk
//!   5. Versioned cache:  ~/.shadowdroid/apks/<EXPECTED_APK_VERSION>/{main,test}.apk
//!   6. Download from GitHub releases (stubbed — `bail!`s with a clear
//!      "not yet implemented; use --apk" message until M5)
//!
//! Sources 1-4 are *developer* sources: we install them as-is, identifying
//! re-install need by APK SHA-256 instead of versionName (so a `gradlew
//! assembleDebug` followed by `shadowdroid connect` reinstalls if and only
//! if bytes changed). Sources 5-6 are *user* sources; versionName must match
//! the CLI's baked-in `EXPECTED_APK_VERSION`.

use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::device::{adb, client::ServerClient};

pub const EXPECTED_APK_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const APP_PACKAGE: &str = "io.github.andriyo.shadowdroid";
pub const TEST_PACKAGE: &str = "io.github.andriyo.shadowdroid.test";
/// Standard AndroidJUnitRunner — we run a normal @Test method in
/// `SERVER_TEST_CLASS` that holds the process open. See ShadowDroidServerTest.kt
/// for why this is the proven pattern over a custom runner subclass.
pub const RUNNER_CLASS: &str = "androidx.test.runner.AndroidJUnitRunner";
pub const SERVER_TEST_CLASS: &str = "io.github.andriyo.shadowdroid.ShadowDroidServerTest";
pub const DEFAULT_PORT: u16 = 7912;
const INSTRUMENT_LOG_PATH: &str = "/sdcard/shadowdroid-instr.log";

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
pub fn resolve_apk(explicit: Option<&Path>) -> Result<ApkPair> {
    // 1. Explicit override (--apk / SHADOWDROID_APK)
    if let Some(p) = explicit {
        return resolve_explicit(p);
    }
    // 2. Repo auto-discovery
    if let Some(pair) = resolve_repo_build()? {
        info!("using local APK at {} (dev mode, source: {})",
              pair.test.display(), pair.source.label());
        return Ok(pair);
    }
    // 3. Local drop-in
    if let Some(pair) = resolve_local_dropin()? {
        info!("using local APK at {} (dev mode, source: {})",
              pair.test.display(), pair.source.label());
        return Ok(pair);
    }
    // 4. Versioned cache
    if let Some(pair) = resolve_versioned_cache()? {
        return Ok(pair);
    }
    // 5. GitHub release — M5
    bail!(
        "no ShadowDroid APK found.\n\
         Tried (in order):\n  \
         1. --apk / SHADOWDROID_APK (not set)\n  \
         2. repo: $CWD or ancestor with server/app/build/outputs/.../*.apk (none)\n  \
         3. ~/.shadowdroid/apks/local/{{main,test}}.apk (missing)\n  \
         4. ~/.shadowdroid/apks/{version}/{{main,test}}.apk (missing)\n  \
         5. GitHub release v{version} (download not implemented yet — M5)\n\
         \n\
         For now, pass --apk pointing at app-debug-androidTest.apk (the sibling\n\
         main APK is auto-discovered), or build the server:\n  \
         cd server && ./gradlew :app:assembleDebug :app:assembleDebugAndroidTest",
        version = EXPECTED_APK_VERSION,
    )
}

fn resolve_explicit(p: &Path) -> Result<ApkPair> {
    let (main, test) = pair_from_path(p)?;
    info!(
        "using local APK at {} (dev mode, source: --apk explicit)",
        test.display()
    );
    Ok(ApkPair { main, test, source: ApkSource::Explicit })
}

fn resolve_repo_build() -> Result<Option<ApkPair>> {
    let cwd = std::env::current_dir().context("cannot read $CWD")?;
    let mut dir: &Path = &cwd;
    loop {
        let test_glob = dir.join("server/app/build/outputs/apk/androidTest/debug");
        if test_glob.is_dir() {
            if let Some(test) = first_apk(&test_glob)? {
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
        Ok(Some(ApkPair { main, test, source: ApkSource::LocalDropIn }))
    } else {
        Ok(None)
    }
}

fn resolve_versioned_cache() -> Result<Option<ApkPair>> {
    let dir = shadowdroid_home()?.join("apks").join(EXPECTED_APK_VERSION);
    let main = dir.join("main.apk");
    let test = dir.join("test.apk");
    if main.is_file() && test.is_file() {
        info!(
            "using cached APK at {} (version {EXPECTED_APK_VERSION})",
            dir.display()
        );
        Ok(Some(ApkPair { main, test, source: ApkSource::VersionedCache }))
    } else {
        Ok(None)
    }
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
    let candidates = [
        parent.join("../../debug"),
        parent.to_path_buf(),
    ];
    for cand in &candidates {
        if cand.is_dir() {
            if let Some(main) = first_apk_matching(cand, "app-debug.apk")?
                .or(first_apk_matching(cand, "main.apk")?)
            {
                if main != *p {
                    return Ok((main, p.to_path_buf()));
                }
            }
        }
    }
    bail!(
        "could not find sibling main APK for {}. \
         Pass a directory containing both APKs, or symlink/copy the main APK next to the test APK.",
        p.display()
    )
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

fn shadowdroid_home() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "andriyo", "ShadowDroid")
        .ok_or_else(|| anyhow!("cannot determine home directory"))?;
    let p = dirs.config_dir().to_path_buf();
    // Fall back to the simpler ~/.shadowdroid for parity with the docs.
    let home_dot = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".shadowdroid"));
    Ok(home_dot.unwrap_or(p))
}

// ── full lifecycle: resolve → install → forward → instrument → poll ──────

/// Make sure the device has our server running and reachable, then return
/// a connected ServerClient ready to use.
pub async fn ensure_ready(
    serial: &str,
    explicit_apk: Option<&Path>,
) -> Result<ServerClient> {
    // Probe early: if the server is already up from a previous connect, we can
    // skip APK resolution, install checks, and any cooldowns. Warm path stays
    // <100ms.
    adb::forward(serial, DEFAULT_PORT, DEFAULT_PORT).await.ok();
    let client = ServerClient::new(DEFAULT_PORT)?;
    if probe(&client).await {
        info!("server already up — reusing");
        return Ok(client);
    }
    // Cold path: resolve + install + start. May need a retry with longer
    // cooldown if Android's system_server hasn't released the UiAutomation
    // slot from a prior dev cycle, or another UI automation process is still
    // claiming it.
    let pair = resolve_apk(explicit_apk)?;
    install_if_needed(serial, &pair).await?;
    adb::forward(serial, DEFAULT_PORT, DEFAULT_PORT).await?;
    start_instrumentation(serial).await?;
    if wait_for_server(serial, &client).await.is_ok() {
        return Ok(client);
    }
    // First start failed — most likely the UiAutomation slot is still owned
    // by a prior instrumentation/app_process. Heavier cleanup + retry.
    warn!("first start attempt failed; cooling down 10s and retrying (UiAutomation slot may need time to release)");
    long_cooldown(serial).await?;
    start_instrumentation(serial).await?;
    wait_for_server(serial, &client).await?;
    Ok(client)
}

/// Heavy-handed cleanup for the case where system_server is holding a stale
/// `UiAutomationService` registration. Force-stop everything, then wait long
/// enough for system_server to actually release the slot (~5-10s observed).
async fn long_cooldown(serial: &str) -> Result<()> {
    adb::am_force_stop(serial, TEST_PACKAGE).await?;
    adb::am_force_stop(serial, APP_PACKAGE).await?;
    adb::kill_instrument_zombies(serial).await?;
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    Ok(())
}

async fn install_if_needed(serial: &str, pair: &ApkPair) -> Result<()> {
    let installed_main = adb::pm_path(serial, APP_PACKAGE).await?.is_some();
    let installed_test = adb::pm_path(serial, TEST_PACKAGE).await?.is_some();
    if !installed_main || !installed_test {
        info!("installing main + test APKs (cold install)");
        adb::install(serial, pair.main.clone()).await?;
        adb::install(serial, pair.test.clone()).await?;
        return Ok(());
    }
    if pair.source.is_dev() {
        // For dev sources we'd ideally hash-compare. For M1, simplest correct
        // behaviour: always reinstall when explicit --apk is given (Source 1),
        // and skip if already installed for the others. Hash comparison is a
        // later optimisation.
        if pair.source == ApkSource::Explicit {
            info!("reinstalling APKs (dev mode, explicit --apk)");
            adb::install(serial, pair.main.clone()).await?;
            adb::install(serial, pair.test.clone()).await?;
        }
        return Ok(());
    }
    // For cached/release sources, version-check would go here. M5.
    Ok(())
}

async fn start_instrumentation(serial: &str) -> Result<()> {
    // Kill any zombie app_process from previous runs — they hold the
    // UiAutomation slot and would cause "already registered" errors.
    adb::kill_instrument_zombies(serial).await?;
    adb::am_force_stop(serial, TEST_PACKAGE).await?;
    adb::am_force_stop(serial, APP_PACKAGE).await?;
    let runner = format!("{TEST_PACKAGE}/{RUNNER_CLASS}");
    adb::am_instrument(
        serial,
        runner,
        Some(SERVER_TEST_CLASS),
        INSTRUMENT_LOG_PATH,
    )
    .await?;
    Ok(())
}

async fn probe(client: &ServerClient) -> bool {
    client.state().await.is_ok()
}

async fn wait_for_server(serial: &str, client: &ServerClient) -> Result<()> {
    // 10s gives Ktor + UiAutomation + JUnit setup enough headroom on slower
    // emulators (Android 16 emulator with fresh APK install can take ~3s
    // for the test framework to fully initialise before our @Before runs).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if probe(client).await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    if let Some(hint) = ui_automation_failure_hint(serial).await {
        bail!("server did not become ready within 10s after `am instrument`.\n{hint}")
    }
    bail!("server did not become ready within 10s after `am instrument`. \
           Check the on-device log: `adb shell cat {INSTRUMENT_LOG_PATH}`")
}

async fn ui_automation_failure_hint(serial: &str) -> Option<String> {
    let log = adb::shell(serial, format!("cat {INSTRUMENT_LOG_PATH} 2>/dev/null"))
        .await
        .ok()?;
    if !log.contains("already registered") {
        return None;
    }

    let owners = adb::shell(
        serial,
        "ps -A -o USER,PID,PPID,NAME,ARGS \
         | grep -E 'app_process|uiautomator|shadowdroid|wetest|atx' \
         | grep -v grep",
    )
    .await
    .unwrap_or_default();

    let mut hint = String::from(
        "Android reports the UiAutomation slot is already registered. Only one \
         UiAutomation owner can run at a time.",
    );
    if owners.contains("com.wetest.uia2.Main") {
        hint.push_str(
            "\nDetected openatx/uiautomator2 (`com.wetest.uia2.Main`) on the device. \
             Stop any host-side uiautomator2/movi watcher that may be respawning it, \
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
