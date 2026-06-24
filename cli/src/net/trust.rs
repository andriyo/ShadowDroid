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

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::device::adb;
use crate::net::ca::CertAuthority;
use crate::net::paths;

const SYSTEM_CACERTS: &str = "/system/etc/security/cacerts";
const USER_CACERTS: &str = "/data/misc/user/0/cacerts-added";
const TMP_CA: &str = "/data/local/tmp/shadowdroid-ca.pem";

/// `net trust [--system|--ui]`.
pub async fn run(serial: &str, system: bool, ui: bool) -> Result<()> {
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

async fn try_system_store(serial: &str, hash: &str) -> (bool, Vec<Value>) {
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

async fn try_user_store(serial: &str, hash: &str) -> bool {
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

/// Verify-by-readback that avoids the false positive where `ls:`'s *error*
/// message echoes the path (so a plain `contains(hash)` wrongly matches).
async fn cert_present(serial: &str, dest: &str) -> bool {
    let out = adb::shell(serial, format!("ls {dest} 2>&1"))
        .await
        .unwrap_or_default();
    !out.to_lowercase().contains("no such file") && out.contains(dest)
}

async fn ui_install(serial: &str) -> Result<()> {
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
pub async fn remove(serial: &str) -> Result<bool> {
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
fn ca_subject_hash() -> Result<String> {
    let ca = paths::ca_cert_path()?;
    let out = std::process::Command::new("openssl")
        .args([
            "x509",
            "-inform",
            "PEM",
            "-subject_hash_old",
            "-noout",
            "-in",
        ])
        .arg(&ca)
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
