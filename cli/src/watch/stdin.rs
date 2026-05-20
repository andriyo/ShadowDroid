//! Optional stdin command reader for `shadowdroid watch`.
//!
//! Each line is either:
//!   - JSON: {"cmd":"tap","id":5}
//!   - shorthand: `tap 5`, `back`, `launch com.foo`, `swipe 100 1500 100 200`
//!
//! Mirrors the `parse_command` function from the legacy `movi` CLI so existing
//! piped scripts (and the `movi` skill) keep working.

#![allow(dead_code)]

use anyhow::Result;
use serde_json::Value;

pub fn parse_command(_line: &str) -> Result<Value> {
    todo!("port from movi/watch.py parse_command")
}
