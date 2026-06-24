//! `profile-snapshot` / `profile-apply` / `profile-reset` — capture and restore
//! the device state that makes UI repros deterministic: animation scales, font
//! scale, density, size, rotation, and the stylus-handwriting tutorial.
//!
//! Host-only (`settings` + `wm` over `adb`). The headline use is the
//! `automation` preset (`profile-apply --preset automation`), which zeroes the
//! three animation scales (the single biggest flakiness fix for UI automation)
//! and disables the Android 14+ stylus-handwriting tutorial that otherwise
//! hijacks the first text-field focus. `connect` disables that tutorial
//! automatically too — see [disable_stylus_tutorial]. `profile-snapshot -o file`
//! then `profile-apply --file file` makes a run reproducible and restorable;
//! `profile-reset` returns to stock defaults.
//!
//! Verify-by-readback: every mutation is followed by a fresh `read` so the
//! emitted `now` reflects what the device actually reports (`adb shell` has no
//! exit code).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::device::adb;

/// The restorable display profile. All fields optional — a snapshot omits
/// whatever the device reports as unset, and an apply only touches present
/// fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Profile {
    #[serde(skip_serializing_if = "Option::is_none")]
    window_animation_scale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transition_animation_scale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    animator_duration_scale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    font_scale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    density: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accelerometer_rotation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_rotation: Option<String>,
    /// `secure stylus_handwriting_enabled` — the Android 14+ "Try out your
    /// stylus" tutorial pops on first text-field focus and hijacks `text` input.
    #[serde(skip_serializing_if = "Option::is_none")]
    stylus_handwriting: Option<String>,
}

/// Disable the stylus-handwriting tutorial that otherwise intercepts the first
/// text-field focus (and steals `text` input). Best-effort, idempotent, and a
/// no-op on devices/Android versions without the setting. Returns the observed
/// state. Called automatically by `connect` and folded into the `automation`
/// preset, so text input "just works" for the common workflow.
pub async fn disable_stylus_tutorial(serial: &str) -> bool {
    put_secure(serial, "stylus_handwriting_enabled", "0").await;
    get_secure(serial, "stylus_handwriting_enabled")
        .await
        .as_deref()
        == Some("0")
}

#[derive(clap::Args)]
pub struct ProfileApplyArgs {
    /// Named preset. `automation` zeroes the three animation scales.
    #[arg(long)]
    pub preset: Option<String>,
    /// Apply a profile previously written by `profile-snapshot -o`.
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// Set all three animation scales to this value (e.g. 0 or 1).
    #[arg(long)]
    pub animations: Option<f32>,
    /// Set the font scale.
    #[arg(long)]
    pub font_scale: Option<f32>,
    /// Set the display density (dpi).
    #[arg(long)]
    pub density: Option<u32>,
    /// Set the display size as WxH (e.g. 1080x2400).
    #[arg(long)]
    pub size: Option<String>,
    /// Pin rotation 0..3 (disables auto-rotate).
    #[arg(long)]
    pub rotation: Option<u8>,
}

// ── snapshot ─────────────────────────────────────────────────────────────────

pub async fn snapshot(serial: &str, out: Option<&PathBuf>) -> Result<()> {
    let profile = read_profile(serial).await;
    let value = serde_json::to_value(&profile)?;
    let saved = match out {
        Some(path) => {
            std::fs::write(path, serde_json::to_string_pretty(&profile)?)
                .with_context(|| format!("writing {}", path.display()))?;
            Some(path.display().to_string())
        }
        None => None,
    };
    emit(
        "profile_snapshot",
        serde_json::json!({ "profile": value, "saved": saved }),
    );
    Ok(())
}

// ── apply ────────────────────────────────────────────────────────────────────

pub async fn apply(serial: &str, args: &ProfileApplyArgs) -> Result<()> {
    let profile = profile_from_args(args)?;
    apply_profile(serial, &profile).await;
    let now = read_profile(serial).await;
    emit(
        "profile_apply",
        serde_json::json!({
            "applied": serde_json::to_value(&profile)?,
            "now": serde_json::to_value(&now)?,
        }),
    );
    Ok(())
}

