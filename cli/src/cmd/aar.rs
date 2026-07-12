//! `shadowdroid aar` — install and manage the in-app debug AAR in an app whose
//! source you can build.
//!
//! The core agent is a `debugImplementation`-only AAR that auto-installs through
//! a merged `ContentProvider` with no app code. Network capture is separate: the
//! optional OkHttp companion AAR needs an application interceptor added to each
//! debug OkHttp client. It does not instrument Cronet, QUIC, or other clients.
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
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::hostenv::{env_truthy, shadowdroid_home};
use crate::release::{
    checksum_for, download_file, download_text, release_base_url, verify_sha256, CHECKSUMS_ASSET,
};

/// Release asset name (matches the `release.yml` staging step).
const AAR_ASSET: &str = "shadowdroid-agent.aar";
const OKHTTP_AAR_ASSET: &str = "shadowdroid-agent-okhttp.aar";
const EXPECTED_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the AAR is placed inside the target app, relative to the Gradle root.
const APP_AAR_RELPATH: &str = "shadowdroid/shadowdroid-agent.aar";
const APP_OKHTTP_AAR_RELPATH: &str = "shadowdroid/shadowdroid-agent-okhttp.aar";

/// Tags the managed dependency line so install is idempotent and remove is exact.
const DEP_MARKER: &str = "shadowdroid-agent (managed by `shadowdroid aar`)";
const OKHTTP_DEP_MARKER: &str = "shadowdroid-agent-okhttp (managed by `shadowdroid aar`)";

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
    /// Install the core debug AAR; optionally add the OkHttp capture companion.
    Install(InstallArgs),
    /// Report whether the AAR is wired into an app (path, dependency, version).
    Status(TargetArgs),
    /// Remove the managed dependency line and the copied AAR file.
    Remove(TargetArgs),

    // ── device verbs (talk to the running in-app agent) ──────────────────
    /// Drain flows captured by a registered provider (currently the OkHttp companion).
    Capture(CaptureArgs),
    /// Arm (or clear) interception for OkHttp companion-captured flows.
    Intercept(InterceptArgs),
    /// Release a held flow, optionally mutating its response.
    Resume(ResumeArgs),
    /// Fail a held flow (the app sees a connection error).
    Drop(IdArgs),
    /// Show the running agent, capture-provider state, matcher, and held flows.
    Agent(JsonArg),
    /// Dump every live coroutine (state, job tree, stacks) via DebugProbes.
    Coroutines(CoroutinesArgs),
}

impl AarCmd {
    pub(crate) fn requires_device(&self) -> bool {
        !matches!(self, Self::Install(_) | Self::Status(_) | Self::Remove(_))
    }
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
    /// Held flow id (from `aar agent`).
    pub id: String,
    /// Override the response status code.
    #[arg(long)]
    pub set_status: Option<u16>,
    /// Replace the response body with this string.
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,
    /// Replace the response body with this file's contents.
    #[arg(long, conflicts_with = "body")]
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
    /// Held flow id (from `aar agent`).
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
    /// Install the optional OkHttp capture/intercept companion AAR. You must
    /// still add ShadowDroidCaptureInterceptor to each debug OkHttpClient.
    #[arg(long)]
    pub okhttp: bool,
    /// Use this local OkHttp companion AAR instead of resolving one.
    #[arg(long, requires = "okhttp")]
    pub okhttp_from: Option<PathBuf>,
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
            if a.content_type.is_some() && a.body.is_none() && a.body_file.is_none() {
                bail!("--content-type requires --body or --body-file");
            }
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
    okhttp_companion: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    okhttp_aar_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    okhttp_aar_path: Option<String>,
    okhttp_dependency_added: bool,
    network_capture_next: &'static str,
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
    copy_atomic(&resolved.path, &dest)?;

    let added = wire_dependency(&module_gradle, DEP_MARKER, APP_AAR_RELPATH)?;

    let mut okhttp_source = None;
    let mut okhttp_dependency_added = false;
    if args.okhttp {
        let companion = resolve_okhttp_aar(args.okhttp_from.as_deref()).await?;
        let companion_dest = root.join(APP_OKHTTP_AAR_RELPATH);
        copy_atomic(&companion.path, &companion_dest)?;
        okhttp_dependency_added =
            wire_dependency(&module_gradle, OKHTTP_DEP_MARKER, APP_OKHTTP_AAR_RELPATH)?;
        okhttp_source = Some(companion.source);
    }

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

