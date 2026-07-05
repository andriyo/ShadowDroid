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
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::hostenv::{env_truthy, shadowdroid_home};
use crate::release::{
    checksum_for, download_file, download_text, release_base_url, verify_sha256, CHECKSUMS_ASSET,
};

/// Release asset name (matches the `release.yml` staging step).
const AAR_ASSET: &str = "shadowdroid-agent.aar";
const EXPECTED_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the AAR is placed inside the target app, relative to the Gradle root.
const APP_AAR_RELPATH: &str = "shadowdroid/shadowdroid-agent.aar";

/// Tags the managed dependency line so install is idempotent and remove is exact.
const DEP_MARKER: &str = "shadowdroid-agent (managed by `shadowdroid aar`)";

/// Delimits the managed coroutine-probes block inside the module build file
/// (the full BEGIN/END lines live in the template; this substring is shared).
const PROBES_MARKER: &str = "shadowdroid coroutine probes (managed by `shadowdroid aar`)";

/// The coroutine-probes block: an AGP ASM visitor that swaps kotlin-stdlib's
/// no-op `DebugProbesKt` for the delegating variant in debug builds, activating
/// kotlinx-coroutines DebugProbes so `aar coroutines` sees live coroutines.
/// Inlined into the module's build.gradle.kts (an `apply(from = …)` script gets
/// its own classloader without AGP/ASM, so a separate file cannot work).
const PROBES_BLOCK: &str = include_str!("coroutine_probes.gradle.kts");

#[derive(Subcommand)]
pub enum AarCmd {
    /// Install the debug AAR into an app and wire one debug-only dependency.
    Install(InstallArgs),
    /// Report whether the AAR is wired into an app (path, dependency, version).
    Status(TargetArgs),
    /// Remove the managed dependency line and the copied AAR file.
    Remove(TargetArgs),

    // ── device verbs (talk to the running in-app agent) ──────────────────
    /// Drain in-app captured HTTP(S) flows; optionally export or persist them.
    Capture(CaptureArgs),
    /// Arm (or clear) in-app, above-TLS interception of matching flows.
    Intercept(InterceptArgs),
    /// Release a held flow, optionally mutating its response.
    Resume(ResumeArgs),
    /// Fail a held flow (the app sees a connection error).
    Drop(IdArgs),
    /// Show the running agent: info, armed matcher, held flows, capture count.
    Agent(JsonArg),
    /// Dump every live coroutine (state, job tree, stacks) via DebugProbes.
    Coroutines(CoroutinesArgs),
}

