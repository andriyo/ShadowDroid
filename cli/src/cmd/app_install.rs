//! `app-install` / `app-reinstall` — the whole app-under-test setup ritual as
//! one structured call: install → (optional) clear data → (optional) grant
//! runtime permissions → (optional) launch + wait for the app to reach the
//! foreground. `app-reinstall` additionally uninstalls first (e.g. to cross a
//! signature change or wipe state).
//!
//! **Fully host-side** (`adb` only): install/uninstall/clear via the ADB
//! protocol, grant via `pm grant`, launch via `am start`, and wait-for-front by
//! polling `dumpsys activity`. It deliberately does NOT use the on-device
//! ShadowDroid server — launching a heavy app-under-test can evict the
//! instrumentation under memory pressure, so reusing it for the wait would be
//! flaky. Keeping this host-only also means it works before ShadowDroid is
//! connected.
//!
//! Package / launch activity / declared permissions all come from one
//! `aapt2 dump badging` of the APK (aapt2 is located on PATH or in the Android
//! SDK build-tools). `--package` overrides the package when aapt2 is absent.

use crate::ids::Serial;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cmd::permissions;
use crate::device::adb;

/// Shared CLI args for `app-install` and `app-reinstall` (wrapped by both
/// subcommands). `reinstall` is supplied by the dispatch, not parsed here.
#[derive(clap::Args)]
pub struct AppInstallArgs {
    /// Path to the APK to install.
    pub apk: PathBuf,
    /// Override the package name (otherwise read from the APK via aapt2).
    #[arg(long)]
    pub package: Option<String>,
    /// Wipe app data after installing.
    #[arg(long)]
    pub clear: bool,
    /// Grant every runtime permission the app declares.
    #[arg(long)]
    pub grant_all: bool,
    /// Grant a specific runtime permission (repeatable).
    #[arg(long)]
    pub grant: Vec<String>,
    /// Launch the app after installing.
    #[arg(long)]
    pub launch: bool,
    /// Launch and wait for the app to reach the foreground (implies --launch).
    #[arg(long)]
    pub wait_front: bool,
    /// Timeout for --wait-front, in milliseconds.
    #[arg(long, default_value_t = 20000)]
    pub timeout_ms: u32,
}

/// One step's outcome, collected into the summary.
struct Step {
    name: &'static str,
    ok: bool,
    detail: serde_json::Value,
}

impl Step {
    fn ok(name: &'static str, detail: impl Into<serde_json::Value>) -> Self {
        Step {
            name,
            ok: true,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<serde_json::Value>) -> Self {
        Step {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "step": self.name, "ok": self.ok, "detail": self.detail })
    }
}

