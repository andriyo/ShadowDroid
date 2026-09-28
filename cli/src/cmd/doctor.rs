//! `shadowdroid doctor [--fix] [--force] [--json]` — diagnose (and optionally
//! repair) the host↔device pipe.
//!
//! Setup failures are where Android tooling taxes people most: an offline
//! device, a missing/mismatched APK, a dropped `adb forward`, a stuck
//! instrumentation, or a *competing* UiAutomation owner (openatx, a stale
//! `app_process`) holding the single device-wide slot. `doctor` aggregates the
//! read-only probes ShadowDroid already performs internally into one report;
//! `--fix` invokes the remediation that [crate::device::installer] and
//! [crate::device::adb] already implement.
//!
//! Pure-read by design: `gather` NEVER starts the server (that is exactly what
//! it diagnoses). Only `--fix` mutates device state, and killing a *foreign*
//! UiAutomation owner is gated behind `--force` — we don't kill processes we
//! didn't spawn without explicit consent.

use crate::ids::Serial;
use anyhow::Result;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::studio;
use crate::device::adb;
use crate::device::installer::{
    self, APP_PACKAGE, DEFAULT_PORT, EXPECTED_APK_VERSION, INSTRUMENT_LOG_PATH, TEST_PACKAGE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "⚠",
            Status::Fail => "✗",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Machine-stable identifier (`adb`, `device`, `apk`, `server`, `owners`).
    pub code: &'static str,
    pub status: Status,
    pub detail: String,
    /// What `--fix` would do, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

/// How the device-wide UiAutomation slot is currently occupied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerClass {
    /// Nothing holding it.
    None,
    /// Only ShadowDroid's own (possibly stuck) instrumentation.
    OursOnly,
    /// A non-ShadowDroid owner (openatx/uiautomator2, atx, foreign app_process).
    Foreign,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub target: Option<Serial>,
    pub checks: Vec<Check>,
    /// Every non-advisory check is `ok`. Advisory checks (see [`ADVISORY_CODES`])
    /// describe optional capabilities, not the driving pipe, and do not gate this
    /// flag — see [`is_healthy`].
    pub healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<bool>,
}

/// Checks whose status must NOT gate `healthy`: they describe optional
/// capabilities layered on top of the host↔device pipe, not the pipe itself.
/// A missing Android Studio debugger, an app that can't be MITM'd, or an
/// un-wired in-app agent shouldn't read as "device pipe broken" to an agent.
const ADVISORY_CODES: &[&str] = &[
    "studio",
    "studio_plugin",
    "debugger_bridge",
    "net_app",
    "agent",
    // Clock drift matters for TLS and tokens but not for the pipe, and
    // `--fix` cannot repair it.
    "clock",
    // Injected faults are deliberate; they change the device, not the pipe.
    "faults",
];

/// `healthy` iff every non-advisory check is `ok`. Single source of truth so the
/// checks appended after `gather` (net_app, agent) recompute health the same way
/// — previously each re-derived it as `all(ok)`, which dropped the advisory
/// exemption and let an advisory warning flip `healthy` to false.
fn is_healthy(checks: &[Check]) -> bool {
    checks
        .iter()
        .all(|c| c.status == Status::Ok || ADVISORY_CODES.contains(&c.code))
}

impl DoctorReport {
    fn from_checks(target: Option<Serial>, checks: Vec<Check>) -> Self {
        let healthy = is_healthy(&checks);
        Self {
            target,
            checks,
            healthy,
            fixed: None,
        }
    }
}

