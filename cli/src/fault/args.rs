//! `shadowdroid fault …` arguments. Each fault kind is its own subcommand so
//! `commands --describe 'fault inject <kind>'` shows exactly the flags it
//! takes; names match `fault kinds`.

use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Subcommand, Debug, Clone)]
pub enum FaultCmd {
    /// List every fault kind: what it does, whether it is a one-off action or a
    /// state `fault clear` undoes, and what it needs. No device required.
    Kinds,
    /// Inject a fault. A state fault stays until `fault clear` or
    /// `--duration-ms`; an action fault happens once. Replies with the fault
    /// record, including its id and how it will be undone.
    #[command(subcommand)]
    Inject(InjectCmd),
    /// List the faults active on the device, oldest first.
    List,
    /// Undo faults and verify each restore. Newest first with --all.
    Clear(ClearArgs),
    /// Emulator snapshots: save a known state before a fault run, load it back after.
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    /// Run a JSON scenario: a sequence of ShadowDroid commands, faults, waits
    /// and checks. Every fault it injected is cleared at the end, pass or fail.
    Run(RunArgs),
    /// Clear one fault when its --duration-ms elapses (spawned by inject).
    #[command(hide = true)]
    Expire(ExpireArgs),
}

#[derive(Args, Debug, Clone)]
#[command(group(clap::ArgGroup::new("which").required(true).args(["ids", "all"])))]
pub struct ClearArgs {
    /// Fault ids from `fault inject` or `fault list`.
    pub ids: Vec<String>,
    /// Clear every fault active on the device.
    #[arg(long)]
    pub all: bool,
    /// Set by the --duration-ms timer: an id that is already gone is not an error.
    #[arg(long, hide = true)]
    pub expired: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ExpireArgs {
    #[arg(long)]
    pub serial: String,
    #[arg(long)]
    pub id: String,
    #[arg(long)]
    pub at_ms: u64,
    #[arg(long)]
    pub forward_authority_dir: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Scenario JSON file (see `commands --guide faults`).
    pub file: PathBuf,
    /// Seed for `pick` steps and proxy faults without their own --seed; the
    /// same seed replays the same choices. Overrides the file's `seed`.
    #[arg(long)]
    pub seed: Option<u64>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SnapshotCmd {
    /// Save the emulator's current state under a name.
    Save { name: String },
    /// Load a saved state (the app and ShadowDroid server restart with it).
    Load { name: String },
    /// List saved snapshots.
    List,
    /// Delete a saved snapshot.
    Delete { name: String },
}

/// Flags every state fault takes.
#[derive(Args, Debug, Clone, Default)]
pub struct Hold {
    /// Clear the fault automatically after this many milliseconds. Without it
    /// the fault stays until `fault clear`.
    #[arg(long, value_parser = clap::value_parser!(u64).range(100..=86_400_000))]
    pub duration_ms: Option<u64>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct AppTarget {
    /// App package. Defaults to the configured app.
    #[arg(long, alias = "package")]
    pub app: Option<String>,
}

/// Which proxied requests a proxy fault hits.
#[derive(Args, Debug, Clone, Default)]
pub struct ProxyScope {
    /// Only requests whose host contains this text.
    #[arg(long)]
    pub host: Option<String>,
    /// Only requests whose path contains this text.
    #[arg(long)]
    pub path: Option<String>,
    /// Share of matching requests hit, 1-100.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub percent: u8,
    /// Seed for which requests are hit when --percent is below 100: the same
    /// seed hits the same requests in the same order.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
}

#[derive(Subcommand, Debug, Clone)]
pub enum InjectCmd {
    /// Background the app and kill its process as Android does under memory
    /// pressure; its task stays in recents so reopening restores saved state.
    ProcessDeath {
        #[command(flatten)]
        target: AppTarget,
        /// Reopen the app afterwards like tapping its icon, then report the new pid.
        #[arg(long)]
        relaunch: bool,
        /// How long to wait for Android to let the backgrounded process be killed.
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1_000..=60_000))]
        timeout_ms: u32,
    },
    /// Destroy activities as soon as the user leaves them.
    DontKeepActivities {
        #[command(flatten)]
        hold: Hold,
    },
    /// Deliver an onTrimMemory level to the app.
    LowMemory {
        #[command(flatten)]
        target: AppTarget,
        /// running-* levels need the app in the foreground; the others in the background.
        #[arg(long, value_enum, default_value_t = TrimLevel::RunningCritical)]
        level: TrimLevel,
    },
    /// Revoke a granted runtime permission (Android kills the app).
    RevokePermission {
        #[command(flatten)]
        target: AppTarget,
        /// Permission name, e.g. android.permission.CAMERA.
        #[arg(long)]
        permission: String,
        #[command(flatten)]
        hold: Hold,
    },
    /// Lock the screen rotation.
    Orientation {
        #[arg(long, value_enum)]
        rotation: Rotation,
        #[command(flatten)]
        hold: Hold,
    },
    /// Switch the system dark theme.
    NightMode {
        #[arg(long, value_enum, default_value_t = OnOff::On)]
        mode: OnOff,
        #[command(flatten)]
        hold: Hold,
    },
    /// Change the system font scale.
    FontScale {
        /// Font scale, 0.5-3.0 (1.0 is the default size).
        #[arg(long)]
        scale: f32,
        #[command(flatten)]
        hold: Hold,
    },
    /// Override the display size and/or density.
    DisplaySize {
        /// Size as WIDTHxHEIGHT pixels, e.g. 1080x1200.
        #[arg(long, required_unless_present = "density")]
        size: Option<String>,
        /// Density in dpi, e.g. 320.
        #[arg(long)]
        density: Option<u32>,
        #[command(flatten)]
        hold: Hold,
    },
    /// Set the app's own locale (per-app language).
    AppLocale {
        #[command(flatten)]
        target: AppTarget,
        /// Language tags separated by commas, e.g. ar-EG or he,en-US.
        #[arg(long)]
        locales: String,
        #[command(flatten)]
        hold: Hold,
    },
    /// Shrink the app's task to part of the screen, as split screen does.
    SplitScreen {
        #[command(flatten)]
        target: AppTarget,
        /// Share of the screen height the app keeps, 20-80.
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u8).range(20..=80))]
        percent: u8,
        #[command(flatten)]
        hold: Hold,
    },
    /// Leave the app (HOME) so it can enter picture-in-picture; reports whether it did.
    Pip {
        #[command(flatten)]
        target: AppTarget,
    },
    /// Force the device into deep Doze.
    Doze {
        #[command(flatten)]
        hold: Hold,
    },
    /// Move the app to an App Standby bucket.
    StandbyBucket {
        #[command(flatten)]
        target: AppTarget,
        #[arg(long, value_enum)]
        bucket: Bucket,
        #[command(flatten)]
        hold: Hold,
    },
    /// Report the battery as unplugged, optionally low and with Battery Saver.
    Battery {
        /// Battery level percent, 0-100 (default: keep the current level).
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        level: Option<u8>,
        /// Also turn Battery Saver on.
        #[arg(long)]
        saver: bool,
        #[command(flatten)]
        hold: Hold,
    },
    /// Report a thermal (overheating) status.
    Thermal {
        #[arg(long, value_enum)]
        status: ThermalStatus,
        #[command(flatten)]
        hold: Hold,
    },
    /// Fill the data partition until only --free-mb remains.
    StorageFull {
        /// Space left free, in MiB.
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=10_240))]
        free_mb: u32,
        /// Allow this on a physical device (other apps run out of space too).
        #[arg(long)]
        allow_physical: bool,
        #[command(flatten)]
        hold: Hold,
    },
    /// Move the system clock.
    Clock {
        /// Milliseconds to move the clock; negative moves it back.
        #[arg(long, allow_hyphen_values = true)]
        offset_ms: i64,
        /// Allow this on a physical device (turns automatic time off until clear).
        #[arg(long)]
        allow_physical: bool,
        #[command(flatten)]
        hold: Hold,
    },
    /// Change the system time zone.
    Timezone {
        /// Olson time zone id, e.g. Asia/Tokyo.
        #[arg(long)]
        tz: String,
        #[command(flatten)]
        hold: Hold,
    },
    /// Keep CPU cores busy.
    CpuLoad {
        /// Busy loops to run (default: one per CPU core).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=64))]
        threads: Option<u8>,
        #[command(flatten)]
        hold: Hold,
    },
    /// Turn airplane mode on.
    AirplaneMode {
        #[command(flatten)]
        hold: Hold,
    },
    /// Turn Wi-Fi off.
    WifiOff {
        #[command(flatten)]
        hold: Hold,
    },
    /// Turn mobile data off.
    MobileDataOff {
        #[command(flatten)]
        hold: Hold,
    },
    /// Toggle airplane mode on and off repeatedly.
    NetworkFlap {
        /// Time spent in each state, in milliseconds.
        #[arg(long, default_value_t = 5_000, value_parser = clap::value_parser!(u32).range(500..=600_000))]
        period_ms: u32,
        #[command(flatten)]
        hold: Hold,
    },
    /// Make every DNS lookup fail.
    DnsFailure {
        #[command(flatten)]
        hold: Hold,
    },
    /// Throttle the emulator link to a cellular speed (emulator only).
    NetworkSpeed {
        #[arg(long, value_enum)]
        profile: SpeedProfile,
        #[command(flatten)]
        hold: Hold,
    },
    /// Add cellular latency to the emulator link (emulator only).
    NetworkLatency {
        #[arg(long, value_enum)]
        profile: LatencyProfile,
        #[command(flatten)]
        hold: Hold,
    },
    /// Answer matching proxied requests with an HTTP error (needs `net start`).
    HttpErrors {
        #[command(flatten)]
        scope: ProxyScope,
        /// HTTP status to answer with.
        #[arg(long, default_value_t = 503, value_parser = clap::value_parser!(u16).range(400..=599))]
        status: u16,
        #[command(flatten)]
        hold: Hold,
    },
    /// Delay matching proxied requests (needs `net start`).
    HttpLatency {
        #[command(flatten)]
        scope: ProxyScope,
        /// Delay in milliseconds.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=120_000))]
        delay_ms: u32,
        /// Extra random delay up to this many milliseconds (seeded by --seed).
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=120_000))]
        jitter_ms: u32,
        #[command(flatten)]
        hold: Hold,
    },
    /// Stream matching proxied responses at a limited rate (needs `net start`).
    Bandwidth {
        #[command(flatten)]
        scope: ProxyScope,
        /// Bytes per second delivered to the app.
        #[arg(long, value_parser = clap::value_parser!(u32).range(100..=100_000_000))]
        bytes_per_sec: u32,
        #[command(flatten)]
        hold: Hold,
    },
    /// Break matching proxied responses partway through (needs `net start`).
    ConnectionReset {
        #[command(flatten)]
        scope: ProxyScope,
        /// Response body bytes delivered before the connection breaks.
        #[arg(long, default_value_t = 0)]
        after_bytes: u32,
        #[command(flatten)]
        hold: Hold,
    },
    /// Deliver only the start of matching proxied response bodies as if
    /// complete (needs `net start`).
    TruncatedResponse {
        #[command(flatten)]
        scope: ProxyScope,
        /// Response body bytes kept.
        #[arg(long, default_value_t = 16)]
        keep_bytes: u32,
        #[command(flatten)]
        hold: Hold,
    },
    /// Fail the TLS handshake for matching HTTPS hosts (needs `net start`).
    TlsFailure {
        /// Hosts containing this text fail their TLS handshake.
        #[arg(long)]
        host: String,
        /// Share of connections that fail, 1-100.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(1..=100))]
        percent: u8,
        /// Seed for which connections fail when --percent is below 100.
        #[arg(long, default_value_t = 0)]
        seed: u64,
        #[command(flatten)]
        hold: Hold,
    },
    /// Ring an incoming phone call (emulator only).
    IncomingCall {
        /// Caller number.
        #[arg(long, default_value = "5551234")]
        number: String,
        #[command(flatten)]
        hold: Hold,
    },
    /// Deliver an incoming SMS (emulator only).
    Sms {
        /// Sender number.
        #[arg(long, default_value = "5551234")]
        from: String,
        /// Message text.
        #[arg(long)]
        text: String,
    },
    /// Turn the screen off.
    ScreenOff {
        #[command(flatten)]
        hold: Hold,
    },
    /// Pull the notification shade or quick settings down over the app.
    NotificationShade {
        #[arg(long, value_enum, default_value_t = Panel::Notifications)]
        panel: Panel,
        #[command(flatten)]
        hold: Hold,
    },
    /// Crash the emulator (emulator only).
    EmulatorCrash {
        /// Boot the emulator again afterwards.
        #[arg(long)]
        relaunch: bool,
        /// With --relaunch, cold boot instead of loading the quick-boot snapshot.
        #[arg(long, requires = "relaunch")]
        cold_boot: bool,
    },
}

