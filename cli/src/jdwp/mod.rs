//! Standalone JDWP debugger backend (`debug --backend jdwp`): the CLI speaks
//! JDWP to the app through the ADB server's `jdwp:<pid>` service, without
//! Android Studio. Design: `docs/jdwp-debugger-design.md`.
//!
//! Layout (mirrors `net/`):
//!   protocol  — command-set/command/event/tag/error numbers
//!   codec     — packet framing + IDSizes-aware value encoding
//!   events    — `Event.Composite` parsing
//!   conn      — handshake, reply demux by packet id, per-request deadlines
//!   vm        — typed JDWP commands
//!   transport — adb `jdwp:<pid>` streams, pid discovery, TCP for tests
//!   resolve   — source file → package → candidate classes (pure)
//!   expr      — condition / log-expression grammar (pure)
//!   logpoints — bounded logpoint event stream with cursors
//!   breakpoints — options, arming, conditions, logpoints, continue-until
//!   session   — breakpoints, deferred binding, suspension state, event loop
//!   inspect   — stack/threads/variables/eval/inspect and value renderers
//!   launch    — `am set-debug-app -w` launch-time attach orchestration
//!   paths     — `~/.shadowdroid/debug/<serial>/<pid>.*` registry layout
//!   daemon    — `__debugd`: JSON-RPC over a 0600 unix socket
//!   control   — client side: spawn, readiness, RPC
//!   commands  — `debug` verb mapping for the jdwp backend

pub mod breakpoints;
pub mod codec;
pub mod commands;
pub mod conn;
pub mod control;
pub mod coroutines;
pub mod daemon;
pub mod eval;
pub mod events;
pub mod expr;
pub mod inspect;
pub mod invoke;
pub mod lambdas;
pub mod launch;
pub mod logpoints;
pub mod members;
pub mod paths;
#[allow(dead_code)] // numbered protocol table; not every entry has a caller yet
pub mod protocol;
pub mod resolve;
pub mod session;
pub mod transport;
pub mod vm;
pub mod watches;

#[cfg(test)]
#[path = "../../tests/support/fake_jdwp.rs"]
mod fake_jdwp;
#[cfg(test)]
mod tests;
