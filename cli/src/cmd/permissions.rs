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
//! a mutation only succeeds when the requested postcondition is present in the
//! readback. An unmet state is a typed, non-zero failure with the observation.

use crate::ids::Serial;
use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::config::{
    quote_device_shell_arg, validate_android_app_op, validate_android_app_op_mode,
    validate_android_package, validate_android_permission,
};
use crate::device::adb;

// ── runtime permissions ─────────────────────────────────────────────────────

pub async fn grant(serial: &Serial, package: &str, perms: &[String]) -> Result<()> {
    change(serial, package, perms, "grant", "perm_grant").await
}

pub async fn revoke(serial: &Serial, package: &str, perms: &[String]) -> Result<()> {
    change(serial, package, perms, "revoke", "perm_revoke").await
}

/// Shared grant/revoke body: snapshot the requested perms, run `pm <verb>` for
/// each, then re-read and report `now` (observed state of the requested perms)
/// and `changed` (those that actually flipped).
async fn change(
    serial: &Serial,
    package: &str,
    perms: &[String],
    pm_verb: &str,
    cmd: &str,
) -> Result<()> {
    let package_arg = checked_package_arg(package)?;
    let permission_args = checked_permission_args(perms)?;
    let before = runtime_perms(serial, package).await?;
    for permission_arg in &permission_args {
        // stdout is normally empty on success; pm prints errors to stderr, which
        // we deliberately ignore in favour of the readback below.
        let _ = adb::shell(
            serial,
            format!("pm {pm_verb} {package_arg} {permission_arg}"),
        )
        .await;
    }
    let after = runtime_perms(serial, package).await?;

    let now: BTreeMap<&String, Option<bool>> =
        perms.iter().map(|p| (p, after.get(p).copied())).collect();
    let changed: Vec<&String> = perms
        .iter()
        .filter(|p| before.get(*p) != after.get(*p))
        .collect();

    let expected = pm_verb == "grant";
    let unmet = permission_postcondition_failures(perms, &after, expected);
    let detail = serde_json::json!({
        "package": package,
        "requested": perms,
        "expected_granted": expected,
        "changed": changed,
        "now": now,
        "unmet": unmet,
    });
    if !unmet.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "permission_postcondition_failed",
            "permission",
            format!(
                "{} permission(s) did not reach the requested {pm_verb} state",
                unmet.len()
            ),
        )
        .detail(detail)
        .next_actions([
            "inspect detail.unmet for permissions that are absent, fixed by policy, or not runtime-grantable",
            "run `shadowdroid perm list <package>` after correcting app manifest/device policy, then retry",
        ])
        .into());
    }

    emit(cmd, detail);
    Ok(())
}

pub async fn list(serial: &Serial, package: &str) -> Result<()> {
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
pub async fn reset(serial: &Serial, package: &str) -> Result<()> {
    let package_arg = checked_package_arg(package)?;
    let before = runtime_perms(serial, package).await?;
    let granted: Vec<&String> = before.iter().filter(|&(_, &g)| g).map(|(p, _)| p).collect();
    for p in &granted {
        validate_android_permission(p)
            .with_context(|| format!("invalid permission returned for {package:?}: {p:?}"))?;
        let permission_arg = quote_device_shell_arg(p);
        let _ = adb::shell(serial, format!("pm revoke {package_arg} {permission_arg}")).await;
    }
    let after = runtime_perms(serial, package).await?;
    let revoked: Vec<&String> = granted
        .iter()
        .copied()
        .filter(|p| after.get(*p) == Some(&false))
        .collect();
    let remaining: Vec<&String> = granted
        .iter()
        .copied()
        .filter(|permission| after.get(*permission) != Some(&false))
        .collect();
    let detail = serde_json::json!({
        "package": package,
        "requested_reset": granted,
        "revoked": revoked,
        "remaining_granted_or_unobserved": remaining,
        "now": after,
    });
    if !remaining.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "permission_reset_incomplete",
            "permission",
            format!("{} permission(s) remained granted or could not be observed", remaining.len()),
        )
        .detail(detail)
        .next_actions([
            "inspect detail.remaining_granted_or_unobserved for policy-fixed permissions",
            "remove the device policy or reinstall/clear the app, then rerun `shadowdroid perm reset`",
        ])
        .into());
    }
    emit("perm_reset", detail);
    Ok(())
}