fn profile_from_args(a: &ProfileApplyArgs) -> Result<Profile> {
    if let Some(file) = &a.file {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        return serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()));
    }

    let mut p = Profile::default();
    match a.preset.as_deref() {
        Some("automation") => {
            p.window_animation_scale = Some("0".into());
            p.transition_animation_scale = Some("0".into());
            p.animator_duration_scale = Some("0".into());
            // The stylus-handwriting tutorial breaks text input — disable it.
            p.stylus_handwriting = Some("0".into());
        }
        Some(other) => bail!("unknown preset '{other}' (known: automation)"),
        None => {}
    }
    if let Some(v) = a.animations {
        let s = v.to_string();
        p.window_animation_scale = Some(s.clone());
        p.transition_animation_scale = Some(s.clone());
        p.animator_duration_scale = Some(s);
    }
    if let Some(v) = a.font_scale {
        p.font_scale = Some(v.to_string());
    }
    if let Some(v) = a.density {
        p.density = Some(v.to_string());
    }
    if let Some(v) = &a.size {
        p.size = Some(v.clone());
    }
    if let Some(v) = a.rotation {
        // Pinning a rotation requires disabling auto-rotate first.
        p.accelerometer_rotation = Some("0".into());
        p.user_rotation = Some(v.to_string());
    }

    if is_empty(&p) {
        bail!("nothing to apply — pass --preset, --file, or one of --animations/--font-scale/--density/--size/--rotation");
    }
    Ok(p)
}

// ── reset ────────────────────────────────────────────────────────────────────

pub async fn reset(serial: &str) -> Result<()> {
    // wm overrides clear via their own `reset` subcommands; the rest go back to
    // stock defaults.
    let _ = adb::shell(serial, "wm size reset").await;
    let _ = adb::shell(serial, "wm density reset").await;
    let defaults = Profile {
        window_animation_scale: Some("1".into()),
        transition_animation_scale: Some("1".into()),
        animator_duration_scale: Some("1".into()),
        font_scale: Some("1.0".into()),
        accelerometer_rotation: Some("1".into()),
        user_rotation: Some("0".into()),
        density: None,
        size: None,
        stylus_handwriting: Some("1".into()),
    };
    apply_profile(serial, &defaults).await;
    let now = read_profile(serial).await;
    emit(
        "profile_reset",
        serde_json::json!({ "now": serde_json::to_value(&now)? }),
    );
    Ok(())
}

// ── read / write device state ────────────────────────────────────────────────

async fn read_profile(serial: &str) -> Profile {
    Profile {
        window_animation_scale: get_global(serial, "window_animation_scale").await,
        transition_animation_scale: get_global(serial, "transition_animation_scale").await,
        animator_duration_scale: get_global(serial, "animator_duration_scale").await,
        font_scale: get_system(serial, "font_scale").await,
        density: get_wm(serial, "density", "density:").await,
        size: get_wm(serial, "size", "size:").await,
        accelerometer_rotation: get_system(serial, "accelerometer_rotation").await,
        user_rotation: get_system(serial, "user_rotation").await,
        stylus_handwriting: get_secure(serial, "stylus_handwriting_enabled").await,
    }
}

async fn apply_profile(serial: &str, p: &Profile) {
    if let Some(v) = &p.window_animation_scale {
        put_global(serial, "window_animation_scale", v).await;
    }
    if let Some(v) = &p.transition_animation_scale {
        put_global(serial, "transition_animation_scale", v).await;
    }
    if let Some(v) = &p.animator_duration_scale {
        put_global(serial, "animator_duration_scale", v).await;
    }
    if let Some(v) = &p.font_scale {
        put_system(serial, "font_scale", v).await;
    }
    if let Some(v) = &p.density {
        let _ = adb::shell(serial, format!("wm density {v}")).await;
    }
    if let Some(v) = &p.size {
        let _ = adb::shell(serial, format!("wm size {v}")).await;
    }
    // Auto-rotate must be set before pinning a rotation.
    if let Some(v) = &p.accelerometer_rotation {
        put_system(serial, "accelerometer_rotation", v).await;
    }
    if let Some(v) = &p.user_rotation {
        put_system(serial, "user_rotation", v).await;
    }
    if let Some(v) = &p.stylus_handwriting {
        put_secure(serial, "stylus_handwriting_enabled", v).await;
    }
}

