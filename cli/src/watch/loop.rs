//! Steady-state watch loop. One emit per real screen change.
//!
//! Wake sources (all feed a single `tokio::sync::mpsc::Sender<Wake>`):
//!   - logcat tail (low-latency event signal on Window/Activity transitions)
//!   - safety-net poll (default 1s) — catches in-screen mutations
//!   - command nudge (after every dispatched action, force a fresh dump)
//!
//! On wake:
//!   - sleep `debounce_ms` to coalesce a storm
//!   - drain remaining wakes
//!   - GET /v1/screen
//!   - hash compare → emit on change
//!   - run watcher rules → dispatch actions, emit `watcher_fired` events
//!   - update `last_hash`

#![allow(dead_code)]

use anyhow::Result;

#[derive(Debug, Clone, Copy)]
pub enum Wake { Event, Poll, Command, Init }

pub async fn run(_app_filter: Option<String>) -> Result<()> {
    todo!("M3")
}
