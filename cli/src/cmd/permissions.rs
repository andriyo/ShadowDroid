//! `perm-grant` / `perm-revoke` / `perm-list` / `perm-reset` and
//! `appop-get` / `appop-set` — structured wrappers over `pm grant|revoke`,
//! `dumpsys package`, and `cmd appops`.
//!
//! Host-only: every call is plain `adb shell`, so these never need the
//! on-device server. They pair with `watch --permission-dialogs allow|deny` —
//! pre-granting lets an agent skip the dialog dance, revoking exercises the
//! denied path, and resetting returns the app to a fresh-prompt state.
//!
//! **Verify-by-readback.** `adb shell` exposes no exit code (see
//! [crate::device::adb::shell]), and `pm grant` prints failures to *stderr*
//! (which we don't capture). So correctness never relies on the command's
//! output: every verb mutates, then re-reads the real state from
//! `dumpsys package` / `cmd appops get` and reports the observed result. The
//! agent can always tell from `now` whether a change actually took.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::device::adb;

// ── runtime permissions ─────────────────────────────────────────────────────

pub async fn grant(serial: &str, package: &str, perms: &[String]) -> Result<()> {
    change(serial, package, perms, "grant", "perm_grant").await
}

pub async fn revoke(serial: &str, package: &str, perms: &[String]) -> Result<()> {
    change(serial, package, perms, "revoke", "perm_revoke").await
}

/// Shared grant/revoke body: snapshot the requested perms, run `pm <verb>` for
/// each, then re-read and report `now` (observed state of the requested perms)
/// and `changed` (those that actually flipped).
async fn change(
    serial: &str,
    package: &str,
    perms: &[String],
    pm_verb: &str,
    cmd: &str,
) -> Result<()> {
    let before = runtime_perms(serial, package).await?;
    for p in perms {
        // stdout is normally empty on success; pm prints errors to stderr, which
        // we deliberately ignore in favour of the readback below.
        let _ = adb::shell(serial, format!("pm {pm_verb} {package} {p}")).await;
    }
    let after = runtime_perms(serial, package).await?;

    let now: BTreeMap<&String, Option<bool>> =
        perms.iter().map(|p| (p, after.get(p).copied())).collect();
    let changed: Vec<&String> = perms
        .iter()
        .filter(|p| before.get(*p) != after.get(*p))
        .collect();

    emit(
        cmd,
        serde_json::json!({
            "package": package,
            "requested": perms,
            "changed": changed,
            "now": now,
        }),
    );
    Ok(())
}

pub async fn list(serial: &str, package: &str) -> Result<()> {
    let perms = runtime_perms(serial, package).await?;
    emit(
        "perm_list",
        serde_json::json!({ "package": package, "permissions": perms }),
    );
    Ok(())
}

/// Revoke every currently-granted runtime permission, returning the package to a
/// fresh-install prompt state (runtime perms default to denied). Revoking may
/// kill the app's process — expected. Permissions fixed by policy simply stay
/// granted in the readback.
pub async fn reset(serial: &str, package: &str) -> Result<()> {
    let before = runtime_perms(serial, package).await?;
    let granted: Vec<&String> = before.iter().filter(|(_, &g)| g).map(|(p, _)| p).collect();
    for p in &granted {
        let _ = adb::shell(serial, format!("pm revoke {package} {p}")).await;
    }
    let after = runtime_perms(serial, package).await?;
    let revoked: Vec<&String> = granted
        .iter()
        .copied()
        .filter(|p| after.get(*p) == Some(&false))
        .collect();
    emit(
        "perm_reset",
        serde_json::json!({
            "package": package,
            "revoked": revoked,
            "now": after,
        }),
    );
    Ok(())
}

pub async fn runtime_perms(serial: &str, package: &str) -> Result<BTreeMap<String, bool>> {
    let dump = adb::shell(serial, format!("dumpsys package {package}")).await?;
    Ok(parse_runtime_perms(&dump))
}

/// Grant `perms` without emitting an action line — for composition by other
/// verbs (e.g. `app-install --grant-all`). Returns the post-grant runtime-perm
/// state so the caller can report what actually took.
pub async fn grant_quiet(
    serial: &str,
    package: &str,
    perms: &[String],
) -> Result<BTreeMap<String, bool>> {
    for p in perms {
        let _ = adb::shell(serial, format!("pm grant {package} {p}")).await;
    }
    runtime_perms(serial, package).await
}

// ── appops ──────────────────────────────────────────────────────────────────