/// Run the read-only checks. Never starts the server or kills anything.
pub async fn gather(device: Option<&str>) -> DoctorReport {
    let mut checks = Vec::new();
    checks.extend(studio_checks());

    // ── C1: adb reachable + device inventory ───────────────────────────────
    let devices = match adb::list_devices_with_state().await {
        Ok(d) => d,
        Err(e) => {
            checks.push(Check {
                code: "adb",
                status: Status::Fail,
                detail: format!(
                    "adb server not reachable ({e}). Is `adb` on PATH and a device/emulator attached?"
                ),
                remedy: None,
            });
            return DoctorReport::from_checks(None, checks);
        }
    };
    if devices.is_empty() {
        checks.push(Check {
            code: "adb",
            status: Status::Warn,
            detail: "no devices attached. Start an emulator or plug in a phone.".into(),
            remedy: None,
        });
        return DoctorReport::from_checks(None, checks);
    }
    let unhealthy: Vec<_> = devices
        .iter()
        .filter(|(_, st)| st != "device")
        .map(|(s, st)| format!("{s} ({st})"))
        .collect();
    let target = resolve_target(device, &devices);
    // Another phone left unauthorized on the bus does not break the pipe to
    // the selected device; only the target's own state gates health.
    let target_usable = target.as_ref().is_some_and(|serial| {
        devices
            .iter()
            .any(|(s, st)| s == serial.as_str() && st == "device")
    });
    let inventory = devices
        .iter()
        .map(|(s, st)| format!("{s} [{st}]"))
        .collect::<Vec<_>>()
        .join(", ");
    checks.push(Check {
        code: "device",
        status: if unhealthy.is_empty() || target_usable {
            Status::Ok
        } else {
            Status::Warn
        },
        detail: if unhealthy.is_empty() {
            format!("{} device(s): {inventory}", devices.len())
        } else {
            format!(
                "{inventory} — unhealthy: {}. `unauthorized` → accept the RSA prompt on the device; `offline` → reconnect/replug.",
                unhealthy.join(", ")
            )
        },
        remedy: None,
    });

    // ── Resolve the target serial for device-specific checks ────────────────
    let Some(serial) = target.clone() else {
        checks.push(Check {
            code: "apk",
            status: Status::Warn,
            detail: "skipped: multiple devices and none selected. Pass --device <serial>.".into(),
            remedy: None,
        });
        return DoctorReport::from_checks(None, checks);
    };

    // ── C2: ShadowDroid APK installed + version ─────────────────────────────
    checks.push(apk_check(&serial).await);

    // ── C3: server reachable (forward + probe, never start) ─────────────────
    let (server, reachable) = server_check(&serial).await;
    checks.push(server);

    // ── C4: UiAutomation slot owners ────────────────────────────────────────
    checks.push(owners_check(&serial, reachable).await);

    // ── C5: device clock vs host (drift breaks TLS, tokens, toast capture) ───
    checks.push(clock_check(&serial).await);

    // ── C6: net proxy state (a dangling http_proxy silently breaks networking)
    checks.push(net_check(&serial).await);

    // ── C7: injected faults still changing the device ───────────────────────
    checks.push(faults_check(&serial));

    DoctorReport::from_checks(Some(serial), checks)
}

/// `device` flag wins; otherwise the sole device in "device" state; else none.
fn resolve_target(device: Option<&str>, devices: &[(String, String)]) -> Option<Serial> {
    if let Some(d) = device {
        return Some(Serial::from(d));
    }
    let ready: Vec<_> = devices.iter().filter(|(_, st)| st == "device").collect();
    match ready.as_slice() {
        [(serial, _)] => Some(Serial::from(serial.as_str())),
        _ => None,
    }
}

async fn apk_check(serial: &Serial) -> Check {
    let main = adb::pm_path(serial, APP_PACKAGE).await.unwrap_or(None);
    let test = adb::pm_path(serial, TEST_PACKAGE).await.unwrap_or(None);
    if main.is_none() || test.is_none() {
        let missing = match (main.is_none(), test.is_none()) {
            (true, true) => "both APKs",
            (true, false) => "main APK",
            _ => "test APK",
        };
        return Check {
            code: "apk",
            status: Status::Fail,
            detail: format!("{missing} not installed."),
            remedy: Some("--fix installs the matching APK pair".into()),
        };
    }
    // The androidTest package carries no versionName (always reports null), so
    // the main package's version is authoritative here; whether the *running*
    // server is the right version is validated separately by the server check.
    let main_v = adb::pm_version(serial, APP_PACKAGE).await.unwrap_or(None);
    if main_v.as_deref() != Some(EXPECTED_APK_VERSION) {
        Check {
            code: "apk",
            status: Status::Warn,
            detail: format!(
                "main APK version {} != expected {EXPECTED_APK_VERSION}.",
                main_v.as_deref().unwrap_or("?"),
            ),
            remedy: Some("--fix reinstalls the matching APK pair".into()),
        }
    } else {
        Check {
            code: "apk",
            status: Status::Ok,
            detail: format!("installed, version {EXPECTED_APK_VERSION}"),
            remedy: None,
        }
    }
}

