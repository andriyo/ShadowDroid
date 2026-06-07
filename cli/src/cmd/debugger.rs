//! Host-side Android Studio debugger bridge commands.
//!
//! These commands talk to the ShadowDroid Android Studio plugin over its local
//! loopback HTTP bridge. They do not need the on-device ShadowDroid server.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_URL: &str = "http://127.0.0.1:50576";
const REGISTRY_PATH: &str = ".shadowdroid/studio-debugger.json";

#[derive(Args)]
pub struct DebuggerArgs {
    /// Android Studio plugin bridge URL. Defaults to the plugin registry, then
    /// http://127.0.0.1:50576.
    #[arg(long, env = "SHADOWDROID_STUDIO_DEBUGGER_URL")]
    pub url: Option<String>,

    #[command(subcommand)]
    pub cmd: DebuggerCmd,
}

#[derive(Subcommand)]
pub enum DebuggerCmd {
    /// Show bridge status, open projects, and active debugger sessions.
    Status,
    /// List active Android Studio debugger sessions.
    Sessions,
    /// List attachable Android processes visible to Android Studio.
    Clients(AndroidClientArgs),
    /// Ask Android Studio to attach its debugger to a running app.
    Attach {
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
        /// App package/process to attach to.
        #[arg(long)]
        package: Option<String>,
        /// Process id to attach to.
        #[arg(long)]
        pid: Option<i32>,
        /// Device serial to prefer when Studio has several devices.
        #[arg(long)]
        device: Option<String>,
        /// Android debugger id/display name. Defaults to Studio's Java Android debugger.
        #[arg(long)]
        debugger: Option<String>,
        /// Android Studio run configuration whose debugger settings should be reused.
        #[arg(long)]
        configuration: Option<String>,
        /// Open Android Studio's built-in attach dialog instead of attaching headlessly.
        #[arg(long)]
        dialog: bool,
    },
    /// Breakpoint commands.
    #[command(subcommand)]
    Break(BreakCmd),
    /// List line breakpoints known to Android Studio.
    Breakpoints,
    /// Pause the selected debug session.
    Pause(SessionSelector),
    /// Resume the selected debug session.
    Resume(SessionSelector),
    /// Step into from the selected suspended session.
    StepIn(SessionSelector),
    /// Step over from the selected suspended session.
    StepOver(SessionSelector),
    /// Step out from the selected suspended session.
    StepOut(SessionSelector),
    /// Stop the selected debug session.
    Stop(SessionSelector),
    /// Print stack frames for the selected suspended session.
    Stack(StackArgs),
    /// Print debugger threads and their stack frames.
    Threads(StackArgs),
    /// Print visible variables for the selected suspended frame.
    Variables(SessionSelector),
}

#[derive(Subcommand)]
pub enum BreakCmd {
    /// Add a Java/Kotlin line breakpoint.
    Line {
        /// Source file path.
        #[arg(long)]
        file: PathBuf,
        /// One-based source line number.
        #[arg(long)]
        line: u32,
        /// Project name or absolute project path when multiple projects are open.
        #[arg(long)]
        project: Option<String>,
        /// Create the breakpoint disabled.
        #[arg(long)]
        disabled: bool,
        /// Create a temporary breakpoint.
        #[arg(long)]
        temporary: bool,
    },
}

#[derive(Args)]
pub struct SessionSelector {
    /// Debug session index from `debugger sessions`.
    #[arg(long)]
    pub session: Option<usize>,
}

#[derive(Args)]
pub struct StackArgs {
    /// Debug session index from `debugger sessions`.
    #[arg(long)]
    pub session: Option<usize>,
    /// Maximum number of frames per stack.
    #[arg(long, default_value_t = 64)]
    pub limit: u32,
}

#[derive(Args)]
pub struct AndroidClientArgs {
    /// Project name or absolute project path when multiple projects are open.
    #[arg(long)]
    pub project: Option<String>,
    /// Filter by app package/process.
    #[arg(long)]
    pub package: Option<String>,
    /// Filter by process id.
    #[arg(long)]
    pub pid: Option<i32>,
    /// Filter by device serial.
    #[arg(long)]
    pub device: Option<String>,
}

