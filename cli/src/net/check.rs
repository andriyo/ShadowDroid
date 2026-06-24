//! `net check` — read-only verdict on whether an app is MITM-able.
//!
//! Host-only (plain `dumpsys`/`pm`), shared by the standalone `net check <pkg>`
//! command and `doctor --app`. The reliable host-side signals are **debuggable**
//! and **targetSdk**; together they decide whether a user-installed CA will be
//! trusted:
//!   - targetSdk ≤ 23: user CAs trusted by default → interceptable.
//!   - targetSdk ≥ 24: user CAs trusted **only** if the app's Network Security
//!     Config opts in (`<debug-overrides>`/trust-anchor `user`). Debug builds
//!     commonly do; release builds usually don't.
//! Reading the NSC itself needs the APK; we report the heuristic verdict + what
//! to verify rather than pulling+parsing it. (Cronet/QUIC + pinning caveats are
//! surfaced as notes — those bypass a user-CA proxy regardless.)

use crate::ids::Serial;
use anyhow::{bail, Result};
use serde::Serialize;

use crate::device::adb;

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub package: String,
    pub installed: bool,
    pub debuggable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_sdk: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_sdk: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_name: Option<String>,
    /// interceptable | conditional | blocked
    pub verdict: String,
    pub reason: String,
    pub notes: Vec<String>,
}

/// Inspect an installed package and produce a verdict. Errors only if the
/// package isn't installed.
pub async fn inspect(serial: &Serial, package: &str) -> Result<CheckReport> {
    if adb::pm_path(serial, package).await?.is_none() {
        bail!("{package} is not installed on {serial}");
    }
    let dump = adb::shell(serial, format!("dumpsys package {package}")).await?;
    let debuggable = dump.contains("DEBUGGABLE");
    let target_sdk = parse_kv_int(&dump, "targetSdk");
    let min_sdk = parse_kv_int(&dump, "minSdk");
    let version_name = adb::pm_version(serial, package).await.ok().flatten();

    let (verdict, reason) = verdict(debuggable, target_sdk);

    let mut notes = Vec::new();
    notes.push(
        "Engine not inspected: OkHttp/HttpURLConnection/Ktor-OkHttp honour the system proxy; \
         Cronet/QUIC/HTTP-3 and cert-pinned clients bypass it — if flows are missing, the app \
         likely uses one of those."
            .to_string(),
    );
    if verdict != "interceptable" {
        notes.push(
            "Confirm by reading the app's Network Security Config (res/xml referenced by \
             android:networkSecurityConfig): a `user` trust-anchor or `<debug-overrides>` makes it \
             interceptable."
                .to_string(),
        );
    }

    Ok(CheckReport {
        package: package.to_string(),
        installed: true,
        debuggable,
        target_sdk,
        min_sdk,
        version_name,
        verdict,
        reason,
        notes,
    })
}

/// `net check <pkg>` — inspect + emit.
pub async fn run(serial: &Serial, package: &str) -> Result<()> {
    let report = inspect(serial, package).await?;
    let mut v = serde_json::to_value(&report).unwrap_or_default();
    if let serde_json::Value::Object(map) = &mut v {
        map.insert("type".into(), serde_json::json!("action"));
        map.insert("cmd".into(), serde_json::json!("net_check"));
    }
    println!("{}", serde_json::to_string(&v).unwrap());
    Ok(())
}

fn verdict(debuggable: bool, target_sdk: Option<u32>) -> (String, String) {
    let ts = target_sdk.unwrap_or(0);
    if ts != 0 && ts <= 23 {
        return (
            "interceptable".into(),
            format!("targetSdk {ts} ≤ 23 trusts user-installed CAs by default."),
        );
    }
    if debuggable {
        (
            "conditional".into(),
            "Debuggable build, targetSdk ≥ 24: interceptable if the Network Security Config trusts \
             user CAs (a `<debug-overrides><certificates src=\"user\"/>` or `user` trust-anchor). \
             Most debug builds do."
                .into(),
        )
    } else {
        (
            "blocked".into(),
            "Release build, targetSdk ≥ 24: user CAs are NOT trusted unless the Network Security \
             Config explicitly opts in. Build a debuggable variant or add a `user` trust-anchor."
                .into(),
        )
    }
}

/// Find `key=NN` in dumpsys output (first hit).
fn parse_kv_int(text: &str, key: &str) -> Option<u32> {
    let needle = format!("{key}=");
    for line in text.lines() {
        if let Some(idx) = line.find(&needle) {
            let rest = &line[idx + needle.len()..];
            let num: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = num.parse() {
                return Some(n);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sdk_and_verdict() {
        let dump = "  versionName=1.2\n  minSdk=24 targetSdk=34\n  flags=[ HAS_CODE ALLOW_BACKUP ]";
        assert_eq!(parse_kv_int(dump, "targetSdk"), Some(34));
        assert_eq!(parse_kv_int(dump, "minSdk"), Some(24));
        assert!(!dump.contains("DEBUGGABLE"));

        assert_eq!(verdict(true, Some(34)).0, "conditional");
        assert_eq!(verdict(false, Some(34)).0, "blocked");
        assert_eq!(verdict(false, Some(21)).0, "interceptable");
    }
}
