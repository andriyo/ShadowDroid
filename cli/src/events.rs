//! The JSON-line events the CLI prints to stdout. The agent-facing contract.
//!
//! IMPORTANT: this shape is the public API. Keep it stable so existing watch
//! stream consumers and generated agent integrations continue to work.

use crate::proto::{AppRef, Element, ImeState, RangeSemantics, ScreenResponse, Viewport};
use serde::Serialize;
use std::fmt;
use std::io::Write;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_NEXT_ACTIONS: usize = 5;

/// Canonical clap command path for this one-shot CLI process (`ui dump`,
/// `app start`, …). Set once immediately after successful parsing so every
/// terminal emitter can attach the same catalog-backed decision guidance.
static CURRENT_COMMAND_PATH: OnceLock<String> = OnceLock::new();
static CURRENT_DEVICE: OnceLock<String> = OnceLock::new();

pub fn set_current_command_path(path: String) {
    let _ = CURRENT_COMMAND_PATH.set(path);
}

pub fn current_command_path() -> Option<&'static str> {
    CURRENT_COMMAND_PATH.get().map(String::as_str)
}

pub fn set_current_device(device: String) {
    if !device.is_empty() {
        let _ = CURRENT_DEVICE.set(device);
    }
}

fn current_device() -> Option<&'static str> {
    CURRENT_DEVICE.get().map(String::as_str)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ScreenFormat {
    Full,
    Compact,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready {
        device: String,
        viewport: Viewport,
        server_version: String,
        app_filter: Option<String>,
        detect_crashes: bool,
        ts: f64,
    },
    Screen {
        ts: f64,
        device: String,
        package: Option<String>,
        activity: Option<String>,
        viewport: Viewport,
        screen_hash: String,
        screen_hash_version: u32,
        content_hash: Option<String>,
        interaction_hash: Option<String>,
        interaction_hash_version: u32,
        element_count: u32,
        ime: ImeState,
        elements: Vec<Element>,
    },
    ScreenCompact {
        ts: f64,
        device: String,
        package: Option<String>,
        activity: Option<String>,
        viewport: Viewport,
        screen_hash: String,
        screen_hash_version: u32,
        content_hash: Option<String>,
        interaction_hash: Option<String>,
        interaction_hash_version: u32,
        element_count: u32,
        ime: CompactIme,
        elements: Vec<CompactElement>,
    },
    Crash(CrashEvent),
    WatcherFired {
        name: String,
        screen_hash: String,
        screen_hash_version: u32,
        matched: Element,
        ts: f64,
    },
    Error {
        stage: String,
        code: String,
        msg: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        retryable: bool,
        detail: serde_json::Value,
        next_actions: Vec<String>,
        ts: f64,
    },
    Warning {
        stage: String,
        code: String,
        msg: String,
        detail: serde_json::Value,
        next_actions: Vec<String>,
        ts: f64,
    },
    /// A completed HTTP(S) transaction through the `net` proxy. Compact by
    /// design — full headers/bodies are fetched on demand via `net show <id>`.
    /// Field shape mirrors the `net` capture wire format so the timeline can
    /// interleave it with `screen`/`crash` events.
    Http {
        ts: f64,
        id: String,
        flow_sequence: u64,
        capture_session_id: String,
        method: String,
        scheme: String,
        host: String,
        path: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        dur_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        req_type: Option<String>,
        req_len: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        resp_type: Option<String>,
        resp_len: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        matched: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rule_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        modified: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        upstream_bypassed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Response body was streamed (SSE/oversized), not captured; `resp_len` is a hint.
        #[serde(default, skip_serializing_if = "is_false")]
        streamed: bool,
        /// Request body was streamed upstream (oversized upload), not captured.
        #[serde(default, skip_serializing_if = "is_false")]
        req_streamed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        redaction_policy: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        redaction_policy_version: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        body_redacted: bool,
        next_actions: Vec<String>,
    },
    /// A flow paused by `net intercept`, awaiting the agent's `net
    /// resume`/`drop`/`respond`. Held until acted on or `hold_deadline_ms`.
    HttpIntercept {
        ts: f64,
        id: String,
        /// "request" (before upstream) or "response" (before returning).
        phase: String,
        method: String,
        scheme: String,
        host: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        req_type: Option<String>,
        req_len: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        resp_type: Option<String>,
        resp_len: u64,
        hold_deadline_ms: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        req_preview: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resp_preview: Option<String>,
        next_actions: Vec<String>,
    },
    /// A TLS handshake the proxy could not complete after presenting a minted
    /// leaf for `host` — almost always the app rejecting the MITM CA (untrusted),
    /// but also cert pinning or a version/cipher mismatch. Emitted so a silent
    /// "no flows captured" turns into a visible, actionable one-liner. Deduped
    /// per host within a session so a retrying client doesn't flood the timeline.
    TlsError {
        ts: f64,
        capture_session_id: String,
        host: String,
        reason: String,
        next_actions: Vec<String>,
    },
    /// A WebSocket connection completed its upgrade (`101`) through the proxy.
    /// One per session; the bidirectional frames arrive as `ws_msg` events and
    /// the teardown as `ws_close`.
    WsOpen {
        ts: f64,
        id: String,
        flow_sequence: u64,
        capture_session_id: String,
        scheme: String,
        host: String,
        path: String,
        url: String,
        status: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        subprotocol: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        permessage_deflate: bool,
        next_actions: Vec<String>,
    },
    /// One reassembled WebSocket message (or control frame). Compact by design —
    /// only a bounded `preview` travels; the full payload is fetched via `net
    /// show <id> --body`.
    WsMsg {
        ts: f64,
        id: String,
        session_id: String,
        capture_session_id: String,
        host: String,
        /// `c2s` (app→server) or `s2c` (server→app).
        dir: String,
        seq: u64,
        /// text | binary | ping | pong | close.
        opcode: String,
        /// Application payload length (decompressed).
        len: u64,
        /// On-wire (compressed) length — only when `compressed`.
        #[serde(skip_serializing_if = "Option::is_none")]
        wire_len: Option<u64>,
        #[serde(default, skip_serializing_if = "is_false")]
        compressed: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_code: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        body_redacted: bool,
        next_actions: Vec<String>,
    },
    /// A WebSocket session closed. Carries the close code/reason (when a close
    /// frame was seen) and per-direction message/byte totals.
    WsClose {
        ts: f64,
        id: String,
        session_id: String,
        capture_session_id: String,
        host: String,
        dur_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_code: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        close_initiator: Option<String>,
        c2s_msgs: u64,
        s2c_msgs: u64,
        c2s_bytes: u64,
        s2c_bytes: u64,
        #[serde(default, skip_serializing_if = "is_zero_u64")]
        dropped: u64,
        next_actions: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactElement {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub klass: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap: Option<[i32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<RangeSemantics>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub clickable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub scrollable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub input: bool,
    // Surfaced in the compact dump so agents can see the current D-pad focus on
    // TV/leanback without `--full`. Only emitted when true, so phones (where the
    // shown elements are rarely focused) pay nothing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub focused: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub checked: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactIme {
    pub keyboard_visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_element: Option<CompactElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_input: Option<CompactElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_actions: Vec<String>,
}

impl From<ImeState> for CompactIme {
    fn from(ime: ImeState) -> Self {
        Self {
            keyboard_visible: ime.keyboard_visible,
            focused_element: ime.focused_element.map(CompactElement::from),
            focused_input: ime.focused_input.map(CompactElement::from),
            detection: ime.detection,
            reason: ime.reason,
            suggested_actions: ime.suggested_actions,
        }
    }
}

impl From<Element> for CompactElement {
    fn from(el: Element) -> Self {
        Self {
            id: el.id,
            handle: el.handle,
            text: el.text,
            desc: el.desc,
            rid: el.rid,
            klass: el.klass,
            tap: el.tap,
            range: el.range,
            actions: el.actions,
            clickable: el.clickable,
            scrollable: el.scrollable,
            input: el.input,
            focused: el.focused,
            selected: el.selected,
            checked: el.checked,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct CrashEvent {
    pub kind: String, // "java" | "native" | "anr"
    pub ts: f64,
    pub package: Option<String>,
    pub pid: Option<i32>,
    pub thread: Option<String>,
    pub exception: Option<String>,
    pub message: Option<String>,
    pub stack: Vec<String>,
    pub caused_by: Vec<CausedBy>,
    pub signal: Option<i32>,
    pub signal_name: Option<String>,
    pub backtrace: Vec<String>,
    pub raw: String,
    pub context: Vec<String>,
    pub device_info: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct CausedBy {
    pub exception: String,
    pub message: Option<String>,
    pub stack: Vec<String>,
}

pub fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn screen_event(device: &str, screen: ScreenResponse, format: ScreenFormat) -> Event {
    let AppRef {
        package, activity, ..
    } = screen.current_app;
    match format {
        ScreenFormat::Full => Event::Screen {
            ts: now_ts(),
            device: device.to_string(),
            package,
            activity,
            viewport: screen.viewport,
            screen_hash: screen.screen_hash,
            screen_hash_version: screen.screen_hash_version,
            content_hash: screen.content_hash,
            interaction_hash: screen.interaction_hash,
            interaction_hash_version: screen.interaction_hash_version,
            element_count: screen.element_count,
            ime: screen.ime,
            elements: screen.elements,
        },
        ScreenFormat::Compact => Event::ScreenCompact {
            ts: now_ts(),
            device: device.to_string(),
            package,
            activity,
            viewport: screen.viewport,
            screen_hash: screen.screen_hash,
            screen_hash_version: screen.screen_hash_version,
            content_hash: screen.content_hash,
            interaction_hash: screen.interaction_hash,
            interaction_hash_version: screen.interaction_hash_version,
            element_count: screen.element_count,
            ime: CompactIme::from(screen.ime),
            elements: screen
                .elements
                .into_iter()
                .map(CompactElement::from)
                .collect(),
        },
    }
}

/// The single stdout sink for the agent-facing contract: print one JSON line.
/// Generic so it serves both the typed [`Event`] stream and ad-hoc result
/// objects. A serialization failure (practically impossible for our types)
/// degrades to `{}` rather than panicking the process.
pub fn emit(value: &impl Serialize) {
    let value = serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({}));
    let value = crate::redaction::redact_output_if_active(value);
    write_stdout(
        format_args!(
            "{}",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
        ),
        true,
    );
}

/// Emit one in-stream record while safely applying the stream's device to any
/// recovery commands already present on the event. Unlike [`emit_result`], this
/// does not add terminal/catalog fallbacks to ordinary timeline data.
pub fn emit_stream_event(value: &impl Serialize, device: &str) {
    emit(&stream_event_value(value, device));
}

fn stream_event_value(value: &impl Serialize, device: &str) -> serde_json::Value {
    let mut value = serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(map) = &mut value {
        let mut context = map.clone();
        context.insert("device".into(), serde_json::json!(device));
        if let Some(actions) = map
            .get("next_actions")
            .and_then(serde_json::Value::as_array)
        {
            let actions = actions
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|action| executable_action(action, &context))
                .collect::<Vec<_>>();
            map.insert("next_actions".into(), serde_json::json!(actions));
        }
    }
    value
}

/// Emit a terminal JSON object and guarantee a non-empty `next_actions` list.
/// Use [`emit`] instead for individual JSONL stream events; their terminal
/// action summary carries the follow-up guidance once the stream ends.
pub fn emit_result(value: &impl Serialize) {
    let mut value = serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(map) = &mut value {
        attach_next_actions(map);
    }
    emit(&value);
}

/// Non-panicking stdout sink used by the crate-local `print!`/`println!`
/// macros. In particular, `BrokenPipe` is expected when an agent has already
/// consumed enough output and closes `head`, `jq`, or another pipeline stage.
pub fn write_stdout(args: fmt::Arguments<'_>, newline: bool) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let rendered = args.to_string();
    let rendered = crate::redaction::redact_text_if_active(&rendered);
    if out.write_all(rendered.as_bytes()).is_err() {
        return;
    }
    if newline {
        let _ = out.write_all(b"\n");
    }
}

/// Redaction-aware, non-panicking stderr sink for direct operational messages.
/// Structured result/error JSON remains on stdout; tracing has its own sink.
pub fn write_stderr(args: fmt::Arguments<'_>, newline: bool) {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let rendered = args.to_string();
    let rendered = crate::redaction::redact_text_if_active(&rendered);
    if out.write_all(rendered.as_bytes()).is_err() {
        return;
    }
    if newline {
        let _ = out.write_all(b"\n");
    }
}

/// Crash/ANR events found by the since-last-command probe, staged for the very
/// next envelope this process emits (action, raw read, or error) as an
/// `"events":[…]` key. One-shot: each CLI invocation emits exactly one result
/// object, so the slot is drained by whichever envelope goes out. Resolved
/// values only (the probe is awaited before stashing) — emission stays sync.
static PENDING_EVENTS: std::sync::Mutex<Option<Vec<serde_json::Value>>> =
    std::sync::Mutex::new(None);

/// Stage probe results for the next emitted envelope. Empty input is a no-op.
pub fn stash_events(events: Vec<serde_json::Value>) {
    if events.is_empty() {
        return;
    }
    if let Ok(mut slot) = PENDING_EVENTS.lock() {
        *slot = Some(events);
    }
}

fn take_events() -> Option<Vec<serde_json::Value>> {
    PENDING_EVENTS.lock().ok().and_then(|mut slot| slot.take())
}

/// Inject staged events into an envelope map (no-op when none are staged).
fn attach_events(m: &mut serde_json::Map<String, serde_json::Value>) {
    if let Some(events) = take_events() {
        m.insert("events".into(), serde_json::Value::Array(events));
    }
}

/// Attach staged events to an arbitrary top-level JSON object (used by raw
/// reads like `ui dump`, which don't go through the action envelope).
pub fn attach_events_to(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(m) = value {
        attach_events(m);
    }
}

/// Build the `{"type":"action","cmd":…, …body}` envelope. Split from
/// [`emit_action`] so the contract can be unit-tested without capturing stdout.
fn action_envelope(cmd: &str, body: &serde_json::Value) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let serde_json::Value::Object(b) = body {
        for (k, v) in b {
            m.insert(k.clone(), v.clone());
        }
    }
    // The envelope owns these keys. Force them after the merge so a nested
    // daemon/server response cannot silently turn a success into another type,
    // command, or semantic status.
    m.insert("type".into(), "action".into());
    m.insert("cmd".into(), cmd.into());
    m.insert("ok".into(), true.into());
    attach_events(&mut m);
    attach_next_actions(&mut m);
    serde_json::Value::Object(m)
}

/// Emit one `{"type":"action","cmd":…, …body}` result line — the shape every
/// one-shot command prints. The single builder for the action envelope (was
/// reimplemented per module).
pub fn emit_action(cmd: &str, body: &serde_json::Value) {
    emit(&action_envelope(cmd, body));
}

/// Build the `{"type":"error","stage":…,"code":…,"msg":…, …extra}` envelope.
fn error_envelope(
    stage: &str,
    code: &str,
    msg: &str,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    // Stable recovery contract: every failure has the same machine fields.
    // Callers override these defaults with domain-specific evidence/actions.
    m.insert("retryable".into(), false.into());
    m.insert("detail".into(), serde_json::json!({}));
    m.insert("next_actions".into(), serde_json::json!([]));
    if let serde_json::Value::Object(b) = extra {
        for (k, v) in b {
            m.insert(k, v);
        }
    }
    // Identity/status fields are reserved and cannot be overridden by detail
    // returned from a subsystem.
    m.insert("type".into(), "error".into());
    m.insert("ok".into(), false.into());
    m.insert("stage".into(), stage.into());
    m.insert("code".into(), code.into());
    m.insert("msg".into(), msg.into());
    // A failed action still reports what happened around it — a tap that
    // errored with element_not_found *because the app crashed* carries the
    // crash in the same error line.
    attach_events(&mut m);
    attach_next_actions(&mut m);
    serde_json::Value::Object(m)
}

fn attach_next_actions(map: &mut serde_json::Map<String, serde_json::Value>) {
    let mut actions = map
        .get("next_actions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|action| !action.is_empty())
        .map(|action| executable_action(action, map))
        .collect::<Vec<_>>();

    let command_path = effective_command_path(map);
    let mut fallbacks = domain_guidance(command_path, map);
    let dynamic_actions = dynamic_next_actions(command_path, map);
    let has_dynamic_actions = !dynamic_actions.is_empty();
    fallbacks.extend(dynamic_actions);
    if !has_dynamic_actions {
        fallbacks.extend(
            command_path
                .map(crate::cmd::introspect::next_actions_for_path)
                .unwrap_or_default(),
        );
    }
    for action in fallbacks {
        let action = executable_action(&action, map);
        if !actions.contains(&action) {
            actions.push(action);
        }
        if actions.len() == MAX_NEXT_ACTIONS {
            break;
        }
    }

    if actions.is_empty() {
        actions.push(match command_path {
            Some(path) => format!("shadowdroid commands --json --describe '{path}'"),
            None => "shadowdroid commands --json --depth 1".to_string(),
        });
    }
    actions.truncate(MAX_NEXT_ACTIONS);
    map.insert("next_actions".into(), serde_json::json!(actions));
}

/// Interactive commands executed inside `watch` still use the public command
/// contracts for the operation they performed. The process-level path remains
/// `watch`, so recover the canonical leaf from the action envelope's `cmd`.
fn effective_command_path(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Option<&'static str> {
    let current = current_command_path();
    if current != Some("watch") {
        return current;
    }
    map.get("cmd")
        .and_then(serde_json::Value::as_str)
        .and_then(watch_action_command_path)
        .or(current)
}

fn watch_action_command_path(cmd: &str) -> Option<&'static str> {
    match cmd {
        "tap" | "tap_and_wait" | "tap_text" | "tap_rid" | "tap_desc" | "tap_text_and_wait"
        | "tap_rid_and_wait" | "tap_desc_and_wait" | "xpath_tap" => Some("ui tap"),
        "double_tap" => Some("ui double-tap"),
        "long_tap" => Some("ui long-tap"),
        "swipe" | "swipe_and_wait" => Some("ui swipe"),
        "drag" | "drag_and_wait" => Some("ui drag"),
        "swipe_ext" | "swipe_ext_and_wait" => Some("ui swipe-ext"),
        "key" => Some("ui key"),
        "text" => Some("ui text"),
        "xpath" => Some("ui find"),
        "toast" => Some("ui toast"),
        "wait_for" | "wait_activity" => Some("ui wait"),
        "launch" => Some("app start"),
        "stop" => Some("app stop"),
        "app_clear" => Some("app clear"),
        "app_wait" => Some("app wait"),
        "app_info" => Some("app info"),
        "screenshot" => Some("ui screenshot"),
        "shell" => Some("device shell"),
        "screen_on" | "wakeup" => Some("device wake"),
        "screen_off" => Some("device sleep"),
        "unlock" => Some("device unlock"),
        "orientation" | "set_orientation" => Some("device orientation"),
        "clipboard" | "set_clipboard" => Some("device clipboard"),
        "open_notification" => Some("device notifications"),
        "open_quick_settings" => Some("device quick-settings"),
        "open_url" => Some("device open-url"),
        "push" => Some("files push"),
        "pull" => Some("files pull"),
        "watch" | "quit" | "add_watcher" | "remove_watcher" | "list_watchers"
        | "clear_watchers" | "permission_dialogs" => Some("watch"),
        _ => None,
    }
}

fn domain_guidance(
    command_path: Option<&str>,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    fn strings(value: Option<&serde_json::Value>) -> Vec<String> {
        match value {
            Some(serde_json::Value::String(value)) if !value.trim().is_empty() => {
                vec![value.trim().to_string()]
            }
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    }

    let mut guidance = Vec::new();
    for key in ["next", "hints", "guidance", "suggested_actions"] {
        guidance.extend(strings(map.get(key)));
    }
    guidance.extend(strings(
        map.get("ime").and_then(|ime| ime.get("suggested_actions")),
    ));
    guidance.extend(strings(
        map.get("sample")
            .and_then(|sample| sample.get("next_actions")),
    ));
    guidance.extend(strings(map.get("recommended_command")));
    if command_path != Some("net check")
        || map.get("verified").and_then(serde_json::Value::as_bool) != Some(true)
    {
        guidance.extend(strings(
            map.get("trust")
                .and_then(|trust| trust.get("recommended_command")),
        ));
    }
    if command_path == Some("ui audit") {
        guidance.extend(strings(map.get("recommendation")));
    }
    guidance
}

/// The WebSocket session id an id belongs to: `w1` → `w1`, `w1.3` → `w1`, and
/// `None` for HTTP flow ids (`f…`). Keeps `net show` follow-ups protocol-correct.
fn ws_session_of(id: &str) -> Option<String> {
    let rest = id.strip_prefix('w')?;
    let session = rest.split('.').next().unwrap_or(rest);
    (!session.is_empty() && session.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("w{session}"))
}

fn dynamic_next_actions(
    command_path: Option<&str>,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    if let Some(mut screen) = map
        .get("screen")
        .and_then(serde_json::Value::as_object)
        .cloned()
    {
        if !screen.contains_key("device")
            && let Some(device) =
                observed_value(map, "device").or_else(|| current_device().map(str::to_owned))
        {
            screen.insert("device".into(), serde_json::json!(device));
        }
        let actions = screen_element_actions(&screen);
        if !actions.is_empty() {
            return actions;
        }
    }
    match command_path {
        Some("devices") => {
            let actions = map
                .get("devices")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter(|device| {
                    device.get("state").and_then(serde_json::Value::as_str) == Some("device")
                })
                .filter_map(|device| device.get("serial").and_then(serde_json::Value::as_str))
                .take(3)
                .map(|serial| format!("shadowdroid -d {} connect", shell_token(serial)))
                .collect::<Vec<_>>();
            if actions.is_empty() {
                vec!["adb devices -l".to_string()]
            } else {
                actions
            }
        }
        Some("doctor") if map.get("healthy").and_then(serde_json::Value::as_bool) == Some(true) => {
            vec![
                "shadowdroid app current".to_string(),
                "shadowdroid ui dump".to_string(),
            ]
        }
        Some("update")
            if map.get("up_to_date").and_then(serde_json::Value::as_bool) == Some(false) =>
        {
            vec![
                "review release_url and update.command, then obtain approval before executing the host-changing update".to_string(),
                "shadowdroid commands --json --describe 'update'".to_string(),
            ]
        }
        Some("usage report") => {
            let mut actions = map
                .get("recommendations")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|recommendation| {
                    recommendation
                        .get("next_action")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::to_owned)
                .take(3)
                .collect::<Vec<_>>();
            actions.push("shadowdroid usage report --days 7".to_string());
            actions
        }
        Some("watch")
            if map.get("status").and_then(serde_json::Value::as_str) == Some("stopped") =>
        {
            vec![
                "shadowdroid watch".to_string(),
                "shadowdroid ui dump".to_string(),
                "shadowdroid why".to_string(),
            ]
        }
        Some("ui dump") => screen_element_actions(map),
        Some("ui find" | "ui wait") => element_actions(map),
        Some("net status") => {
            if let Some(held) = map
                .get("daemon")
                .and_then(|daemon| daemon.get("held_flows"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .next()
                .and_then(|flow| flow.get("id"))
                .and_then(serde_json::Value::as_str)
            {
                let id = shell_token(held);
                return vec![
                    format!("shadowdroid net show {id} --body"),
                    format!("shadowdroid net resume {id}"),
                    format!("shadowdroid net drop {id}"),
                    format!("shadowdroid net respond {id}"),
                ];
            }
            let running = map.get("running").and_then(serde_json::Value::as_bool);
            let wired = map
                .get("pointed_at_proxy")
                .and_then(serde_json::Value::as_bool);
            if running == Some(true) && wired == Some(true) {
                vec![
                    "shadowdroid watch".to_string(),
                    "shadowdroid net log".to_string(),
                    "shadowdroid net stop".to_string(),
                ]
            } else if running == Some(true) {
                vec![
                    "shadowdroid net stop".to_string(),
                    "shadowdroid net start".to_string(),
                    "shadowdroid doctor --json".to_string(),
                ]
            } else {
                vec!["shadowdroid net start".to_string()]
            }
        }
        Some("net start") => {
            let mut actions = map
                .get("apps")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .take(2)
                .map(|package| format!("shadowdroid net check {}", shell_token(package)))
                .collect::<Vec<_>>();
            actions.extend([
                "shadowdroid watch".to_string(),
                "shadowdroid net status".to_string(),
            ]);
            actions
        }
        Some("net check")
            if map.get("verified").and_then(serde_json::Value::as_bool) == Some(true) =>
        {
            let mut actions = map
                .get("probe")
                .and_then(|probe| probe.get("flow"))
                .and_then(|flow| flow.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(|id| vec![format!("shadowdroid net show {} --body", shell_token(id))])
                .unwrap_or_default();
            actions.extend([
                "shadowdroid net log".to_string(),
                "shadowdroid watch".to_string(),
            ]);
            actions
        }
        Some("net check")
            if map
                .get("probe")
                .and_then(|probe| probe.get("outcome"))
                .and_then(serde_json::Value::as_str)
                == Some("proxy_not_running") =>
        {
            let package = map
                .get("package")
                .and_then(serde_json::Value::as_str)
                .map(shell_token)
                .unwrap_or_else(|| "PACKAGE".into());
            vec![
                "shadowdroid net start".into(),
                format!("shadowdroid net check --probe {package}"),
                "shadowdroid net status".into(),
            ]
        }
        Some("net log")
            if map.get("cmd").and_then(serde_json::Value::as_str) == Some("net_log_clear") =>
        {
            vec![
                "shadowdroid net checkpoint".to_string(),
                "shadowdroid net rule list".to_string(),
                "shadowdroid watch".to_string(),
            ]
        }
        Some("net log") => map
            .get("ids")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .take(3)
            .map(|id| format!("shadowdroid net show {} --body", shell_token(id)))
            .collect(),
        Some("net checkpoint") => map
            .get("checkpoint")
            .and_then(serde_json::Value::as_str)
            .map(|checkpoint| {
                vec![
                    format!(
                        "shadowdroid net log --after-checkpoint {}",
                        shell_token(checkpoint)
                    ),
                    "shadowdroid net log clear".to_string(),
                    "shadowdroid watch".to_string(),
                ]
            })
            .unwrap_or_default(),
        Some("net show") => map
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(|id| {
                if map.get("held").and_then(serde_json::Value::as_bool) == Some(true) {
                    vec![
                        format!("shadowdroid net resume {}", shell_token(id)),
                        format!("shadowdroid net drop {}", shell_token(id)),
                    ]
                } else if let Some(session) = ws_session_of(id) {
                    // HAR/curl are HTTP concepts; a WebSocket id drills to its
                    // session and the jsonl dump instead.
                    vec![
                        format!("shadowdroid net ws {}", shell_token(&session)),
                        "shadowdroid net export jsonl --protocol websocket".to_string(),
                    ]
                } else {
                    vec![
                        format!("shadowdroid net export har {}", shell_token(id)),
                        format!("shadowdroid net export curl {}", shell_token(id)),
                    ]
                }
            })
            .unwrap_or_default(),
        Some("net rule add") => map
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(|id| {
                vec![
                    format!("shadowdroid net rule rm {}", shell_token(id)),
                    "shadowdroid net rule list".to_string(),
                    "shadowdroid watch".to_string(),
                ]
            })
            .unwrap_or_default(),
        Some("debug sessions") => debugger_session_actions(map),
        Some("debug clients") => debugger_client_actions(map),
        _ => Vec::new(),
    }
}

fn screen_element_actions(map: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(elements) = map.get("elements").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    let mut used_ids = std::collections::BTreeSet::new();

    // Range controls are semantic actions even when UIAutomator does not mark
    // them clickable (notably Compose Slider).
    for element in elements.iter().filter_map(serde_json::Value::as_object) {
        if let Some(action) = range_element_action(map, element) {
            if let Some(id) = element.get("id").and_then(serde_json::Value::as_u64) {
                used_ids.insert(id);
            }
            actions.push(action);
            break;
        }
    }

    // Inputs come first: tapping a text field is not the real task, and treating
    // any node with center coordinates as clickable can activate the wrong
    // parent. Spell out the one value the caller must choose while preserving
    // the observed id/hash/device in the command template.
    for element in elements.iter().filter_map(serde_json::Value::as_object) {
        if element.get("input").and_then(serde_json::Value::as_bool) != Some(true) {
            continue;
        }
        if let Some(action) = input_element_action(map, element) {
            if let Some(id) = element.get("id").and_then(serde_json::Value::as_u64) {
                used_ids.insert(id);
            }
            actions.push(action);
            break;
        }
    }

    for element in elements.iter().filter_map(serde_json::Value::as_object) {
        let Some(id) = element.get("id").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        if used_ids.contains(&id)
            || element.get("input").and_then(serde_json::Value::as_bool) == Some(true)
            || element
                .get("clickable")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            continue;
        }
        if let Some(action) = clickable_element_action(map, element) {
            actions.push(action);
            used_ids.insert(id);
        }
        if actions.len() >= 3 {
            break;
        }
    }

    if actions.is_empty() {
        actions.extend([
            "shadowdroid ui audit".to_string(),
            "shadowdroid layout snapshot".to_string(),
        ]);
    }
    actions
}

fn element_actions(map: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(element) = map.get("element").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    if element.get("input").and_then(serde_json::Value::as_bool) == Some(true) {
        return input_element_action(map, element).into_iter().collect();
    }
    if let Some(action) = range_element_action(map, element) {
        return vec![action];
    }
    clickable_element_action(map, element).into_iter().collect()
}

fn range_element_action(
    map: &serde_json::Map<String, serde_json::Value>,
    element: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let supports = element
        .get("actions")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .any(|action| action.as_str() == Some("set_progress"));
    if !supports || element.get("range").is_none() {
        return None;
    }
    let (target, guard) = element_target_clause(element)?;
    let mut command = format!("shadowdroid ui set-progress {target} --percent 50");
    append_target_guard(&mut command, map, guard);
    command.push_str(" --observe");
    Some(command)
}

fn clickable_element_action(
    map: &serde_json::Map<String, serde_json::Value>,
    element: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if element
        .get("clickable")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    let (target, guard) = element_target_clause(element)?;
    let mut command = format!("shadowdroid ui tap {target}");
    append_target_guard(&mut command, map, guard);
    command.push_str(" --observe");
    Some(command)
}

fn input_element_action(
    map: &serde_json::Map<String, serde_json::Value>,
    element: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let (target, guard) = element_target_clause(element)?;
    let mut command = format!("shadowdroid ui text VALUE {target}");
    append_target_guard(&mut command, map, guard);
    command.push_str(" --observe");
    let command = specialize_action(&command, map);
    Some(format!(
        "replace VALUE with the intended text, then run `{command}`"
    ))
}

#[derive(Clone, Copy)]
enum ElementTargetGuard {
    Interaction,
    Handle,
    StrictScreen,
}

/// Stable selectors are preferable because they survive harmless layout-list
/// renumbering. A screen-bound handle is the safe fallback; an unqualified
/// numeric id is emitted only for older payloads and remains strictly guarded.
fn element_target_clause(
    element: &serde_json::Map<String, serde_json::Value>,
) -> Option<(String, ElementTargetGuard)> {
    if let Some(rid) = element.get("rid").and_then(serde_json::Value::as_str) {
        return Some((
            format!("--rid {} --exact", shell_token(rid)),
            ElementTargetGuard::Interaction,
        ));
    }
    if let Some(desc) = element.get("desc").and_then(serde_json::Value::as_str) {
        return Some((
            format!("--desc {} --exact", shell_token(desc)),
            ElementTargetGuard::Interaction,
        ));
    }
    if let Some(handle) = element.get("handle").and_then(serde_json::Value::as_str) {
        return Some((
            format!("--handle {}", shell_token(handle)),
            ElementTargetGuard::Handle,
        ));
    }
    let id = element.get("id").and_then(serde_json::Value::as_u64)?;
    Some((format!("--id {id}"), ElementTargetGuard::StrictScreen))
}

fn append_target_guard(
    command: &mut String,
    map: &serde_json::Map<String, serde_json::Value>,
    guard: ElementTargetGuard,
) {
    match guard {
        ElementTargetGuard::Interaction => {
            if let Some(hash) = map
                .get("interaction_hash")
                .and_then(serde_json::Value::as_str)
            {
                command.push_str(&format!(" --if-interaction {}", shell_token(hash)));
            } else if let Some(hash) = map.get("screen_hash").and_then(serde_json::Value::as_str) {
                command.push_str(&format!(" --if-screen {}", shell_token(hash)));
            }
        }
        ElementTargetGuard::Handle => {}
        ElementTargetGuard::StrictScreen => {
            if let Some(hash) = map.get("screen_hash").and_then(serde_json::Value::as_str) {
                command.push_str(&format!(" --if-screen {}", shell_token(hash)));
            }
        }
    }
}

fn debugger_session_actions(map: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(sessions) = map.get("sessions").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let preferred_device =
        observed_value(map, "device").or_else(|| current_device().map(str::to_owned));
    let sessions = sessions
        .iter()
        .filter_map(serde_json::Value::as_object)
        .collect::<Vec<_>>();
    let suspended = |session: &&serde_json::Map<String, serde_json::Value>| {
        session
            .get("suspended")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    };
    let selected = if let Some(device) = preferred_device.as_deref() {
        let matching = sessions
            .iter()
            .copied()
            .filter(|session| debugger_device_matches(session, device))
            .collect::<Vec<_>>();
        let matching_suspended = matching
            .iter()
            .copied()
            .filter(|session| {
                session
                    .get("suspended")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            })
            .collect::<Vec<_>>();
        if matching_suspended.len() == 1 {
            matching_suspended.first().copied()
        } else if matching.len() == 1 {
            matching.first().copied()
        } else {
            None
        }
    } else {
        let suspended_sessions = sessions
            .iter()
            .copied()
            .filter(suspended)
            .collect::<Vec<_>>();
        if suspended_sessions.len() == 1 {
            suspended_sessions.first().copied()
        } else if sessions.len() == 1 {
            sessions.first().copied()
        } else {
            None
        }
    };
    let Some(session) = selected else {
        return Vec::new();
    };
    let Some(id) = session.get("id").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    if session
        .get("suspended")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        vec![
            format!("shadowdroid debug stack --session {}", shell_token(id)),
            format!("shadowdroid debug variables --session {}", shell_token(id)),
            format!("shadowdroid debug resume --session {}", shell_token(id)),
        ]
    } else {
        vec![format!(
            "shadowdroid debug pause --session {}",
            shell_token(id)
        )]
    }
}

fn debugger_client_actions(map: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(clients) = map.get("clients").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let clients = clients
        .iter()
        .filter_map(serde_json::Value::as_object)
        .collect::<Vec<_>>();
    if clients.is_empty() {
        return Vec::new();
    }
    let preferred_package =
        observed_value(map, "package").or_else(|| observed_value(map, "requested_app"));
    let preferred_device =
        observed_value(map, "device").or_else(|| current_device().map(str::to_owned));
    let matching = clients
        .iter()
        .copied()
        .filter(|client| {
            preferred_package.as_deref().is_none_or(|package| {
                client.get("package").and_then(serde_json::Value::as_str) == Some(package)
            })
        })
        .filter(|client| {
            preferred_device
                .as_deref()
                .is_none_or(|device| debugger_device_matches(client, device))
        })
        .collect::<Vec<_>>();
    let [client] = matching.as_slice() else {
        return Vec::new();
    };
    let Some(package) = client.get("package").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let mut command = format!(
        "shadowdroid debug attach --package {}",
        shell_token(package)
    );
    if let Some(pid) = client.get("pid").and_then(serde_json::Value::as_i64) {
        command.push_str(&format!(" --pid {pid}"));
    }
    vec![command]
}

fn debugger_device_matches(
    value: &serde_json::Map<String, serde_json::Value>,
    preferred: &str,
) -> bool {
    let Some(device) = value.get("device").and_then(serde_json::Value::as_object) else {
        return false;
    };
    ["serial", "avd", "avd_name"]
        .into_iter()
        .filter_map(|key| device.get(key).and_then(serde_json::Value::as_str))
        .any(|candidate| candidate == preferred)
}

/// Fill safe, already-observed identifiers into catalog command templates.
/// Unknown values stay as explicit `<placeholder>` tokens rather than guessing.
fn specialize_action(template: &str, map: &serde_json::Map<String, serde_json::Value>) -> String {
    const PLACEHOLDERS: &[(&str, &[&str])] = &[
        ("<pkg>", &["package", "app"]),
        ("<package>", &["package", "app"]),
        ("<app>", &["package", "app"]),
        ("<id>", &["id"]),
        ("<session>", &["session"]),
        ("<permission>", &["permission"]),
        ("<op>", &["op"]),
        ("<mode>", &["mode"]),
        ("<device>", &["device"]),
        ("<remote>", &["remote"]),
        ("<local>", &["local"]),
    ];
    let mut action = template.to_string();
    for (placeholder, keys) in PLACEHOLDERS {
        let Some(value) = keys.iter().find_map(|key| observed_value(map, key)) else {
            continue;
        };
        action = action.replace(placeholder, &shell_token(&value));
    }
    if let Some(device) = observed_value(map, "device")
        .or_else(|| observed_value(map, "target"))
        .or_else(|| current_device().map(str::to_owned))
        && let Some(command) = action.strip_prefix("shadowdroid ")
        && !command.starts_with("-d ")
        && !command.starts_with("--device ")
    {
        action = format!("shadowdroid -d {} {command}", shell_token(&device));
    }
    action
}

fn executable_action(template: &str, map: &serde_json::Map<String, serde_json::Value>) -> String {
    let action = specialize_action(template, map);
    if action.starts_with("shadowdroid ")
        && action.contains('<')
        && let Some(path) = crate::cmd::introspect::command_path_for_invocation(&action)
    {
        let discovery = format!("shadowdroid commands --json --describe '{path}'");
        return specialize_action(&discovery, map);
    }
    action
}

fn observed_value(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    fn scalar(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
            serde_json::Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
    }

    map.get(key)
        .and_then(scalar)
        .or_else(|| map.get("detail")?.get(key).and_then(scalar))
        .or_else(|| map.get("current_app")?.get(key).and_then(scalar))
        .or_else(|| map.get("install_report")?.get(key).and_then(scalar))
}

pub(crate) fn shell_token(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "._:/@+-=".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Emit one `{"type":"error","stage":…,"code":…,"msg":…, …extra}` line on
/// stdout — the same stream as results, so an agent reads one stream and
/// branches on `type`. Like one-shot action results, it carries no `ts`: only
/// streamed timeline events (`watch`) are timestamped, since only they need
/// ordering. The trailing flush guards callers that `process::exit` right after
/// (the clap usage path, `main`).
pub fn emit_error(stage: &str, code: &str, msg: &str, extra: serde_json::Value) {
    emit(&error_envelope(stage, code, msg, extra));
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Element;

    #[test]
    fn action_envelope_has_type_and_cmd_and_no_ts() {
        let _guard = ENVELOPE_TEST_LOCK.lock().unwrap();
        let v = action_envelope("tap", &serde_json::json!({"x": 1, "matched": true}));
        assert_eq!(v["type"], "action");
        assert_eq!(v["cmd"], "tap");
        assert_eq!(v["ok"], true);
        assert_eq!(v["x"], 1);
        assert_eq!(v["matched"], true);
        assert!(
            v["next_actions"]
                .as_array()
                .is_some_and(|actions| !actions.is_empty())
        );
        // One-shot results carry no `ts` (only streamed timeline events do).
        assert!(v.get("ts").is_none(), "action must not carry ts: {v}");
    }

    #[test]
    fn action_body_cannot_override_reserved_envelope_fields() {
        let _guard = ENVELOPE_TEST_LOCK.lock().unwrap();
        let value = action_envelope(
            "real_command",
            &serde_json::json!({
                "type": "error",
                "cmd": "other_command",
                "ok": false,
                "payload": true,
            }),
        );
        assert_eq!(value["type"], "action");
        assert_eq!(value["cmd"], "real_command");
        assert_eq!(value["ok"], true);
        assert_eq!(value["payload"], true);
    }

    #[test]
    fn error_envelope_has_required_fields_and_no_ts() {
        let _guard = ENVELOPE_TEST_LOCK.lock().unwrap();
        let v = error_envelope(
            "usage",
            "usage",
            "bad flag",
            serde_json::json!({"arg": "--x"}),
        );
        assert_eq!(v["type"], "error");
        assert_eq!(v["ok"], false);
        assert_eq!(v["stage"], "usage");
        assert_eq!(v["code"], "usage");
        assert_eq!(v["msg"], "bad flag");
        assert_eq!(v["retryable"], false);
        assert!(v["detail"].is_object());
        assert!(v["next_actions"].is_array());
        assert!(!v["next_actions"].as_array().unwrap().is_empty());
        assert_eq!(v["arg"], "--x");
        assert!(v.get("ts").is_none(), "error must not carry ts: {v}");
    }

    #[test]
    fn error_detail_cannot_override_reserved_envelope_fields() {
        let _guard = ENVELOPE_TEST_LOCK.lock().unwrap();
        let value = error_envelope(
            "real_stage",
            "real_code",
            "real message",
            serde_json::json!({
                "type": "action",
                "ok": true,
                "stage": "other",
                "code": "other",
                "msg": "other",
                "detail": {"kept": true},
            }),
        );
        assert_eq!(value["type"], "error");
        assert_eq!(value["ok"], false);
        assert_eq!(value["stage"], "real_stage");
        assert_eq!(value["code"], "real_code");
        assert_eq!(value["msg"], "real message");
        assert_eq!(value["detail"]["kept"], true);
    }

    fn full() -> Element {
        Element {
            id: 1,
            handle: None,
            text: Some("Hi".into()),
            desc: None,
            klass: Some("android.widget.Button".into()),
            rid: None,
            bounds: Some([0, 0, 4, 4]),
            tap: Some([2, 2]),
            range: Some(RangeSemantics {
                kind: "float".into(),
                min: 0.0,
                max: 1.0,
                current: 0.5,
                step: serde_json::Value::Null,
            }),
            actions: vec!["set_progress".into()],
            clickable: true,
            long_clickable: true,
            scrollable: false,
            checkable: true,
            focusable: true,
            enabled: false,
            selected: false,
            checked: false,
            focused: true,
            password: true,
            input: false,
        }
    }

    #[test]
    fn stashed_events_ride_the_next_envelope_once() {
        // Serialized via a lock so parallel tests don't race the global slot.
        let _guard = ENVELOPE_TEST_LOCK.lock().unwrap();
        stash_events(vec![serde_json::json!({"type":"crash","kind":"java"})]);
        let v = action_envelope("tap", &serde_json::json!({"x": 1}));
        assert_eq!(v["events"][0]["kind"], "java");
        // Drained: the next envelope is clean.
        let v2 = action_envelope("tap", &serde_json::json!({"x": 1}));
        assert!(v2.get("events").is_none(), "{v2}");
    }

    #[test]
    fn stashed_events_ride_error_envelopes() {
        let _guard = ENVELOPE_TEST_LOCK.lock().unwrap();
        stash_events(vec![serde_json::json!({"type":"crash","kind":"anr"})]);
        let v = error_envelope(
            "run",
            "element_not_found",
            "no match",
            serde_json::json!({}),
        );
        assert_eq!(v["events"][0]["kind"], "anr");
    }

    #[test]
    fn empty_stash_is_a_no_op() {
        let _guard = ENVELOPE_TEST_LOCK.lock().unwrap();
        stash_events(Vec::new());
        let v = action_envelope("tap", &serde_json::json!({}));
        assert!(v.get("events").is_none(), "{v}");
    }

    static ENVELOPE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn compact_element_drops_bounds_and_false_flags() {
        let json = serde_json::to_string(&CompactElement::from(full())).unwrap();
        // Bounds and the extra UIAutomator flags are not part of the compact shape.
        assert!(!json.contains("bounds"), "{json}");
        assert!(!json.contains("long_clickable"), "{json}");
        assert!(!json.contains("focusable"), "{json}");
        // False flags inside the compact shape are still omitted.
        assert!(!json.contains("scrollable"), "{json}");
        // The actionable bits survive.
        assert!(json.contains("\"clickable\":true"), "{json}");
        assert!(json.contains("\"tap\":[2,2]"), "{json}");
        assert!(json.contains("\"range\""), "{json}");
        assert!(json.contains("\"set_progress\""), "{json}");
    }

    #[test]
    fn dynamic_values_are_shell_quoted_and_device_scoped() {
        let map = serde_json::json!({
            "device": "emulator-5554; unsafe",
            "package": "com.example; unsafe",
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            specialize_action("shadowdroid app start <pkg>", &map),
            "shadowdroid -d 'emulator-5554; unsafe' app start 'com.example; unsafe'"
        );
    }

    #[test]
    fn unresolved_runtime_templates_become_exact_discovery_commands() {
        let map = serde_json::json!({"device": "emulator-5554"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            executable_action("shadowdroid ui tap --id <id>", &map),
            "shadowdroid -d emulator-5554 commands --json --describe 'ui tap'"
        );

        let map = serde_json::json!({"device": "emulator-5554", "package": "com.example"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            executable_action("shadowdroid app start <pkg>", &map),
            "shadowdroid -d emulator-5554 app start com.example"
        );
    }

    #[test]
    fn devices_offer_one_exact_connect_per_online_serial() {
        let map = serde_json::json!({
            "devices": [
                {"serial":"emulator-5554","state":"device"},
                {"serial":"offline-1","state":"offline"}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            dynamic_next_actions(Some("devices"), &map),
            ["shadowdroid -d emulator-5554 connect"]
        );
    }

    #[test]
    fn matched_clickable_element_becomes_guarded_observed_tap() {
        let map = serde_json::json!({
            "screen_hash": "abc123",
            "element": {"id": 7, "clickable": true}
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            element_actions(&map),
            ["shadowdroid ui tap --id 7 --if-screen abc123 --observe"]
        );
    }

    #[test]
    fn next_actions_prefer_stable_selectors_then_screen_bound_handles() {
        let rid_map = serde_json::json!({
            "screen_hash": "strict-1",
            "interaction_hash": "i:1111111111111111",
            "element": {
                "id": 7,
                "handle": "i:1111111111111111/e:2",
                "rid": "com.example:id/run",
                "desc": "Run mission",
                "clickable": true
            }
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            element_actions(&rid_map),
            [
                "shadowdroid ui tap --rid com.example:id/run --exact --if-interaction i:1111111111111111 --observe"
            ]
        );

        let desc_map = serde_json::json!({
            "screen_hash": "strict-2",
            "interaction_hash": "i:2222222222222222",
            "element": {
                "id": 9,
                "handle": "i:2222222222222222/e:0",
                "desc": "Open settings",
                "clickable": true
            }
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            element_actions(&desc_map),
            [
                "shadowdroid ui tap --desc 'Open settings' --exact --if-interaction i:2222222222222222 --observe"
            ]
        );

        let handle_map = serde_json::json!({
            "screen_hash": "strict-3",
            "interaction_hash": "i:3333333333333333",
            "element": {
                "id": 11,
                "handle": "i:3333333333333333/e:4",
                "clickable": true
            }
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            element_actions(&handle_map),
            ["shadowdroid ui tap --handle i:3333333333333333/e:4 --observe"]
        );
    }

    #[test]
    fn matched_range_element_becomes_guarded_verified_progress_action() {
        let map = serde_json::json!({
            "screen_hash": "abc123",
            "element": {
                "id": 9,
                "range": {"type":"float","min":0,"max":1,"current":0.2},
                "actions": ["set_progress"]
            }
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            element_actions(&map),
            ["shadowdroid ui set-progress --id 9 --percent 50 --if-screen abc123 --observe"]
        );
    }

    #[test]
    fn input_elements_get_text_guidance_and_coordinate_only_labels_are_not_tapped() {
        let map = serde_json::json!({
            "device": "emulator-5554",
            "screen_hash": "screen-1",
            "elements": [
                {"id": 1, "input": true, "tap": [10, 10]},
                {"id": 2, "tap": [20, 20]},
                {"id": 4, "input": true, "clickable": true, "tap": [25, 25]},
                {"id": 3, "clickable": true, "tap": [30, 30]}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let actions = screen_element_actions(&map);
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert!(actions[0].contains(
            "shadowdroid -d emulator-5554 ui text VALUE --id 1 --if-screen screen-1 --observe"
        ));
        assert_eq!(
            actions[1],
            "shadowdroid ui tap --id 3 --if-screen screen-1 --observe"
        );
        assert!(actions.iter().all(|action| !action.contains("--id 2")));
        assert!(actions.iter().all(|action| !action.contains("--id 4")));
    }

    #[test]
    fn matched_input_prefers_text_over_tap() {
        let map = serde_json::json!({
            "device": "emulator-5554",
            "screen_hash": "screen-2",
            "element": {"id": 9, "input": true, "clickable": true, "tap": [4, 4]}
        })
        .as_object()
        .unwrap()
        .clone();
        let actions = element_actions(&map);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].contains("ui text VALUE --id 9"), "{actions:?}");
        assert!(!actions[0].contains("ui tap"), "{actions:?}");
    }

    #[test]
    fn observed_action_uses_embedded_screen_instead_of_redundant_dump() {
        let map = serde_json::json!({
            "device": "emulator-5554",
            "screen": {
                "screen_hash": "after-action",
                "elements": [{"id": 9, "clickable": true}]
            }
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            dynamic_next_actions(Some("ui tap"), &map),
            ["shadowdroid ui tap --id 9 --if-screen after-action --observe"]
        );
    }

    #[test]
    fn debugger_sessions_follow_selected_device_and_suspension_state() {
        let map = serde_json::json!({
            "device": "emulator-5556",
            "sessions": [
                {"id":"wrong", "suspended":true, "device":{"serial":"emulator-5554"}},
                {"id":"selected", "suspended":true, "device":{"serial":"emulator-5556"}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let actions = debugger_session_actions(&map);
        assert!(actions.iter().all(|action| action.contains("selected")));
        assert!(actions.iter().any(|action| action.contains("debug stack")));

        let running = serde_json::json!({
            "device": "emulator-5556",
            "sessions": [
                {"id":"running", "suspended":false, "device":{"serial":"emulator-5556"}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            debugger_session_actions(&running),
            ["shadowdroid debug pause --session running"]
        );
    }

    #[test]
    fn debugger_clients_require_one_app_and_device_match() {
        let map = serde_json::json!({
            "device": "emulator-5556",
            "current_app": {"package":"com.target"},
            "clients": [
                {"package":"com.other", "pid":11, "device":{"serial":"emulator-5556"}},
                {"package":"com.target", "pid":22, "device":{"serial":"emulator-5556"}},
                {"package":"com.target", "pid":33, "device":{"serial":"emulator-5554"}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            debugger_client_actions(&map),
            ["shadowdroid debug attach --package com.target --pid 22"]
        );

        let ambiguous = serde_json::json!({
            "clients": [
                {"package":"com.one", "pid":1},
                {"package":"com.two", "pid":2}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(debugger_client_actions(&ambiguous).is_empty());
    }

    #[test]
    fn running_but_miswired_proxy_gets_repair_actions() {
        let map = serde_json::json!({
            "running": true,
            "pointed_at_proxy": false
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            dynamic_next_actions(Some("net status"), &map),
            [
                "shadowdroid net stop",
                "shadowdroid net start",
                "shadowdroid doctor --json"
            ]
        );
    }

    #[test]
    fn net_status_prioritizes_actions_for_a_currently_held_flow() {
        let map = serde_json::json!({
            "running": true,
            "pointed_at_proxy": true,
            "daemon": {"held_flows": [{"id": "f19; unsafe"}]}
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            dynamic_next_actions(Some("net status"), &map),
            [
                "shadowdroid net show 'f19; unsafe' --body",
                "shadowdroid net resume 'f19; unsafe'",
                "shadowdroid net drop 'f19; unsafe'",
                "shadowdroid net respond 'f19; unsafe'"
            ]
        );
    }

    #[test]
    fn net_start_uses_configured_apps_not_free_form_hint_text() {
        let map = serde_json::json!({
            "device": "emulator-5554",
            "apps": ["com.example.app"],
            "hint": "next: `net check <pkg>` to confirm trust"
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            dynamic_next_actions(Some("net start"), &map),
            [
                "shadowdroid net check com.example.app",
                "shadowdroid watch",
                "shadowdroid net status"
            ]
        );
        assert!(domain_guidance(Some("net start"), &map).is_empty());
    }

    #[test]
    fn verified_net_check_does_not_recommend_reinstalling_trust() {
        let map = serde_json::json!({
            "verified": true,
            "trust": {"recommended_command": "shadowdroid net trust --push"},
            "probe": {"flow": {"id": "f7"}}
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(domain_guidance(Some("net check"), &map).is_empty());
        assert_eq!(
            dynamic_next_actions(Some("net check"), &map),
            [
                "shadowdroid net show f7 --body",
                "shadowdroid net log",
                "shadowdroid watch"
            ]
        );
    }

    #[test]
    fn usage_report_surfaces_its_highest_priority_improvement() {
        let map = serde_json::json!({
            "recommendations": [{
                "priority": "highest",
                "next_action": "convert ui dump fallback errors to a typed diagnostic"
            }]
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            dynamic_next_actions(Some("usage report"), &map),
            [
                "convert ui dump fallback errors to a typed diagnostic",
                "shadowdroid usage report --days 7"
            ]
        );
    }

    #[test]
    fn update_check_never_promotes_the_mutating_installer_command() {
        let map = serde_json::json!({
            "up_to_date": false,
            "release_url": "https://github.com/example/release",
            "update": {
                "command": "curl https://example/installer.sh | sh",
                "requires_confirmation": true
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let actions = dynamic_next_actions(Some("update"), &map);
        assert!(actions.iter().all(|action| !action.contains("| sh")));
        assert!(actions.iter().any(|action| action.contains("approval")));
    }

    #[test]
    fn watch_action_ids_map_to_public_command_contracts() {
        let cases = [
            ("tap_and_wait", "ui tap"),
            ("text", "ui text"),
            ("launch", "app start"),
            ("shell", "device shell"),
            ("push", "files push"),
            ("wait_for", "ui wait"),
        ];
        for (cmd, path) in cases {
            assert_eq!(watch_action_command_path(cmd), Some(path), "{cmd}");
        }
    }

    #[test]
    fn domain_specific_guidance_precedes_catalog_fallbacks() {
        let mut map = serde_json::json!({
            "hints": ["shadowdroid log --last 5m"],
            "next_actions": []
        })
        .as_object()
        .unwrap()
        .clone();
        attach_next_actions(&mut map);
        assert_eq!(map["next_actions"][0], "shadowdroid log --last 5m");
    }

    #[test]
    fn stream_errors_carry_the_same_recovery_primitives() {
        let value = serde_json::to_value(Event::Error {
            stage: "screen".into(),
            code: "screen_read_failed".into(),
            msg: "connection closed".into(),
            input: None,
            retryable: true,
            detail: serde_json::json!({"attempt": 1}),
            next_actions: vec!["shadowdroid doctor --json".into()],
            ts: 1.0,
        })
        .unwrap();
        assert_eq!(value["code"], "screen_read_failed");
        assert_eq!(value["retryable"], true);
        assert!(value["detail"].is_object());
        assert!(!value["next_actions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn stream_recovery_actions_are_device_scoped_and_quoted() {
        let event = Event::Warning {
            stage: "net_watch".into(),
            code: "net_events_unavailable".into(),
            msg: "not running".into(),
            detail: serde_json::json!({}),
            next_actions: vec!["shadowdroid net start".into()],
            ts: 1.0,
        };
        let value = stream_event_value(&event, "emulator-5554; unsafe");
        assert_eq!(
            value["next_actions"],
            serde_json::json!(["shadowdroid -d 'emulator-5554; unsafe' net start"])
        );
    }

    #[test]
    fn held_flow_and_tls_events_expose_immediate_recovery() {
        let held = serde_json::to_value(Event::HttpIntercept {
            ts: 1.0,
            id: "f1".into(),
            phase: "response".into(),
            method: "GET".into(),
            scheme: "https".into(),
            host: "api.example.com".into(),
            path: "/v1".into(),
            status: Some(200),
            req_type: None,
            req_len: 0,
            resp_type: Some("application/json".into()),
            resp_len: 2,
            hold_deadline_ms: 30_000,
            req_preview: None,
            resp_preview: Some("{}".into()),
            next_actions: vec!["shadowdroid net resume f1".into()],
        })
        .unwrap();
        assert_eq!(held["type"], "http_intercept");
        assert_eq!(held["next_actions"][0], "shadowdroid net resume f1");

        let tls = serde_json::to_value(Event::TlsError {
            ts: 2.0,
            capture_session_id: "n-test".into(),
            host: "api.example.com".into(),
            reason: "certificate rejected".into(),
            next_actions: vec!["shadowdroid net check com.example".into()],
        })
        .unwrap();
        assert_eq!(tls["type"], "tls_error");
        assert!(!tls["next_actions"].as_array().unwrap().is_empty());
    }
}