pub async fn runtime_perms(serial: &Serial, package: &str) -> Result<BTreeMap<String, bool>> {
    let package_arg = checked_package_arg(package)?;
    let dump = adb::shell(serial, format!("dumpsys package {package_arg}")).await?;
    Ok(parse_runtime_perms(&dump))
}

/// Grant `perms` without emitting an action line — for composition by other
/// verbs (e.g. `app-install --grant-all`). Returns the post-grant runtime-perm
/// state so the caller can report what actually took.
pub async fn grant_quiet(
    serial: &Serial,
    package: &str,
    perms: &[String],
) -> Result<BTreeMap<String, bool>> {
    let package_arg = checked_package_arg(package)?;
    let permission_args = checked_permission_args(perms)?;
    for permission_arg in permission_args {
        let _ = adb::shell(serial, format!("pm grant {package_arg} {permission_arg}")).await;
    }
    runtime_perms(serial, package).await
}

// ── appops ──────────────────────────────────────────────────────────────────

/// Android stores app-op policy at two independent scopes. UID modes govern
/// when present; package modes are otherwise effective. Requiring the caller
/// to choose makes mutation semantics stable across Android releases.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AppOpScope {
    Uid,
    Package,
}

impl AppOpScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Uid => "uid",
            Self::Package => "package",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct AppOpObservation {
    uid_modes: BTreeMap<String, String>,
    package_modes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
struct AppOpState {
    uid_mode: Option<String>,
    package_mode: Option<String>,
    governing_scope: Option<&'static str>,
    effective_mode: Option<String>,
}

impl AppOpObservation {
    fn states(&self) -> BTreeMap<String, AppOpState> {
        let mut names = self
            .uid_modes
            .keys()
            .chain(self.package_modes.keys())
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let uid_mode = self.uid_modes.get(&name).cloned();
                let package_mode = self.package_modes.get(&name).cloned();
                let (governing_scope, effective_mode) = if let Some(mode) = &uid_mode {
                    (Some("uid"), Some(mode.clone()))
                } else if let Some(mode) = &package_mode {
                    (Some("package"), Some(mode.clone()))
                } else {
                    (None, None)
                };
                (
                    name,
                    AppOpState {
                        uid_mode,
                        package_mode,
                        governing_scope,
                        effective_mode,
                    },
                )
            })
            .collect()
    }

    fn resolved_name(&self, requested: &str) -> Option<String> {
        let states = self.states();
        if states.contains_key(requested) {
            return Some(requested.to_string());
        }
        if let Some(canonical) = canonical_appop_name(requested)
            && states.contains_key(&canonical)
        {
            return Some(canonical);
        }
        // Numeric ids are rendered back by Android as their debug name. A
        // scoped observation groups UID/package rows first, so one remaining
        // operation is unambiguous without crossing scopes.
        (requested.chars().all(|c| c.is_ascii_digit()) && states.len() == 1)
            .then(|| states.into_keys().next())
            .flatten()
    }

    fn state_for(&self, requested: &str) -> Option<(String, AppOpState)> {
        let name = self.resolved_name(requested)?;
        self.states().remove(&name).map(|state| (name, state))
    }
}

pub async fn appop_get(serial: &Serial, package: &str, op: Option<&str>) -> Result<()> {
    let package_arg = checked_package_arg(package)?;
    let cmd = match op {
        Some(op) => {
            validate_android_app_op(op)
                .with_context(|| format!("invalid app-op identifier {op:?}"))?;
            format!(
                "cmd appops get {package_arg} {}",
                quote_device_shell_arg(op)
            )
        }
        None => format!("cmd appops get {package_arg}"),
    };
    let out = adb::shell(serial, cmd).await?;
    let observation = parse_appops(&out);
    emit(
        "appop_get",
        serde_json::json!({
            "package": package,
            "requested_op": op,
            "ops": observation.states(),
            "precedence": "uid mode governs when present; otherwise package mode governs",
        }),
    );
    Ok(())
}

