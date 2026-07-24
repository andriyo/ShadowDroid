//! Android `screenrecord` capability discovery and device metadata.

use crate::ids::Serial;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Capabilities {
    #[serde(default)]
    pub executable: String,
    pub version: Option<String>,
    pub unlimited_time_limit: bool,
    pub display_id: bool,
    pub size: bool,
    pub bit_rate: bool,
    pub bugreport: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceMetadata {
    pub serial: String,
    pub model: Option<String>,
    pub sdk: Option<u32>,
}

pub async fn probe(serial: &Serial) -> Result<Capabilities> {
    let discovered = crate::device::adb::shell(serial, "command -v screenrecord").await?;
    let path = discovered.lines().next().unwrap_or("").trim();
    if path.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "video_backend_unavailable",
            "video",
            "Android screenrecord is not available on this device",
        )
        .detail(serde_json::json!({
            "device": serial.as_str(),
            "backend": "screenrecord",
            "screenrecord_path": null,
        }))
        .next_actions([
            "shadowdroid device info",
            "shadowdroid doctor --json",
            "shadowdroid commands --json --describe 'video record'",
        ])
        .into());
    }
    if !path.starts_with('/') || path.len() > 4096 || path.contains('\0') {
        return Err(anyhow!("screenrecord returned an invalid executable path"));
    }
    let quoted = crate::config::quote_device_shell_arg(path);
    let canonical = crate::device::adb::shell(
        serial,
        format!("readlink -f {quoted} 2>/dev/null || printf '%s\\n' {quoted}"),
    )
    .await?;
    let executable = canonical.lines().next().unwrap_or("").trim().to_string();
    if !executable.starts_with('/') || executable.len() > 4096 || executable.contains('\0') {
        return Err(anyhow!("screenrecord returned an invalid canonical path"));
    }
    let help = crate::device::adb::shell(
        serial,
        format!(
            "{} --help 2>&1",
            crate::config::quote_device_shell_arg(&executable)
        ),
    )
    .await?;
    let version = help.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        lower
            .contains("screenrecord")
            .then(|| {
                line.split_whitespace()
                    .find(|part| part.chars().any(|ch| ch.is_ascii_digit()) && part.contains('.'))
                    .map(|part| {
                        part.trim_start_matches('v')
                            .trim_end_matches(|ch: char| !ch.is_ascii_digit())
                            .to_string()
                    })
            })
            .flatten()
    });
    Ok(Capabilities {
        executable,
        version,
        unlimited_time_limit: help.contains("--time-limit")
            && (help.contains("0 = unlimited")
                || help.contains("0 means no time limit")
                || help.contains("0 = no time limit")
                || help.contains("0: no time limit")
                || help.contains("remove the time limit")),
        display_id: help.contains("--display-id"),
        size: help.contains("--size"),
        bit_rate: help.contains("--bit-rate"),
        bugreport: help.contains("--bugreport"),
    })
}

pub async fn device_metadata(serial: &Serial) -> DeviceMetadata {
    let model = crate::device::adb::shell(serial, "getprop ro.product.model")
        .await
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let sdk = crate::device::adb::shell(serial, "getprop ro.build.version.sdk")
        .await
        .ok()
        .and_then(|value| value.trim().parse().ok());
    DeviceMetadata {
        serial: serial.to_string(),
        model,
        sdk,
    }
}

