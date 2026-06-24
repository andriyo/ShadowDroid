//! Host-side MITM networking proxy — the `net` namespace.
//!
//! ShadowDroid's networking layer lets an agent **see** an app's HTTP(S) traffic
//! alongside screen changes, **intercept** flows to inspect/modify them in the
//! reasoning loop, and apply declarative **rules** — modelled on mitmproxy but
//! hand-rolled on `hyper`+`tokio-rustls`+`rcgen` to keep the single binary.
//!
//! Runtime shape:
//!   - The proxy runs as a **background daemon** ([daemon]) so a *held*
//!     intercepted flow survives across the agent's discrete one-shot commands.
//!   - Control is a loopback-TCP socket (port in a `.ctl` file under
//!     `~/.shadowdroid/net/`, [paths]); `net resume`/`drop`/`status`/… are
//!     short-lived clients of it ([control]).
//!   - The device is pointed at the proxy with `adb reverse` + `settings put
//!     global http_proxy` ([commands::start]); trust via the ShadowDroid CA
//!     ([ca], installed by [trust]).
//!
//! `net` is **host-only**: like `doctor`/`perm`, it composes `adb` + the daemon
//! and never needs the on-device UI server — except `trust --ui`, which drives
//! the Settings cert-install flow with ShadowDroid's own UI automation.

pub mod ca;
pub mod check;
pub mod commands;
pub mod control;
pub mod daemon;
pub mod export;
pub mod flow;
pub mod paths;
pub mod proxy;
pub mod store;
pub mod trust;

/// Default proxy listen/forward port (device `http_proxy` → host proxy).
pub const DEFAULT_PROXY_PORT: u16 = 8080;

/// A flow matcher — explicit, composable fields (agent-legible). All present
/// fields must match (AND). Host/path/method are case-insensitive substring
/// matches; status is exact. Empty matcher matches everything.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Matcher {
    pub host: Option<String>,
    pub path: Option<String>,
    pub method: Option<String>,
    pub status: Option<u16>,
}

/// A mutation applied to a held flow on `net resume` (mirrors mitmproxy's flow
/// edits). All fields optional; absent = leave as-is.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Mutation {
    pub set_status: Option<u16>,
    pub set_headers: Vec<(String, String)>,
    pub remove_headers: Vec<String>,
    /// Raw replacement body (base64 on the wire to stay binary-safe).
    pub body: Option<Vec<u8>>,
    /// `(regex, replacement)` applied to the (textual) body.
    pub replace: Option<(String, String)>,
    pub delay_ms: Option<u32>,
    /// Request-phase only: redirect the outgoing request.
    pub set_url: Option<String>,
}

impl Mutation {
    pub fn is_noop(&self) -> bool {
        self.set_status.is_none()
            && self.set_headers.is_empty()
            && self.remove_headers.is_empty()
            && self.body.is_none()
            && self.replace.is_none()
            && self.delay_ms.is_none()
            && self.set_url.is_none()
    }
}

/// A declarative rule (P3). `kind` selects the transform; `args` are
/// kind-specific positionals (e.g. map-local → `[path]`, set-status → `[code]`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuleSpec {
    pub kind: String,
    #[serde(default)]
    pub matcher: Matcher,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Config handed to the daemon process at spawn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DaemonConfig {
    pub serial: crate::ids::Serial,
    pub port: u16,
    /// Best-effort app scoping (host allowlist is the practical filter today).
    pub app_filters: Vec<String>,
    pub anticache: bool,
    pub anticomp: bool,
}