pub async fn appop_set(
    serial: &Serial,
    package: &str,
    op: &str,
    mode: &str,
    scope: AppOpScope,
) -> Result<()> {
    let package_arg = checked_package_arg(package)?;
    validate_android_app_op(op).with_context(|| format!("invalid app-op identifier {op:?}"))?;
    validate_android_app_op_mode(mode).with_context(|| format!("invalid app-op mode {mode:?}"))?;
    let op_arg = quote_device_shell_arg(op);
    let mode_arg = quote_device_shell_arg(mode);
    let before_out = adb::shell(serial, format!("cmd appops get {package_arg} {op_arg}")).await?;
    let before = parse_appops(&before_out);
    let scope_flag = match scope {
        AppOpScope::Uid => "--uid ",
        AppOpScope::Package => "",
    };
    adb::shell(
        serial,
        format!("cmd appops set {scope_flag}{package_arg} {op_arg} {mode_arg}"),
    )
    .await?;
    let out = adb::shell(serial, format!("cmd appops get {package_arg} {op_arg}")).await?;
    let after = parse_appops(&out);
    let before_state = before.state_for(op);
    let after_state = after.state_for(op);
    let resolved_op = after_state
        .as_ref()
        .map(|(name, _)| name.clone())
        .or_else(|| before_state.as_ref().map(|(name, _)| name.clone()))
        .or_else(|| canonical_appop_name(op))
        .unwrap_or_else(|| op.to_string());
    let now = after_state.as_ref().and_then(|(_, state)| match scope {
        AppOpScope::Uid => state.uid_mode.clone(),
        AppOpScope::Package => state.package_mode.clone(),
    });
    let before_effective = before_state
        .as_ref()
        .and_then(|(_, state)| state.effective_mode.clone());
    let after_effective = after_state
        .as_ref()
        .and_then(|(_, state)| state.effective_mode.clone());
    let detail = serde_json::json!({
        "package": package,
        "op": op,
        "resolved_op": resolved_op,
        "requested_scope": scope.as_str(),
        "requested_mode": mode,
        "observed_scope_mode": now,
        "before": before_state.map(|(_, state)| state),
        "after": after_state.map(|(_, state)| state),
        "effective_changed": before_effective != after_effective,
        "postcondition_met": now.as_deref() == Some(mode),
    });
    if now.as_deref() != Some(mode) {
        return Err(crate::diagnostic::DiagnosticError::new(
            "appop_postcondition_failed",
            "appop",
            format!(
                "app-op {op} at {} scope did not reach requested mode {mode}",
                scope.as_str()
            ),
        )
        .detail(detail)
        .next_actions([
            "inspect detail.after and detail.observed_scope_mode for the mode Android accepted or normalized",
            "run `shadowdroid appops get <package> <op>`; choose the governing scope explicitly or correct permission/device policy, then retry",
        ])
        .into());
    }
    emit("appop_set", detail);
    Ok(())
}

fn permission_postcondition_failures(
    permissions: &[String],
    observed: &BTreeMap<String, bool>,
    expected: bool,
) -> Vec<serde_json::Value> {
    permissions
        .iter()
        .filter_map(|permission| {
            let actual = observed.get(permission).copied();
            (actual != Some(expected)).then(|| {
                serde_json::json!({
                    "permission": permission,
                    "expected_granted": expected,
                    "observed_granted": actual,
                })
            })
        })
        .collect()
}

fn canonical_appop_name(op: &str) -> Option<String> {
    let (_, public_name) = op.split_once(':')?;
    Some(
        public_name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect(),
    )
}

fn checked_package_arg(package: &str) -> Result<String> {
    validate_android_package(package)
        .with_context(|| format!("invalid Android package {package:?}"))?;
    Ok(quote_device_shell_arg(package))
}

fn checked_permission_args(perms: &[String]) -> Result<Vec<String>> {
    perms
        .iter()
        .map(|permission| {
            validate_android_permission(permission)
                .with_context(|| format!("invalid Android permission {permission:?}"))?;
            Ok(quote_device_shell_arg(permission))
        })
        .collect()
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
        if let Some((perm, rest)) = t.split_once(": granted=")
            && !perm.is_empty()
        {
            map.insert(perm.to_string(), rest.starts_with("true"));
        }
    }
    map
}

