//! Every `fault` failure is a typed `fault_*` code with stage `fault`, a
//! retryable flag, and concrete next actions.

use crate::diagnostic::DiagnosticError;
use serde_json::{Value, json};

fn err(code: &'static str, message: impl Into<String>, retryable: bool) -> DiagnosticError {
    DiagnosticError::new(code, "fault", message).retryable(retryable)
}

/// The device is already in the state the fault would create.
pub fn no_effect(message: impl Into<String>) -> anyhow::Error {
    err("fault_no_effect", message, false)
        .next_actions(["shadowdroid fault list", "shadowdroid fault kinds"])
        .into()
}

pub fn invalid_param(message: impl Into<String>) -> anyhow::Error {
    err("fault_invalid_param", message, false)
        .next_actions(["shadowdroid commands --json --describe 'fault inject'"])
        .into()
}

/// The fault was applied but its effect could not be observed; it was rolled back.
pub fn verification_failed(message: impl Into<String>) -> anyhow::Error {
    err("fault_verification_failed", message, true)
        .next_actions(["shadowdroid fault list", "shadowdroid doctor"])
        .into()
}

pub fn device_rejected(output: &str, detail: Value) -> anyhow::Error {
    let mut detail = detail;
    detail["device_output"] = json!(output.trim().chars().take(600).collect::<String>());
    let hint = detail["hint"].as_str().map(str::to_string);
    let mut error = err(
        "fault_device_rejected",
        "the device refused the fault",
        false,
    )
    .detail(detail);
    if let Some(hint) = hint {
        error = error.next_actions([hint]);
    }
    error.into()
}

pub fn app_not_running(serial: &str, package: &str) -> anyhow::Error {
    err(
        "fault_app_not_running",
        format!("{package} is not running"),
        false,
    )
    .detail(json!({"app": package}))
    .next_actions([format!("shadowdroid -d {serial} app start {package}")])
    .into()
}

pub fn app_not_installed(serial: &str, package: &str) -> anyhow::Error {
    err(
        "fault_app_not_installed",
        format!("{package} is not installed on {serial}"),
        false,
    )
    .detail(json!({"app": package}))
    .next_actions([format!("shadowdroid -d {serial} app install <apk>")])
    .into()
}

pub fn requires_app(kind: &str) -> anyhow::Error {
    err(
        "fault_requires_app",
        format!("{kind} needs an app: pass --app <package> or configure a default app"),
        false,
    )
    .next_actions([format!("shadowdroid fault inject {kind} --app <package>")])
    .into()
}

pub fn requires_emulator(kind: &str) -> anyhow::Error {
    err(
        "fault_requires_emulator",
        format!("{kind} drives the emulator console, so it needs an emulator"),
        false,
    )
    .next_actions(["shadowdroid devices", "shadowdroid fault kinds"])
    .into()
}

pub fn requires_allow(kind: &str) -> anyhow::Error {
    err(
        "fault_requires_allow_physical",
        format!("{kind} affects everything on a physical device; re-run with --allow-physical to accept that"),
        false,
    )
    .next_actions([format!("shadowdroid fault inject {kind} --allow-physical …")])
    .into()
}

pub fn unsupported_api(kind: &str, sdk: u32, min_sdk: u32) -> anyhow::Error {
    err(
        "fault_unsupported_api",
        format!("{kind} needs Android API {min_sdk}+; this device runs API {sdk}"),
        false,
    )
    .detail(json!({"sdk": sdk, "min_sdk": min_sdk}))
    .next_actions(["shadowdroid fault kinds"])
    .into()
}

pub fn requires_proxy(serial: &str, kind: &str) -> anyhow::Error {
    err(
        "fault_requires_proxy",
        format!("{kind} runs in the ShadowDroid proxy, which is not running for {serial}"),
        false,
    )
    .next_actions([
        format!("shadowdroid -d {serial} net start"),
        format!("shadowdroid -d {serial} net check <app>"),
    ])
    .into()
}

pub fn proxy_unsupported(serial: &str) -> anyhow::Error {
    err(
        "fault_proxy_outdated",
        "the running proxy predates traffic faults; restart it",
        false,
    )
    .next_actions([
        format!("shadowdroid -d {serial} net stop"),
        format!("shadowdroid -d {serial} net start"),
    ])
    .into()
}

pub fn conflict(
    serial: &str,
    kind: &str,
    active_id: &str,
    active_kind: &str,
    resource: &str,
) -> anyhow::Error {
    err(
        "fault_conflict",
        format!("{kind} would change {resource}, which active fault {active_id} ({active_kind}) already controls"),
        false,
    )
    .detail(json!({"active_fault": {"id": active_id, "kind": active_kind}, "resource": resource}))
    .next_actions([format!("shadowdroid -d {serial} fault clear {active_id}")])
    .into()
}

pub fn not_found(serial: &str, id: &str) -> anyhow::Error {
    err(
        "fault_not_found",
        format!("no active fault {id} on {serial}"),
        false,
    )
    .next_actions([format!("shadowdroid -d {serial} fault list")])
    .into()
}

pub fn restore_failed(serial: &str, failed: Value) -> anyhow::Error {
    let ids: Vec<String> = failed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|fault| fault["id"].as_str().map(str::to_string))
        .collect();
    err(
        "fault_restore_failed",
        "some faults could not be verified as undone; they stay listed so the clear can be retried",
        true,
    )
    .detail(json!({"failed": failed}))
    .next_actions(
        ids.iter()
            .map(|id| format!("shadowdroid -d {serial} fault clear {id}"))
            .chain([format!("shadowdroid -d {serial} fault list")]),
    )
    .into()
}

pub fn scenario_invalid(message: impl Into<String>) -> anyhow::Error {
    err("fault_scenario_invalid", message, false)
        .next_actions(["shadowdroid commands --guide faults"])
        .into()
}