pub async fn appop_get(serial: &str, package: &str, op: Option<&str>) -> Result<()> {
    let cmd = match op {
        Some(op) => format!("cmd appops get {package} {op}"),
        None => format!("cmd appops get {package}"),
    };
    let out = adb::shell(serial, cmd).await?;
    emit(
        "appop_get",
        serde_json::json!({ "package": package, "ops": parse_appops(&out) }),
    );
    Ok(())
}

pub async fn appop_set(serial: &str, package: &str, op: &str, mode: &str) -> Result<()> {
    let _ = adb::shell(serial, format!("cmd appops set {package} {op} {mode}")).await;
    let out = adb::shell(serial, format!("cmd appops get {package} {op}")).await?;
    let now = parse_appops(&out).get(op).cloned();
    emit(
        "appop_set",
        serde_json::json!({
            "package": package,
            "op": op,
            "mode_requested": mode,
            "now": now,
        }),
    );
    Ok(())
}

// ── parsers (pure; unit-tested) ─────────────────────────────────────────────

/// Extract `<permission> -> granted` from the **runtime permissions** blocks of
/// `dumpsys package <pkg>`. Install/normal permissions are skipped (they aren't
/// grant/revoke-able). When a permission appears in more than one runtime block
/// (e.g. a legacy top-level block and the per-user one), the later occurrence
/// wins.
fn parse_runtime_perms(dumpsys: &str) -> BTreeMap<String, bool> {
    let mut map = BTreeMap::new();
    let mut in_runtime = false;
    for line in dumpsys.lines() {
        let t = line.trim();
        // Section headers look like "runtime permissions:", "install
        // permissions:", "requested permissions:", "declared permissions:".
        if t.ends_with("permissions:") {
            in_runtime = t == "runtime permissions:";
            continue;
        }
        if !in_runtime {
            continue;
        }
        // Perm line: "android.permission.CAMERA: granted=true, flags=[ ... ]"
        if let Some((perm, rest)) = t.split_once(": granted=") {
            if !perm.is_empty() {
                map.insert(perm.to_string(), rest.starts_with("true"));
            }
        }
    }
    map
}

/// Parse `cmd appops get` output into `<OP> -> mode`. Lines look like
/// `CAMERA: allow; time=... ago` or `RECORD_AUDIO: deny`. Non-op lines
/// (attribution detail, "Uid mode:", etc.) are skipped by requiring the key to
/// be an UPPER_SNAKE op name.
fn parse_appops(out: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in out.lines() {
        let Some((op, rest)) = line.trim().split_once(':') else {
            continue;
        };
        let op = op.trim();
        if op.is_empty()
            || !op
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        {
            continue;
        }
        // mode is the first token after the colon, before any "; time=..." tail.
        let mode = rest
            .trim()
            .split([';', ' '])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !mode.is_empty() {
            map.insert(op.to_string(), mode);
        }
    }
    map
}

// ── output ──────────────────────────────────────────────────────────────────

/// Emit one `{"type":"action","cmd":…}` result line — thin adapter over the
/// shared [`crate::events::emit_action`].
fn emit(cmd: &str, body: serde_json::Value) {
    crate::events::emit_action(cmd, &body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_perms_only() {
        let dump = "\
  Package [com.example] (abc):
    install permissions:
      android.permission.INTERNET: granted=true
    User 0: ceDataInode=12345 installed=true
      gids=[3003]
      runtime permissions:
        android.permission.POST_NOTIFICATIONS: granted=true, flags=[ USER_SET ]
        android.permission.CAMERA: granted=false, flags=[ ]
        android.permission.ACCESS_FINE_LOCATION: granted=true, flags=[ USER_SENSITIVE_WHEN_GRANTED ]
";
        let m = parse_runtime_perms(dump);
        // install perm excluded
        assert_eq!(m.get("android.permission.INTERNET"), None);
        assert_eq!(m.get("android.permission.POST_NOTIFICATIONS"), Some(&true));
        assert_eq!(m.get("android.permission.CAMERA"), Some(&false));
        assert_eq!(
            m.get("android.permission.ACCESS_FINE_LOCATION"),
            Some(&true)
        );
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn parses_appops() {
        let out = "\
CAMERA: allow; time=+1d0h0m0s ago
RECORD_AUDIO: deny
COARSE_LOCATION: ignore; rejectTime=...
  Reject count: 3
Uid mode: foo";
        let m = parse_appops(out);
        assert_eq!(m.get("CAMERA"), Some(&"allow".to_string()));
        assert_eq!(m.get("RECORD_AUDIO"), Some(&"deny".to_string()));
        assert_eq!(m.get("COARSE_LOCATION"), Some(&"ignore".to_string()));
        // "Reject count" and "Uid mode" are not ops
        assert_eq!(m.len(), 3);
    }
}