    let final_state = inspect(root, Some(&module))?;
    let okhttp_companion = final_state.okhttp_companion_installed;
    let report = InstallReport {
        action: "install",
        app: root.display().to_string(),
        module,
        aar_source: resolved.source.to_string(),
        aar_version: resolved.version.clone(),
        aar_path: APP_AAR_RELPATH.to_string(),
        dependency_added: added,
        okhttp_companion,
        okhttp_aar_source: okhttp_source
            .or_else(|| okhttp_companion.then(|| "existing project wiring".to_string())),
        okhttp_aar_path: final_state
            .okhttp_aar_present
            .then(|| APP_OKHTTP_AAR_RELPATH.to_string()),
        okhttp_dependency_added,
        network_capture_next: if okhttp_companion {
            "add ShadowDroidCaptureInterceptor() to each debug OkHttpClient, then rebuild and relaunch"
        } else {
            "rerun aar install with --okhttp, then add ShadowDroidCaptureInterceptor() to each debug OkHttpClient"
        },
        coroutine_probes: args.coroutine_probes,
        build: build_result,
    };

    if build_result == "failed" {
        return Err(crate::diagnostic::DiagnosticError::new(
            "aar_app_build_failed",
            "aar_install",
            format!(
                ":{}:assembleDebug failed after wiring the AAR",
                report.module
            ),
        )
        .detail(serde_json::json!({
            "install_report": &report,
            "failed_task": format!(":{}:assembleDebug", report.module),
        }))
        .next_actions([
            format!(
                "fix the first Gradle error from :{}:assembleDebug",
                report.module
            ),
            format!(
                "rerun `shadowdroid aar install --project-root {:?} --module {} --build`",
                report.app, report.module,
            ),
        ])
        .into());
    }

    if args.json {
        crate::events::emit_result(&report);
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
        if report.okhttp_companion {
            println!(
                "  network:    OkHttp companion AAR wired{} (source: {})",
                if report.okhttp_dependency_added {
                    ""
                } else {
                    " (already present)"
                },
                report.okhttp_aar_source.as_deref().unwrap_or("unknown"),
            );
        } else {
            println!(
                "  network:    no capture provider installed (the core AAR does not capture HTTP)"
            );
        }
        println!("  next:       {}", report.network_capture_next);
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
            "\nThe agent auto-starts in debug builds. Launch the app, then verify it with:\n  \
             shadowdroid aar agent"
        );
    }

    Ok(())
}

