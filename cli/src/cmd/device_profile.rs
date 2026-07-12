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
//! Verify-by-readback: every mutation is followed by a fresh read and an exact
//! postcondition check. The ADB shell transport used here does not expose a
//! remote exit code, so transport failures are propagated and semantic failures
//! are detected from readback.

use crate::ids::Serial;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;
use std::path::PathBuf;

use crate::device::adb;

/// The restorable display profile. All fields optional — a snapshot omits
/// whatever the device reports as unset, and an apply only touches present
/// fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, PartialEq, Serialize)]
struct PostconditionMismatch {
    field: String,
    requested: serde_json::Value,
    observed: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
struct WmState {
    effective: Option<String>,
    override_value: Option<String>,
}

/// Disable the stylus-handwriting tutorial that otherwise intercepts the first
/// text-field focus (and steals `text` input). Best-effort, idempotent, and a
/// no-op on devices/Android versions without the setting. Returns the observed
/// state. Called automatically by `connect` and folded into the `automation`
/// preset, so text input "just works" for the common workflow.
pub async fn disable_stylus_tutorial(serial: &Serial) -> bool {
    // `connect` deliberately treats this convenience as best-effort; profile
    // apply/reset use the fallible mutation helpers directly and never discard
    // their transport errors.
    let _ = put_secure(serial, "stylus_handwriting_enabled", "0").await;
    get_secure(serial, "stylus_handwriting_enabled")
        .await
        .ok()
        .flatten()
        .as_deref()
        == Some("0")
}

#[derive(clap::Args)]
pub struct ProfileApplyArgs {
    /// Named preset. `automation` zeroes the three animation scales.
    #[arg(long)]
    pub preset: Option<String>,
    /// Apply a profile previously written by `profile-snapshot -o`.
    #[arg(
        long,
        conflicts_with_all = [
            "preset",
            "animations",
            "font_scale",
            "density",
            "size",
            "rotation"
        ]
    )]
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

pub async fn snapshot(serial: &Serial, out: Option<&PathBuf>) -> Result<()> {
    let profile = read_profile(serial).await?;
    let value = serde_json::to_value(&profile)?;
    let saved = match out {
        Some(path) => {
            crate::cmd::artifact::write_json(path, &value)?;
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

pub async fn apply(serial: &Serial, args: &ProfileApplyArgs) -> Result<()> {
    let profile = profile_from_args(args)?;
    apply_profile(serial, &profile).await?;
    let now = read_profile(serial).await?;
    enforce_postcondition(
        serial,
        "apply",
        &profile,
        &now,
        profile_mismatches(&profile, &now),
    )?;
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
        let path_arg = command_arg(&file.display().to_string());
        let text = std::fs::read_to_string(file).map_err(|error| {
            crate::diagnostic::DiagnosticError::new(
                "profile_file_read_failed",
                "input",
                format!("could not read profile file {}", file.display()),
            )
            .detail(serde_json::json!({
                "path": file,
                "error": error.to_string(),
            }))
            .next_actions([
                "shadowdroid commands --json --describe 'profile apply'".to_string(),
                format!("shadowdroid profile snapshot --out {path_arg}"),
            ])
        })?;
        let profile: Profile = serde_json::from_str(&text).map_err(|error| {
            crate::diagnostic::DiagnosticError::new(
                "profile_file_invalid",
                "input",
                format!("profile file {} is not valid", file.display()),
            )
            .detail(serde_json::json!({
                "path": file,
                "error": error.to_string(),
                "line": error.line(),
                "column": error.column(),
            }))
            .next_actions([
                "shadowdroid commands --json --describe 'profile apply'".to_string(),
                format!("shadowdroid profile snapshot --out {path_arg}"),
            ])
        })?;
        if is_empty(&profile) {
            return Err(no_changes_error(Some(file)));
        }
        return normalize_profile(profile);
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
        Some(other) => {
            return Err(crate::diagnostic::DiagnosticError::new(
                "profile_unknown_preset",
                "input",
                format!("unknown profile preset {other:?}"),
            )
            .detail(serde_json::json!({
                "preset": other,
                "allowed": ["automation"],
            }))
            .next_actions([
                "shadowdroid commands --json --describe 'profile apply'",
                "shadowdroid profile apply --preset automation",
            ])
            .into());
        }
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
        return Err(no_changes_error(None));
    }
    normalize_profile(p)
}

fn no_changes_error(source: Option<&PathBuf>) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "profile_no_changes_requested",
        "input",
        "profile apply requires a preset, non-empty file, or at least one setting",
    )
    .detail(serde_json::json!({
        "source": source,
        "accepted": [
            "--preset automation",
            "--file <snapshot.json>",
            "--animations <scale>",
            "--font-scale <scale>",
            "--density <dpi>",
            "--size <WxH>",
            "--rotation <0..3>"
        ],
    }))
    .next_actions([
        "shadowdroid commands --json --describe 'profile apply'",
        "shadowdroid profile apply --preset automation",
    ])
    .into()
}