/// Parse `cmd appops get` without collapsing UID and package modes. Android 16
/// can return both `Uid mode: CAMERA: foreground` and `CAMERA: allow`; the UID
/// row governs, so last-write-wins parsing can produce a dangerous false
/// success depending only on output order.
fn parse_appops(out: &str) -> AppOpObservation {
    let mut observation = AppOpObservation::default();
    for line in out.lines() {
        let trimmed = line.trim();
        let (uid_scoped, candidate) = match trimmed.strip_prefix("Uid mode: ") {
            Some(candidate) => (true, candidate),
            None => (false, trimmed),
        };
        let Some((op, rest)) = candidate.split_once(':') else {
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
            if uid_scoped {
                observation.uid_modes.insert(op.to_string(), mode);
            } else {
                observation.package_modes.insert(op.to_string(), mode);
            }
        }
    }
    observation
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
    fn parses_appops_without_collapsing_uid_and_package_scopes() {
        let out = "\
CAMERA: allow; time=+1d0h0m0s ago
RECORD_AUDIO: deny
COARSE_LOCATION: ignore; rejectTime=...
  Reject count: 3
Uid mode: CAMERA: foreground
Uid mode: foo";
        let observation = parse_appops(out);
        assert_eq!(
            observation.uid_modes.get("CAMERA"),
            Some(&"foreground".to_string())
        );
        assert_eq!(
            observation.package_modes.get("CAMERA"),
            Some(&"allow".to_string())
        );
        assert_eq!(
            observation.package_modes.get("RECORD_AUDIO"),
            Some(&"deny".to_string())
        );
        assert_eq!(
            observation.package_modes.get("COARSE_LOCATION"),
            Some(&"ignore".to_string())
        );
        let state = observation.states().remove("CAMERA").unwrap();
        assert_eq!(state.governing_scope, Some("uid"));
        assert_eq!(state.effective_mode.as_deref(), Some("foreground"));
    }

    #[test]
    fn appop_governing_mode_is_independent_of_android_output_order() {
        for output in [
            "Uid mode: CAMERA: foreground\nCAMERA: allow\n",
            "CAMERA: allow\nUid mode: CAMERA: foreground\n",
        ] {
            let state = parse_appops(output).states().remove("CAMERA").unwrap();
            assert_eq!(state.uid_mode.as_deref(), Some("foreground"));
            assert_eq!(state.package_mode.as_deref(), Some("allow"));
            assert_eq!(state.governing_scope, Some("uid"));
            assert_eq!(state.effective_mode.as_deref(), Some("foreground"));
        }
    }

    #[test]
    fn rejects_injected_permission_inputs_before_building_shell_arguments() {
        for package in [
            "com.example;id",
            "com.example\nother",
            "com.$(id)",
            "com.'example'",
        ] {
            assert!(
                checked_package_arg(package).is_err(),
                "accepted {package:?}"
            );
        }
        for permission in [
            "android.permission.CAMERA;id",
            "android.permission.CAMERA\nNEXT",
            "android.permission.$(id)",
            "android.permission.'CAMERA'",
        ] {
            assert!(
                checked_permission_args(&[permission.to_string()]).is_err(),
                "accepted {permission:?}"
            );
        }
    }

    #[test]
    fn quotes_valid_permission_inputs() {
        assert_eq!(
            checked_package_arg("com.example.app").unwrap(),
            "'com.example.app'"
        );
        assert_eq!(
            checked_permission_args(&["android.permission.CAMERA".into()]).unwrap(),
            vec!["'android.permission.CAMERA'".to_string()]
        );
    }

    #[test]
    fn postcondition_reports_absent_and_wrong_permission_states() {
        let observed = BTreeMap::from([
            ("android.permission.CAMERA".to_string(), true),
            ("android.permission.RECORD_AUDIO".to_string(), false),
        ]);
        let requested = vec![
            "android.permission.CAMERA".to_string(),
            "android.permission.RECORD_AUDIO".to_string(),
            "android.permission.POST_NOTIFICATIONS".to_string(),
        ];
        let failures = permission_postcondition_failures(&requested, &observed, true);
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["observed_granted"], false);
        assert!(failures[1]["observed_granted"].is_null());
    }

    #[test]
    fn public_appop_name_matches_android_readback_name() {
        assert_eq!(
            canonical_appop_name("android:camera").as_deref(),
            Some("CAMERA")
        );
        assert_eq!(
            canonical_appop_name("android:read-clipboard").as_deref(),
            Some("READ_CLIPBOARD")
        );
        assert_eq!(canonical_appop_name("CAMERA"), None);
    }
}