#[derive(Args)]
pub struct CoroutinesArgs {
    /// Include the full DebugProbes text dump (job hierarchy + stack traces).
    #[arg(long)]
    pub dump: bool,
    /// Stack frames to show per coroutine in the structured list (0 = none).
    #[arg(long, default_value_t = 6)]
    pub frames: u32,
    /// Cap the structured coroutine list (state counts are always complete).
    #[arg(long, default_value_t = 200)]
    pub limit: u32,
    /// Write the full text dump to this file (implies the dump is collected).
    #[arg(short = 'o', long)]
    pub out: Option<PathBuf>,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct CaptureArgs {
    /// Drain (clear) the agent buffer after reading.
    #[arg(long)]
    pub clear: bool,
    /// Write the flows as FlowRecord JSONL to this file.
    #[arg(short = 'o', long)]
    pub out: Option<PathBuf>,
    /// Generate a fixtures manifest + response files into this directory.
    #[arg(long)]
    pub fixtures: Option<PathBuf>,
    /// Append the flows to the `net` session store (so `net log`/`export` see them).
    #[arg(long)]
    pub store: bool,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct InterceptArgs {
    /// Host substring to match.
    #[arg(long)]
    pub host: Option<String>,
    /// Path substring to match.
    #[arg(long)]
    pub path: Option<String>,
    /// HTTP method to match (exact, case-insensitive).
    #[arg(long)]
    pub method: Option<String>,
    /// GraphQL operationName to match (exact).
    #[arg(long)]
    pub operation: Option<String>,
    /// Per-flow hold budget in ms (fail-open on expiry).
    #[arg(long)]
    pub hold_ms: Option<u64>,
    /// Disarm interception instead of arming.
    #[arg(long)]
    pub clear: bool,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ResumeArgs {
    /// Held flow id (from `aar status`).
    pub id: String,
    /// Override the response status code.
    #[arg(long)]
    pub set_status: Option<u16>,
    /// Replace the response body with this string.
    #[arg(long)]
    pub body: Option<String>,
    /// Replace the response body with this file's contents.
    #[arg(long)]
    pub body_file: Option<PathBuf>,
    /// Content-Type for the replaced body.
    #[arg(long)]
    pub content_type: Option<String>,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct IdArgs {
    /// Held flow id (from `aar status`).
    pub id: String,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct JsonArg {
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct InstallArgs {
    /// Gradle module to wire (e.g. `androidApp`). Auto-detected if omitted.
    #[arg(long)]
    pub module: Option<String>,
    /// Use this local AAR file instead of resolving one (dev / testing).
    #[arg(long)]
    pub from: Option<PathBuf>,
    /// Also activate kotlinx-coroutines DebugProbes in debug builds (build-time
    /// bytecode swap of the stdlib probe stub), enabling `aar coroutines`.
    #[arg(long)]
    pub coroutine_probes: bool,
    /// After wiring, run `:<module>:assembleDebug` to verify the app compiles.
    #[arg(long)]
    pub build: bool,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct TargetArgs {
    /// Gradle module (auto-detected if omitted).
    #[arg(long)]
    pub module: Option<String>,
    /// Emit a single JSON object instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(cmd: &AarCmd, project: Option<&Path>, device: Option<&str>) -> Result<()> {
    use crate::cmd::agent;

    // Device verbs talk to the running in-app agent; resolve a serial first.
    match cmd {
        AarCmd::Capture(a) => {
            let serial = crate::cli::resolve_serial(device).await?;
            return agent::capture(
                &serial,
                a.clear,
                a.out.as_ref(),
                a.fixtures.as_ref(),
                a.store,
                a.json,
            )
            .await;
        }
        AarCmd::Intercept(a) => {
            let serial = crate::cli::resolve_serial(device).await?;
            return if a.clear {
                agent::intercept_clear(&serial, a.json).await
            } else {
                agent::intercept(
                    &serial,
                    a.host.as_deref(),
                    a.path.as_deref(),
                    a.method.as_deref(),
                    a.operation.as_deref(),
                    a.hold_ms,
                    a.json,
                )
                .await
            };
        }
        AarCmd::Resume(a) => {
            let serial = crate::cli::resolve_serial(device).await?;
            let body = match (&a.body, &a.body_file) {
                (Some(b), _) => Some(b.clone()),
                (None, Some(f)) => Some(
                    fs::read_to_string(f)
                        .with_context(|| format!("read --body-file {}", f.display()))?,
                ),
                (None, None) => None,
            };
            return agent::resume(
                &serial,
                &a.id,
                a.set_status,
                body,
                a.content_type.as_deref(),
                a.json,
            )
            .await;
        }
        AarCmd::Drop(a) => {
            let serial = crate::cli::resolve_serial(device).await?;
            return agent::drop_flow(&serial, &a.id, a.json).await;
        }
        AarCmd::Agent(a) => {
            let serial = crate::cli::resolve_serial(device).await?;
            return agent::status(&serial, a.json).await;
        }
        AarCmd::Coroutines(a) => {
            let serial = crate::cli::resolve_serial(device).await?;
            return agent::coroutines(
                &serial,
                a.dump || a.out.is_some(),
                a.frames,
                a.limit,
                a.out.as_ref(),
                a.json,
            )
            .await;
        }
        _ => {}
    }

    // Host-only verbs: pure filesystem + Gradle on the host, no device.
    let root = canonical_root(project.unwrap_or_else(|| Path::new(".")))?;
    match cmd {
        AarCmd::Install(a) => install(a, &root).await,
        AarCmd::Status(a) => status(a, &root),
        AarCmd::Remove(a) => remove(a, &root),
        _ => unreachable!("device verbs handled above"),
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
    coroutine_probes: bool,
    build: &'static str,
}

async fn install(args: &InstallArgs, root: &Path) -> Result<()> {
    let module = match &args.module {
        Some(m) => m.clone(),
        None => detect_app_module(root)?,
    };
    let module_gradle = module_build_gradle(root, &module)?;

    // Resolve the AAR (dev → cache → release) and place it in the app.
    let resolved = resolve_aar(args.from.as_deref()).await?;
    let dest = root.join(APP_AAR_RELPATH);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(&resolved.path, &dest).with_context(|| format!("copy AAR to {}", dest.display()))?;

    let added = wire_dependency(&module_gradle)?;

    if args.coroutine_probes {
        wire_probes_block(&module_gradle)?;
    }

    let mut build_result = "skipped";
    if args.build {
        build_result = if gradle_assemble_debug(root, &module)? {
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
        coroutine_probes: args.coroutine_probes,
        build: build_result,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "✓ ShadowDroid agent AAR installed into `{}` (module :{})",
            report.app, report.module
        );
        println!(
            "  source:     {} (version {})",
            report.aar_source, report.aar_version
        );
        println!("  aar:        {}", report.aar_path);
        println!(
            "  dependency: {}",
            if added {
                "added debugImplementation line"
            } else {
                "already present (idempotent)"
            }
        );
        if args.coroutine_probes {
            println!(
                "  coroutines: probes activation wired (managed block in the module \
                 build file); rebuild + relaunch, then `shadowdroid aar coroutines`"
            );
        }
        match build_result {
            "ok" => println!("  build:      :{}:assembleDebug succeeded", report.module),
            "failed" => println!(
                "  build:      :{}:assembleDebug FAILED (see output above)",
                report.module
            ),
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
pub struct StatusReport {
    pub app: String,
    pub module: String,
    pub dependency_present: bool,
    pub aar_present: bool,
    pub aar_path: String,
    pub coroutine_probes: bool,
    pub installed: bool,
}

/// Inspect an app's Gradle root for the agent wiring. Reused by `aar status`
/// and surfaced in `doctor`.
pub fn inspect(root: &Path, module: Option<&str>) -> Result<StatusReport> {
    let module = match module {
        Some(m) => m.to_string(),
        None => detect_app_module(root)?,
    };
    let module_gradle = module_build_gradle(root, &module)?;
    let gradle_text = fs::read_to_string(&module_gradle).unwrap_or_default();
    let dependency_present = gradle_text.contains(DEP_MARKER);
    let aar_present = root.join(APP_AAR_RELPATH).is_file();
    let coroutine_probes = gradle_text.contains(PROBES_MARKER);
    Ok(StatusReport {
        app: root.display().to_string(),
        module,
        dependency_present,
        aar_present,
        aar_path: APP_AAR_RELPATH.to_string(),
        coroutine_probes,
        installed: dependency_present && aar_present,
    })
}

fn status(args: &TargetArgs, root: &Path) -> Result<()> {
    let report = inspect(root, args.module.as_deref())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.installed {
        println!(
            "✓ agent AAR installed in `{}` (module :{})",
            report.app, report.module
        );
        println!(
            "  coroutine probes: {}",
            if report.coroutine_probes {
                "wired (debug builds activate DebugProbes)"
            } else {
                "not wired (add with `aar install --coroutine-probes`)"
            }
        );
    } else {
        println!(
            "✗ agent AAR not fully installed in `{}` (module :{})",
            report.app, report.module
        );
        println!("  dependency line: {}", yes_no(report.dependency_present));
        println!("  aar file:        {}", yes_no(report.aar_present));
        println!(
            "  install with: shadowdroid aar install --project-root {}",
            report.app
        );
    }
    Ok(())
}

// ── remove ───────────────────────────────────────────────────────────────────

fn remove(args: &TargetArgs, root: &Path) -> Result<()> {
    let module = match &args.module {
        Some(m) => m.clone(),
        None => detect_app_module(root)?,
    };
    let module_gradle = module_build_gradle(root, &module)?;
    let removed_dep = unwire_dependency(&module_gradle)?;
    let removed_probes = unwire_probes(&module_gradle)?;

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
                "coroutine_probes_removed": removed_probes,
                "aar_removed": removed_aar,
            }))?
        );
    } else {
        println!(
            "✓ removed agent AAR from `{}` (module :{})",
            root.display(),
            module
        );
        println!(
            "  dependency line: {}",
            if removed_dep { "removed" } else { "was absent" }
        );
        println!(
            "  aar file:        {}",
            if removed_aar { "removed" } else { "was absent" }
        );
        if removed_probes {
            println!("  coroutine probes: removed (apply line + script)");
        }
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
            "`{}` is not a Gradle root (no settings.gradle[.kts]). Pass --project-root <path>.",
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
    let text =
        fs::read_to_string(&settings).with_context(|| format!("read {}", settings.display()))?;

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
                if content.contains("com.android.application")
                    || content.contains("androidApplication")
                {
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
    bail!(
        "module `{}` has no build.gradle[.kts] under {}",
        module,
        dir.display()
    )
}

// ── gradle file editing ───────────────────────────────────────────────────────

fn managed_lines() -> [String; 2] {
    [
        format!("    // {DEP_MARKER} — debug-only in-app debug agent"),
        format!(
            "    debugImplementation(files(rootProject.file(\"{}\")))",
            APP_AAR_RELPATH
        ),
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

/// Append the managed coroutine-probes block to the module build file.
/// Idempotent. Kotlin DSL only: the block declares a Kotlin class against
/// AGP/ASM APIs, which a Groovy build file cannot host.
fn wire_probes_block(build_gradle: &Path) -> Result<bool> {
    if !build_gradle
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("kts"))
    {
        bail!(
            "--coroutine-probes requires a Kotlin DSL build file (build.gradle.kts); \
             `{}` is Groovy. Convert the module or wire the probes manually.",
            build_gradle.display()
        );
    }
    let content = fs::read_to_string(build_gradle)
        .with_context(|| format!("read {}", build_gradle.display()))?;
    if content.contains(PROBES_MARKER) {
        return Ok(false);
    }
    let mut out = content;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(PROBES_BLOCK);
    fs::write(build_gradle, out).with_context(|| format!("write {}", build_gradle.display()))?;
    Ok(true)
}

/// Remove the managed coroutine-probes block (BEGIN through END marker line).
fn unwire_probes(build_gradle: &Path) -> Result<bool> {
    let content = match fs::read_to_string(build_gradle) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    if !content.contains(PROBES_MARKER) {
        return Ok(false);
    }
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        if !in_block && line.contains(">>>") && line.contains(PROBES_MARKER) {
            in_block = true;
            continue;
        }
        if in_block {
            if line.contains("<<<") && line.contains(PROBES_MARKER) {
                in_block = false;
            }
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    let mut text = out.join("\n");
    text.push('\n');
    fs::write(build_gradle, text).with_context(|| format!("write {}", build_gradle.display()))?;
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
    let gradlew = root.join(if cfg!(windows) {
        "gradlew.bat"
    } else {
        "gradlew"
    });
    if !gradlew.is_file() {
        bail!(
            "no Gradle wrapper at {} — cannot --build",
            gradlew.display()
        );
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
            return Ok(ResolvedAar {
                path: p,
                source: "repo build".into(),
                version: "dev".into(),
            });
        }
        // 3. local drop-in
        let dropin = shadowdroid_home()?.join("agent/local").join(AAR_ASSET);
        if dropin.is_file() {
            return Ok(ResolvedAar {
                path: dropin,
                source: "local drop-in".into(),
                version: "dev".into(),
            });
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

    let base = release_base_url(EXPECTED_VERSION);
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

fn versioned_cache_dir() -> Result<PathBuf> {
    Ok(shadowdroid_home()?.join("agent").join(EXPECTED_VERSION))
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "present"
    } else {
        "missing"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kts(dir: &tempfile::TempDir) -> PathBuf {
        let p = dir.path().join("build.gradle.kts");
        fs::write(
            &p,
            "plugins {\n    id(\"com.android.application\")\n}\n\ndependencies {\n}\n",
        )
        .unwrap();
        p
    }

    #[test]
    fn probes_block_wires_once_and_unwires_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let gradle = kts(&dir);
        let before = fs::read_to_string(&gradle).unwrap();

        assert!(wire_probes_block(&gradle).unwrap());
        let wired = fs::read_to_string(&gradle).unwrap();
        assert!(wired.contains(PROBES_MARKER));
        assert!(wired.contains("ShadowDroidCoroutineProbesFactory"));
        // Idempotent: second wire is a no-op.
        assert!(!wire_probes_block(&gradle).unwrap());
        assert_eq!(fs::read_to_string(&gradle).unwrap(), wired);

        assert!(unwire_probes(&gradle).unwrap());
        let after = fs::read_to_string(&gradle).unwrap();
        assert!(!after.contains(PROBES_MARKER));
        assert!(!after.contains("ShadowDroidCoroutineProbesFactory"));
        assert_eq!(after.trim_end(), before.trim_end());
        // Nothing left to unwire.
        assert!(!unwire_probes(&gradle).unwrap());
    }

    #[test]
    fn probes_block_rejects_groovy_build_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("build.gradle");
        fs::write(&p, "dependencies {\n}\n").unwrap();
        let err = wire_probes_block(&p).unwrap_err().to_string();
        assert!(err.contains("build.gradle.kts"), "unexpected error: {err}");
    }

    #[test]
    fn probes_block_survives_content_between_markers_only() {
        let dir = tempfile::tempdir().unwrap();
        let gradle = kts(&dir);
        wire_probes_block(&gradle).unwrap();
        // A user line added after the block must survive removal.
        let mut text = fs::read_to_string(&gradle).unwrap();
        text.push_str("\n// user note: keep me\n");
        fs::write(&gradle, text).unwrap();

        unwire_probes(&gradle).unwrap();
        let after = fs::read_to_string(&gradle).unwrap();
        assert!(after.contains("keep me"));
        assert!(!after.contains(PROBES_MARKER));
    }
}
