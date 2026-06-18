//! `shadowdroid aar` — install and manage the in-app debug AAR in an app whose
//! source you can build.
//!
//! The agent (the ShadowDroid in-app debug library) is a `debugImplementation`-only
//! AAR that auto-installs through a merged `ContentProvider` — zero app code. It
//! is a base for many debugging/development capabilities, not just network
//! capture.
//!
//! This verb is **host-only**: pure filesystem + Gradle on the host, no device.
//! It resolves the AAR (explicit → repo build → versioned cache → GitHub
//! release, mirroring the APK precedence chain in [crate::device::installer]),
//! copies it into the app, and idempotently wires a single dependency line into
//! the app's application module. The CLI surface is deliberately small and
//! scriptable so an AI agent can use it efficiently (`--json` everywhere).

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Release asset name (matches the `release.yml` staging step).
const AAR_ASSET: &str = "shadowdroid-agent.aar";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
const EXPECTED_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the AAR is placed inside the target app, relative to the Gradle root.
const APP_AAR_RELPATH: &str = "shadowdroid/shadowdroid-agent.aar";

/// Tags the managed dependency line so install is idempotent and remove is exact.
const DEP_MARKER: &str = "shadowdroid-agent (managed by `shadowdroid aar`)";

#[derive(Subcommand)]
pub enum AarCmd {
    /// Install the debug AAR into an app and wire one debug-only dependency.
    Install(InstallArgs),
    /// Report whether the AAR is wired into an app (path, dependency, version).
    Status(TargetArgs),
    /// Remove the managed dependency line and the copied AAR file.
    Remove(TargetArgs),
}

#[derive(Args)]
pub struct InstallArgs {
    /// Path to the app's Gradle project root (the dir with settings.gradle[.kts]).
    #[arg(long, default_value = ".")]
    pub app: PathBuf,
    /// Gradle module to wire (e.g. `androidApp`). Auto-detected if omitted.
    #[arg(long)]
    pub module: Option<String>,
    /// Use this local AAR file instead of resolving one (dev / testing).
    #[arg(long)]
    pub from: Option<PathBuf>,
    /// After wiring, run `:<module>:assembleDebug` to verify the app compiles.
    #[arg(long)]
    pub build: bool,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct TargetArgs {
    /// Path to the app's Gradle project root.
    #[arg(long, default_value = ".")]
    pub app: PathBuf,
    /// Gradle module (auto-detected if omitted).
    #[arg(long)]
    pub module: Option<String>,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(cmd: &AarCmd) -> Result<()> {
    match cmd {
        AarCmd::Install(a) => install(a).await,
        AarCmd::Status(a) => status(a),
        AarCmd::Remove(a) => remove(a),
    }
}

// ── install ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct InstallReport {
    action: &'static str,
    app: String,
    module: String,
    aar_source: String,
    aar_version: String,
    aar_path: String,
    dependency_added: bool,
    build: &'static str,
}

async fn install(args: &InstallArgs) -> Result<()> {
    let root = canonical_root(&args.app)?;
    let module = match &args.module {
        Some(m) => m.clone(),
        None => detect_app_module(&root)?,
    };
    let module_gradle = module_build_gradle(&root, &module)?;

    // Resolve the AAR (dev → cache → release) and place it in the app.
    let resolved = resolve_aar(args.from.as_deref()).await?;
    let dest = root.join(APP_AAR_RELPATH);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(&resolved.path, &dest)
        .with_context(|| format!("copy AAR to {}", dest.display()))?;

    let added = wire_dependency(&module_gradle)?;

    let mut build_result = "skipped";
    if args.build {
        build_result = if gradle_assemble_debug(&root, &module)? {
            "ok"
        } else {
            "failed"
        };
    }

    let report = InstallReport {
        action: "install",
        app: root.display().to_string(),
        module,
        aar_source: resolved.source.to_string(),
        aar_version: resolved.version.clone(),
        aar_path: APP_AAR_RELPATH.to_string(),
        dependency_added: added,
        build: build_result,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "✓ ShadowDroid agent AAR installed into `{}` (module :{})",
            report.app, report.module
        );
        println!("  source:     {} (version {})", report.aar_source, report.aar_version);
        println!("  aar:        {}", report.aar_path);
        println!(
            "  dependency: {}",
            if added {
                "added debugImplementation line"
            } else {
                "already present (idempotent)"
            }
        );
        match build_result {
            "ok" => println!("  build:      :{}:assembleDebug succeeded", report.module),
            "failed" => println!("  build:      :{}:assembleDebug FAILED (see output above)", report.module),
            _ => println!("  build:      skipped (pass --build to verify)"),
        }
        println!(
            "\nThe agent auto-starts in debug builds. Launch the app and confirm with:\n  \
             adb logcat -s ShadowDroidAgent"
        );
    }