async fn get_global(serial: &str, key: &str) -> Option<String> {
    setting_value(
        adb::shell(serial, format!("settings get global {key}"))
            .await
            .ok()?,
    )
}

async fn get_system(serial: &str, key: &str) -> Option<String> {
    setting_value(
        adb::shell(serial, format!("settings get system {key}"))
            .await
            .ok()?,
    )
}

async fn put_global(serial: &str, key: &str, value: &str) {
    let _ = adb::shell(serial, format!("settings put global {key} {value}")).await;
}

async fn put_system(serial: &str, key: &str, value: &str) {
    let _ = adb::shell(serial, format!("settings put system {key} {value}")).await;
}

async fn get_secure(serial: &str, key: &str) -> Option<String> {
    setting_value(
        adb::shell(serial, format!("settings get secure {key}"))
            .await
            .ok()?,
    )
}

async fn put_secure(serial: &str, key: &str, value: &str) {
    let _ = adb::shell(serial, format!("settings put secure {key} {value}")).await;
}

/// Read `wm <sub>` and pull the effective value: the `Override <label>` line if
/// present, else the `Physical <label>` line. e.g. `wm size` →
/// "Physical size: 1080x2424" / "Override size: 1080x2400".
async fn get_wm(serial: &str, sub: &str, label: &str) -> Option<String> {
    let out = adb::shell(serial, format!("wm {sub}")).await.ok()?;
    parse_wm(&out, &format!("Override {label}"))
        .or_else(|| parse_wm(&out, &format!("Physical {label}")))
}

fn parse_wm(out: &str, key: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
        .filter(|v| !v.is_empty())
}

fn setting_value(raw: String) -> Option<String> {
    let v = raw.trim();
    (!v.is_empty() && v != "null").then(|| v.to_string())
}

fn is_empty(p: &Profile) -> bool {
    p.window_animation_scale.is_none()
        && p.transition_animation_scale.is_none()
        && p.animator_duration_scale.is_none()
        && p.font_scale.is_none()
        && p.density.is_none()
        && p.size.is_none()
        && p.accelerometer_rotation.is_none()
        && p.user_rotation.is_none()
        && p.stylus_handwriting.is_none()
}

fn emit(cmd: &str, body: serde_json::Value) {
    crate::events::emit_action(cmd, &body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_value_treats_null_as_none() {
        assert_eq!(setting_value("null\n".into()), None);
        assert_eq!(setting_value("  \n".into()), None);
        assert_eq!(setting_value("0.0\n".into()), Some("0.0".to_string()));
    }

    #[test]
    fn parse_wm_prefers_present_line() {
        let out = "Physical size: 1080x2424\nOverride size: 1080x2400";
        assert_eq!(
            parse_wm(out, "Override size:"),
            Some("1080x2400".to_string())
        );
        assert_eq!(
            parse_wm(out, "Physical size:"),
            Some("1080x2424".to_string())
        );
        assert_eq!(parse_wm("Physical size: 1080x2424", "Override size:"), None);
    }

    #[test]
    fn automation_preset_zeroes_animations() {
        let args = ProfileApplyArgs {
            preset: Some("automation".into()),
            file: None,
            animations: None,
            font_scale: None,
            density: None,
            size: None,
            rotation: None,
        };
        let p = profile_from_args(&args).unwrap();
        assert_eq!(p.window_animation_scale.as_deref(), Some("0"));
        assert_eq!(p.transition_animation_scale.as_deref(), Some("0"));
        assert_eq!(p.animator_duration_scale.as_deref(), Some("0"));
        // automation also disables the stylus tutorial that breaks text input
        assert_eq!(p.stylus_handwriting.as_deref(), Some("0"));
        assert_eq!(p.font_scale, None);
    }
}
