//! Android Studio discovery and ShadowDroid plugin installation.
//!
//! The device server APK resolver lives under `device::installer`; this module
//! mirrors that distribution shape for the IDE side:
//!
//!   1. `--plugin PATH` / `SHADOWDROID_STUDIO_PLUGIN`
//!   2. repo auto-discovery: `shadowdroid-plugin/build/distributions/*.zip`
//!   3. dev drop-in: `~/.shadowdroid/plugins/local/*.zip`
//!   4. versioned cache: `~/.shadowdroid/plugins/<version>/shadowdroid-studio-plugin.zip`
//!   5. GitHub release fallback.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use tracing::info;
use zip::ZipArchive;

use crate::cmd::skill;
use crate::cmd::studio_contract;
use crate::hostenv::{env_truthy, home_dir, shadowdroid_home};
use crate::release::{
    download_file, download_text, expected_sha, release_asset_url, release_base_url, sha256_file,
    verify_sha256, CHECKSUMS_ASSET,
};

const EXPECTED_PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_PLUGIN_ASSET: &str = "shadowdroid-studio-plugin.zip";
const PLUGIN_DIR_NAME: &str = "shadowdroid-plugin";

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Deprecated: `shadowdroid init` installs the Studio plugin by default.
    #[arg(long, conflicts_with = "no_studio_plugin")]
    pub install_studio_plugin: bool,
    /// Only inspect Android Studio; do not install/update the plugin.
    #[arg(long)]
    pub no_studio_plugin: bool,
    /// Do not install/update agent skill files.
    #[arg(long)]
    pub no_skills: bool,
    /// Android Studio installation path, .app bundle, product-info.json, or launcher.
    #[arg(long, env = "SHADOWDROID_ANDROID_STUDIO", value_name = "PATH")]
    pub studio: Option<PathBuf>,
    /// Local ShadowDroid plugin ZIP to install instead of resolving from cache/release.
    #[arg(long, env = "SHADOWDROID_STUDIO_PLUGIN", value_name = "PATH")]
    pub plugin: Option<PathBuf>,
    /// Emit JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StudioArgs {
    #[command(subcommand)]
    pub cmd: StudioCmd,
}

#[derive(Subcommand, Debug)]
pub enum StudioCmd {
    /// Detect Android Studio, plugin state, and the running ShadowDroid bridge.
    Status(StudioStatusArgs),
    /// Install or update the ShadowDroid Android Studio plugin.
    Install(StudioInstallArgs),
}

