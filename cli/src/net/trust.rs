//! `net trust` — install the ShadowDroid CA into the device trust store.
//!
//! Two stores, tried in order on a rooted device:
//!   - **system** (`/system/etc/security/cacerts/<hash>.0`): trusted by *all*
//!     apps, but on Android 14+ this lives in a **read-only APEX** and can't be
//!     written without an apex-remount trick — so it usually fails on modern
//!     emulators.
//!   - **user** (`/data/misc/user/0/cacerts-added/<hash>.0`): trusted by
//!     *debuggable* apps whose Network Security Config opts into `user`
//!     trust-anchors (common for debug builds, e.g. Livd's `<debug-overrides>`).
//!     Writable via root, no apex fight — the pragmatic path on Android 14+.
//!
//! `--system` forces the system store only; `--ui` drives the Settings flow on a
//! non-root device. Verify-by-readback throughout: `<hash>` is the OpenSSL
//! `subject_hash_old` (the filename Android keys CAs by).

use crate::ids::Serial;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::device::adb;
use crate::net::ca::CertAuthority;
use crate::net::paths;

const SYSTEM_CACERTS: &str = "/system/etc/security/cacerts";
const USER_CACERTS: &str = "/data/misc/user/0/cacerts-added";
const TMP_CA: &str = "/data/local/tmp/shadowdroid-ca.pem";

/// `net trust [--system|--ui]`.
pub async fn run(serial: &Serial, auto: bool, system: bool, ui: bool) -> Result<()> {
    let selected = [auto, system, ui].into_iter().filter(|v| *v).count();
    if selected > 1 {
        bail!("choose only one trust mode: --auto, --system, or --ui");
    }
    let _ = CertAuthority::load_or_generate()?;
    if ui {
        return ui_install(serial).await;
    }

    let hash = ca_subject_hash()?;
    if adb::shell(serial, "id -u").await?.trim() != "0" {
        emit(json!({
            "installed": false,
            "store": "none",
            "reason": "adbd is not root. On an emulator run `adb root` then retry, or use `net trust --ui` on a real device.",
        }));
        return Ok(());
    }

    // Stage the cert once in a shell-writable tmp.
    adb::push(serial, paths::ca_cert_path()?, TMP_CA.to_string()).await?;

    let (sys_ok, sys_steps) = try_system_store(serial, &hash).await;
    let mut store = if sys_ok { "system" } else { "none" };
    let mut installed = sys_ok;

    // Fall back to the user store unless the caller demanded system-only.
    let mut user_ok = false;
    if !sys_ok && !system {
        user_ok = try_user_store(serial, &hash).await;
        if user_ok {
            store = "user";
            installed = true;
        }
    }

    emit(json!({
        "installed": installed,
        "store": store,
        "hash": hash,
        "system_store": sys_ok,
        "user_store": user_ok,
        "system_steps": sys_steps,
        "note": if installed {
            "trusted. Restart the app under test so it re-reads the trust store (net start force-stops it)."
        } else {
            "system store is APEX-locked (Android 14+) and the user-store push failed; try `net trust --ui`."
        },
    }));
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct TrustEvidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub adbd_root: bool,
    pub ca_generated: bool,
    pub system_store: bool,
    pub user_store: bool,
    pub system_store_status: String,
    pub user_store_status: String,
    pub recommended_command: String,
    pub recommendation_reason: String,
}

/// Read-only CA/trust-store evidence for `net check` and diagnostics.
pub async fn evidence(serial: &Serial, play_store_image: bool) -> TrustEvidence {
    let ca_generated = CertAuthority::load_or_generate().is_ok();
    let hash = ca_subject_hash().ok();
    let adbd_root = adb::shell(serial, "id -u")
        .await
        .map(|out| out.trim() == "0")
        .unwrap_or(false);
    let (system_status, user_status) = if let Some(hash) = &hash {
        let sys = format!("{SYSTEM_CACERTS}/{hash}.0");
        let usr = format!("{USER_CACERTS}/{hash}.0");
        (
            cert_status(serial, &sys).await,
            cert_status(serial, &usr).await,
        )
    } else {
        (CertStatus::Missing, CertStatus::Missing)
    };
    let system_store = system_status == CertStatus::Verified;
    let user_store = user_status == CertStatus::Verified;
    let (recommended_command, recommendation_reason) = if play_store_image || !adbd_root {
        (
            "shadowdroid net trust --ui".to_string(),
            "device does not expose root adbd (common on Play Store/locked images), so install the CA through Android Settings".to_string(),
        )
    } else {
        (
            "shadowdroid net trust --auto".to_string(),
            "root adbd is available, so ShadowDroid can push the CA directly and fall back between stores".to_string(),
        )
    };
    TrustEvidence {
        hash,
        adbd_root,
        ca_generated,
        system_store,
        user_store,
        system_store_status: system_status.as_str().to_string(),
        user_store_status: user_status.as_str().to_string(),
        recommended_command,
        recommendation_reason,
    }
}

