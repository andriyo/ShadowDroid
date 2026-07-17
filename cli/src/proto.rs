//! Wire types for the on-device HTTP API.
//!
//! Kept deliberately small — only what we actually serialise on the wire.

use serde::{Deserialize, Serialize};

// ── /v1/state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerState {
    pub server_version: String,
    pub api_version: String,
    pub ui_automator_version: String,
    pub android_sdk: u32,
    pub android_release: String,
    pub viewport: Viewport,
    pub current_app: AppRef,
    /// True on leanback / Android TV devices, where the UI is focus + D-pad
    /// driven. Agents should use `ui focus` / `ui key dpad_*` instead of
    /// coordinate/selector taps. `default` keeps back-compat with older servers
    /// that predate this field.
    #[serde(default)]
    pub is_television: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Viewport {
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRef {
    pub package: Option<String>,
    pub activity: Option<String>,
    pub pid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at_ms: Option<u64>,
}

// ── /v1/screen ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenResponse {
    pub screen_hash: String,
    /// Canonical screen-identity schema. Servers predating the versioned,
    /// length-delimited hash are interpreted as v1.
    #[serde(default = "default_screen_hash_version")]
    pub screen_hash_version: u32,
    /// Explicit strict-content identity. New servers expose `c:<screen_hash>`;
    /// `screen_hash` remains unchanged for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// Actionable structure identity, excluding explicitly volatile content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_hash: Option<String>,
    #[serde(default = "default_interaction_hash_version")]
    pub interaction_hash_version: u32,
    #[serde(default = "default_snapshot_state")]
    pub snapshot_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at_ms: Option<u64>,
    pub viewport: Viewport,
    pub current_app: AppRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_tree: Option<UiTreeSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub element_count: u32,
    #[serde(default, skip_serializing_if = "ImeState::is_empty")]
    pub ime: ImeState,
    pub elements: Vec<Element>,
}

const fn default_screen_hash_version() -> u32 {
    1
}

const fn default_interaction_hash_version() -> u32 {
    1
}

