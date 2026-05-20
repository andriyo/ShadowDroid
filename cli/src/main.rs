//! `shadowdroid` — drive Android apps with streaming JSON events.
//!
//! Module map:
//!   cli         — clap argument definitions, dispatch
//!   device      — talking to the on-device server (HTTP) + adb (ADB protocol)
//!   watch       — event loop, debounce, crash watcher, watcher rule engine
//!   dump        — XML / element JSON model
//!   proto       — wire types (mirrors docs/protocol.md)
//!   events      — JSON event emission (stdout) + types
//!
//! Each module is documented in the file header. See README.md for repo layout
//! and docs/architecture.md for the big picture.

mod cli;
mod device;
mod dump;
mod events;
mod proto;
mod watch;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("shadowdroid=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    cli::run().await
}