async fn try_system_store(serial: &Serial, hash: &str) -> (bool, Vec<Value>) {
    let dest = format!("{SYSTEM_CACERTS}/{hash}.0");
    let mut steps = Vec::new();
    let remount = adb::shell(
        serial,
        "mount -o rw,remount / 2>&1; mount -o rw,remount /system 2>&1",
    )
    .await
    .unwrap_or_default();
    steps.push(json!({"remount": remount.trim()}));
    let copy = adb::shell(
        serial,
        format!("cp {TMP_CA} {dest} 2>&1 && chmod 644 {dest} && echo OK || echo FAIL"),
    )
    .await
    .unwrap_or_default();
    steps.push(json!({"copy": copy.trim()}));
    (cert_present(serial, &dest).await, steps)
}

async fn try_user_store(serial: &Serial, hash: &str) -> bool {
    let dest = format!("{USER_CACERTS}/{hash}.0");
    let _ = adb::shell(
        serial,
        format!(
            "mkdir -p {USER_CACERTS}; cp {TMP_CA} {dest} && chmod 644 {dest}; \
             chown system:system {dest} 2>/dev/null; restorecon {dest} 2>/dev/null; echo done"
        ),
    )
    .await;
    cert_present(serial, &dest).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CertStatus {
    Verified,
    Missing,
    Unreadable,
    Mismatch,
    Invalid,
}

impl CertStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
            Self::Mismatch => "mismatch",
            Self::Invalid => "invalid_certificate",
        }
    }
}

/// Verify the exact active CA by readback. A matching Android subject-hash
/// filename is insufficient: an older certificate with the same subject has
/// the same path, and locked devices may echo that path in a Permission denied
/// error. Only successfully-read certificate bytes that match our CA count.
pub(crate) async fn cert_present(serial: &Serial, dest: &str) -> bool {
    cert_status(serial, dest).await == CertStatus::Verified
}

async fn cert_status(serial: &Serial, dest: &str) -> CertStatus {
    const EXIT_MARKER: &str = "__shadowdroid_cert_ls_exit__:";
    let listing = adb::shell(serial, format!("ls {dest} 2>&1; echo {EXIT_MARKER}$?"))
        .await
        .unwrap_or_default();
    if let Some(status) = classify_cert_listing(&listing) {
        return status;
    }

    let Ok(installed) = adb::pull(serial, dest.to_string()).await else {
        return CertStatus::Unreadable;
    };
    let Ok(local_path) = paths::ca_cert_path() else {
        return CertStatus::Invalid;
    };
    let Ok(local) = std::fs::read(local_path) else {
        return CertStatus::Invalid;
    };
    match certificates_match(&local, &installed) {
        Ok(true) => CertStatus::Verified,
        Ok(false) => CertStatus::Mismatch,
        Err(_) => CertStatus::Invalid,
    }
}

/// `None` means `ls` proved the file readable and identity comparison should
/// continue. Every error state is explicit so echoed paths never count as CA
/// evidence.
fn classify_cert_listing(listing: &str) -> Option<CertStatus> {
    const EXIT_MARKER: &str = "__shadowdroid_cert_ls_exit__:";
    let lower = listing.to_lowercase();
    if lower.contains("permission denied") {
        return Some(CertStatus::Unreadable);
    }
    if lower.contains("no such file") {
        return Some(CertStatus::Missing);
    }
    let listed = listing
        .lines()
        .find_map(|line| line.trim().strip_prefix(EXIT_MARKER))
        .and_then(|exit| exit.parse::<i32>().ok())
        == Some(0);
    if !listed {
        return Some(CertStatus::Unreadable);
    }
    None
}

fn certificates_match(expected: &[u8], installed: &[u8]) -> Result<bool> {
    Ok(certificate_der(expected)? == certificate_der(installed)?)
}

