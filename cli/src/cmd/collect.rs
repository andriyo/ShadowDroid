//! `shadowdroid collect --app PKG [-o DIR] [--no-screenshot]` — gather a
//! self-contained diagnostic bundle: the agent's "I give up" artifact.
//!
//! Fans out over reads that already exist — `doctor` diagnostics, `getprop`
//! device info, recent logcat (+ the crash buffer), and (if the server is up)
//! a screen dump, screenshot, current activity, and app info — into a directory
//! plus a `collect.json` manifest. More useful day-to-day than a full
//! `adb bugreport`, and it **degrades gracefully**: if the on-device server
//! can't start, the host-side diagnostics (logs, device info, doctor report)
//! are still captured.
//!
//! Privacy: the bundle is written locally and never uploaded. Screenshots and
//! logs may contain PII — treat the directory accordingly before sharing it.

use crate::ids::Serial;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::doctor;
use crate::device::{adb, installer};

#[derive(Debug, Clone, serde::Serialize)]
struct ArtifactPrivacy {
    name: String,
    kind: String,
    redaction: String,
    potentially_sensitive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    regions_redacted: Option<usize>,
}

/// Accumulates files written under one bundle directory, tracking which
/// captures succeeded and which failed (failures are non-fatal).
struct Bundle {
    dir: PathBuf,
    captured: Vec<String>,
    errors: Vec<String>,
    artifacts: Vec<ArtifactPrivacy>,
    policy: Option<crate::redaction::Policy>,
}

impl Bundle {
    fn new(dir: PathBuf, policy: Option<crate::redaction::Policy>) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create bundle dir {}", dir.display()))?;
        Ok(Self {
            dir,
            captured: Vec::new(),
            errors: Vec::new(),
            artifacts: Vec::new(),
            policy,
        })
    }

    fn record_artifact(
        &mut self,
        name: &str,
        kind: &str,
        redaction: &str,
        potentially_sensitive: bool,
        regions_redacted: Option<usize>,
    ) {
        self.artifacts.push(ArtifactPrivacy {
            name: name.to_string(),
            kind: kind.to_string(),
            redaction: redaction.to_string(),
            potentially_sensitive,
            regions_redacted,
        });
    }

    fn write_text(&mut self, name: &str, content: &str) {
        self.write_text_kind(name, content, "text");
    }

    fn write_text_kind(&mut self, name: &str, content: &str, kind: &str) {
        let (content, redaction, potentially_sensitive) = match &self.policy {
            Some(policy) => (policy.redact_text(content), policy.label(), false),
            None => (content.to_string(), "not_requested", true),
        };
        match std::fs::write(self.dir.join(name), content) {
            Ok(()) => {
                self.captured.push(name.to_string());
                self.record_artifact(name, kind, redaction, potentially_sensitive, None);
            }
            Err(e) => self.errors.push(format!("{name}: write failed: {e}")),
        }
    }

    fn write_screenshot(
        &mut self,
        name: &str,
        bytes: &[u8],
        report: Option<&crate::redaction::PixelRedactionReport>,
    ) {
        match std::fs::write(self.dir.join(name), bytes) {
            Ok(()) => {
                self.captured.push(name.to_string());
                self.record_artifact(
                    name,
                    "screenshot",
                    report.map_or("not_requested", |report| report.method),
                    true,
                    report.map(|report| report.regions_redacted),
                );
            }
            Err(e) => self.errors.push(format!("{name}: write failed: {e}")),
        }
    }

    fn write_json(&mut self, name: &str, value: &serde_json::Value) {
        let value = self
            .policy
            .as_ref()
            .map(|policy| policy.redact_json_value(value))
            .unwrap_or_else(|| value.clone());
        match serde_json::to_string_pretty(&value) {
            Ok(s) => self.write_text_kind(name, &s, "json"),
            Err(e) => self.errors.push(format!("{name}: serialize failed: {e}")),
        }
    }
}