/// Probes the already-established forward without mutating lifecycle state.
/// Returns
/// `(check, reachable)` where `reachable` means the server answered at all
/// (regardless of version). A not-yet-started server is a `warn`, not a `fail` —
/// `shadowdroid connect` is the normal way to start it.
async fn server_check(serial: &Serial) -> (Check, bool) {
    let Ok(Some(client)) = installer::probe_existing(serial, true).await else {
        return (
            Check {
                code: "server",
                status: Status::Warn,
                detail: "no reachable server on the existing adb forward".into(),
                remedy: Some("run `shadowdroid connect` to establish or repair the server".into()),
            },
            false,
        );
    };
    match client.state().await {
        Ok(state) if state.server_version == EXPECTED_APK_VERSION => (
            Check {
                code: "server",
                status: Status::Ok,
                detail: format!(
                    "reachable on :{DEFAULT_PORT} (server {}, UIA {}, Android {}/SDK {})",
                    state.server_version,
                    state.ui_automator_version,
                    state.android_release,
                    state.android_sdk
                ),
                remedy: None,
            },
            true,
        ),
        Ok(state) => (
            Check {
                code: "server",
                status: Status::Warn,
                detail: format!(
                    "reachable but version {} != expected {EXPECTED_APK_VERSION}",
                    state.server_version
                ),
                remedy: Some("--fix reinstalls + restarts the server".into()),
            },
            // Reachable (answered) — the version warning already drives --fix
            // via the unhealthy report, so owners should read as "ours, up".
            true,
        ),
        Err(_) => (
            Check {
                code: "server",
                status: Status::Warn,
                detail: format!(
                    "not reachable on :{DEFAULT_PORT}. Run `shadowdroid connect` (or `doctor --fix`) to start it."
                ),
                remedy: Some("--fix runs the install + instrument lifecycle".into()),
            },
            false,
        ),
    }
}

async fn owners_check(serial: &Serial, reachable: bool) -> Check {
    let owners = adb::ps_ui_automation_owners(serial)
        .await
        .unwrap_or_default();
    match classify_owners(&owners) {
        OwnerClass::None => Check {
            code: "owners",
            status: Status::Ok,
            detail: "no competing UiAutomation owners".into(),
            remedy: None,
        },
        OwnerClass::OursOnly if reachable => Check {
            code: "owners",
            status: Status::Ok,
            detail: "ShadowDroid owns the UiAutomation slot".into(),
            remedy: None,
        },
        OwnerClass::OursOnly => Check {
            code: "owners",
            status: Status::Warn,
            detail: "a ShadowDroid instrumentation process is present but the server isn't responding — likely stuck.".into(),
            remedy: Some("--fix kills the stuck process and restarts".into()),
        },
        OwnerClass::Foreign => Check {
            code: "owners",
            status: Status::Fail,
            detail: format!(
                "a non-ShadowDroid UiAutomation owner is holding the slot:\n{}",
                indent(&holder_lines(&owners).collect::<Vec<_>>().join("\n"))
            ),
            remedy: Some(
                "--fix --force kills it and reclaims the slot (without --force we won't kill a process we didn't spawn)".into(),
            ),
        },
    }
}

/// Signatures of processes that hold (or launch a holder of) the UiAutomation
/// slot: instrumentation runs and UI Automator-based agents. `ps` also lists
/// every other `app_process` tool — scrcpy (`com.genymobile.scrcpy.Server`),
/// Android Studio's device mirroring (`com.android.tools.screensharing`),
/// short-lived `am`/`pm`/`cmd` invocations — which never hold the slot.
const UI_AUTOMATION_HOLDER_SIGNATURES: &[&str] = &[
    "instrument",
    "uiautomator",
    "androidx.test",
    "wetest",
    "atx",
];

/// `ps` lines (from [adb::ps_ui_automation_owners]) that can hold the slot.
fn holder_lines(owners: &str) -> impl Iterator<Item = &str> {
    owners.lines().filter(|line| {
        let line = line.to_ascii_lowercase();
        line.contains("shadowdroid")
            || UI_AUTOMATION_HOLDER_SIGNATURES
                .iter()
                .any(|signature| line.contains(signature))
    })
}

