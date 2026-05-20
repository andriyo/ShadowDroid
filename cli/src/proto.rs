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
    #[serde(default)]
    pub clickable: bool,
    #[serde(default)]
    pub long_clickable: bool,
    #[serde(default)]
    pub scrollable: bool,
    #[serde(default)]
    pub checkable: bool,
    #[serde(default)]
    pub focusable: bool,
    #[serde(default = "_true")]
    pub enabled: bool,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub password: bool,
    #[serde(default)]
    pub input: bool,
}

fn _true() -> bool {
    true
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
