//! Wire types for the on-device HTTP API. Mirrors docs/protocol.md.
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
}

// ── /v1/screen ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenResponse {
    pub screen_hash: String,
    pub viewport: Viewport,
    pub current_app: AppRef,
    pub element_count: u32,
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Element {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub klass: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
    pub bounds: [i32; 4],
    pub tap: [i32; 2],
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindResp {
    pub matched: Option<Element>,
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FindTapResp {
    pub matched: Element,
    pub x: i32,
    pub y: i32,
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
            text: Some("Go".into()),
            desc: None,
            klass: None,
            rid: None,
            bounds: [0, 0, 10, 10],
            tap: [5, 5],
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