/// Parse and canonicalize every value before it can enter an Android shell
/// command. Profile files are user-authored/committable input, so deserializing
/// into strings alone is not a trust boundary.
fn normalize_profile(mut profile: Profile) -> Result<Profile> {
    normalize_float(
        "window_animation_scale",
        &mut profile.window_animation_scale,
        "a finite number greater than or equal to 0",
        |value| value >= 0.0,
    )?;
    normalize_float(
        "transition_animation_scale",
        &mut profile.transition_animation_scale,
        "a finite number greater than or equal to 0",
        |value| value >= 0.0,
    )?;
    normalize_float(
        "animator_duration_scale",
        &mut profile.animator_duration_scale,
        "a finite number greater than or equal to 0",
        |value| value >= 0.0,
    )?;
    normalize_float(
        "font_scale",
        &mut profile.font_scale,
        "a finite number greater than 0",
        |value| value > 0.0,
    )?;
    normalize_integer(
        "density",
        &mut profile.density,
        1..=u32::MAX,
        "an integer greater than 0",
    )?;
    normalize_size_field(&mut profile.size)?;
    normalize_integer(
        "accelerometer_rotation",
        &mut profile.accelerometer_rotation,
        0..=1,
        "0 or 1",
    )?;
    normalize_integer(
        "user_rotation",
        &mut profile.user_rotation,
        0..=3,
        "an integer from 0 through 3",
    )?;
    normalize_integer(
        "stylus_handwriting",
        &mut profile.stylus_handwriting,
        0..=1,
        "0 or 1",
    )?;
    Ok(profile)
}

fn normalize_float<F>(
    field: &str,
    value: &mut Option<String>,
    expected: &str,
    valid: F,
) -> Result<()>
where
    F: FnOnce(f64) -> bool,
{
    let Some(raw) = value else {
        return Ok(());
    };
    let parsed = finite_number(raw)
        .filter(|value| valid(*value))
        .ok_or_else(|| invalid_profile_value(field, raw, expected))?;
    *raw = parsed.to_string();
    Ok(())
}

fn normalize_integer(
    field: &str,
    value: &mut Option<String>,
    allowed: RangeInclusive<u32>,
    expected: &str,
) -> Result<()> {
    let Some(raw) = value else {
        return Ok(());
    };
    let parsed = raw
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|value| allowed.contains(value))
        .ok_or_else(|| invalid_profile_value(field, raw, expected))?;
    *raw = parsed.to_string();
    Ok(())
}

fn normalize_size_field(value: &mut Option<String>) -> Result<()> {
    let Some(raw) = value else {
        return Ok(());
    };
    let (width, height) = normalize_size(raw)
        .filter(|(width, height)| *width > 0 && *height > 0)
        .ok_or_else(|| invalid_profile_value("size", raw, "positive integers formatted as WxH"))?;
    *raw = format!("{width}x{height}");
    Ok(())
}

fn invalid_profile_value(field: &str, value: &str, expected: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(
        "profile_invalid_value",
        "input",
        format!("profile field {field:?} has an invalid value"),
    )
    .detail(serde_json::json!({
        "field": field,
        "value": value,
        "expected": expected,
    }))
    .next_actions([
        "shadowdroid commands --json --describe 'profile apply'",
        "shadowdroid profile apply --preset automation",
    ])
    .into()
}

