//! `shadowdroid watch` — the streaming subcommand.
//!
//! Submodules:
//!   loop      — main poll/debounce/hash-diff/emit loop
//!   logcat    — crash watcher (Java/native/ANR via logcat tail)
//!   watcher   — rule engine (declarative popup-killers)
//!   stdin     — optional stdin reader for bidirectional command input

pub mod logcat;
pub mod r#loop;
pub mod stdin;
pub mod watcher;