#[derive(Args, Debug)]
pub struct StudioStatusArgs {
    /// Android Studio installation path, .app bundle, product-info.json, or launcher.
    #[arg(long, env = "SHADOWDROID_ANDROID_STUDIO", value_name = "PATH")]
    pub studio: Option<PathBuf>,
    /// Emit JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StudioInstallArgs {
    /// Android Studio installation path, .app bundle, product-info.json, or launcher.
    #[arg(long, env = "SHADOWDROID_ANDROID_STUDIO", value_name = "PATH")]
    pub studio: Option<PathBuf>,
    /// Local ShadowDroid plugin ZIP to install instead of resolving from cache/release.
    #[arg(long, env = "SHADOWDROID_STUDIO_PLUGIN", value_name = "PATH")]
    pub plugin: Option<PathBuf>,
    /// Emit JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StudioInfo {
    pub path: String,
    pub product_info: String,
    pub name: String,
    pub version: Option<String>,
    pub build_number: Option<String>,
    pub product_code: Option<String>,
    pub data_directory_name: String,
    pub plugins_dir: String,
    pub shadowdroid_plugin_installed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub path: String,
    pub source: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BridgeInfo {
    pub registry: String,
    pub present: bool,
    pub running: bool,
    pub url: Option<String>,
    pub pid: Option<u64>,
    pub projects: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StudioReport {
    pub android_studios: Vec<StudioInfo>,
    pub bridge: BridgeInfo,
    pub guidance: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub studio: StudioInfo,
    pub plugin: PluginInfo,
    pub installed_dir: String,
    pub restart_required: bool,
    pub guidance: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProductInfo {
    name: Option<String>,
    version: Option<String>,
    #[serde(rename = "buildNumber")]
    build_number: Option<String>,
    #[serde(rename = "productCode")]
    product_code: Option<String>,
    #[serde(rename = "dataDirectoryName")]
    data_directory_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BridgeRegistry {
    url: Option<String>,
    pid: Option<u64>,
    #[serde(default)]
    projects: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginSource {
    Explicit,
    RepoBuild,
    LocalDropIn,
    VersionedCache,
    GithubRelease,
}

impl PluginSource {
    fn label(self) -> &'static str {
        match self {
            Self::Explicit => "--plugin / SHADOWDROID_STUDIO_PLUGIN",
            Self::RepoBuild => "repo auto-discovery",
            Self::LocalDropIn => "~/.shadowdroid/plugins/local/",
            Self::VersionedCache => "~/.shadowdroid/plugins/<version>/",
            Self::GithubRelease => "GitHub release",
        }
    }
}

#[derive(Debug, Clone)]
struct PluginZip {
    path: PathBuf,
    source: PluginSource,
}

pub async fn run(args: &StudioArgs) -> Result<()> {
    match &args.cmd {
        StudioCmd::Status(args) => status(args.studio.as_deref(), args.json).await,
        StudioCmd::Install(args) => {
            install(args.studio.as_deref(), args.plugin.as_deref(), args.json).await
        }
    }
}

pub async fn run_init(args: &InitArgs) -> Result<()> {
    let install_studio = !args.no_studio_plugin || args.install_studio_plugin;
    if install_studio {
        if let Err(err) = install(args.studio.as_deref(), args.plugin.as_deref(), args.json).await {
            if args.json {
                print_json(&serde_json::json!({
                    "type": "init_step",
                    "step": "studio_plugin",
                    "ok": false,
                    "error": err.to_string(),
                    "next_command": "shadowdroid init --no-studio-plugin",
                }));
            } else {
                eprintln!("Studio plugin: {err}");
                eprintln!(
                    "Next: install Android Studio or pass --studio, then rerun `shadowdroid init`."
                );
            }
        }
    } else {
        status(args.studio.as_deref(), args.json).await?;
    }

    if !args.no_skills {
        let skills = skill::install_default_skills();
        if args.json {
            println!("{}", serde_json::to_string(&skills)?);
        } else {
            print_skill_install_human(&skills);
        }
    }

    if install_studio {
        status(args.studio.as_deref(), args.json).await?;
    }
    Ok(())
}

async fn status(explicit_studio: Option<&Path>, json: bool) -> Result<()> {
    let report = status_report(explicit_studio)?;
    if json {
        print_json(&report);
    } else {
        print_status_human(&report);
    }
    Ok(())
}

pub fn status_report(explicit_studio: Option<&Path>) -> Result<StudioReport> {
    let studios = discover_android_studios(explicit_studio)?;
    let bridge = bridge_status()?;
    let mut guidance = Vec::new();

    if studios.is_empty() {
        guidance.push("Android Studio was not detected. Pass --studio /path/to/Android Studio.app or install Android Studio first.".into());
    } else if studios.iter().any(|s| !s.shadowdroid_plugin_installed) {
        guidance.push(
            "Run `shadowdroid studio install` to install the ShadowDroid Android Studio plugin."
                .into(),
        );
    } else if !bridge.running {
        guidance.push("Restart Android Studio and open an Android project; the plugin registers the debugger bridge on project startup.".into());
    }

    let report = StudioReport {
        android_studios: studios,
        bridge,
        guidance,
    };
    Ok(report)
}

async fn install(
    explicit_studio: Option<&Path>,
    explicit_plugin: Option<&Path>,
    json: bool,
) -> Result<()> {
    let mut studios = discover_android_studios(explicit_studio)?;
    if studios.is_empty() {
        bail!(
            "Android Studio was not detected. Pass --studio /path/to/Android Studio.app, \
             or install Android Studio and retry."
        );
    }
    if explicit_studio.is_none() && studios.len() > 1 {
        let choices = studios
            .iter()
            .map(|s| format!("  - {}", s.path))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "multiple Android Studio installations were detected; choose one with --studio:\n{choices}"
        );
    }

    let plugin = resolve_plugin(explicit_plugin, true).await?;
    let plugin_info = PluginInfo {
        path: plugin.path.display().to_string(),
        source: plugin.source.label().into(),
        sha256: sha256_file(&plugin.path)?,
    };

    let selected = studios.remove(0);
    let installed_dir = install_plugin_zip(&plugin.path, Path::new(&selected.plugins_dir))?;
    let refreshed = studio_info_from_product_info(
        Path::new(&selected.path),
        Path::new(&selected.product_info),
    )?;
    let report = InstallReport {
        studio: refreshed,
        plugin: plugin_info,
        installed_dir: installed_dir.display().to_string(),
        restart_required: true,
        guidance: vec![
            "Restart Android Studio to load or update the plugin.".into(),
            "After restart, run `shadowdroid debug status` to confirm the bridge.".into(),
        ],
    };

    if json {
        print_json(&report);
    } else {
        print_install_human(&report);
    }
    Ok(())
}

fn discover_android_studios(explicit: Option<&Path>) -> Result<Vec<StudioInfo>> {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(expand_home(path));
    } else {
        candidates.extend(default_studio_candidates());
    }

    let mut by_product_info: BTreeMap<PathBuf, StudioInfo> = BTreeMap::new();
    for candidate in candidates {
        if let Some((root, product_info)) = find_product_info(&candidate) {
            let key = product_info
                .canonicalize()
                .unwrap_or_else(|_| product_info.clone());
            if by_product_info.contains_key(&key) {
                continue;
            }
            match studio_info_from_product_info(&root, &product_info) {
                Ok(info) => {
                    if info.product_code.as_deref() == Some("AI")
                        || info.name.to_ascii_lowercase().contains("android studio")
                    {
                        by_product_info.insert(key, info);
                    }
                }
                Err(err) => {
                    info!(
                        "skipping Android Studio candidate {}: {err}",
                        candidate.display()
                    );
                }
            }
        }
    }
    Ok(by_product_info.into_values().collect())
}

fn studio_info_from_product_info(root: &Path, product_info_path: &Path) -> Result<StudioInfo> {
    let text = fs::read_to_string(product_info_path)
        .with_context(|| format!("read {}", product_info_path.display()))?;
    let product: ProductInfo = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", product_info_path.display()))?;
    let data_directory_name = product
        .data_directory_name
        .clone()
        .ok_or_else(|| anyhow!("product-info.json has no dataDirectoryName"))?;
    let plugins_dir = plugins_dir(&data_directory_name)?;
    let installed_dir = plugins_dir.join(PLUGIN_DIR_NAME);
    Ok(StudioInfo {
        path: root.display().to_string(),
        product_info: product_info_path.display().to_string(),
        name: product.name.unwrap_or_else(|| "Android Studio".into()),
        version: product.version,
        build_number: product.build_number,
        product_code: product.product_code,
        data_directory_name,
        plugins_dir: plugins_dir.display().to_string(),
        shadowdroid_plugin_installed: installed_dir.is_dir(),
    })
}

fn find_product_info(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let path = expand_home(path);
    if path.is_file() {
        if path.file_name().and_then(|s| s.to_str()) == Some("product-info.json") {
            let root = product_root_from_info(&path)?;
            return Some((root, path));
        }
        for ancestor in path.ancestors().take(5) {
            if let Some(found) = find_product_info_in_dir(ancestor) {
                return Some(found);
            }
        }
        return None;
    }
    find_product_info_in_dir(&path)
}

fn find_product_info_in_dir(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let candidates = [
        dir.join("Contents/Resources/product-info.json"),
        dir.join("Resources/product-info.json"),
        dir.join("product-info.json"),
    ];
    for product_info in candidates {
        if product_info.is_file() {
            return Some((dir.to_path_buf(), product_info));
        }
    }
    None
}

fn product_root_from_info(product_info: &Path) -> Option<PathBuf> {
    let parent = product_info.parent()?;
    if parent.file_name().and_then(|s| s.to_str()) == Some("Resources") {
        let root = parent.parent()?;
        if root.file_name().and_then(|s| s.to_str()) == Some("Contents") {
            return root.parent().map(Path::to_path_buf);
        }
        return Some(root.to_path_buf());
    }
    Some(parent.to_path_buf())
}

fn default_studio_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("ANDROID_STUDIO_HOME") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("STUDIO_HOME") {
        candidates.push(PathBuf::from(path));
    }
    candidates.extend(path_lookup("studio"));
    candidates.extend(path_lookup("studio.sh"));
    candidates.extend(path_lookup("studio64.exe"));

    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/Applications/Android Studio.app"));
        candidates.push(PathBuf::from("/Applications/Android Studio Preview.app"));
        if let Ok(home) = home_dir() {
            candidates.push(home.join("Applications/Android Studio.app"));
            candidates.push(home.join("Applications/Android Studio Preview.app"));
            collect_named_dirs(
                &home.join("Library/Application Support/JetBrains/Toolbox/apps"),
                "Android Studio.app",
                7,
                &mut candidates,
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        candidates.push(PathBuf::from("/opt/android-studio"));
        candidates.push(PathBuf::from("/usr/local/android-studio"));
        if let Ok(home) = home_dir() {
            candidates.push(home.join("android-studio"));
            collect_named_dirs(
                &home.join(".local/share/JetBrains/Toolbox/apps"),
                "android-studio",
                7,
                &mut candidates,
            );
        }
    }

    #[cfg(target_os = "windows")]
    {
        for var in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(var).map(PathBuf::from) {
                candidates.push(base.join("Programs/Android Studio"));
                candidates.push(base.join("Android/Android Studio"));
            }
        }
    }

    candidates
}

fn path_lookup(name: &str) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .filter(|candidate| candidate.is_file())
        .collect()
}