// ── reset ────────────────────────────────────────────────────────────────────

pub async fn reset(serial: &Serial) -> Result<()> {
    // wm overrides clear via their own `reset` subcommands; the rest go back to
    // stock defaults.
    adb::shell(serial, "wm size reset").await?;
    adb::shell(serial, "wm density reset").await?;
    let defaults = normalize_profile(Profile {
        window_animation_scale: Some("1".into()),
        transition_animation_scale: Some("1".into()),
        animator_duration_scale: Some("1".into()),
        font_scale: Some("1.0".into()),
        accelerometer_rotation: Some("1".into()),
        user_rotation: Some("0".into()),
        density: None,
        size: None,
        stylus_handwriting: Some("1".into()),
    })?;
    apply_profile(serial, &defaults).await?;
    let now = read_profile(serial).await?;
    let size_state = get_wm_state(serial, "size", "size:").await?;
    let density_state = get_wm_state(serial, "density", "density:").await?;
    let mut mismatches = profile_mismatches(&defaults, &now);
    mismatches.extend(reset_override_mismatches(&size_state, &density_state));
    enforce_postcondition(serial, "reset", &defaults, &now, mismatches)?;
    emit(
        "profile_reset",
        serde_json::json!({ "now": serde_json::to_value(&now)? }),
    );
    Ok(())
}

// ── read / write device state ────────────────────────────────────────────────

async fn read_profile(serial: &Serial) -> Result<Profile> {
    Ok(Profile {
        window_animation_scale: get_global(serial, "window_animation_scale").await?,
        transition_animation_scale: get_global(serial, "transition_animation_scale").await?,
        animator_duration_scale: get_global(serial, "animator_duration_scale").await?,
        font_scale: get_system(serial, "font_scale").await?,
        density: get_wm(serial, "density", "density:").await?,
        size: get_wm(serial, "size", "size:").await?,
        accelerometer_rotation: get_system(serial, "accelerometer_rotation").await?,
        user_rotation: get_system(serial, "user_rotation").await?,
        stylus_handwriting: get_secure(serial, "stylus_handwriting_enabled").await?,
    })
}

async fn apply_profile(serial: &Serial, p: &Profile) -> Result<()> {
    if let Some(v) = &p.window_animation_scale {
        put_global(serial, "window_animation_scale", v).await?;
    }
    if let Some(v) = &p.transition_animation_scale {
        put_global(serial, "transition_animation_scale", v).await?;
    }
    if let Some(v) = &p.animator_duration_scale {
        put_global(serial, "animator_duration_scale", v).await?;
    }
    if let Some(v) = &p.font_scale {
        put_system(serial, "font_scale", v).await?;
    }
    if let Some(v) = &p.density {
        adb::shell(serial, format!("wm density {v}")).await?;
    }
    if let Some(v) = &p.size {
        adb::shell(serial, format!("wm size {v}")).await?;
    }
    // Auto-rotate must be set before pinning a rotation.
    if let Some(v) = &p.accelerometer_rotation {
        put_system(serial, "accelerometer_rotation", v).await?;
    }
    if let Some(v) = &p.user_rotation {
        put_system(serial, "user_rotation", v).await?;
    }
    if let Some(v) = &p.stylus_handwriting {
        put_secure(serial, "stylus_handwriting_enabled", v).await?;
    }
    Ok(())
}

async fn get_global(serial: &Serial, key: &str) -> Result<Option<String>> {
    Ok(setting_value(
        adb::shell(serial, format!("settings get global {key}")).await?,
    ))
}

async fn get_system(serial: &Serial, key: &str) -> Result<Option<String>> {
    Ok(setting_value(
        adb::shell(serial, format!("settings get system {key}")).await?,
    ))
}

async fn put_global(serial: &Serial, key: &str, value: &str) -> Result<()> {
    adb::shell(serial, format!("settings put global {key} {value}")).await?;
    Ok(())
}

