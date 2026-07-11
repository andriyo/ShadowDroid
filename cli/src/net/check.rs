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
//!
//! Reading the NSC itself needs the APK; we report the heuristic verdict + what
//! to verify rather than pulling+parsing it. (Cronet/QUIC + pinning caveats are
//! surfaced as notes — those bypass a user-CA proxy regardless.)

use crate::ids::Serial;
use anyhow::Result;
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
    pub device_image: DeviceImage,
    pub trust: crate::net::trust::TrustEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_trusted_by_app: Option<bool>,
    pub ca_trust_basis: String,
    /// interceptable | conditional | blocked
    pub verdict: String,
    pub reason: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceImage {
    pub kind: String,
    pub play_store_image: bool,
    pub google_apis: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub characteristics: Option<String>,
}

/// Inspect an installed package and produce a verdict. Errors only if the
/// package isn't installed. `tctx` carries the resolved CA and trust posture
/// (assertion/cache) so the store-trust portion honors `proxy.ca_trusted`.
pub async fn inspect(
    serial: &Serial,
    package: &str,
    tctx: &crate::net::trust::TrustContext,
) -> Result<CheckReport> {
    crate::config::validate_android_package(package)?;
    if adb::pm_path(serial, package).await?.is_none() {
        let device = crate::events::shell_token(serial.as_str());
        let package_token = crate::events::shell_token(package);
        return Err(crate::diagnostic::DiagnosticError::new(
            "package_not_installed",
            "net",
            format!("{package} is not installed on {serial}"),
        )
        .detail(serde_json::json!({
            "device": serial.as_str(),
            "package": package,
        }))
        .next_actions([
            "shadowdroid commands --json --describe 'app install'".to_string(),
            format!("shadowdroid -d {device} app current"),
            format!(
                "install {package_token} on the selected device, then rerun `shadowdroid -d {device} net check {package_token}`"
            ),
        ])
        .into());
    }
    let package_arg = crate::config::quote_device_shell_arg(package);
    let dump = adb::shell(serial, format!("dumpsys package {package_arg}")).await?;
    let debuggable = dump.contains("DEBUGGABLE");
    let target_sdk = parse_kv_int(&dump, "targetSdk");
    let min_sdk = parse_kv_int(&dump, "minSdk");
    let version_name = adb::pm_version(serial, package).await.ok().flatten();
    let device_image = inspect_device_image(serial).await;
    let trust = crate::net::trust::evidence(serial, device_image.play_store_image, tctx).await;

    let (verdict, reason) = verdict(debuggable, target_sdk);
    let (ca_trusted_by_app, ca_trust_basis) =
        app_ca_trust_expectation(&trust, debuggable, target_sdk);

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
    if ca_trusted_by_app.is_none() {
        notes.push(
            "Actual per-app CA trust was not proven by `net check`: Android does not expose that \
             for arbitrary targetSdk ≥ 24 apps. Close the loop by running `net start`, exercising \
             a known HTTPS request, then reading `net log` for a decrypted `http` event."
                .to_string(),
        );
    }
    let store_unreadable =
        trust.system_store_status == "unreadable" || trust.user_store_status == "unreadable";
    let store_mismatch =
        trust.system_store_status == "mismatch" || trust.user_store_status == "mismatch";
    if store_unreadable {
        notes.push(
            "Android denied shell readback of at least one trust store, so `net check` does not claim that the ShadowDroid CA is installed. Verify with a known HTTPS request and `net log`."
                .to_string(),
        );
    }
    if store_mismatch {
        notes.push(
            "A certificate exists at the ShadowDroid subject-hash path, but its identity does not match the active proxy CA. Reinstall the active CA before HTTPS interception."
                .to_string(),
        );
    }
    if trust.basis == "asserted" {
        notes.push(
            "CA store trust is asserted via proxy.ca_trusted (not probed); the store readback and \
             install were skipped. Re-run with `net check --fresh` to verify against the device."
                .to_string(),
        );
    }
    if trust.basis == "probed" && !trust.system_store && !trust.user_store && !store_unreadable {
        notes.push(format!(
            "ShadowDroid CA was not found in the device trust stores. Recommended setup: `{}`.",
            trust.recommended_command
        ));
    }

    Ok(CheckReport {
        package: package.to_string(),
        installed: true,
        debuggable,
        target_sdk,
        min_sdk,
        version_name,
        device_image,
        trust,
        ca_trusted_by_app,
        ca_trust_basis,
        verdict,
        reason,
        notes,
    })
}

/// `net check <pkg>` — inspect + emit.
pub async fn run(
    serial: &Serial,
    package: &str,
    tctx: &crate::net::trust::TrustContext,
) -> Result<()> {
    let report = inspect(serial, package, tctx).await?;
    let value = serde_json::to_value(&report).unwrap_or_default();
    crate::events::emit_action("net_check", &value);
    Ok(())
}