fn default_snapshot_state() -> String {
    "unknown".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiTreeSnapshot {
    pub sampled_at_ms: u64,
    pub age_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StableScreenResponse {
    pub stable: bool,
    pub settle_ms: u64,
    pub quiet_period_ms: u64,
    pub screen: ScreenResponse,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImeState {
    #[serde(default)]
    pub keyboard_visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_element: Option<Element>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_input: Option<Element>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_actions: Vec<String>,
}

impl ImeState {
    pub fn is_empty(&self) -> bool {
        !self.keyboard_visible
            && self.focused_element.is_none()
            && self.focused_input.is_none()
            && self.detection.is_none()
            && self.reason.is_none()
            && self.suggested_actions.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Element {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub klass: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
    #[serde(default)]
    pub bounds: Option<[i32; 4]>,
    #[serde(default)]
    pub tap: Option<[i32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<RangeSemantics>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    // Flags are omitted from serialized `screen --full` / `find --full` output
    // when they hold their default, so the agent only pays tokens for what's
    // set. `enabled` defaults to true, so it's the inverse — emitted only when an
    // element is disabled. Deserialization is unaffected (the server, with
    // encodeDefaults, sends every flag).
    #[serde(default, skip_serializing_if = "is_false")]
    pub clickable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub long_clickable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub scrollable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub checkable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub focusable: bool,
    #[serde(default = "_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub checked: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub focused: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub password: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub input: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeSemantics {
    #[serde(rename = "type")]
    pub kind: String,
    pub min: f32,
    pub max: f32,
    pub current: f32,
    #[serde(default)]
    pub step: serde_json::Value,
}

fn _true() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_true(b: &bool) -> bool {
    *b
}

// ── /v1/app/* ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct AppStartResp {
    #[serde(default = "_true")]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default)]
    pub launcher_activities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppInfo {
    pub version_name: Option<String>,
    pub version_code: i32,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppWaitResp {
    pub matched: bool,
    pub current: Option<String>,
}

// ── /v1/orientation, /v1/clipboard, /v1/shell ───────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct OrientationResp {
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClipResp {
    pub value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShellResp {
    pub input: String,
    pub output: String,
    pub exit_code: Option<i32>,
}

// ── /v1/find, /v1/find_tap, /v1/xpath ────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SelectorQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub klass: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xpath: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub exact: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clickable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub coordinate_fallback: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindResp {
    pub matched: Option<Element>,
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindTapResp {
    pub matched: Element,
    #[serde(default)]
    pub activated_element: Option<Element>,
    #[serde(default)]
    pub actionable_resolved: Option<bool>,
    #[serde(default)]
    pub input_delivered: Option<bool>,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetProgressResp {
    pub matched: Element,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_before: Option<RangeSemantics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_after: Option<RangeSemantics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_value: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<f32>,
    pub verified: bool,
    pub target_reached: bool,
    #[serde(default)]
    pub control_quantized: bool,
    pub input_delivered: bool,
    pub action: String,
    pub coordinate_fallback: bool,
    pub expected_precision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScrollResp {
    pub matched: bool,
    pub x: i32,
    pub y: i32,
    pub swipes: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct XpathReq {
    pub query: String,
    pub tap: bool,
    #[serde(default)]
    pub coordinate_fallback: bool,
}

// ── /v1/toast/* ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ToastRecentResp {
    pub toasts: Vec<ToastEvent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToastEvent {
    pub package: Option<String>,
    pub text: String,
    pub ts: u64,
}

// ── /v1/files/* ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct FileWriteResp {
    pub path: String,
    pub bytes: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileListResp {
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

// ── /v1/device ───────────────────────────────────────────────────────────────

/// One-shot detailed device info from `GET /v1/device`. Older servers (before
/// this route existed) return 404; the CLI falls back to `/v1/state` + getprop.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub brand: String,
    pub device: String,
    pub product: String,
    pub fingerprint: String,
    pub android_release: String,
    pub android_sdk: u32,
    pub locale: String,
    pub density_dpi: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OkResponse {
    #[serde(default = "_true")]
    pub ok: bool,
}

// ── error envelope ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Element {
        Element {
            id: 3,
            handle: None,
            text: Some("Go".into()),
            desc: None,
            klass: None,
            rid: None,
            bounds: Some([0, 0, 10, 10]),
            tap: Some([5, 5]),
            range: None,
            actions: Vec::new(),
            clickable: true,
            long_clickable: false,
            scrollable: false,
            checkable: false,
            focusable: false,
            enabled: true,
            selected: false,
            checked: false,
            focused: false,
            password: false,
            input: false,
        }
    }

    #[test]
    fn screen_response_keeps_ime_backward_compatible() {
        let body = r#"{
            "screen_hash":"abc",
            "viewport":{"w":1,"h":2},
            "current_app":{},
            "element_count":0,
            "elements":[]
        }"#;
        let screen: ScreenResponse = serde_json::from_str(body).unwrap();
        assert!(screen.ime.is_empty());
        assert_eq!(screen.screen_hash_version, 1);
        assert!(screen.content_hash.is_none());
        assert!(screen.interaction_hash.is_none());
        assert_eq!(screen.interaction_hash_version, 1);
        assert_eq!(screen.snapshot_state, "unknown");
        assert!(screen.captured_at_ms.is_none());
        assert!(screen.ui_tree.is_none());

        let json = serde_json::to_string(&screen).unwrap();
        assert!(!json.contains("\"ime\""), "{json}");
    }

    #[test]
    fn serialized_element_drops_default_flags() {
        let json = serde_json::to_string(&sample()).unwrap();
        // Truthy flags stay…
        assert!(json.contains("\"clickable\":true"), "{json}");
        // …falsy flags and the redundant enabled:true are omitted.
        assert!(!json.contains("long_clickable"), "{json}");
        assert!(!json.contains("focusable"), "{json}");
        assert!(!json.contains("\"input\""), "{json}");
        assert!(!json.contains("\"enabled\""), "{json}");
    }

    #[test]
    fn disabled_element_still_serializes_enabled_false() {
        let mut el = sample();
        el.enabled = false;
        let json = serde_json::to_string(&el).unwrap();
        assert!(json.contains("\"enabled\":false"), "{json}");
    }

    #[test]
    fn serialized_element_round_trips_through_defaults() {
        // The on-device server sends every flag; the compacted form we emit must
        // still deserialize back to the same element.
        let el = sample();
        let json = serde_json::to_string(&el).unwrap();
        let back: Element = serde_json::from_str(&json).unwrap();
        assert_eq!(back.enabled, el.enabled);
        assert_eq!(back.clickable, el.clickable);
        assert_eq!(back.input, el.input);
    }
}
