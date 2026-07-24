//! Host-side Android screen recording.
//!
//! `video start` re-execs the current binary as a per-device daemon. The daemon
//! owns the long-running ADB shell, rotates crash-safe MP4 segments, persists a
//! searchable JSONL timeline, and exposes a tiny loopback control plane for
//! `status`, `mark`, and `stop`. It does not require or start the on-device
//! ShadowDroid UiAutomation server.

mod backend;
mod commands;
mod control;
mod daemon;
mod paths;
mod session;

use crate::ids::Serial;
use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct VideoArgs {
    #[command(subcommand)]
    pub command: VideoCmd,

    /// Populated from the global redaction policy after config resolution.
    #[arg(skip)]
    pub(crate) redact: bool,
    #[arg(skip)]
    pub(crate) redaction: crate::redaction::PolicySpec,
}

impl VideoArgs {
    pub fn is_daemon(&self) -> bool {
        matches!(self.command, VideoCmd::Daemon(_))
    }

    /// Recording may explicitly opt into starting a named target. Passive
    /// lifecycle reads and teardown must never boot an emulator.
    pub fn allows_target_start(&self) -> bool {
        matches!(self.command, VideoCmd::Start(_) | VideoCmd::Record(_))
    }
}

#[derive(Subcommand, Debug)]
pub enum VideoCmd {
    /// Record in the foreground until --duration elapses or Ctrl-C is pressed.
    Record(RecordArgs),
    /// Start a detached per-device recording session.
    Start(StartArgs),
    /// Report the active recorder, or running:false when this device is idle.
    Status,
    /// Add a timestamped label to the active recording timeline.
    Mark {
        /// Searchable marker text, for example "before checkout".
        #[arg(value_name = "LABEL")]
        label: String,
    },
    /// Gracefully finalize and pull the active recording. Idempotent when idle.
    Stop,
    /// Internal detached recorder entrypoint.
    #[command(hide = true)]
    Daemon(DaemonArgs),
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// New session-bundle directory. Existing paths are never overwritten.
    #[arg(short = 'o', long, value_name = "DIR")]
    pub out: PathBuf,
    #[command(flatten)]
    pub capture: CaptureArgs,
}

#[derive(Args, Debug)]
pub struct RecordArgs {
    /// New session-bundle directory. Existing paths are never overwritten.
    #[arg(short = 'o', long, value_name = "DIR")]
    pub out: PathBuf,
    /// Stop automatically after this duration (minimum 1s; for example 30s or 2m).
    /// Without it, record until Ctrl-C.
    #[arg(long, value_name = "DURATION")]
    pub duration: Option<String>,
    #[command(flatten)]
    pub capture: CaptureArgs,
}

#[derive(Args, Clone, Debug)]
pub struct CaptureArgs {
    /// Recording backend. `auto` currently selects Android screenrecord.
    #[arg(long, value_enum, default_value_t = VideoBackendArg::Auto)]
    pub backend: VideoBackendArg,
    /// Video dimensions as WIDTHxHEIGHT. Omit to use the device display size.
    #[arg(long, value_name = "WIDTHxHEIGHT")]
    pub size: Option<String>,
    /// Encoder bit rate in bits per second.
    #[arg(long, value_name = "BPS")]
    pub bit_rate: Option<u32>,
    /// Android logical display id (only on screenrecord versions that support it).
    #[arg(long, value_name = "ID")]
    pub display_id: Option<u64>,
    /// Add Android's diagnostic screenrecord overlay.
    #[arg(long)]
    pub bugreport: bool,
    /// Maximum length of each crash-safe MP4 segment.
    #[arg(
        long,
        default_value_t = 170,
        value_parser = clap::value_parser!(u32).range(5..=3600),
        value_name = "SECONDS"
    )]
    pub segment_seconds: u32,
    /// Keep one segment across device rotation instead of splitting at rotation.
    #[arg(long)]
    pub no_split_on_rotation: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum VideoBackendArg {
    Auto,
    Screenrecord,
}

impl VideoBackendArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Screenrecord => "screenrecord",
        }
    }
}

#[derive(Args, Clone, Debug)]
pub struct DaemonArgs {
    #[arg(long)]
    pub serial: String,
    #[arg(long)]
    pub startup_id: String,
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub out: PathBuf,
    #[command(flatten)]
    pub capture: CaptureArgs,
    #[arg(long)]
    pub capture_redact: bool,
    #[arg(long)]
    pub redaction_json_key: Vec<String>,
    #[arg(long)]
    pub redaction_pattern: Vec<String>,
}

pub async fn run(args: &VideoArgs, serial: &Serial) -> Result<()> {
    match &args.command {
        VideoCmd::Record(record) => {
            commands::record(serial, record, args.redact, args.redaction.clone()).await
        }
        VideoCmd::Start(start) => {
            let value = commands::start(serial, start, args.redact, args.redaction.clone()).await?;
            crate::events::emit_action("video_start", &value);
            Ok(())
        }
        VideoCmd::Status => commands::status(serial).await,
        VideoCmd::Mark { label } => commands::mark(serial, label).await,
        VideoCmd::Stop => commands::stop(serial, "explicit").await,
        VideoCmd::Daemon(daemon_args) => daemon::run(daemon_args.clone()).await,
    }
}