async fn inspect_device_image(serial: &Serial) -> DeviceImage {
    let play_store_image = adb::pm_path(serial, "com.android.vending")
        .await
        .ok()
        .flatten()
        .is_some();
    let google_apis = adb::pm_path(serial, "com.google.android.gms")
        .await
        .ok()
        .flatten()
        .is_some();
    let product = getprop(serial, "ro.product.name").await;
    let characteristics = getprop(serial, "ro.build.characteristics").await;
    let kind = if play_store_image {
        "play_store"
    } else if google_apis {
        "google_apis"
    } else {
        "aosp_or_generic"
    }
    .to_string();
    DeviceImage {
        kind,
        play_store_image,
        google_apis,
        product,
        characteristics,
    }
}

async fn getprop(serial: &Serial, key: &str) -> Option<String> {
    adb::shell(serial, format!("getprop {key}"))
        .await
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn app_ca_trust_expectation(
    trust: &crate::net::trust::TrustEvidence,
    debuggable: bool,
    target_sdk: Option<u32>,
) -> (Option<bool>, String) {
    // Asserted trust: the store (system vs user) is unknown, so only the
    // targetSdk<=23 case is definite; otherwise defer to the NSC heuristic.
    if trust.basis == "asserted" {
        if target_sdk.is_some_and(|sdk| sdk <= 23) {
            return (
                Some(true),
                "CA trust asserted (proxy.ca_trusted) and targetSdk <= 23 trusts user CAs by default.".into(),
            );
        }
        return (
            None,
            "CA trust asserted (proxy.ca_trusted); for targetSdk >= 24 whether the app honors it still depends on its Network Security Config (or the CA being in the system store).".into(),
        );
    }
    if trust.system_store {
        return (
            Some(true),
            "ShadowDroid CA is in the system store, which Android exposes to apps unless the client pins or bypasses the platform trust manager.".into(),
        );
    }
    if trust.system_store_status == "unreadable" || trust.user_store_status == "unreadable" {
        return (
            None,
            "Android denied shell readback of a trust store, so the active ShadowDroid CA identity could not be verified.".into(),
        );
    }
    if !trust.user_store {
        return (
            Some(false),
            "ShadowDroid CA is not installed in either the system or user trust store.".into(),
        );
    }
    if target_sdk.is_some_and(|sdk| sdk <= 23) {
        return (
            Some(true),
            "ShadowDroid CA is in the user store and targetSdk <= 23 trusts user CAs by default."
                .into(),
        );
    }
    if debuggable {
        return (
            None,
            "ShadowDroid CA is in the user store; targetSdk >= 24 debuggable apps trust it only when their Network Security Config opts into user CAs.".into(),
        );
    }
    (
        Some(false),
        "ShadowDroid CA is only in the user store; targetSdk >= 24 release apps do not trust user CAs unless their Network Security Config explicitly opts in.".into(),
    )
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

    #[test]
    fn app_ca_trust_expectation_distinguishes_actual_and_conditional() {
        let mut trust = crate::net::trust::TrustEvidence {
            hash: Some("abcd".into()),
            adbd_root: true,
            ca_generated: true,
            system_store: false,
            user_store: true,
            system_store_status: "missing".into(),
            user_store_status: "verified".into(),
            basis: "probed".into(),
            recommended_command: "shadowdroid net trust --auto".into(),
            recommendation_reason: "root".into(),
        };
        assert_eq!(app_ca_trust_expectation(&trust, true, Some(34)).0, None);
        assert_eq!(
            app_ca_trust_expectation(&trust, false, Some(34)).0,
            Some(false)
        );
        assert_eq!(
            app_ca_trust_expectation(&trust, false, Some(23)).0,
            Some(true)
        );
        trust.system_store = true;
        assert_eq!(
            app_ca_trust_expectation(&trust, false, Some(34)).0,
            Some(true)
        );
    }

    #[test]
    fn asserted_basis_defers_to_sdk_heuristic() {
        let trust = crate::net::trust::TrustEvidence {
            hash: None,
            adbd_root: false,
            ca_generated: true,
            system_store: false,
            user_store: false,
            system_store_status: "asserted".into(),
            user_store_status: "asserted".into(),
            basis: "asserted".into(),
            recommended_command: "shadowdroid net trust --ui".into(),
            recommendation_reason: "asserted".into(),
        };
        // Asserted + old SDK is definitely trusted; asserted + modern SDK is
        // unknown (depends on NSC / system store), never a hard "not installed".
        assert_eq!(
            app_ca_trust_expectation(&trust, false, Some(21)).0,
            Some(true)
        );
        assert_eq!(app_ca_trust_expectation(&trust, false, Some(34)).0, None);
    }
}