async fn put_system(serial: &Serial, key: &str, value: &str) -> Result<()> {
    adb::shell(serial, format!("settings put system {key} {value}")).await?;
    Ok(())
}

async fn get_secure(serial: &Serial, key: &str) -> Result<Option<String>> {
    Ok(setting_value(
        adb::shell(serial, format!("settings get secure {key}")).await?,
    ))
}

async fn put_secure(serial: &Serial, key: &str, value: &str) -> Result<()> {
    adb::shell(serial, format!("settings put secure {key} {value}")).await?;
    Ok(())
}

/// Read `wm <sub>` and pull the effective value: the `Override <label>` line if
/// present, else the `Physical <label>` line. e.g. `wm size` →
/// "Physical size: 1080x2424" / "Override size: 1080x2400".
async fn get_wm(serial: &Serial, sub: &str, label: &str) -> Result<Option<String>> {
    Ok(get_wm_state(serial, sub, label).await?.effective)
}

async fn get_wm_state(serial: &Serial, sub: &str, label: &str) -> Result<WmState> {
    let out = adb::shell(serial, format!("wm {sub}")).await?;
    let override_value = parse_wm(&out, &format!("Override {label}"));
    let physical = parse_wm(&out, &format!("Physical {label}"));
    Ok(WmState {
        effective: override_value.clone().or(physical),
        override_value,
    })
}

fn profile_mismatches(requested: &Profile, observed: &Profile) -> Vec<PostconditionMismatch> {
    profile_fields(requested)
        .into_iter()
        .zip(profile_fields(observed))
        .filter_map(
            |((field, requested_value), (observed_field, observed_value))| {
                debug_assert_eq!(field, observed_field);
                let requested_value = requested_value?;
                if observed_value
                    .is_some_and(|value| values_equivalent(field, requested_value, value))
                {
                    return None;
                }
                Some(PostconditionMismatch {
                    field: field.to_string(),
                    requested: serde_json::Value::String(requested_value.to_string()),
                    observed: observed_value
                        .map(|value| serde_json::Value::String(value.to_string()))
                        .unwrap_or(serde_json::Value::Null),
                })
            },
        )
        .collect()
}

