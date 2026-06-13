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

use anyhow::Result;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::studio;
use crate::device::adb;
use crate::device::client::ServerClient;
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
    pub target: Option<String>,
    pub checks: Vec<Check>,
    /// Every check is `ok`.
    pub healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<bool>,
}

impl DoctorReport {
    fn from_checks(target: Option<String>, checks: Vec<Check>) -> Self {
        let healthy = checks.iter().all(|c| c.status == Status::Ok);
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
    let inventory = devices
        .iter()
        .map(|(s, st)| format!("{s} [{st}]"))
        .collect::<Vec<_>>()
        .join(", ");
    checks.push(Check {
        code: "device",
        status: if unhealthy.is_empty() {
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
    let target = resolve_target(device, &devices);
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

    DoctorReport::from_checks(Some(serial), checks)
}

/// `device` flag wins; otherwise the sole device in "device" state; else none.
fn resolve_target(device: Option<&str>, devices: &[(String, String)]) -> Option<String> {
    if let Some(d) = device {
        return Some(d.to_string());
    }
    let ready: Vec<_> = devices.iter().filter(|(_, st)| st == "device").collect();
    match ready.as_slice() {
        [(serial, _)] => Some(serial.clone()),
        _ => None,
    }
}

async fn apk_check(serial: &str) -> Check {
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

/// Sets up the (idempotent) port forward, then probes `/v1/state` once. Returns
/// `(check, reachable)` where `reachable` means the server answered at all
/// (regardless of version). A not-yet-started server is a `warn`, not a `fail` —
/// `shadowdroid connect` is the normal way to start it.
async fn server_check(serial: &str) -> (Check, bool) {
    // The forward is required to reach the device server and is idempotent.
    adb::forward(serial, DEFAULT_PORT, DEFAULT_PORT).await.ok();
    let Ok(client) = ServerClient::new(DEFAULT_PORT) else {
        return (
            Check {
                code: "server",
                status: Status::Fail,
                detail: "could not build HTTP client for localhost port".into(),
                remedy: None,
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
                    state.server_version, state.ui_automator_version, state.android_release, state.android_sdk
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

async fn owners_check(serial: &str, reachable: bool) -> Check {
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
                indent(&owners)
            ),
            remedy: Some(
                "--fix --force kills it and reclaims the slot (without --force we won't kill a process we didn't spawn)".into(),
            ),
        },
    }
}

/// Classify `ps` output (from [adb::ps_ui_automation_owners]). Every matched
/// line contains one of `app_process|uiautomator|shadowdroid|wetest|atx`; our
/// own instrumentation lines always mention `shadowdroid`, so any line without
/// it is foreign.
fn classify_owners(owners: &str) -> OwnerClass {
    let lines: Vec<&str> = owners.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return OwnerClass::None;
    }
    if lines.iter().any(|l| !l.contains("shadowdroid")) {
        OwnerClass::Foreign
    } else {
        OwnerClass::OursOnly
    }
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
async fn clock_check(serial: &str) -> Check {
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
            }
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
pub async fn run(device: Option<&str>, fix: bool, force: bool, json: bool) -> Result<()> {
    let mut report = gather(device).await;

    if fix && !report.healthy {
        report = apply_fix(device, report, force).await;
    } else if fix {
        report.fixed = Some(false);
    }

    if json {
        println!("{}", serde_json::to_string(&report)?);
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

    // Re-read owners fresh: refuse to clobber a foreign owner without --force.
    let owners = adb::ps_ui_automation_owners(&serial)
        .await
        .unwrap_or_default();
    if classify_owners(&owners) == OwnerClass::Foreign && !force {
        eprintln!(
            "doctor --fix: a non-ShadowDroid UiAutomation owner is present. Re-run with --force \
             to kill it and reclaim the slot, or stop it yourself first."
        );
        let mut r = report;
        r.fixed = Some(false);
        return r;
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
    matches!(code, "apk" | "server" | "owners")
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
                    remedy: Some("run `shadowdroid init` after installing Android Studio, or configure android_studio in .shadowdroid.json".into()),
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
        if c.status != Status::Ok {
            if let Some(remedy) = &c.remedy {
                println!("    → {remedy}");
            }
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