fn collect_named_dirs(base: &Path, name: &str, max_depth: usize, out: &mut Vec<PathBuf>) {
    if max_depth == 0 || !base.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|s| s.to_str()) == Some(name) {
            out.push(path.clone());
        }
        collect_named_dirs(&path, name, max_depth - 1, out);
    }
}

fn plugins_dir(data_directory_name: &str) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = home_dir()?;
        Ok(home
            .join("Library/Application Support/Google")
            .join(data_directory_name)
            .join("plugins"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("APPDATA is not set"))?;
        Ok(appdata
            .join("Google")
            .join(data_directory_name)
            .join("plugins"))
    }
    #[cfg(target_os = "linux")]
    {
        let home = home_dir()?;
        Ok(home.join(".local/share/Google").join(data_directory_name))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = data_directory_name;
        bail!("unsupported platform for Android Studio plugin auto-install")
    }
}

async fn resolve_plugin(explicit: Option<&Path>, allow_download: bool) -> Result<PluginZip> {
    if let Some(path) = explicit {
        return resolve_explicit_plugin(path);
    }

    let disable_dev_sources = env_truthy("SHADOWDROID_DISABLE_DEV_SOURCES");
    if !disable_dev_sources {
        if let Some(plugin) = resolve_repo_plugin()? {
            return Ok(plugin);
        }
        if let Some(plugin) = resolve_local_dropin()? {
            return Ok(plugin);
        }
    }
    if let Some(plugin) = resolve_versioned_cache()? {
        return Ok(plugin);
    }
    if allow_download {
        return download_github_release().await;
    }
    bail!(
        "ShadowDroid Studio plugin ZIP was not found locally. Run `shadowdroid studio install` \
         to download it, or pass --plugin /path/to/{RELEASE_PLUGIN_ASSET}."
    )
}