pub fn validate_capture(
    capture: &super::CaptureArgs,
    capabilities: &Capabilities,
) -> Result<Option<(u32, u32)>> {
    if capture.backend != super::VideoBackendArg::Auto
        && capture.backend != super::VideoBackendArg::Screenrecord
    {
        return Err(anyhow!("unsupported video backend"));
    }
    let size = capture.size.as_deref().map(parse_size).transpose()?;
    if size.is_some() && !capabilities.size {
        return unsupported("--size", capabilities);
    }
    if let Some(bit_rate) = capture.bit_rate
        && bit_rate < 100_000
    {
        return Err(crate::diagnostic::DiagnosticError::new(
            "video_invalid_bit_rate",
            "input",
            "video bit rate must be at least 100000 bits per second",
        )
        .detail(serde_json::json!({
            "bit_rate": bit_rate,
            "minimum": 100_000,
        }))
        .next_actions([
            "shadowdroid commands --json --describe 'video record'",
            "rerun without --bit-rate to use Android's default",
        ])
        .into());
    }
    if capture.bit_rate.is_some() && !capabilities.bit_rate {
        return unsupported("--bit-rate", capabilities);
    }
    if capture.display_id.is_some() && !capabilities.display_id {
        return unsupported("--display-id", capabilities);
    }
    if capture.bugreport && !capabilities.bugreport {
        return unsupported("--bugreport", capabilities);
    }
    if capture.segment_seconds > 180 && !capabilities.unlimited_time_limit {
        return Err(crate::diagnostic::DiagnosticError::new(
            "video_backend_option_unsupported",
            "video",
            "this screenrecord version limits segments to 180 seconds",
        )
        .detail(serde_json::json!({
            "option": "--segment-seconds",
            "requested": capture.segment_seconds,
            "maximum": 180,
            "capabilities": capabilities,
        }))
        .next_actions([
            "rerun with `--segment-seconds 170`",
            "shadowdroid commands --json --describe 'video record'",
        ])
        .into());
    }
    Ok(size)
}

fn unsupported<T>(option: &str, capabilities: &Capabilities) -> Result<T> {
    Err(crate::diagnostic::DiagnosticError::new(
        "video_backend_option_unsupported",
        "video",
        format!("this screenrecord version does not support {option}"),
    )
    .detail(serde_json::json!({
        "option": option,
        "capabilities": capabilities,
    }))
    .next_actions([
        format!("rerun without {option}"),
        "shadowdroid commands --json --describe 'video record'".to_string(),
    ])
    .into())
}

fn parse_size(value: &str) -> Result<(u32, u32)> {
    let invalid = || {
        crate::diagnostic::DiagnosticError::new(
            "video_invalid_size",
            "input",
            "video size must be WIDTHxHEIGHT with positive integer dimensions",
        )
        .detail(serde_json::json!({
            "size": value,
            "expected": "WIDTHxHEIGHT",
        }))
        .next_actions([
            "rerun with a size such as `--size 1280x720`",
            "shadowdroid commands --json --describe 'video record'",
        ])
    };
    let Some((width, height)) = value.split_once(['x', 'X']) else {
        return Err(invalid().into());
    };
    let width = width.parse::<u32>().map_err(|_| invalid())?;
    let height = height.parse::<u32>().map_err(|_| invalid())?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(invalid().into());
    }
    Ok((width, height))
}

pub async fn orientation(serial: &Serial) -> Option<u8> {
    let output = crate::device::adb::shell(
        serial,
        "dumpsys input 2>/dev/null | grep -m 1 SurfaceOrientation; \
         dumpsys window displays 2>/dev/null | grep -m 1 mDisplayRotation",
    )
    .await
    .ok()?;
    for line in output.lines() {
        if let Some(value) = line
            .split_once("SurfaceOrientation:")
            .and_then(|(_, value)| value.split_whitespace().next())
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value <= 3)
        {
            return Some(value);
        }
        if let Some(value) = line
            .split_once("mDisplayRotation=ROTATION_")
            .and_then(|(_, value)| value.chars().next())
            .and_then(|value| value.to_digit(10))
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value <= 3)
        {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> Capabilities {
        Capabilities {
            executable: "/system/bin/screenrecord".into(),
            version: Some("1.4".into()),
            unlimited_time_limit: true,
            display_id: true,
            size: true,
            bit_rate: true,
            bugreport: true,
        }
    }

    #[test]
    fn validates_size_and_capture_options() {
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        for invalid in ["", "1280", "0x720", "1280x0", "x720", "aXb"] {
            assert!(parse_size(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn legacy_backend_rejects_overlong_segment() {
        let capture = super::super::CaptureArgs {
            backend: super::super::VideoBackendArg::Auto,
            size: None,
            bit_rate: None,
            display_id: None,
            bugreport: false,
            segment_seconds: 181,
            no_split_on_rotation: false,
        };
        let mut caps = capabilities();
        caps.unlimited_time_limit = false;
        assert!(validate_capture(&capture, &caps).is_err());
    }
}