pub async fn run(serial: &Serial, args: &AppInstallArgs, reinstall: bool) -> Result<()> {
    if !args.apk.is_file() {
        bail!("APK not found: {}", args.apk.display());
    }
    // One aapt2 read gives package + launch activity + declared permissions.
    let meta = resolve_meta(&args.apk);
    let package = args
        .package
        .clone()
        .or_else(|| meta.as_ref().ok().map(|m| m.package.clone()))
        .ok_or_else(|| anyhow!("could not determine package from APK; pass --package <pkg>"))?;
    let activity = meta.as_ref().ok().and_then(|m| m.launch_activity.clone());

    let mut steps: Vec<Step> = Vec::new();
    let mut front_activity: Option<String> = None;

    // ── (reinstall) uninstall first ─────────────────────────────────────────
    if reinstall {
        match adb::uninstall(serial, package.clone()).await {
            Ok(()) => steps.push(Step::ok("uninstall", package.clone())),
            Err(e) => steps.push(Step::ok("uninstall", format!("skipped: {e}"))), // not installed
        }
    }

    // ── install ─────────────────────────────────────────────────────────────
    match adb::install(serial, args.apk.clone()).await {
        Ok(()) => steps.push(Step::ok("install", args.apk.display().to_string())),
        Err(e) => {
            let hint = if is_signature_mismatch(&e) {
                " — installed build is signed differently; use `app-reinstall` to replace it"
            } else {
                ""
            };
            steps.push(Step::fail("install", format!("{e}{hint}")));
            return emit_summary(&package, args, reinstall, steps, front_activity);
        }
    }

    // ── (clear) wipe app data ───────────────────────────────────────────────
    if args.clear {
        match adb::shell(serial, format!("pm clear {package}")).await {
            Ok(o) if o.trim() == "Success" => steps.push(Step::ok("clear", "Success")),
            Ok(o) => steps.push(Step::fail("clear", o.trim().to_string())),
            Err(e) => steps.push(Step::fail("clear", e.to_string())),
        }
    }

    // ── (grant) runtime permissions ─────────────────────────────────────────
    if args.grant_all || !args.grant.is_empty() {
        // Use the APK-declared permissions (from aapt2), not a just-installed
        // dumpsys — the runtime-permission block can be momentarily empty right
        // after install. `pm grant` ignores non-runtime perms; the readback in
        // grant_quiet reports which ones actually took.
        let perms = if args.grant_all {
            meta.as_ref()
                .map(|m| m.permissions.clone())
                .unwrap_or_default()
        } else {
            args.grant.clone()
        };
        match permissions::grant_quiet(serial, &package, &perms).await {
            Ok(now) => {
                let granted: Vec<&String> =
                    now.iter().filter(|(_, &g)| g).map(|(p, _)| p).collect();
                steps.push(Step::ok(
                    "grant",
                    serde_json::json!({ "requested": perms, "granted": granted }),
                ));
            }
            Err(e) => steps.push(Step::fail("grant", e.to_string())),
        }
    }

    // ── (launch) host-side via am start / monkey ────────────────────────────
    if args.launch || args.wait_front {
        let launch = match &activity {
            Some(act) => adb::shell(serial, format!("am start -n {package}/{act}")).await,
            None => {
                adb::shell(
                    serial,
                    format!("monkey -p {package} -c android.intent.category.LAUNCHER 1"),
                )
                .await
            }
        };
        match launch {
            Ok(out) if !launch_failed(&out) => steps.push(Step::ok("launch", package.clone())),
            Ok(out) => steps.push(Step::fail("launch", out.trim().to_string())),
            Err(e) => steps.push(Step::fail("launch", e.to_string())),
        }

        // ── (wait-front) poll dumpsys for the package in the foreground ──────
        if args.wait_front {
            let (matched, fg) = wait_front(serial, &package, args.timeout_ms).await;
            front_activity = fg.clone();
            let step = if matched { Step::ok } else { Step::fail };
            steps.push(step(
                "wait_front",
                serde_json::json!({ "matched": matched, "current": fg }),
            ));
        }
    }

    emit_summary(&package, args, reinstall, steps, front_activity)
}