fn certificate_der(bytes: &[u8]) -> Result<Vec<u8>> {
    let bytes = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|start| &bytes[start..])
        .unwrap_or(bytes);
    if bytes.starts_with(b"-----BEGIN CERTIFICATE-----") {
        let (_, pem) = x509_parser::pem::parse_x509_pem(bytes)
            .map_err(|e| anyhow::anyhow!("parse PEM certificate: {e}"))?;
        pem.parse_x509()
            .map_err(|e| anyhow::anyhow!("parse X.509 certificate: {e}"))?;
        return Ok(pem.contents);
    }
    x509_parser::parse_x509_certificate(bytes)
        .map_err(|e| anyhow::anyhow!("parse DER certificate: {e}"))?;
    Ok(bytes.to_vec())
}

async fn ui_install(serial: &Serial) -> Result<()> {
    let dest = "/sdcard/Download/shadowdroid-ca.crt";
    adb::push(serial, paths::ca_cert_path()?, dest.to_string()).await?;
    let _ = adb::shell(serial, "am start -a android.settings.SECURITY_SETTINGS").await;
    emit(json!({
        "store": "ui",
        "pushed": dest,
        "installed": null,
        "instructions": [
            "Settings → Security → Encryption & credentials → Install a certificate → CA certificate",
            format!("choose the pushed file: {dest}"),
            "accept the 'your network may be monitored' warning (expected for a debug MITM CA)",
        ],
        "note": "Requires a screen-lock credential. Re-run `net check <pkg>` to confirm trust once installed.",
    }));
    Ok(())
}

/// Remove the ShadowDroid CA from both stores (root). Returns whether it's gone.
pub async fn remove(serial: &Serial) -> Result<bool> {
    if adb::shell(serial, "id -u").await.unwrap_or_default().trim() != "0" {
        return Ok(false);
    }
    let Ok(hash) = ca_subject_hash() else {
        return Ok(false);
    };
    let sys = format!("{SYSTEM_CACERTS}/{hash}.0");
    let usr = format!("{USER_CACERTS}/{hash}.0");
    let _ = adb::shell(
        serial,
        format!("mount -o rw,remount /system 2>/dev/null; rm -f {sys} {usr}"),
    )
    .await;
    Ok(!cert_present(serial, &sys).await && !cert_present(serial, &usr).await)
}

/// The OpenSSL `subject_hash_old` of our CA cert. Requires `openssl` on PATH.
pub(crate) fn ca_subject_hash() -> Result<String> {
    ca_subject_hash_of(&paths::ca_cert_path()?)
}

/// `subject_hash_old` of an arbitrary CA cert file — used by `net ca info` to
/// hash the CA in a scratch dir, and by [`ca_subject_hash`] for the live one.
pub(crate) fn ca_subject_hash_of(ca: &std::path::Path) -> Result<String> {
    let out = std::process::Command::new("openssl")
        .args([
            "x509",
            "-inform",
            "PEM",
            "-subject_hash_old",
            "-noout",
            "-in",
        ])
        .arg(ca)
        .output()
        .map_err(|e| anyhow::anyhow!("run openssl (is it on PATH?): {e}"))?;
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if hash.is_empty() {
        bail!("openssl produced no subject hash for {}", ca.display());
    }
    Ok(hash)
}

fn emit(body: Value) {
    crate::events::emit_action("net_trust", &body);
}

#[cfg(test)]
mod tests {
    use super::{certificates_match, classify_cert_listing, CertStatus};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    fn test_ca(common_name: &str) -> String {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .self_signed(&KeyPair::generate().unwrap())
            .unwrap()
            .pem()
    }

    #[test]
    fn certificate_identity_rejects_same_subject_with_different_key() {
        let active = test_ca("ShadowDroid CA");
        let stale_same_subject = test_ca("ShadowDroid CA");
        assert!(certificates_match(active.as_bytes(), active.as_bytes()).unwrap());
        assert!(!certificates_match(active.as_bytes(), stale_same_subject.as_bytes()).unwrap());
    }

    #[test]
    fn permission_denied_path_is_not_certificate_evidence() {
        let output = "ls: /data/misc/user/0/cacerts-added/7f45a904.0: Permission denied\n\
                      __shadowdroid_cert_ls_exit__:1\n";
        assert_eq!(classify_cert_listing(output), Some(CertStatus::Unreadable));
        assert_eq!(
            classify_cert_listing(
                "ls: /system/etc/security/cacerts/7f45a904.0: No such file or directory\n\
                 __shadowdroid_cert_ls_exit__:1\n"
            ),
            Some(CertStatus::Missing)
        );
        assert_eq!(
            classify_cert_listing(
                "/system/etc/security/cacerts/7f45a904.0\n\
                 __shadowdroid_cert_ls_exit__:0\n"
            ),
            None
        );
    }
}