/// Classify `ps` output. Our own instrumentation lines always mention
/// `shadowdroid`; any other holder line is foreign.
fn classify_owners(owners: &str) -> OwnerClass {
    let lines: Vec<&str> = holder_lines(owners).collect();
    if lines.is_empty() {
        return OwnerClass::None;
    }
    if lines.iter().any(|l| !l.contains("shadowdroid")) {
        OwnerClass::Foreign
    } else {
        OwnerClass::OursOnly
    }
}

/// PIDs of foreign holders (`ps -o USER,PID,…`: PID is the second column).
fn foreign_holder_pids(owners: &str) -> Vec<u32> {
    holder_lines(owners)
        .filter(|line| !line.contains("shadowdroid"))
        .filter_map(|line| line.split_whitespace().nth(1)?.parse().ok())
        .collect()
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Allowed device↔host clock difference before we warn. The adb round-trip plus
/// `date +%s`'s one-second granularity add ~1s of measurement noise.
const CLOCK_TOLERANCE_SECS: i64 = 2;

/// Compare the device wall clock to the host's. Drift breaks TLS/cert validation
/// and token expiry in the app under test, and silently defeats ShadowDroid's
/// own toast capture (the CLI computes `since_ts` from the host clock while the
/// server timestamps toasts with the device clock). Read-only / warn-only:
/// device time isn't settable without root, so `--fix` can't repair it.
async fn clock_check(serial: &Serial) -> Check {
    // Sample the host clock right before the round-trip.
    let host = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let raw = match adb::shell(serial, "date +%s").await {
        Ok(s) => s,
        Err(e) => {
            return Check {
                code: "clock",
                status: Status::Warn,
                detail: format!("could not read device clock: {e}"),
                remedy: None,
            };
        }
    };
    let Ok(device) = raw.trim().parse::<i64>() else {
        return Check {
            code: "clock",
            status: Status::Warn,
            detail: format!("could not parse device time {:?}", raw.trim()),
            remedy: None,
        };
    };
    let skew = device - host; // positive ⇒ device ahead of host
    if skew.abs() <= CLOCK_TOLERANCE_SECS {
        Check {
            code: "clock",
            status: Status::Ok,
            detail: format!("device clock within {}s of host", skew.abs()),
            remedy: None,
        }
    } else {
        let dir = if skew > 0 { "ahead of" } else { "behind" };
        Check {
            code: "clock",
            status: Status::Warn,
            detail: format!(
                "device clock is ~{}s {dir} the host — can break TLS/cert validation, token expiry, and toast capture.",
                skew.abs()
            ),
            remedy: Some(
                "not auto-fixable: enable automatic date & time on the device, or cold-boot/resync the emulator".into(),
            ),
        }
    }
}

/// Entry point dispatched from `cli::run`.
pub async fn run(
    device: Option<&str>,
    fix: bool,
    force: bool,
    json: bool,
    app: Option<&str>,
    project: Option<&std::path::Path>,
    config: &crate::config::ShadowDroidConfig,
) -> Result<()> {
    let mut report = gather(device).await;

    if fix && !report.healthy {
        report = apply_fix(device, report, force).await;
    } else if fix {
        report.fixed = Some(false);
    }

    // `doctor --app <pkg>`: append the per-app interceptability verdict (`net
    // check`). Read-only, so it sits outside the fix flow.
    if let (Some(app), Some(serial)) = (app, report.target.clone()) {
        // Resolve a config alias (e.g. `Sample`) to its package before checking,
        // like every other app-taking command — otherwise `net check` looks up a
        // package literally named `Sample` and reports it "not installed".
        let package = config
            .resolve_app(Some(serial.as_str()), Some(app))
            .await
            .ok()
            .and_then(|r| r.package)
            .unwrap_or_else(|| app.to_string());
        let inspected = match crate::net::trust::TrustContext::resolve(config, &serial, false) {
            Ok(tctx) => crate::net::check::inspect(&serial, &package, &tctx).await,
            Err(e) => Err(e),
        };
        let check = match inspected {
            Ok(rep) => {
                let status = match rep.static_verdict.as_str() {
                    "interceptable" => Status::Ok,
                    "conditional" => Status::Warn,
                    _ => Status::Fail,
                };
                Check {
                    code: "net_app",
                    status,
                    detail: format!(
                        "{} — unverified; static heuristic: {} ({})",
                        rep.package, rep.static_verdict, rep.static_reason
                    ),
                    remedy: None,
                }
            }
            Err(e) => Check {
                code: "net_app",
                status: Status::Warn,
                detail: format!("net check {package}: {e}"),
                remedy: None,
            },
        };
        report.checks.push(check);
        report.healthy = is_healthy(&report.checks);
    }

    // `--project-root <path>` (or config `project`): append the in-app debug-agent
    // wiring status (the same thing `aar status` reports). Source-side and
    // read-only — independent of the device, sits outside the fix flow.
    if let Some(project) = project {
        let check = match crate::cmd::aar::inspect(project, None) {
            Ok(s) if s.installed => Check {
                code: "agent",
                status: Status::Ok,
                detail: format!("in-app debug agent wired into :{} ({})", s.module, s.app),
                remedy: None,
            },
            Ok(s) => Check {
                code: "agent",
                status: Status::Warn,
                detail: format!(
                    "in-app debug agent not installed in {} (module :{}): dependency {}, aar {}",
                    s.app,
                    s.module,
                    if s.dependency_present {
                        "present"
                    } else {
                        "missing"
                    },
                    if s.aar_present { "present" } else { "missing" },
                ),
                remedy: Some("shadowdroid aar install".to_string()),
            },
            Err(e) => Check {
                code: "agent",
                status: Status::Warn,
                detail: format!("agent status for {}: {e}", project.display()),
                remedy: None,
            },
        };
        report.checks.push(check);
        report.healthy = is_healthy(&report.checks);
    }

    if !report.healthy {
        let next_actions = if fix {
            vec!["inspect detail.checks and follow each remaining remedy"]
        } else {
            vec!["shadowdroid doctor --fix --json"]
        };
        return Err(crate::diagnostic::DiagnosticError::new(
            "doctor_unhealthy",
            "doctor",
            if fix {
                "ShadowDroid remains unhealthy after the requested repair"
            } else {
                "one or more required ShadowDroid health checks failed"
            },
        )
        .detail(serde_json::to_value(&report)?)
        .next_actions(next_actions)
        .into());
    }
    if json {
        crate::events::emit_action("doctor", &serde_json::to_value(&report)?);
    } else {
        print_human(&report, fix);
    }
    Ok(())
}

/// Apply remediation, then re-gather so the report reflects the new state.
async fn apply_fix(device: Option<&str>, report: DoctorReport, force: bool) -> DoctorReport {
    let Some(serial) = report.target.clone() else {
        // Nothing device-specific to fix (no target resolved).
        let mut r = report;
        r.fixed = Some(false);
        return r;
    };

    // Net: wiring left by a stopped ShadowDroid proxy silently breaks the
    // device's networking. Restore the state that session recorded — never a
    // proxy or `adb reverse` ShadowDroid did not set up. Independent of the
    // UiAutomation slot, so do it first even if the fixes below get gated.
    if !crate::net::control::is_running(&serial).await {
        match crate::net::commands::recover_recorded_wiring(&serial).await {
            Ok(warnings) => {
                for warning in warnings {
                    eprintln!("doctor --fix: {warning}");
                }
            }
            Err(error) => eprintln!("doctor --fix: could not restore proxy wiring: {error:#}"),
        }
    }

    // Faults whose timer did not fire, or whose restore failed, are leftovers:
    // clear them. Active faults within their time are deliberate and stay.
    for line in crate::fault::clear_stale(&serial).await {
        eprintln!("doctor --fix: {line}");
    }

    // Re-read owners fresh: refuse to clobber a foreign owner without --force.
    let owners = adb::ps_ui_automation_owners(&serial)
        .await
        .unwrap_or_default();
    if classify_owners(&owners) == OwnerClass::Foreign && !force {
        eprintln!(
            "doctor --fix: a non-ShadowDroid UiAutomation owner is present. Re-run with --force \
             to kill it and reclaim the slot, or stop it yourself first."
        );
        // Re-gather so the report reflects the net clear above.
        let mut r = gather(device).await;
        r.fixed = Some(false);
        return r;
    }
    if classify_owners(&owners) == OwnerClass::Foreign && force {
        eprintln!("doctor --fix --force: stopping foreign UiAutomation owners…");
        let _ = adb::kill_processes(&serial, &foreign_holder_pids(&owners)).await;
    }

    // If the APK itself is wrong or missing, a still-running server pins the
    // stale install and ensure_ready's warm path ("server already up — reusing")
    // would skip the reinstall. Kill it first to force a cold bring-up. (Safe:
    // the foreign-owner guard above already ran, so this only kills our own /
    // --force-authorised processes.)
    let apk_broken = report
        .checks
        .iter()
        .any(|c| c.code == "apk" && c.status != Status::Ok);
    if apk_broken {
        adb::kill_instrument_zombies(&serial).await.ok();
    }

    // ensure_ready handles the whole lifecycle: kill zombies → (re)install if
    // the version/bytes differ → forward → am instrument → poll for readiness.
    eprintln!("doctor --fix: reclaiming the device and (re)starting the server…");
    match installer::ensure_ready(&serial, None, false).await {
        Ok(_) => {
            let mut r = gather(device).await;
            r.fixed = Some(true);
            r
        }
        Err(e) => {
            eprintln!("doctor --fix: bring-up failed: {e}");
            eprintln!("Inspect the on-device log: `adb shell cat {INSTRUMENT_LOG_PATH}`");
            let mut r = gather(device).await;
            r.fixed = Some(false);
            r
        }
    }
}

/// Checks `--fix` can actually repair. The rest (`device` offline/unauthorized,
/// `clock` drift) are advisory — surfaced, but not something we auto-fix.
fn is_fixable(code: &str) -> bool {
    matches!(code, "apk" | "server" | "owners" | "net" | "faults")
}

/// Faults `fault inject` left on the device. Active ones are reported as
/// deliberate; overdue (their --duration-ms timer never ran) or
/// restore-failed ones are leftovers `--fix` clears.
fn faults_check(serial: &Serial) -> Check {
    let summary = crate::fault::summary(serial.as_str());
    if summary.active.is_empty() && summary.stale.is_empty() {
        return Check {
            code: "faults",
            status: Status::Ok,
            detail: "no injected faults".into(),
            remedy: None,
        };
    }
    if !summary.stale.is_empty() {
        return Check {
            code: "faults",
            status: Status::Warn,
            detail: format!(
                "{} injected fault(s) left behind (overdue or not restored): {}",
                summary.stale.len(),
                summary.stale.join(", ")
            ),
            remedy: Some("clear them with `fault clear <id>` (doctor --fix does)".into()),
        };
    }
    Check {
        code: "faults",
        status: Status::Warn,
        detail: format!(
            "{} injected fault(s) active: {}",
            summary.active.len(),
            summary.active.join(", ")
        ),
        remedy: Some("`fault clear --all` when the experiment is done".into()),
    }
}

/// Package-agnostic `net` proxy state. The headline value is catching wiring
/// left by a stopped ShadowDroid proxy session (the device still points at a
/// proxy nobody runs), which silently breaks networking; `--fix` restores the
/// state that session recorded. A proxy ShadowDroid did not set up — Charles,
/// a corporate proxy — is reported but never flagged or touched.
async fn net_check(serial: &Serial) -> Check {
    let running = crate::net::control::is_running(serial).await;
    let recorded = crate::net::commands::has_recorded_wiring(serial).unwrap_or(false);
    let http_proxy = adb::shell(serial, "settings get global http_proxy")
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "null" && s != ":0");
    net_check_from(http_proxy, running, recorded)
}