fn resolve_explicit_plugin(path: &Path) -> Result<PluginZip> {
    let path = if path.is_dir() {
        newest_zip(path)?.ok_or_else(|| anyhow!("no plugin ZIP found in {}", path.display()))?
    } else {
        path.to_path_buf()
    };
    ensure_zip(&path)?;
    Ok(PluginZip {
        path,
        source: PluginSource::Explicit,
    })
}

fn resolve_repo_plugin() -> Result<Option<PluginZip>> {
    let cwd = std::env::current_dir().context("cannot read $CWD")?;
    for dir in cwd.ancestors() {
        let distributions = dir.join("shadowdroid-plugin/build/distributions");
        if distributions.is_dir() {
            if let Some(path) = newest_zip(&distributions)? {
                return Ok(Some(PluginZip {
                    path,
                    source: PluginSource::RepoBuild,
                }));
            }
        }
    }
    Ok(None)
}

fn resolve_local_dropin() -> Result<Option<PluginZip>> {
    let dir = shadowdroid_home()?.join("plugins/local");
    if !dir.is_dir() {
        return Ok(None);
    }
    let path = dir.join(RELEASE_PLUGIN_ASSET);
    let path = if path.is_file() {
        Some(path)
    } else {
        newest_zip(&dir)?
    };
    Ok(path.map(|path| PluginZip {
        path,
        source: PluginSource::LocalDropIn,
    }))
}