    if build_result == "failed" {
        bail!("app build failed after wiring the AAR");
    }
    Ok(())
}

// ── status ──────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StatusReport {
    app: String,
    module: String,
    dependency_present: bool,
    aar_present: bool,
    aar_path: String,
    installed: bool,
}

fn status(args: &TargetArgs) -> Result<()> {
    let root = canonical_root(&args.app)?;
    let module = match &args.module {
        Some(m) => m.clone(),
        None => detect_app_module(&root)?,
    };
    let module_gradle = module_build_gradle(&root, &module)?;
    let dep_present = fs::read_to_string(&module_gradle)
        .map(|c| c.contains(DEP_MARKER))
        .unwrap_or(false);
    let aar_path = root.join(APP_AAR_RELPATH);
    let aar_present = aar_path.is_file();

    let report = StatusReport {
        app: root.display().to_string(),
        module,
        dependency_present: dep_present,
        aar_present,
        aar_path: APP_AAR_RELPATH.to_string(),
        installed: dep_present && aar_present,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.installed {
        println!("✓ agent AAR installed in `{}` (module :{})", report.app, report.module);
    } else {
        println!("✗ agent AAR not fully installed in `{}` (module :{})", report.app, report.module);
        println!("  dependency line: {}", yes_no(report.dependency_present));
        println!("  aar file:        {}", yes_no(report.aar_present));
        println!("  install with: shadowdroid aar install --app {}", report.app);
    }
    Ok(())
}

// ── remove ───────────────────────────────────────────────────────────────────

fn remove(args: &TargetArgs) -> Result<()> {
    let root = canonical_root(&args.app)?;
    let module = match &args.module {
        Some(m) => m.clone(),
        None => detect_app_module(&root)?,
    };
    let module_gradle = module_build_gradle(&root, &module)?;
    let removed_dep = unwire_dependency(&module_gradle)?;

    let aar_path = root.join(APP_AAR_RELPATH);
    let removed_aar = aar_path.is_file();
    if removed_aar {
        fs::remove_file(&aar_path).with_context(|| format!("remove {}", aar_path.display()))?;
    }
    // Tidy the shadowdroid/ dir if now empty.
    if let Some(dir) = aar_path.parent() {
        let _ = fs::remove_dir(dir);
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "action": "remove",
                "app": root.display().to_string(),
                "module": module,
                "dependency_removed": removed_dep,
                "aar_removed": removed_aar,
            }))?
        );
    } else {
        println!("✓ removed agent AAR from `{}` (module :{})", root.display(), module);
        println!("  dependency line: {}", if removed_dep { "removed" } else { "was absent" });
        println!("  aar file:        {}", if removed_aar { "removed" } else { "was absent" });
    }
    Ok(())
}

// ── app project introspection ─────────────────────────────────────────────────

fn canonical_root(app: &Path) -> Result<PathBuf> {
    let root = app
        .canonicalize()
        .with_context(|| format!("app path does not exist: {}", app.display()))?;
    if !settings_file(&root).is_some() {
        bail!(
            "`{}` is not a Gradle root (no settings.gradle[.kts]). Pass --app <project root>.",
            root.display()
        );
    }
    Ok(root)
}

