//! The JSON-line events the CLI prints to stdout. The agent-facing contract.
//!
//! IMPORTANT: this shape is the public API. The legacy `movi` CLI prints
//! exactly these shapes. ShadowDroid keeps wire-compat so the user-level
//! `movi` skill keeps working unchanged.

use crate::proto::{AppRef, Element, ScreenResponse, Viewport};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
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

pub fn screen_event(device: &str, screen: ScreenResponse) -> Event {
    let AppRef {
        package,
        activity,
        pid: _,
    } = screen.current_app;
    Event::Screen {
        ts: now_ts(),
        device: device.to_string(),
        package,
        activity,
        viewport: screen.viewport,
        screen_hash: screen.screen_hash,
        element_count: screen.element_count,
        elements: screen.elements,
    }
}

pub fn emit(e: &Event) {
    println!("{}", serde_json::to_string(e).unwrap());
}