fn resolve_versioned_cache() -> Result<Option<PluginZip>> {
    let path = versioned_plugin_cache_file()?;
    if path.is_file() {
        Ok(Some(PluginZip {
            path,
            source: PluginSource::VersionedCache,
        }))
    } else {
        Ok(None)
    }
}

async fn download_github_release() -> Result<PluginZip> {
    let cache_file = versioned_plugin_cache_file()?;
    let cache_dir = cache_file
        .parent()
        .ok_or_else(|| anyhow!("invalid cache path {}", cache_file.display()))?;
    let staging_dir = shadowdroid_home()?.join("plugins").join(format!(
        ".download-{}-{}",
        EXPECTED_PLUGIN_VERSION,
        std::process::id()
    ));
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .with_context(|| format!("remove stale {}", staging_dir.display()))?;
    }
    fs::create_dir_all(&staging_dir)
        .with_context(|| format!("create {}", staging_dir.display()))?;

    let base = release_base_url(EXPECTED_PLUGIN_VERSION);
    let plugin_url = release_asset_url(&base, RELEASE_PLUGIN_ASSET);
    let sums_url = release_asset_url(&base, CHECKSUMS_ASSET);
    info!("downloading ShadowDroid Android Studio plugin from {base}");

    let checksums = download_text(&sums_url)
        .await
        .with_context(|| format!("download {CHECKSUMS_ASSET} from GitHub release"))?;
    let expected = expected_sha(
        option_env!("SHADOWDROID_RELEASE_STUDIO_PLUGIN_SHA256"),
        &checksums,
        RELEASE_PLUGIN_ASSET,
    )?;

    let staging_file = staging_dir.join(RELEASE_PLUGIN_ASSET);
    download_file(&plugin_url, &staging_file)
        .await
        .with_context(|| format!("download {RELEASE_PLUGIN_ASSET}"))?;
    verify_sha256(&staging_file, &expected)
        .with_context(|| format!("verify {RELEASE_PLUGIN_ASSET}"))?;

    if cache_dir.exists() {
        fs::remove_dir_all(cache_dir)
            .with_context(|| format!("replace {}", cache_dir.display()))?;
    }
    fs::create_dir_all(cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;
    fs::rename(&staging_file, &cache_file)
        .with_context(|| format!("move plugin ZIP into {}", cache_file.display()))?;
    fs::remove_dir_all(&staging_dir).ok();

    Ok(PluginZip {
        path: cache_file,
        source: PluginSource::GithubRelease,
    })
}

