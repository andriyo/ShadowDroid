//! The JSON-line events the CLI prints to stdout. The agent-facing contract.
//!
//! IMPORTANT: this shape is the public API. Keep it stable so existing watch
//! stream consumers and generated agent integrations continue to work.

use crate::proto::{AppRef, Element, ImeState, ScreenResponse, Viewport};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

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
        element_count: u32,
        ime: CompactIme,
        elements: Vec<CompactElement>,
    },
    Crash(CrashEvent),
    WatcherFired {
        name: String,
        screen_hash: String,
        matched: Element,
        ts: f64,
    },
    Error {
        stage: String,
        msg: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        ts: f64,
    },
    Warning {
        stage: String,
        msg: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        suggested_command: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
        ts: f64,
    },
    /// A completed HTTP(S) transaction through the `net` proxy. Compact by
    /// design — full headers/bodies are fetched on demand via `net show <id>`.
    /// Field shape mirrors the `net` capture wire format so the timeline can
    /// interleave it with `screen`/`crash` events.
    Http {
        ts: f64,
        id: String,
        method: String,
        scheme: String,
        host: String,
        path: String,
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
        #[serde(default, skip_serializing_if = "is_false")]
        modified: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Response body was streamed (SSE/oversized), not captured; `resp_len` is a hint.
        #[serde(default, skip_serializing_if = "is_false")]
        streamed: bool,
        /// Request body was streamed upstream (oversized upload), not captured.
        #[serde(default, skip_serializing_if = "is_false")]
        req_streamed: bool,
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
    },
    /// A TLS handshake the proxy could not complete after presenting a minted
    /// leaf for `host` — almost always the app rejecting the MITM CA (untrusted),
    /// but also cert pinning or a version/cipher mismatch. Emitted so a silent
    /// "no flows captured" turns into a visible, actionable one-liner. Deduped
    /// per host within a session so a retrying client doesn't flood the timeline.
    TlsError {
        ts: f64,
        host: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactElement {
    pub id: u32,
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
            text: el.text,
            desc: el.desc,
            rid: el.rid,
            klass: el.klass,
            tap: el.tap,
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
        package,
        activity,
        pid: _,
    } = screen.current_app;
    match format {
        ScreenFormat::Full => Event::Screen {
            ts: now_ts(),
            device: device.to_string(),
            package,
            activity,
            viewport: screen.viewport,
            screen_hash: screen.screen_hash,
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
    println!(
        "{}",
        serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
    );
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
    m.insert("type".into(), "action".into());
    m.insert("cmd".into(), cmd.into());
    if let serde_json::Value::Object(b) = body {
        for (k, v) in b {
            m.insert(k.clone(), v.clone());
        }
    }
    attach_events(&mut m);
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
    m.insert("type".into(), "error".into());
    m.insert("stage".into(), stage.into());
    m.insert("code".into(), code.into());
    m.insert("msg".into(), msg.into());
    if let serde_json::Value::Object(b) = extra {
        for (k, v) in b {
            m.insert(k, v);
        }
    }
    // A failed action still reports what happened around it — a tap that
    // errored with element_not_found *because the app crashed* carries the
    // crash in the same error line.
    attach_events(&mut m);
    serde_json::Value::Object(m)
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
        assert_eq!(v["x"], 1);
        assert_eq!(v["matched"], true);
        // One-shot results carry no `ts` (only streamed timeline events do).
        assert!(v.get("ts").is_none(), "action must not carry ts: {v}");
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
        assert_eq!(v["stage"], "usage");
        assert_eq!(v["code"], "usage");
        assert_eq!(v["msg"], "bad flag");
        assert_eq!(v["arg"], "--x");
        assert!(v.get("ts").is_none(), "error must not carry ts: {v}");
    }

    fn full() -> Element {
        Element {
            id: 1,
            text: Some("Hi".into()),
            desc: None,
            klass: Some("android.widget.Button".into()),
            rid: None,
            bounds: Some([0, 0, 4, 4]),
            tap: Some([2, 2]),
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
    }
}
