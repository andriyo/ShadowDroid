//! One function per CLI subcommand. Each:
//!   - calls a ServerClient method (or several)
//!   - returns an `events::ActionResult` that the CLI emits as a JSON line
//!
//! This is the "translation layer" between CLI verbs and HTTP endpoints. Keep
//! the dumb mapping here so the CLI dispatch stays one-line.

#![allow(dead_code)]

use crate::device::client::ServerClient;
use crate::events::ActionResult;
use anyhow::Result;

pub async fn tap_xy(s: &ServerClient, x: i32, y: i32) -> Result<ActionResult> {
    s.tap_xy(x, y).await?;
    Ok(ActionResult::Tap { id: None, x, y })
}

// …rest of the action functions sketched and implemented in M2.
