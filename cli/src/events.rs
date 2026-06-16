//! The JSON-line events the CLI prints to stdout. The agent-facing contract.
//!
//! IMPORTANT: this shape is the public API. Keep it stable so existing watch
//! stream consumers and generated agent integrations continue to work.

use crate::proto::{AppRef, Element, ScreenResponse, Viewport};
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
        elements: Vec<CompactElement>,
    },
    Action(ActionResult),
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
}

#[derive(Debug, Serialize)]
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
    pub tap: [i32; 2],
    #[serde(default, skip_serializing_if = "is_false")]
    pub clickable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub scrollable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub input: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub checked: bool,
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
            selected: el.selected,
            checked: el.checked,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ActionResult {
    Tap {
        id: Option<u32>,
        x: i32,
        y: i32,
    },
    Swipe {
        from: [i32; 2],
        to: [i32; 2],
        duration_ms: u32,
    },
    Key {
        name: String,
    },
    Text {
        value: String,
        clear: bool,
    },
    Launch {
        package: String,
    },
    Stop {
        package: String,
    },
    Screenshot {
        path: String,
        bytes: u64,
    },
    Shell {
        input: String,
        output: String,
        exit_code: Option<i32>,
    },
    // …etc, one variant per cmd
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
            elements: screen
                .elements
                .into_iter()
                .map(CompactElement::from)
                .collect(),
        },
    }
}

pub fn emit(e: &Event) {
    println!("{}", serde_json::to_string(e).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Element;

    fn full() -> Element {
        Element {
            id: 1,
            text: Some("Hi".into()),
            desc: None,
            klass: Some("android.widget.Button".into()),
            rid: None,
            bounds: [0, 0, 4, 4],
            tap: [2, 2],
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