/// Poll the foreground component until it belongs to `package` or the timeout
/// elapses. Returns `(matched, last_seen_foreground)`.
async fn wait_front(serial: &Serial, package: &str, timeout_ms: u32) -> (bool, Option<String>) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    let prefix = format!("{package}/");
    loop {
        let fg = adb::foreground_activity(serial).await;
        if let Some(fg) = &fg {
            if fg.starts_with(&prefix) {
                return (true, Some(fg.clone()));
            }
        }
        if Instant::now() >= deadline {
            return (false, fg);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn emit_summary(
    package: &str,
    args: &AppInstallArgs,
    reinstall: bool,
    steps: Vec<Step>,
    front_activity: Option<String>,
) -> Result<()> {
    let ok = steps.iter().all(|s| s.ok);
    println!(
        "{}",
        serde_json::json!({
            "type": "action",
            "cmd": if reinstall { "app_reinstall" } else { "app_install" },
            "package": package,
            "apk": args.apk.display().to_string(),
            "ok": ok,
            "steps": steps.iter().map(Step::to_json).collect::<Vec<_>>(),
            "front_activity": front_activity,
        })
    );
    Ok(())
}

fn is_signature_mismatch(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") || s.contains("signatures do not match")
}

// ── APK metadata via aapt2 ───────────────────────────────────────────────────

struct ApkMeta {
    package: String,
    launch_activity: Option<String>,
    permissions: Vec<String>,
}

fn resolve_meta(apk: &Path) -> Result<ApkMeta> {
    let aapt = find_aapt()
        .ok_or_else(|| anyhow!("aapt2/aapt not found on PATH or in the Android SDK build-tools"))?;
    let out = std::process::Command::new(&aapt)
        .arg("dump")
        .arg("badging")
        .arg(apk)
        .output()
        .with_context(|| format!("running {} dump badging", aapt.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_badging(&text).ok_or_else(|| anyhow!("no `package: name=` line in aapt2 badging output"))
}

/// Pull package, launchable-activity, and declared uses-permissions out of
/// `aapt2 dump badging` output.
fn parse_badging(badging: &str) -> Option<ApkMeta> {
    let mut package = None;
    let mut launch_activity = None;
    let mut permissions = Vec::new();
    for line in badging.lines() {
        let line = line.trim();
        if let Some(v) = badging_attr(line, "package: name=") {
            package = Some(v);
        } else if let Some(v) = badging_attr(line, "launchable-activity: name=") {
            launch_activity = Some(v);
        } else if let Some(v) = badging_attr(line, "uses-permission: name=") {
            permissions.push(v);
        }
    }
    package.map(|package| ApkMeta {
        package,
        launch_activity,
        permissions,
    })
}

/// Extract the `'...'`-quoted value following `<key>` on a badging line.
fn badging_attr(line: &str, key: &str) -> Option<String> {
    line.strip_prefix(key)
        .and_then(|rest| rest.strip_prefix('\''))
        .and_then(|rest| rest.split('\'').next())
        .map(str::to_string)
}

/// Locate an `aapt2` (or legacy `aapt`) binary: PATH first, then the newest
/// `build-tools/<ver>/aapt2` under any known SDK root.
fn find_aapt() -> Option<PathBuf> {
    for name in ["aapt2", "aapt"] {
        if let Some(p) = which(name) {
            return Some(p);
        }
    }
    for root in sdk_roots() {
        let Ok(entries) = std::fs::read_dir(root.join("build-tools")) else {
            continue;
        };
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("aapt2").is_file())
            .collect();
        versions.sort(); // lexical sort is good enough to prefer a recent version
        if let Some(latest) = versions.pop() {
            return Some(latest.join("aapt2"));
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn sdk_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(v) = std::env::var_os(var) {
            roots.push(PathBuf::from(v));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join("Library/Android/sdk")); // macOS
        roots.push(home.join("Android/Sdk")); // Linux
    }
    roots
}

/// Classify `am start`/`monkey` output. Both print unlocalized AOSP English;
/// failures announce themselves with an `Error…` line ("Error:", "Error type
/// 3", monkey's "** Error: …"), an exception ("java.lang.SecurityException:
/// …"), or monkey's "… monkey aborted" epitaph. Matching must be line-anchored
/// (not a bare substring test) so the echoed component in
/// "Starting: Intent { cmp=com.x/.ErrorDemoActivity }" doesn't read as a
/// failure.
fn launch_failed(out: &str) -> bool {
    out.lines().any(|line| {
        let l = line.trim_start().trim_start_matches("** ").trim_start();
        l.starts_with("Error") || l.contains("Exception:") || l.contains("monkey aborted")
    })
}

#[cfg(test)]
mod tests {
    use super::{launch_failed, parse_badging};

    #[test]
    fn launch_failure_is_line_anchored() {
        // Component names containing "Error" are not failures.
        assert!(!launch_failed(
            "Starting: Intent { cmp=com.example/.ErrorDemoActivity }\n"
        ));
        // Benign warnings are not failures.
        assert!(!launch_failed(
            "Warning: Activity not started, its current task has already been brought to the front\n"
        ));
        assert!(launch_failed(
            "Starting: Intent { cmp=com.example/.Main }\nError type 3\nError: Activity class {com.example/.Main} does not exist.\n"
        ));
        assert!(launch_failed(
            "java.lang.SecurityException: Permission Denial: starting Intent\n"
        ));
        // Monkey's no-launcher failure has no Error/Exception wording at all.
        assert!(launch_failed(
            "** No activities found to run, monkey aborted.\n"
        ));
        assert!(launch_failed("** Error: Unable to launch\n"));
        assert!(!launch_failed(
            "Events injected: 1\n## Network stats: elapsed time=32ms\n"
        ));
    }

    #[test]
    fn parses_badging_fields() {
        let out = "package: name='com.livd' versionCode='149' versionName='3.0.49'\n\
                   uses-permission: name='android.permission.INTERNET'\n\
                   uses-permission: name='android.permission.ACCESS_FINE_LOCATION'\n\
                   launchable-activity: name='com.livdapp.client.MainActivity'  label=''";
        let m = parse_badging(out).unwrap();
        assert_eq!(m.package, "com.livd");
        assert_eq!(
            m.launch_activity.as_deref(),
            Some("com.livdapp.client.MainActivity")
        );
        assert_eq!(
            m.permissions,
            vec![
                "android.permission.INTERNET".to_string(),
                "android.permission.ACCESS_FINE_LOCATION".to_string(),
            ]
        );
        assert!(parse_badging("no package line").is_none());
    }
}