pub async fn run(
    serial: &Serial,
    app: Option<String>,
    out: Option<PathBuf>,
    screenshot: bool,
    redact_screenshots: bool,
) -> Result<()> {
    if redact_screenshots && !crate::redaction::is_enabled() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "screenshot_redaction_not_enabled",
            "input",
            "--redact-screenshots requires the global --redact flag or redaction.enabled=true",
        )
        .next_actions(["rerun with --redact --redact-screenshots"])
        .into());
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = out.unwrap_or_else(|| std::env::temp_dir().join(format!("shadowdroid-collect-{ts}")));
    let mut bundle = Bundle::new(dir, crate::redaction::active_policy())?;

    // ── always-available host-side diagnostics ──────────────────────────────
    let diag = doctor::gather(Some(serial)).await;
    let healthy = diag.healthy;
    let diag_value = serde_json::to_value(&diag).unwrap_or(serde_json::Value::Null);
    bundle.write_json("diagnostics.json", &diag_value);

    bundle.write_json("device_info.json", &adb::device_info(serial).await);
    bundle.write_text(
        "logcat.txt",
        &adb::recent_logcat(serial, 500).await.join("\n"),
    );
    if let Ok(crash) = adb::shell(serial, "logcat -d -b crash -t 200").await
        && !crash.trim().is_empty()
    {
        bundle.write_text("logcat_crash.txt", &crash);
    }
    if let Ok(owners) = adb::ps_ui_automation_owners(serial).await {
        let body = if owners.is_empty() {
            "(none)"
        } else {
            owners.as_str()
        };
        bundle.write_text("ui_automation_owners.txt", body);
    }

    // ── server-backed captures (best-effort; degrade if unavailable) ─────────
    // any_apk_version=true: collect wants whatever server is already up (the v1
    // API is stable across versions) — it should never force a reinstall just to
    // grab a screen dump. `doctor --fix` is the verb that enforces the version.
    let client = match installer::ensure_ready(serial, None, true).await {
        Ok(c) => Some(c),
        Err(e) => {
            bundle.errors.push(format!("server unavailable: {e}"));
            eprintln!("collect: server unavailable ({e}); capturing host-side diagnostics only");
            None
        }
    };
    let server_ok = client.is_some();
    let mut captured_screen = None;
    if let Some(client) = &client {
        match client.state().await {
            Ok(s) => bundle.write_json("state.json", &serde_json::to_value(&s).unwrap_or_default()),
            Err(e) => bundle.errors.push(format!("state: {e}")),
        }
        match client.app_current().await {
            Ok(a) => bundle.write_json(
                "current.json",
                &serde_json::to_value(&a).unwrap_or_default(),
            ),
            Err(e) => bundle.errors.push(format!("current: {e}")),
        }
        match client.screen().await {
            Ok(s) => {
                bundle.write_json("screen.json", &serde_json::to_value(&s).unwrap_or_default());
                captured_screen = Some(s);
            }
            Err(e) => bundle.errors.push(format!("screen: {e}")),
        }
        if screenshot {
            match client.screenshot_png().await {
                Ok(bytes) if redact_screenshots => {
                    if let Some(screen) = &captured_screen {
                        match crate::redaction::redact_png_if_active(&bytes, screen) {
                            Ok((bytes, report)) => {
                                bundle.write_screenshot("screenshot.png", &bytes, Some(&report))
                            }
                            Err(e) => bundle.errors.push(format!(
                                "screenshot redaction failed; screenshot omitted: {e}"
                            )),
                        }
                    } else {
                        bundle.errors.push(
                            "screenshot redaction needs screen.json; screenshot omitted".into(),
                        );
                    }
                }
                Ok(bytes) => bundle.write_screenshot("screenshot.png", &bytes, None),
                Err(e) => bundle.errors.push(format!("screenshot: {e}")),
            }
        }
        if let Some(pkg) = &app {
            match client.app_info(pkg).await {
                Ok(info) => bundle.write_json(
                    "app_info.json",
                    &serde_json::json!({
                        "package": pkg,
                        "version_name": info.version_name,
                        "version_code": info.version_code,
                        "label": info.label,
                    }),
                ),
                Err(e) => bundle.errors.push(format!("app_info: {e}")),
            }
        }
    }

    // ── manifest + agent-facing summary line ─────────────────────────────────
    let manifest = serde_json::json!({
        "type": "collect",
        "ts": ts,
        "device": serial,
        "app": app,
        "server_ok": server_ok,
        "captured": bundle.captured,
        "errors": bundle.errors,
        "redaction": bundle.policy.as_ref().map(|policy| serde_json::json!({
            "enabled": true,
            "policy": policy.label(),
            "version": crate::redaction::POLICY_VERSION,
            "screenshot_pixels_requested": redact_screenshots,
        })).unwrap_or_else(|| serde_json::json!({
            "enabled": false,
            "screenshot_pixels_requested": false,
        })),
        "artifacts": bundle.artifacts,
        "diagnostics": diag_value,
    });
    bundle.write_json("collect.json", &manifest);

    crate::events::emit_action(
        "collect",
        &serde_json::json!({
            "bundle": bundle.dir.display().to_string(),
            "server_ok": server_ok,
            "healthy": healthy,
            "captured": bundle.captured,
            "errors": bundle.errors,
            "redaction_enabled": bundle.policy.is_some(),
            "redaction_policy": bundle.policy.as_ref().map(|policy| policy.label()),
            "artifacts": bundle.artifacts,
        }),
    );
    Ok(())
}