fn net_check_from(http_proxy: Option<String>, running: bool, recorded: bool) -> Check {
    match (http_proxy, running, recorded) {
        (Some(hp), true, _) => Check {
            code: "net",
            status: Status::Ok,
            detail: format!("proxy active; device http_proxy={hp}"),
            remedy: None,
        },
        (Some(hp), false, true) => Check {
            code: "net",
            status: Status::Warn,
            detail: format!(
                "a stopped ShadowDroid proxy left the device pointed at http_proxy={hp} — this silently breaks the device's networking."
            ),
            remedy: Some("restore it with `shadowdroid net stop` (or `doctor --fix`)".into()),
        },
        (Some(hp), false, false) => Check {
            code: "net",
            status: Status::Ok,
            detail: format!("device http_proxy={hp} was not set by ShadowDroid; left unchanged."),
            remedy: None,
        },
        (None, true, _) => Check {
            code: "net",
            status: Status::Warn,
            detail: "a net proxy daemon is running but the device isn't pointed at it.".into(),
            remedy: Some("`net start` to wire it up, or `net stop` to shut the daemon down".into()),
        },
        (None, false, _) => Check {
            code: "net",
            status: Status::Ok,
            detail: "inactive.".into(),
            remedy: None,
        },
    }
}

fn studio_checks() -> Vec<Check> {
    let mut checks = Vec::new();
    match studio::status_report(None) {
        Ok(report) => {
            if report.android_studios.is_empty() {
                checks.push(Check {
                    code: "studio",
                    status: Status::Warn,
                    detail: "Android Studio was not detected.".into(),
                    remedy: Some("run `shadowdroid init` after installing Android Studio, or configure android_studio in .shadowdroid/config.json".into()),
                });
            } else {
                let installed = report
                    .android_studios
                    .iter()
                    .filter(|studio| studio.shadowdroid_plugin_installed)
                    .count();
                checks.push(Check {
                    code: "studio",
                    status: Status::Ok,
                    detail: format!(
                        "{} Android Studio install(s), ShadowDroid plugin installed in {installed}",
                        report.android_studios.len()
                    ),
                    remedy: None,
                });
                if installed == 0 {
                    checks.push(Check {
                        code: "studio_plugin",
                        status: Status::Warn,
                        detail: "ShadowDroid Android Studio plugin is not installed.".into(),
                        remedy: Some(
                            "run `shadowdroid init` to install the plugin and skills".into(),
                        ),
                    });
                } else if installed < report.android_studios.len() {
                    checks.push(Check {
                        code: "studio_plugin",
                        status: Status::Warn,
                        detail: "ShadowDroid plugin is installed in only some detected Android Studio installs.".into(),
                        remedy: Some("run `shadowdroid studio install --studio <path>` for the Android Studio you use".into()),
                    });
                } else {
                    checks.push(Check {
                        code: "studio_plugin",
                        status: Status::Ok,
                        detail: "ShadowDroid Android Studio plugin installed".into(),
                        remedy: None,
                    });
                }
            }

            if report.bridge.running {
                checks.push(Check {
                    code: "debugger_bridge",
                    status: Status::Ok,
                    detail: format!(
                        "registered at {}",
                        report.bridge.url.as_deref().unwrap_or("unknown URL")
                    ),
                    remedy: None,
                });
            } else if report.bridge.present {
                checks.push(Check {
                    code: "debugger_bridge",
                    status: Status::Warn,
                    detail: "bridge registry exists, but the recorded Android Studio process is not running.".into(),
                    remedy: Some("restart Android Studio and open an Android project; then run `shadowdroid debug status`".into()),
                });
            } else {
                checks.push(Check {
                    code: "debugger_bridge",
                    status: Status::Warn,
                    detail: "debugger bridge is not registered.".into(),
                    remedy: Some("run `shadowdroid init`, restart Android Studio, and open an Android project".into()),
                });
            }
        }
        Err(err) => checks.push(Check {
            code: "studio",
            status: Status::Warn,
            detail: format!("could not inspect Android Studio: {err}"),
            remedy: Some("run `shadowdroid init` to retry setup".into()),
        }),
    }
    checks
}