pub async fn run(args: &DebuggerArgs) -> Result<()> {
    let bridge = BridgeClient::new(args.url.as_deref())?;
    let value = match &args.cmd {
        DebuggerCmd::Status => bridge.get("/v1/status", &[]).await?,
        DebuggerCmd::Sessions => bridge.get("/v1/sessions", &[]).await?,
        DebuggerCmd::Clients(filter) => {
            let pid_s = filter.pid.map(|pid| pid.to_string());
            let params = [
                ("project", filter.project.as_deref()),
                ("package", filter.package.as_deref()),
                ("pid", pid_s.as_deref()),
                ("device", filter.device.as_deref()),
            ];
            bridge.get("/v1/clients", &params).await?
        }
        DebuggerCmd::Attach {
            project,
            package,
            pid,
            device,
            debugger,
            configuration,
            dialog,
        } => {
            let pid_s = pid.map(|pid| pid.to_string());
            let dialog_s = dialog.to_string();
            let params = [
                ("project", project.as_deref()),
                ("package", package.as_deref()),
                ("pid", pid_s.as_deref()),
                ("device", device.as_deref()),
                ("debugger", debugger.as_deref()),
                ("configuration", configuration.as_deref()),
                ("dialog", Some(dialog_s.as_str())),
            ];
            bridge.get("/v1/attach", &params).await?
        }
        DebuggerCmd::Break(BreakCmd::Line {
            file,
            line,
            project,
            disabled,
            temporary,
        }) => {
            let canonical = canonicalize_for_bridge(file)?;
            let line_s = line.to_string();
            let enabled_s = (!*disabled).to_string();
            let temporary_s = temporary.to_string();
            let params = [
                ("file", Some(canonical.as_str())),
                ("line", Some(line_s.as_str())),
                ("project", project.as_deref()),
                ("enabled", Some(enabled_s.as_str())),
                ("temporary", Some(temporary_s.as_str())),
            ];
            bridge.get("/v1/breakpoints/line", &params).await?
        }
        DebuggerCmd::Breakpoints => bridge.get("/v1/breakpoints", &[]).await?,
        DebuggerCmd::Pause(selector) => control(&bridge, "pause", selector).await?,
        DebuggerCmd::Resume(selector) => control(&bridge, "resume", selector).await?,
        DebuggerCmd::StepIn(selector) => control(&bridge, "step_into", selector).await?,
        DebuggerCmd::StepOver(selector) => control(&bridge, "step_over", selector).await?,
        DebuggerCmd::StepOut(selector) => control(&bridge, "step_out", selector).await?,
        DebuggerCmd::Stop(selector) => control(&bridge, "stop", selector).await?,
        DebuggerCmd::Stack(args) => {
            let session_s = args.session.map(|s| s.to_string());
            let limit_s = args.limit.to_string();
            let params = [
                ("session", session_s.as_deref()),
                ("limit", Some(limit_s.as_str())),
            ];
            bridge.get("/v1/session/stack", &params).await?
        }
        DebuggerCmd::Threads(args) => {
            let session_s = args.session.map(|s| s.to_string());
            let limit_s = args.limit.to_string();
            let params = [
                ("session", session_s.as_deref()),
                ("limit", Some(limit_s.as_str())),
            ];
            bridge.get("/v1/session/threads", &params).await?
        }
        DebuggerCmd::Variables(selector) => {
            let session_s = selector.session.map(|s| s.to_string());
            let params = [("session", session_s.as_deref())];
            bridge.get("/v1/session/variables", &params).await?
        }
    };
    emit(&value)?;
    Ok(())
}

async fn control(
    bridge: &BridgeClient,
    action: &'static str,
    selector: &SessionSelector,
) -> Result<Value> {
    let session_s = selector.session.map(|s| s.to_string());
    let params = [("action", Some(action)), ("session", session_s.as_deref())];
    bridge.get("/v1/session/control", &params).await
}

fn emit(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn canonicalize_for_bridge(path: &Path) -> Result<String> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("source file not found: {}", path.display()))?;
    Ok(canonical.display().to_string())
}

struct BridgeClient {
    base_url: String,
    http: reqwest::Client,
}

impl BridgeClient {
    fn new(explicit_url: Option<&str>) -> Result<Self> {
        let base_url = resolve_url(explicit_url)?;
        Ok(Self {
            base_url,
            http: reqwest::Client::new(),
        })
    }

    async fn get(&self, path: &str, params: &[(&str, Option<&str>)]) -> Result<Value> {
        let url = self.url(path, params);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| {
                format!(
                    "connecting to Android Studio debugger bridge at {}. Install/start the ShadowDroid Android Studio plugin or pass --url.",
                    self.base_url
                )
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("reading debugger bridge response")?;
        let value: Value = serde_json::from_str(&body)
            .with_context(|| format!("debugger bridge returned non-JSON response: {body}"))?;
        if !status.is_success() {
            let message = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            bail!("debugger bridge request failed (HTTP {status}): {message}");
        }
        Ok(value)
    }

    fn url(&self, path: &str, params: &[(&str, Option<&str>)]) -> String {
        let mut url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let query = params
            .iter()
            .filter_map(|(key, value)| value.map(|v| (*key, v)))
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        url
    }
}

fn resolve_url(explicit_url: Option<&str>) -> Result<String> {
    if let Some(url) = explicit_url {
        if !url.trim().is_empty() {
            return Ok(url.trim().to_string());
        }
    }
    if let Some(url) = registry_url()? {
        return Ok(url);
    }
    Ok(DEFAULT_URL.to_string())
}

fn registry_url() -> Result<Option<String>> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    let path = PathBuf::from(home).join(REGISTRY_PATH);
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading debugger bridge registry {}", path.display()))?;
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing debugger bridge registry {}", path.display()))?;
    Ok(value
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
        .map(|url| url.trim().to_string()))
}