impl InjectCmd {
    /// The kind name, as in `fault kinds`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ProcessDeath { .. } => "process-death",
            Self::DontKeepActivities { .. } => "dont-keep-activities",
            Self::LowMemory { .. } => "low-memory",
            Self::RevokePermission { .. } => "revoke-permission",
            Self::Orientation { .. } => "orientation",
            Self::NightMode { .. } => "night-mode",
            Self::FontScale { .. } => "font-scale",
            Self::DisplaySize { .. } => "display-size",
            Self::AppLocale { .. } => "app-locale",
            Self::SplitScreen { .. } => "split-screen",
            Self::Pip { .. } => "pip",
            Self::Doze { .. } => "doze",
            Self::StandbyBucket { .. } => "standby-bucket",
            Self::Battery { .. } => "battery",
            Self::Thermal { .. } => "thermal",
            Self::StorageFull { .. } => "storage-full",
            Self::Clock { .. } => "clock",
            Self::Timezone { .. } => "timezone",
            Self::CpuLoad { .. } => "cpu-load",
            Self::AirplaneMode { .. } => "airplane-mode",
            Self::WifiOff { .. } => "wifi-off",
            Self::MobileDataOff { .. } => "mobile-data-off",
            Self::NetworkFlap { .. } => "network-flap",
            Self::DnsFailure { .. } => "dns-failure",
            Self::NetworkSpeed { .. } => "network-speed",
            Self::NetworkLatency { .. } => "network-latency",
            Self::HttpErrors { .. } => "http-errors",
            Self::HttpLatency { .. } => "http-latency",
            Self::Bandwidth { .. } => "bandwidth",
            Self::ConnectionReset { .. } => "connection-reset",
            Self::TruncatedResponse { .. } => "truncated-response",
            Self::TlsFailure { .. } => "tls-failure",
            Self::IncomingCall { .. } => "incoming-call",
            Self::Sms { .. } => "sms",
            Self::ScreenOff { .. } => "screen-off",
            Self::NotificationShade { .. } => "notification-shade",
            Self::EmulatorCrash { .. } => "emulator-crash",
        }
    }

    pub fn app_mut(&mut self) -> Option<&mut Option<String>> {
        match self {
            Self::ProcessDeath { target, .. }
            | Self::LowMemory { target, .. }
            | Self::RevokePermission { target, .. }
            | Self::AppLocale { target, .. }
            | Self::SplitScreen { target, .. }
            | Self::Pip { target }
            | Self::StandbyBucket { target, .. } => Some(&mut target.app),
            _ => None,
        }
    }

    pub fn duration_ms(&self) -> Option<u64> {
        match self {
            Self::DontKeepActivities { hold }
            | Self::RevokePermission { hold, .. }
            | Self::Orientation { hold, .. }
            | Self::NightMode { hold, .. }
            | Self::FontScale { hold, .. }
            | Self::DisplaySize { hold, .. }
            | Self::AppLocale { hold, .. }
            | Self::SplitScreen { hold, .. }
            | Self::Doze { hold }
            | Self::StandbyBucket { hold, .. }
            | Self::Battery { hold, .. }
            | Self::Thermal { hold, .. }
            | Self::StorageFull { hold, .. }
            | Self::Clock { hold, .. }
            | Self::Timezone { hold, .. }
            | Self::CpuLoad { hold, .. }
            | Self::AirplaneMode { hold }
            | Self::WifiOff { hold }
            | Self::MobileDataOff { hold }
            | Self::NetworkFlap { hold, .. }
            | Self::DnsFailure { hold }
            | Self::NetworkSpeed { hold, .. }
            | Self::NetworkLatency { hold, .. }
            | Self::HttpErrors { hold, .. }
            | Self::HttpLatency { hold, .. }
            | Self::Bandwidth { hold, .. }
            | Self::ConnectionReset { hold, .. }
            | Self::TruncatedResponse { hold, .. }
            | Self::TlsFailure { hold, .. }
            | Self::IncomingCall { hold, .. }
            | Self::ScreenOff { hold }
            | Self::NotificationShade { hold, .. } => hold.duration_ms,
            Self::ProcessDeath { .. }
            | Self::LowMemory { .. }
            | Self::Pip { .. }
            | Self::Sms { .. }
            | Self::EmulatorCrash { .. } => None,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimLevel {
    RunningModerate,
    RunningLow,
    RunningCritical,
    UiHidden,
    Background,
    Moderate,
    Complete,
}

impl TrimLevel {
    pub fn android_name(self) -> &'static str {
        match self {
            Self::RunningModerate => "RUNNING_MODERATE",
            Self::RunningLow => "RUNNING_LOW",
            Self::RunningCritical => "RUNNING_CRITICAL",
            Self::UiHidden => "HIDDEN",
            Self::Background => "BACKGROUND",
            Self::Moderate => "MODERATE",
            Self::Complete => "COMPLETE",
        }
    }
    pub fn needs_foreground(self) -> bool {
        matches!(
            self,
            Self::RunningModerate | Self::RunningLow | Self::RunningCritical
        )
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    #[value(name = "0")]
    R0,
    #[value(name = "90")]
    R90,
    #[value(name = "180")]
    R180,
    #[value(name = "270")]
    R270,
}

impl Rotation {
    /// Android's `user_rotation` value.
    pub fn index(self) -> u8 {
        match self {
            Self::R0 => 0,
            Self::R90 => 1,
            Self::R180 => 2,
            Self::R270 => 3,
        }
    }
    pub fn degrees(self) -> u16 {
        u16::from(self.index()) * 90
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnOff {
    On,
    Off,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Active,
    WorkingSet,
    Frequent,
    Rare,
    Restricted,
}

impl Bucket {
    pub fn android_value(self) -> u32 {
        match self {
            Self::Active => 10,
            Self::WorkingSet => 20,
            Self::Frequent => 30,
            Self::Rare => 40,
            Self::Restricted => 45,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalStatus {
    Light,
    Moderate,
    Severe,
    Critical,
    Emergency,
    Shutdown,
}

impl ThermalStatus {
    /// `android.os.PowerManager.THERMAL_STATUS_*`.
    pub fn code(self) -> u8 {
        match self {
            Self::Light => 1,
            Self::Moderate => 2,
            Self::Severe => 3,
            Self::Critical => 4,
            Self::Emergency => 5,
            Self::Shutdown => 6,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeedProfile {
    Gsm,
    Gprs,
    Edge,
    Umts,
    Hsdpa,
    Lte,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyProfile {
    Gprs,
    Edge,
    Umts,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Notifications,
    QuickSettings,
}

impl ValueName for SpeedProfile {}
impl ValueName for LatencyProfile {}

/// The CLI spelling of a value enum, for echoing params and console commands.
pub trait ValueName: ValueEnum {
    fn name(&self) -> String {
        self.to_possible_value()
            .map(|value| value.get_name().to_string())
            .unwrap_or_default()
    }
}

impl ValueName for TrimLevel {}
impl ValueName for Rotation {}
impl ValueName for OnOff {}
impl ValueName for Bucket {}
impl ValueName for ThermalStatus {}
impl ValueName for Panel {}