fn print_human(report: &DoctorReport, fix: bool) {
    for c in &report.checks {
        println!("{} [{}] {}", c.status.glyph(), c.code, c.detail);
        // Show the remedy for anything not OK — including issues that survived
        // a --fix run, so the user sees what's left to do manually.
        if c.status != Status::Ok
            && let Some(remedy) = &c.remedy
        {
            println!("    → {remedy}");
        }
    }
    let fixable_remaining = report
        .checks
        .iter()
        .any(|c| c.status != Status::Ok && is_fixable(c.code));
    match (report.healthy, fix) {
        (true, true) if report.fixed == Some(true) => println!("\n✓ fixed — all checks pass."),
        (true, _) => println!("\n✓ all checks pass."),
        (false, true) if fixable_remaining => {
            println!("\n✗ issues remain after --fix (see above). Some may need --force.")
        }
        (false, true) => {
            println!("\n⚠ remaining issues aren't auto-fixable — see the remedies above.")
        }
        (false, false) if fixable_remaining => {
            println!("\nRun `shadowdroid doctor --fix` to attempt repairs.")
        }
        (false, false) => {
            println!("\n⚠ issues aren't auto-fixable by --fix — see the remedies above.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(code: &'static str, status: Status) -> Check {
        Check {
            code,
            status,
            detail: String::new(),
            remedy: None,
        }
    }

    const SCRCPY: &str = "shell 4100 1 app_process /system/bin com.genymobile.scrcpy.Server 3.1";
    const MIRRORING: &str =
        "shell 4200 1 app_process /system/bin com.android.tools.screensharing.Main --socket=screen";
    const APPIUM: &str =
        "u0_a150 4300 1 io.appium.uiautomator2.server.test io.appium.uiautomator2.server.test";
    const ESPRESSO: &str = "shell 4400 1 app_process /system/bin com.android.commands.am.Am instrument -w com.example.test/androidx.test.runner.AndroidJUnitRunner";
    const OURS: &str = "shell 4500 1 app_process io.github.andriyo.shadowdroid.test/androidx.test.runner.AndroidJUnitRunner";

    #[test]
    fn screen_mirroring_tools_are_not_ui_automation_owners() {
        let tools = [SCRCPY, MIRRORING].join("\n");
        assert_eq!(classify_owners(&tools), OwnerClass::None);
        assert!(foreign_holder_pids(&tools).is_empty());
        assert_eq!(
            classify_owners(&[SCRCPY, OURS].join("\n")),
            OwnerClass::OursOnly
        );
        let mixed = [SCRCPY, APPIUM, ESPRESSO, MIRRORING, OURS].join("\n");
        assert_eq!(classify_owners(&mixed), OwnerClass::Foreign);
        // --fix --force kills only the foreign holders.
        assert_eq!(foreign_holder_pids(&mixed), vec![4300, 4400]);
    }

    #[test]
    fn only_proxy_wiring_recorded_by_shadowdroid_is_flagged() {
        let foreign = net_check_from(Some("10.0.2.2:8888".into()), false, false);
        assert_eq!(foreign.status, Status::Ok, "{}", foreign.detail);
        let leftover = net_check_from(Some("localhost:8080".into()), false, true);
        assert_eq!(leftover.status, Status::Warn);
        assert_eq!(
            net_check_from(Some("localhost:8080".into()), true, true).status,
            Status::Ok
        );
        assert_eq!(net_check_from(None, false, false).status, Status::Ok);
    }

    #[test]
    fn clock_drift_is_advisory() {
        assert!(is_healthy(&[
            check("device", Status::Ok),
            check("server", Status::Ok),
            check("clock", Status::Warn),
        ]));
    }

    #[test]
    fn advisory_warnings_do_not_flip_healthy() {
        // A core check failing → unhealthy.
        assert!(!is_healthy(&[
            check("server", Status::Ok),
            check("owners", Status::Fail),
        ]));
        // Advisory checks (net_app conditional, agent not-installed, idle bridge)
        // warning/failing while the core pipe is ok → still healthy. This is the
        // regression: configuring a project used to flip healthy to false purely
        // because the optional AAR wasn't installed.
        assert!(is_healthy(&[
            check("device", Status::Ok),
            check("apk", Status::Ok),
            check("server", Status::Ok),
            check("owners", Status::Ok),
            check("net_app", Status::Warn),
            check("agent", Status::Warn),
            check("debugger_bridge", Status::Warn),
        ]));
        // Even a hard net_app fail (app not interceptable) is advisory.
        assert!(is_healthy(&[
            check("server", Status::Ok),
            check("net_app", Status::Fail),
        ]));
    }

    #[test]
    fn classifies_owners() {
        assert_eq!(classify_owners(""), OwnerClass::None);
        assert_eq!(classify_owners("   \n  \n"), OwnerClass::None);
        assert_eq!(
            classify_owners("shell 1234 1 app_process io.github.andriyo.shadowdroid.test/..."),
            OwnerClass::OursOnly
        );
        assert_eq!(
            classify_owners("u0_a99 555 1 app_process com.wetest.uia2.Main"),
            OwnerClass::Foreign
        );
        // mixed → foreign (something else is also holding it)
        assert_eq!(
            classify_owners(
                "shell 1 1 app_process ...shadowdroid...\nu0_a99 2 1 app_process com.wetest.uia2.Main"
            ),
            OwnerClass::Foreign
        );
    }
}