fn copy_atomic(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut source_file =
        fs::File::open(source).with_context(|| format!("open AAR source {}", source.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary AAR beside {}", destination.display()))?;
    std::io::copy(&mut source_file, &mut temp).with_context(|| {
        format!(
            "copy AAR from {} to temporary file beside {}",
            source.display(),
            destination.display(),
        )
    })?;
    temp.flush()
        .with_context(|| format!("flush temporary AAR for {}", destination.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync temporary AAR for {}", destination.display()))?;
    if let Ok(metadata) = fs::metadata(destination).or_else(|_| fs::metadata(source)) {
        temp.as_file()
            .set_permissions(metadata.permissions())
            .with_context(|| {
                format!(
                    "set permissions on temporary AAR for {}",
                    destination.display()
                )
            })?;
    }
    temp.persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace AAR {}", destination.display()))?;
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
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
    pub okhttp_dependency_present: bool,
    pub okhttp_aar_present: bool,
    pub okhttp_aar_path: String,
    pub okhttp_companion_installed: bool,
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
    let okhttp_dependency_present = gradle_text.contains(OKHTTP_DEP_MARKER);
    let okhttp_aar_present = root.join(APP_OKHTTP_AAR_RELPATH).is_file();
    let coroutine_probes = gradle_text.contains(PROBES_MARKER);
    Ok(StatusReport {
        app: root.display().to_string(),
        module,
        dependency_present,
        aar_present,
        aar_path: APP_AAR_RELPATH.to_string(),
        okhttp_dependency_present,
        okhttp_aar_present,
        okhttp_aar_path: APP_OKHTTP_AAR_RELPATH.to_string(),
        okhttp_companion_installed: okhttp_dependency_present && okhttp_aar_present,
        coroutine_probes,
        installed: dependency_present && aar_present,
    })
}

fn status(args: &TargetArgs, root: &Path) -> Result<()> {
    let report = inspect(root, args.module.as_deref())?;
    if args.json {
        crate::events::emit_result(&report);
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
        if report.okhttp_companion_installed {
            println!(
                "  network capture: OkHttp companion wired; ensure every debug OkHttpClient adds ShadowDroidCaptureInterceptor()"
            );
        } else {
            println!("  network capture: unavailable (core AAR has no HTTP capture provider)");
            println!("  next: rerun `shadowdroid aar install --okhttp`, add ShadowDroidCaptureInterceptor(), rebuild, and relaunch");
        }
    } else {
        println!(
            "✗ agent AAR not fully installed in `{}` (module :{})",
            report.app, report.module
        );
        println!("  dependency line: {}", yes_no(report.dependency_present));
        println!("  aar file:        {}", yes_no(report.aar_present));
        println!(
            "  OkHttp dependency: {}",
            yes_no(report.okhttp_dependency_present)
        );
        println!("  OkHttp AAR file:   {}", yes_no(report.okhttp_aar_present));
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
    let removed_dep = unwire_dependency(&module_gradle, DEP_MARKER, AAR_ASSET)?;
    let removed_okhttp_dep =
        unwire_dependency(&module_gradle, OKHTTP_DEP_MARKER, OKHTTP_AAR_ASSET)?;
    let removed_probes = unwire_probes(&module_gradle)?;

    let aar_path = root.join(APP_AAR_RELPATH);
    let removed_aar = aar_path.is_file();
    if removed_aar {
        fs::remove_file(&aar_path).with_context(|| format!("remove {}", aar_path.display()))?;
    }
    let okhttp_aar_path = root.join(APP_OKHTTP_AAR_RELPATH);
    let removed_okhttp_aar = okhttp_aar_path.is_file();
    if removed_okhttp_aar {
        fs::remove_file(&okhttp_aar_path)
            .with_context(|| format!("remove {}", okhttp_aar_path.display()))?;
    }
    // Tidy the shadowdroid/ dir if now empty.
    if let Some(dir) = aar_path.parent() {
        let _ = fs::remove_dir(dir);
    }

    if args.json {
        crate::events::emit_result(&serde_json::json!({
            "action": "remove",
            "app": root.display().to_string(),
            "module": module,
            "dependency_removed": removed_dep,
            "okhttp_dependency_removed": removed_okhttp_dep,
            "coroutine_probes_removed": removed_probes,
            "aar_removed": removed_aar,
            "okhttp_aar_removed": removed_okhttp_aar,
        }));
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
        println!(
            "  OkHttp companion: {}",
            if removed_okhttp_dep || removed_okhttp_aar {
                "removed"
            } else {
                "was absent"
            }
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
    if settings_file(&root).is_none() {
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

fn managed_lines(marker: &str, aar_relpath: &str) -> [String; 2] {
    [
        format!("    // {marker} — debug-only in-app debug agent"),
        format!(
            "    debugImplementation(files(rootProject.file(\"{}\")))",
            aar_relpath
        ),
    ]
}

/// Insert the managed `debugImplementation` line into the module's
/// `dependencies {}` block. Idempotent: returns false if already present.
fn wire_dependency(build_gradle: &Path, marker: &str, aar_relpath: &str) -> Result<bool> {
    let content = fs::read_to_string(build_gradle)
        .with_context(|| format!("read {}", build_gradle.display()))?;
    if content.contains(marker) {
        return Ok(false);
    }

    let managed = managed_lines(marker, aar_relpath);
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
fn unwire_dependency(build_gradle: &Path, marker: &str, asset_name: &str) -> Result<bool> {
    let content = match fs::read_to_string(build_gradle) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    if !content.contains(marker) {
        return Ok(false);
    }

    let mut out: Vec<String> = Vec::new();
    let mut skip_next = false;
    for line in content.lines() {
        if line.contains(marker) {
            skip_next = true; // drop the marker line; the dep line is next
            continue;
        }
        if skip_next {
            skip_next = false;
            if line.contains(asset_name) {
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
    resolve_agent_asset(
        explicit,
        "--from",
        AAR_ASSET,
        "agent/shadowdroid-agent/build/outputs/aar/shadowdroid-agent-release.aar",
    )
    .await
}

async fn resolve_okhttp_aar(explicit: Option<&Path>) -> Result<ResolvedAar> {
    resolve_agent_asset(
        explicit,
        "--okhttp-from",
        OKHTTP_AAR_ASSET,
        "agent/shadowdroid-agent-okhttp/build/outputs/aar/shadowdroid-agent-okhttp-release.aar",
    )
    .await
}

async fn resolve_agent_asset(
    explicit: Option<&Path>,
    explicit_flag: &str,
    asset: &str,
    repo_build_relpath: &str,
) -> Result<ResolvedAar> {
    // 1. explicit local file
    if let Some(p) = explicit {
        if !p.is_file() {
            bail!("{explicit_flag} AAR not found: {}", p.display());
        }
        return Ok(ResolvedAar {
            path: p.to_path_buf(),
            source: format!("explicit ({explicit_flag})"),
            version: "dev".into(),
        });
    }

    let disable_dev = env_truthy("SHADOWDROID_DISABLE_DEV_SOURCES");
    if !disable_dev {
        // 2. repo auto-discovery: a freshly-built AAR in the ShadowDroid checkout.
        if let Some(p) = resolve_repo_build(repo_build_relpath)? {
            return Ok(ResolvedAar {
                path: p,
                source: "repo build".into(),
                version: "dev".into(),
            });
        }
        // 3. local drop-in
        let dropin = shadowdroid_home()?.join("agent/local").join(asset);
        if dropin.is_file() {
            return Ok(ResolvedAar {
                path: dropin,
                source: "local drop-in".into(),
                version: "dev".into(),
            });
        }
    }

    // 4. versioned cache
    let cached = versioned_cache_dir()?.join(asset);
    if cached.is_file() && cached_asset_is_verified(&cached) {
        return Ok(ResolvedAar {
            path: cached,
            source: "versioned cache".into(),
            version: EXPECTED_VERSION.into(),
        });
    }

    // 5. GitHub release
    let path = download_release_aar(asset).await?;
    Ok(ResolvedAar {
        path,
        source: "GitHub release".into(),
        version: EXPECTED_VERSION.into(),
    })
}

fn resolve_repo_build(repo_build_relpath: &str) -> Result<Option<PathBuf>> {
    let cwd = std::env::current_dir().context("cannot read $CWD")?;
    let mut dir: &Path = &cwd;
    loop {
        let release_aar = dir.join(repo_build_relpath);
        if release_aar.is_file() {
            return Ok(Some(release_aar));
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Ok(None),
        }
    }
}

async fn download_release_aar(asset: &str) -> Result<PathBuf> {
    let cache_dir = versioned_cache_dir()?;
    fs::create_dir_all(&cache_dir).with_context(|| format!("create {}", cache_dir.display()))?;

    let base = release_base_url(EXPECTED_VERSION);
    let aar_url = format!("{base}/{asset}");
    let sums_url = format!("{base}/{CHECKSUMS_ASSET}");

    let checksums = download_text(&sums_url)
        .await
        .with_context(|| format!("download {CHECKSUMS_ASSET} from {base}"))?;
    let expected = checksum_for(&checksums, asset)
        .ok_or_else(|| anyhow!("no checksum for {asset} in {CHECKSUMS_ASSET} at {base}"))?;

    let dest = cache_dir.join(asset);
    let temp_path = tempfile::NamedTempFile::new_in(&cache_dir)
        .with_context(|| format!("create temporary download in {}", cache_dir.display()))?
        .into_temp_path();
    download_file(&aar_url, &temp_path)
        .await
        .with_context(|| format!("download {asset}"))?;
    verify_sha256(&temp_path, &expected).with_context(|| format!("verify {asset}"))?;
    fs::File::open(&temp_path)
        .with_context(|| format!("open downloaded {asset} for sync"))?
        .sync_all()
        .with_context(|| format!("sync downloaded {asset}"))?;
    temp_path
        .persist(&dest)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically cache {asset} at {}", dest.display()))?;
    write_checksum_sidecar(&dest, &expected)?;
    Ok(dest)
}

fn cached_asset_is_verified(path: &Path) -> bool {
    let expected = match fs::read_to_string(checksum_sidecar(path)) {
        Ok(value) => value,
        Err(_) => return false,
    };
    verify_sha256(path, expected.trim()).is_ok()
}

fn checksum_sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".sha256");
    PathBuf::from(name)
}

fn write_checksum_sidecar(path: &Path, expected: &str) -> Result<()> {
    let sidecar = checksum_sidecar(path);
    let parent = sidecar.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create checksum sidecar beside {}", path.display()))?;
    writeln!(temp, "{expected}")
        .with_context(|| format!("write checksum sidecar for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("flush checksum sidecar for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync checksum sidecar for {}", path.display()))?;
    temp.persist(&sidecar)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically cache checksum at {}", sidecar.display()))?;
    Ok(())
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
    fn core_and_okhttp_dependencies_are_managed_independently() {
        let dir = tempfile::tempdir().unwrap();
        let gradle = kts(&dir);

        assert!(wire_dependency(&gradle, DEP_MARKER, APP_AAR_RELPATH).unwrap());
        assert!(wire_dependency(&gradle, OKHTTP_DEP_MARKER, APP_OKHTTP_AAR_RELPATH).unwrap());
        let wired = fs::read_to_string(&gradle).unwrap();
        assert!(wired.contains(APP_AAR_RELPATH));
        assert!(wired.contains(APP_OKHTTP_AAR_RELPATH));

        assert!(unwire_dependency(&gradle, DEP_MARKER, AAR_ASSET).unwrap());
        let core_removed = fs::read_to_string(&gradle).unwrap();
        assert!(!core_removed.contains(APP_AAR_RELPATH));
        assert!(core_removed.contains(APP_OKHTTP_AAR_RELPATH));

        assert!(unwire_dependency(&gradle, OKHTTP_DEP_MARKER, OKHTTP_AAR_ASSET).unwrap());
        assert!(!fs::read_to_string(&gradle)
            .unwrap()
            .contains(APP_OKHTTP_AAR_RELPATH));
    }

    #[test]
    fn versioned_cache_requires_a_matching_checksum_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let asset = dir.path().join(AAR_ASSET);
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        fs::write(&asset, b"abc").unwrap();

        assert!(!cached_asset_is_verified(&asset));
        write_checksum_sidecar(&asset, sha).unwrap();
        assert!(cached_asset_is_verified(&asset));

        fs::write(&asset, b"truncated").unwrap();
        assert!(!cached_asset_is_verified(&asset));
    }

    #[test]
    fn project_aar_copy_atomically_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.aar");
        let destination = dir.path().join("app/shadowdroid-agent.aar");
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&source, b"complete-new-aar").unwrap();
        fs::write(&destination, b"old-aar").unwrap();

        copy_atomic(&source, &destination).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"complete-new-aar");
    }

    #[test]
    fn inspect_reports_companion_from_final_project_state() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            dir.path().join("settings.gradle.kts"),
            "include(\":app\")\n",
        )
        .unwrap();
        let core = managed_lines(DEP_MARKER, APP_AAR_RELPATH);
        let okhttp = managed_lines(OKHTTP_DEP_MARKER, APP_OKHTTP_AAR_RELPATH);
        fs::write(
            app.join("build.gradle.kts"),
            format!(
                "plugins {{ id(\"com.android.application\") }}\n\
                 dependencies {{\n\
                 {}\n{}\n{}\n{}\n\
                 }}\n",
                core[0], core[1], okhttp[0], okhttp[1],
            ),
        )
        .unwrap();
        let assets = dir.path().join("shadowdroid");
        fs::create_dir_all(&assets).unwrap();
        fs::write(dir.path().join(APP_AAR_RELPATH), b"core").unwrap();
        fs::write(dir.path().join(APP_OKHTTP_AAR_RELPATH), b"okhttp").unwrap();

        let status = inspect(dir.path(), Some("app")).unwrap();

        assert!(status.installed);
        assert!(status.okhttp_companion_installed);
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
