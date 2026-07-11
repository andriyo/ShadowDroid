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
//!   release     — GitHub release asset download + SHA-256 verification
//!   hostenv     — host home/data directories + env-var toggles
//!
//! Each module is documented in the file header. See README.md for the public
//! command surface.

// Rust's standard print macros panic when a downstream agent intentionally
// closes a pipe early (`commands --json | head`). Route all unqualified stdout
// writes in this crate through a non-panicking sink instead. Broken pipes are a
// normal consumer decision, not a ShadowDroid crash.
macro_rules! print {
    ($($arg:tt)*) => {{
        crate::events::write_stdout(format_args!($($arg)*), false)
    }};
}

macro_rules! println {
    () => {{ crate::events::write_stdout(format_args!(""), true) }};
    ($($arg:tt)*) => {{
        crate::events::write_stdout(format_args!($($arg)*), true)
    }};
}

mod cli;
mod cmd;
mod config;
mod crashscan;
mod device;
mod diagnostic;
mod events;
mod fusion;
mod hostenv;
mod ids;
mod net;
mod proto;
mod release;
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
        let exit_code = cli::process_exit_code_of(&err).unwrap_or(1);
        cli::report_error(&err);
        std::process::exit(exit_code);
    }
}
