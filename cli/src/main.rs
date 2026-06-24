//! `shadowdroid` — drive Android apps with streaming JSON events.
//!
//! Module map:
//!   cli         — clap argument definitions, dispatch
//!   cmd         — larger multi-step subcommands (doctor, collect, …)
//!   device      — talking to the on-device server (HTTP) + adb (ADB protocol)
//!   watch       — event loop, debounce, crash watcher, watcher rule engine
//!   proto       — wire types for the on-device HTTP API
//!   selector    — canonical text-selector normalization + matching
//!   events      — JSON event emission (stdout) + types
//!
//! Each module is documented in the file header. See README.md for the public
//! command surface.

mod cli;
mod cmd;
mod config;
mod device;
mod events;
mod ids;
mod net;
mod proto;
mod selector;
mod update;
mod watch;

#[tokio::main]
async fn main() {
    // `--quiet`/`-q` (or SHADOWDROID_QUIET) suppresses our own operational logs so
    // stdout stays clean JSON even under `2>&1`. It's read here, ahead of clap,
    // because tracing is initialized before argument dispatch. An explicit
    // `RUST_LOG` (via the default env filter) still takes precedence.
    let quiet = std::env::args()
        .skip(1)
        .any(|a| a == "-q" || a == "--quiet")
        || std::env::var_os("SHADOWDROID_QUIET")
            .is_some_and(|v| !matches!(v.to_str(), Some("") | Some("0") | Some("false")));
    let default_filter = if quiet { "off" } else { "shadowdroid=info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .init();

    // A failed command prints one `{"type":"error",…}` line on stdout (not
    // anyhow's `Error: …` on stderr) so the JSON contract holds for failures too.
    if let Err(err) = cli::run().await {
        cli::report_error(&err);
        std::process::exit(1);
    }
}