fn install_plugin_zip(zip_path: &Path, plugins_dir: &Path) -> Result<PathBuf> {
    ensure_zip(zip_path)?;
    fs::create_dir_all(plugins_dir).with_context(|| format!("create {}", plugins_dir.display()))?;

    let file = fs::File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("read plugin ZIP {}", zip_path.display()))?;
    let top_dir = archive_top_dir(&mut archive)?;
    if top_dir != PLUGIN_DIR_NAME {
        bail!("unexpected plugin archive root `{top_dir}`; expected `{PLUGIN_DIR_NAME}`");
    }

    let installed_dir = plugins_dir.join(&top_dir);
    if installed_dir.exists() {
        fs::remove_dir_all(&installed_dir)
            .with_context(|| format!("replace {}", installed_dir.display()))?;
    }

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(enclosed) = entry.enclosed_name() else {
            bail!("unsafe path in plugin ZIP: {}", entry.name());
        };
        let out_path = plugins_dir.join(enclosed);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .with_context(|| format!("create {}", out_path.display()))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let mut out = fs::File::create(&out_path)
                .with_context(|| format!("create {}", out_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("extract {}", out_path.display()))?;
        }
    }

    if !installed_dir.join("lib").is_dir() {
        bail!(
            "plugin ZIP extracted but {} has no lib directory",
            installed_dir.display()
        );
    }
    Ok(installed_dir)
}

fn archive_top_dir<R: Read + io::Seek>(archive: &mut ZipArchive<R>) -> Result<String> {
    let mut top: Option<String> = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let Some(enclosed) = entry.enclosed_name() else {
            bail!("unsafe path in plugin ZIP: {}", entry.name());
        };
        let Some(first) = enclosed.components().next() else {
            continue;
        };
        let first = first.as_os_str().to_string_lossy().to_string();
        match &top {
            Some(existing) if existing != &first => {
                bail!("plugin ZIP contains multiple top-level entries: {existing}, {first}")
            }
            None => top = Some(first),
            _ => {}
        }
    }
    top.ok_or_else(|| anyhow!("plugin ZIP is empty"))
}

fn newest_zip(dir: &Path) -> Result<Option<PathBuf>> {
    let mut zips = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("zip") {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            zips.push((modified, path));
        }
    }
    zips.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(zips.into_iter().map(|(_, path)| path).next())
}

fn ensure_zip(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("plugin ZIP not found: {}", path.display());
    }
    if path.extension().and_then(|s| s.to_str()) != Some("zip") {
        bail!("plugin artifact must be a .zip file: {}", path.display());
    }
    Ok(())
}

fn bridge_status() -> Result<BridgeInfo> {
    let registry = shadowdroid_home()?.join(studio_contract::REGISTRY_FILE);
    if !registry.is_file() {
        return Ok(BridgeInfo {
            registry: registry.display().to_string(),
            present: false,
            running: false,
            url: None,
            pid: None,
            projects: Vec::new(),
        });
    }
    let text =
        fs::read_to_string(&registry).with_context(|| format!("read {}", registry.display()))?;
    let parsed: BridgeRegistry =
        serde_json::from_str(&text).with_context(|| format!("parse {}", registry.display()))?;
    let running = parsed.pid.map(process_running).unwrap_or(false);
    Ok(BridgeInfo {
        registry: registry.display().to_string(),
        present: true,
        running,
        url: parsed.url,
        pid: parsed.pid,
        projects: parsed.projects,
    })
}

