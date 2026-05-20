//! Declarative popup-killers. Pure port of movi/watcher.py.
//!
//! Rule shape (same as movi):
//!   {
//!     "name":   "allow_notifications",
//!     "when":   {"text": "Allow"},
//!     "then":   [{"cmd": "tap_text", "value": "Allow"}],
//!     "max_fires": 1
//!   }
//!
//! On every emitted screen, check each rule. If `when` matches an element,
//! mark rule fired (anti-loop: skip if screen_hash unchanged from last fire),
//! dispatch each command in `then`, emit `watcher_fired`.

#![allow(dead_code)]

use crate::proto::Element;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WatcherRule {
    pub name: String,
    pub when: WhenQuery,
    pub then: Vec<serde_json::Value>, // raw CLI command JSON
    #[serde(default)]
    pub max_fires: Option<u32>,
    #[serde(default)]
    pub fire_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WhenQuery {
    pub text: Option<String>,
    pub rid: Option<String>,
    pub desc: Option<String>,
    pub klass: Option<String>,
    pub clickable: Option<bool>,
}

impl WhenQuery {
    pub fn matches(&self, _el: &Element) -> bool {
        todo!()
    }
}

#[derive(Default)]
pub struct WatcherSet {
    inner: Mutex<Vec<WatcherRule>>,
}