fn settings_file(root: &Path) -> Option<PathBuf> {
    for name in ["settings.gradle.kts", "settings.gradle"] {
        let p = root.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Auto-detect the Android *application* module by scanning the settings file's
/// `include(":…")` entries and looking for the Android application plugin in
/// each module's build script.
fn detect_app_module(root: &Path) -> Result<String> {
    let settings = settings_file(root).ok_or_else(|| anyhow!("no settings file"))?;
    let text = fs::read_to_string(&settings).with_context(|| format!("read {}", settings.display()))?;

    let mut candidates = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("include(") {
            // include(":androidApp")  /  include(":foo:bar")
            for raw in rest.split(',') {
                if let Some(path) = raw.split('"').nth(1) {
                    if let Some(m) = path.strip_prefix(':') {
                        candidates.push(m.to_string());
                    }
                }
            }
        }
    }

    for module in &candidates {
        let gradle = module_build_gradle(root, module);
        if let Ok(gradle) = gradle {
            if let Ok(content) = fs::read_to_string(&gradle) {
                // Match the Android application plugin specifically — not the
                // bare Gradle `application` plugin used by JVM modules.
                if content.contains("com.android.application") || content.contains("androidApplication") {
                    return Ok(module.clone());
                }
            }
        }
    }

    bail!(
        "could not auto-detect an Android application module in {} (scanned: {}). \
         Pass --module <name>.",
        root.display(),
        if candidates.is_empty() {
            "none".to_string()
        } else {
            candidates.join(", ")
        }
    )
}

fn module_build_gradle(root: &Path, module: &str) -> Result<PathBuf> {
    let rel: PathBuf = module.split(':').filter(|s| !s.is_empty()).collect();
    let dir = root.join(rel);
    for name in ["build.gradle.kts", "build.gradle"] {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!("module `{}` has no build.gradle[.kts] under {}", module, dir.display())
}

// ── gradle file editing ───────────────────────────────────────────────────────

fn managed_lines() -> [String; 2] {
    [
        format!("    // {DEP_MARKER} — debug-only in-app debug agent"),
        format!("    debugImplementation(files(rootProject.file(\"{}\")))", APP_AAR_RELPATH),
    ]
}

/// Insert the managed `debugImplementation` line into the module's
/// `dependencies {}` block. Idempotent: returns false if already present.
fn wire_dependency(build_gradle: &Path) -> Result<bool> {
    let content = fs::read_to_string(build_gradle)
        .with_context(|| format!("read {}", build_gradle.display()))?;
    if content.contains(DEP_MARKER) {
        return Ok(false);
    }

    let managed = managed_lines();
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();

    let dep_idx = lines.iter().position(|l| {
        let t = l.trim_start();
        t.starts_with("dependencies {") || t.starts_with("dependencies{")
    });

    match dep_idx {
        Some(i) => {
            lines.insert(i + 1, managed[1].clone());
            lines.insert(i + 1, managed[0].clone());
        }
        None => {
            lines.push(String::new());
            lines.push("dependencies {".to_string());
            lines.push(managed[0].clone());
            lines.push(managed[1].clone());
            lines.push("}".to_string());
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    fs::write(build_gradle, out).with_context(|| format!("write {}", build_gradle.display()))?;
    Ok(true)
}

/// Remove the managed marker comment and the dependency line that follows it.
fn unwire_dependency(build_gradle: &Path) -> Result<bool> {
    let content = match fs::read_to_string(build_gradle) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    if !content.contains(DEP_MARKER) {
        return Ok(false);
    }

    let mut out: Vec<String> = Vec::new();
    let mut skip_next = false;
    for line in content.lines() {
        if line.contains(DEP_MARKER) {
            skip_next = true; // drop the marker line; the dep line is next
            continue;
        }
        if skip_next {
            skip_next = false;
            if line.contains("shadowdroid-agent.aar") {
                continue; // drop the managed dependency line
            }
        }
        out.push(line.to_string());
    }

    let mut text = out.join("\n");
    text.push('\n');
    fs::write(build_gradle, text).with_context(|| format!("write {}", build_gradle.display()))?;
    Ok(true)
}

fn gradle_assemble_debug(root: &Path, module: &str) -> Result<bool> {
    let gradlew = root.join(if cfg!(windows) { "gradlew.bat" } else { "gradlew" });
    if !gradlew.is_file() {
        bail!("no Gradle wrapper at {} — cannot --build", gradlew.display());
    }
    let task = format!(":{module}:assembleDebug");
    eprintln!("→ {} {}", gradlew.display(), task);
    let status = Command::new(&gradlew)
        .current_dir(root)
        .arg("--console=plain")
        .arg(&task)
        .status()
        .with_context(|| format!("run {} {}", gradlew.display(), task))?;
    Ok(status.success())
}

// ── AAR source resolution (mirrors installer.rs APK precedence) ───────────────

struct ResolvedAar {
    path: PathBuf,
    source: String,
    version: String,
}

async fn resolve_aar(explicit: Option<&Path>) -> Result<ResolvedAar> {
    // 1. explicit --from
    if let Some(p) = explicit {
        if !p.is_file() {
            bail!("--from AAR not found: {}", p.display());
        }
        return Ok(ResolvedAar {
            path: p.to_path_buf(),
            source: "explicit (--from)".into(),
            version: "dev".into(),
        });
    }

    let disable_dev = env_truthy("SHADOWDROID_DISABLE_DEV_SOURCES");
    if !disable_dev {
        // 2. repo auto-discovery: a freshly-built AAR in the ShadowDroid checkout.
        if let Some(p) = resolve_repo_build()? {
            return Ok(ResolvedAar { path: p, source: "repo build".into(), version: "dev".into() });
        }
        // 3. local drop-in
        let dropin = shadowdroid_home()?.join("agent/local").join(AAR_ASSET);
        if dropin.is_file() {
            return Ok(ResolvedAar { path: dropin, source: "local drop-in".into(), version: "dev".into() });
        }
    }

    // 4. versioned cache
    let cached = versioned_cache_dir()?.join(AAR_ASSET);
    if cached.is_file() {
        return Ok(ResolvedAar {
            path: cached,
            source: "versioned cache".into(),
            version: EXPECTED_VERSION.into(),
        });
    }

    // 5. GitHub release
    let path = download_release_aar().await?;
    Ok(ResolvedAar {
        path,
        source: "GitHub release".into(),
        version: EXPECTED_VERSION.into(),
    })
}

fn resolve_repo_build() -> Result<Option<PathBuf>> {
    let cwd = std::env::current_dir().context("cannot read $CWD")?;
    let mut dir: &Path = &cwd;
    loop {
        let release_aar =
            dir.join("agent/shadowdroid-agent/build/outputs/aar/shadowdroid-agent-release.aar");
        if release_aar.is_file() {
            return Ok(Some(release_aar));
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Ok(None),
        }
    }
}

async fn download_release_aar() -> Result<PathBuf> {
    let cache_dir = versioned_cache_dir()?;
    fs::create_dir_all(&cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;

    let base = release_base_url();
    let aar_url = format!("{base}/{AAR_ASSET}");
    let sums_url = format!("{base}/{CHECKSUMS_ASSET}");

    let checksums = download_text(&sums_url)
        .await
        .with_context(|| format!("download {CHECKSUMS_ASSET} from {base}"))?;
    let expected = checksum_for(&checksums, AAR_ASSET)
        .ok_or_else(|| anyhow!("no checksum for {AAR_ASSET} in {CHECKSUMS_ASSET} at {base}"))?;

    let dest = cache_dir.join(AAR_ASSET);
    download_file(&aar_url, &dest)
        .await
        .with_context(|| format!("download {AAR_ASSET}"))?;
    verify_sha256(&dest, &expected).with_context(|| format!("verify {AAR_ASSET}"))?;
    Ok(dest)
}

// ── small self-contained helpers (repo convention: per-module duplication) ────

fn release_base_url() -> String {
    let template = option_env!("SHADOWDROID_RELEASE_BASE_URL")
        .unwrap_or("https://github.com/andriyo/ShadowDroid/releases/download/v{version}");
    template
        .replace("{version}", EXPECTED_VERSION)
        .trim_end_matches('/')
        .to_string()
}

async fn download_text(url: &str) -> Result<String> {
    Ok(reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "shadowdroid")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

async fn download_file(url: &str, path: &Path) -> Result<()> {
    let bytes = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "shadowdroid")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

fn checksum_for(checksums: &str, asset: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let sha = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == asset {
            normalize_sha256(sha).ok()
        } else {
            None
        }
    })
}

fn normalize_sha256(value: &str) -> Result<String> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.len() == 64 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(lower)
    } else {
        bail!("invalid SHA-256 digest: {value}")
    }
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if actual != expected {
        bail!("checksum mismatch for {}: expected {expected}, got {actual}", path.display());
    }
    Ok(())
}

fn versioned_cache_dir() -> Result<PathBuf> {
    Ok(shadowdroid_home()?.join("agent").join(EXPECTED_VERSION))
}

fn shadowdroid_home() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "andriyo", "ShadowDroid")
        .ok_or_else(|| anyhow!("cannot determine home directory"))?;
    let home_dot = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".shadowdroid"));
    Ok(home_dot.unwrap_or_else(|| dirs.config_dir().to_path_buf()))
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "present"
    } else {
        "missing"
    }
}
