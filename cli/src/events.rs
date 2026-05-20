//! The JSON-line events the CLI prints to stdout. The agent-facing contract.
//!
//! IMPORTANT: this shape is the public API. The legacy `movi` CLI prints
//! exactly these shapes. ShadowDroid keeps wire-compat so the user-level
//! `movi` skill keeps working unchanged.

use crate::proto::{AppRef, Element, Viewport};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready {
        device: String,
        viewport: Viewport,
        server_version: String,
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
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ActionResult {
    Tap { id: Option<u32>, x: i32, y: i32 },
    Swipe { from: [i32; 2], to: [i32; 2], duration_ms: u32 },
    Key { name: String },
    Text { value: String, clear: bool },
    Launch { package: String },
    Stop { package: String },
    Screenshot { path: String, bytes: u64 },
    Shell { input: String, output: String, exit_code: Option<i32> },
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

pub fn emit(e: &Event) {
    println!("{}", serde_json::to_string(e).unwrap());
}
