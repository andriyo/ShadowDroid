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

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cmd::doctor;
use crate::device::{adb, installer};

/// Accumulates files written under one bundle directory, tracking which
/// captures succeeded and which failed (failures are non-fatal).
struct Bundle {
    dir: PathBuf,
    captured: Vec<String>,
    errors: Vec<String>,
}

impl Bundle {
    fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create bundle dir {}", dir.display()))?;
        Ok(Self {
            dir,
            captured: Vec::new(),
            errors: Vec::new(),
        })
    }

    fn write_text(&mut self, name: &str, content: &str) {
        match std::fs::write(self.dir.join(name), content) {
            Ok(()) => self.captured.push(name.to_string()),
            Err(e) => self.errors.push(format!("{name}: write failed: {e}")),
        }
    }

    fn write_bytes(&mut self, name: &str, bytes: &[u8]) {
        match std::fs::write(self.dir.join(name), bytes) {
            Ok(()) => self.captured.push(name.to_string()),
            Err(e) => self.errors.push(format!("{name}: write failed: {e}")),
        }
    }

    fn write_json(&mut self, name: &str, value: &serde_json::Value) {
        match serde_json::to_string_pretty(value) {
            Ok(s) => self.write_text(name, &s),
            Err(e) => self.errors.push(format!("{name}: serialize failed: {e}")),
        }
    }
}

pub async fn run(
    serial: &str,
    app: Option<String>,
    out: Option<PathBuf>,
    screenshot: bool,
) -> Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = out.unwrap_or_else(|| std::env::temp_dir().join(format!("shadowdroid-collect-{ts}")));
    let mut bundle = Bundle::new(dir)?;

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
    if let Ok(crash) = adb::shell(serial, "logcat -d -b crash -t 200").await {
        if !crash.trim().is_empty() {
            bundle.write_text("logcat_crash.txt", &crash);
        }
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
                bundle.write_json("screen.json", &serde_json::to_value(&s).unwrap_or_default())
            }
            Err(e) => bundle.errors.push(format!("screen: {e}")),
        }
        if screenshot {
            match client.screenshot_png().await {
                Ok(bytes) => bundle.write_bytes("screenshot.png", &bytes),
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
        "diagnostics": diag_value,
    });
    bundle.write_json("collect.json", &manifest);

    println!(
        "{}",
        serde_json::json!({
            "type": "action",
            "cmd": "collect",
            "bundle": bundle.dir.display().to_string(),
            "server_ok": server_ok,
            "healthy": healthy,
            "captured": bundle.captured,
            "errors": bundle.errors,
        })
    );
    Ok(())
}