fn profile_fields(profile: &Profile) -> [(&'static str, Option<&str>); 9] {
    [
        (
            "window_animation_scale",
            profile.window_animation_scale.as_deref(),
        ),
        (
            "transition_animation_scale",
            profile.transition_animation_scale.as_deref(),
        ),
        (
            "animator_duration_scale",
            profile.animator_duration_scale.as_deref(),
        ),
        ("font_scale", profile.font_scale.as_deref()),
        ("density", profile.density.as_deref()),
        ("size", profile.size.as_deref()),
        (
            "accelerometer_rotation",
            profile.accelerometer_rotation.as_deref(),
        ),
        ("user_rotation", profile.user_rotation.as_deref()),
        ("stylus_handwriting", profile.stylus_handwriting.as_deref()),
    ]
}

fn values_equivalent(field: &str, requested: &str, observed: &str) -> bool {
    if field == "size" {
        return normalize_size(requested)
            .zip(normalize_size(observed))
            .is_some_and(|(requested, observed)| requested == observed);
    }
    if is_numeric_field(field) {
        return finite_number(requested)
            .zip(finite_number(observed))
            .is_some_and(|(requested, observed)| requested == observed);
    }
    requested.trim() == observed.trim()
}

fn is_numeric_field(field: &str) -> bool {
    matches!(
        field,
        "window_animation_scale"
            | "transition_animation_scale"
            | "animator_duration_scale"
            | "font_scale"
            | "density"
            | "accelerometer_rotation"
            | "user_rotation"
            | "stylus_handwriting"
    )
}

fn finite_number(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn normalize_size(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.trim().split_once(['x', 'X'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

fn reset_override_mismatches(size: &WmState, density: &WmState) -> Vec<PostconditionMismatch> {
    [("size", size), ("density", density)]
        .into_iter()
        .filter_map(|(field, state)| {
            state
                .override_value
                .as_ref()
                .map(|override_value| PostconditionMismatch {
                    field: field.to_string(),
                    requested: serde_json::json!({ "override": null }),
                    observed: serde_json::json!({
                        "override": override_value,
                        "effective": state.effective,
                    }),
                })
        })
        .collect()
}

fn enforce_postcondition(
    serial: &Serial,
    operation: &str,
    requested: &Profile,
    observed: &Profile,
    mismatches: Vec<PostconditionMismatch>,
) -> Result<()> {
    if mismatches.is_empty() {
        return Ok(());
    }

    let device = command_arg(serial.as_str());
    let mut next_actions = vec![
        format!("shadowdroid -d {device} profile snapshot"),
        format!("shadowdroid -d {device} doctor --json"),
    ];
    if operation == "reset" {
        next_actions.push(format!("shadowdroid -d {device} profile reset"));
    }
    let count = mismatches.len();
    Err(crate::diagnostic::DiagnosticError::new(
        "profile_postcondition_failed",
        "profile",
        format!("{count} profile field(s) did not reach the requested state"),
    )
    .retryable(true)
    .detail(serde_json::json!({
        "operation": operation,
        "device": serial,
        "requested": requested,
        "observed": observed,
        "mismatches": mismatches,
    }))
    .next_actions(next_actions)
    .into())
}

fn command_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
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

    fn empty_args() -> ProfileApplyArgs {
        ProfileApplyArgs {
            preset: None,
            file: None,
            animations: None,
            font_scale: None,
            density: None,
            size: None,
            rotation: None,
        }
    }

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
            ..empty_args()
        };
        let p = profile_from_args(&args).unwrap();
        assert_eq!(p.window_animation_scale.as_deref(), Some("0"));
        assert_eq!(p.transition_animation_scale.as_deref(), Some("0"));
        assert_eq!(p.animator_duration_scale.as_deref(), Some("0"));
        // automation also disables the stylus tutorial that breaks text input
        assert_eq!(p.stylus_handwriting.as_deref(), Some("0"));
        assert_eq!(p.font_scale, None);
    }

    #[test]
    fn numeric_readback_formatting_is_equivalent() {
        let requested = Profile {
            window_animation_scale: Some("1".into()),
            transition_animation_scale: Some("0".into()),
            animator_duration_scale: Some("1.00".into()),
            font_scale: Some("1".into()),
            density: Some("420".into()),
            accelerometer_rotation: Some("0".into()),
            user_rotation: Some("1".into()),
            stylus_handwriting: Some("0".into()),
            ..Profile::default()
        };
        let observed = Profile {
            window_animation_scale: Some("1.0".into()),
            transition_animation_scale: Some("0.0".into()),
            animator_duration_scale: Some("1".into()),
            font_scale: Some("1.0".into()),
            density: Some("420.0".into()),
            accelerometer_rotation: Some("0.0".into()),
            user_rotation: Some("1.0".into()),
            stylus_handwriting: Some("0.0".into()),
            ..Profile::default()
        };

        assert!(profile_mismatches(&requested, &observed).is_empty());
    }

    #[test]
    fn different_and_missing_values_have_per_field_evidence() {
        let requested = Profile {
            font_scale: Some("1".into()),
            density: Some("420".into()),
            ..Profile::default()
        };
        let observed = Profile {
            font_scale: Some("1.25".into()),
            ..Profile::default()
        };

        assert_eq!(
            profile_mismatches(&requested, &observed),
            vec![
                PostconditionMismatch {
                    field: "font_scale".into(),
                    requested: serde_json::json!("1"),
                    observed: serde_json::json!("1.25"),
                },
                PostconditionMismatch {
                    field: "density".into(),
                    requested: serde_json::json!("420"),
                    observed: serde_json::Value::Null,
                },
            ]
        );
    }

    #[test]
    fn size_comparison_normalizes_case_and_whitespace() {
        assert!(values_equivalent("size", " 1080 X 2400 ", "1080x2400"));
        assert!(!values_equivalent("size", "1080x2400", "1080x2424"));
    }

    #[test]
    fn reset_requires_wm_overrides_to_be_absent() {
        let size = WmState {
            effective: Some("1080x2400".into()),
            override_value: Some("1080x2400".into()),
        };
        let density = WmState {
            effective: Some("420".into()),
            override_value: None,
        };

        assert_eq!(
            reset_override_mismatches(&size, &density),
            vec![PostconditionMismatch {
                field: "size".into(),
                requested: serde_json::json!({"override": null}),
                observed: serde_json::json!({
                    "override": "1080x2400",
                    "effective": "1080x2400",
                }),
            }]
        );
    }

    #[test]
    fn profile_postcondition_error_is_typed_and_actionable() {
        let requested = Profile {
            font_scale: Some("1".into()),
            ..Profile::default()
        };
        let observed = Profile {
            font_scale: Some("1.25".into()),
            ..Profile::default()
        };
        let mismatches = profile_mismatches(&requested, &observed);
        let serial = Serial::from("emulator-5554");

        let err =
            enforce_postcondition(&serial, "apply", &requested, &observed, mismatches).unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&err),
            "profile_postcondition_failed"
        );
        let diagnostic = err
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.detail["mismatches"][0]["field"], "font_scale");
        assert_eq!(diagnostic.detail["mismatches"][0]["requested"], "1");
        assert_eq!(diagnostic.detail["mismatches"][0]["observed"], "1.25");
        assert_eq!(
            diagnostic.next_actions,
            [
                "shadowdroid -d emulator-5554 profile snapshot",
                "shadowdroid -d emulator-5554 doctor --json",
            ]
        );
    }

    #[test]
    fn profile_input_errors_have_stable_codes() {
        let no_changes = profile_from_args(&empty_args()).unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&no_changes),
            "profile_no_changes_requested"
        );

        let unknown = profile_from_args(&ProfileApplyArgs {
            preset: Some("fast-ish".into()),
            ..empty_args()
        })
        .unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&unknown),
            "profile_unknown_preset"
        );
    }

    #[test]
    fn profile_values_are_validated_and_canonicalized_before_shell_use() {
        let profile = profile_from_args(&ProfileApplyArgs {
            animations: Some(1.0),
            size: Some(" 1080 X 2400 ".into()),
            rotation: Some(3),
            ..empty_args()
        })
        .unwrap();
        assert_eq!(profile.window_animation_scale.as_deref(), Some("1"));
        assert_eq!(profile.size.as_deref(), Some("1080x2400"));
        assert_eq!(profile.user_rotation.as_deref(), Some("3"));

        let err = profile_from_args(&ProfileApplyArgs {
            size: Some("1080x2400; reboot".into()),
            ..empty_args()
        })
        .unwrap_err();
        assert_eq!(crate::cli::error_code_of(&err), "profile_invalid_value");
        let diagnostic = err
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.detail["field"], "size");
        assert_eq!(diagnostic.detail["value"], "1080x2400; reboot");
    }

    #[test]
    fn committed_profile_cannot_inject_shell_or_hide_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let injected = dir.path().join("injected.json");
        std::fs::write(&injected, r#"{"density":"420; reboot"}"#).unwrap();
        let err = profile_from_args(&ProfileApplyArgs {
            file: Some(injected),
            ..empty_args()
        })
        .unwrap_err();
        assert_eq!(crate::cli::error_code_of(&err), "profile_invalid_value");

        let unknown = dir.path().join("unknown.json");
        std::fs::write(
            &unknown,
            r#"{"window_animation_scale":"0","command":"reboot"}"#,
        )
        .unwrap();
        let err = profile_from_args(&ProfileApplyArgs {
            file: Some(unknown),
            ..empty_args()
        })
        .unwrap_err();
        assert_eq!(crate::cli::error_code_of(&err), "profile_file_invalid");
    }

    #[test]
    fn empty_profile_file_is_a_typed_noop_error() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "{}").unwrap();
        let err = profile_from_args(&ProfileApplyArgs {
            file: Some(empty),
            ..empty_args()
        })
        .unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&err),
            "profile_no_changes_requested"
        );
    }
}