fn versioned_plugin_cache_file() -> Result<PathBuf> {
    Ok(shadowdroid_home()?
        .join("plugins")
        .join(EXPECTED_PLUGIN_VERSION)
        .join(RELEASE_PLUGIN_ASSET))
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    if raw == "~" {
        return home_dir().unwrap_or_else(|_| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn process_running(pid: u64) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        Command::new("tasklist")
            .args(["/FI", &filter])
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn print_status_human(report: &StudioReport) {
    if report.android_studios.is_empty() {
        println!("Android Studio: not detected");
    } else {
        println!("Android Studio:");
        for studio in &report.android_studios {
            let version = studio
                .build_number
                .as_deref()
                .or(studio.version.as_deref())
                .unwrap_or("unknown build");
            let plugin = if studio.shadowdroid_plugin_installed {
                "installed"
            } else {
                "not installed"
            };
            println!("  - {} ({version})", studio.path);
            println!("    plugins: {}", studio.plugins_dir);
            println!("    ShadowDroid plugin: {plugin}");
        }
    }

    if report.bridge.running {
        println!(
            "Debugger bridge: {}",
            report.bridge.url.as_deref().unwrap_or("registered")
        );
    } else if report.bridge.present {
        println!("Debugger bridge: registry exists, but the recorded process is not running");
    } else {
        println!("Debugger bridge: not registered");
    }
    for item in &report.guidance {
        println!("Next: {item}");
    }
}

fn print_install_human(report: &InstallReport) {
    println!("Installed ShadowDroid Android Studio plugin:");
    println!("  plugin: {}", report.plugin.path);
    println!("  source: {}", report.plugin.source);
    println!("  studio: {}", report.studio.path);
    println!("  destination: {}", report.installed_dir);
    for item in &report.guidance {
        println!("Next: {item}");
    }
}

fn print_skill_install_human(value: &serde_json::Value) {
    let installed = value
        .get("installed")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let skipped = value
        .get("skipped")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let failed = value
        .get("failed")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    println!("Agent skills: installed/updated {installed}, skipped {skipped}, failed {failed}");
    if let Some(items) = value.get("skipped").and_then(serde_json::Value::as_array) {
        for item in items {
            let agent = item
                .get("agent")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let path = item
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let reason = item
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("skipped");
            println!("  - skipped {agent}: {path} ({reason})");
        }
    }
    if let Some(items) = value.get("failed").and_then(serde_json::Value::as_array) {
        for item in items {
            let agent = item
                .get("agent")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let error = item
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            println!("  - failed {agent}: {error}");
        }
    }
}

fn print_json<T: Serialize>(value: &T) {
    println!("{}", serde_json::to_string(value).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_plugin_zip_extracts_nested_entries() {
        use std::io::Write;
        use zip::write::{SimpleFileOptions, ZipWriter};
        use zip::CompressionMethod;

        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("plugin.zip");
        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut zw = ZipWriter::new(file);
            let deflated =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            zw.start_file("shadowdroid-plugin/lib/plugin.jar", deflated)
                .unwrap();
            zw.write_all(b"JARBYTES-deflated").unwrap();
            zw.start_file("shadowdroid-plugin/META-INF/plugin.xml", stored)
                .unwrap();
            zw.write_all(b"<idea-plugin/>").unwrap();
            zw.finish().unwrap();
        }

        let plugins_dir = tmp.path().join("plugins");
        let installed = install_plugin_zip(&zip_path, &plugins_dir).unwrap();

        assert!(installed.join("lib").is_dir());
        assert_eq!(
            fs::read(installed.join("lib").join("plugin.jar")).unwrap(),
            b"JARBYTES-deflated"
        );
        assert_eq!(
            fs::read(installed.join("META-INF").join("plugin.xml")).unwrap(),
            b"<idea-plugin/>"
        );
    }
}
