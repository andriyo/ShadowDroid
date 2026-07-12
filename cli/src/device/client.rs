//! HTTP client to the on-device ShadowDroid server.
//!
//! Sensible timeouts:
//!   - 2s connect
//!   - 30s for UI operations (most return in <100ms; some shell commands may
//!     legitimately take longer)
//!
//! On a 4xx/5xx response, we try to deserialise the wire error envelope so the
//! caller gets the structured `code` + `message` rather than just a raw status.

use crate::proto::*;
use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Response};
use serde::Serialize;
use std::path::Path;
use std::time::Duration;
use tokio_util::io::ReaderStream;

#[derive(Clone)]
pub struct ServerClient {
    base: String, // e.g. "http://127.0.0.1:7912/v1"
    http: Client,
    transfer_http: Client,
}

impl ServerClient {
    pub fn new(port: u16) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30))
            .build()?;
        // File transfers are streamed and may legitimately exceed the normal
        // operation wall-clock budget. Bound connect and idle reads instead of
        // cutting off a healthy progressing transfer at a fixed duration.
        let transfer_http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base: format!("http://127.0.0.1:{port}/v1"),
            http,
            transfer_http,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .send()
            .await?;
        check_then_json(resp).await
    }

    async fn get_retry<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        attempts: usize,
        initial_delay: Duration,
    ) -> Result<T> {
        debug_assert!(attempts > 0);
        let mut delay = initial_delay;
        for attempt in 1..=attempts {
            match self.get(path).await {
                Ok(value) => return Ok(value),
                Err(err) if attempt < attempts && is_transient_transport_error(&err) => {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("attempts > 0 and loop always returns")
    }

    async fn post<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .json(body)
            .send()
            .await?;
        check_then_json(resp).await
    }

    /// POST with a command-specific wall-clock budget. Long-running server
    /// operations such as app waits and device shell commands must not inherit
    /// the generic 30-second UI timeout, but they also must not run without a
    /// host-side deadline.
    async fn post_with_timeout<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .timeout(timeout)
            .json(body)
            .send()
            .await?;
        check_then_json(resp).await
    }

    async fn post_empty<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}{}", self.base, path))
            .send()
            .await?;
        check_then_json(resp).await
    }

    // ── inspection ───────────────────────────────────────────────

    pub async fn state(&self) -> Result<ServerState> {
        self.get("/state").await
    }

    /// `GET /v1/device` — detailed device facts. Returns a typed 404 marker via
    /// the error when the server predates this route, so the caller can fall
    /// back to `/state` + getprop.
    pub async fn device(&self) -> Result<DeviceInfo> {
        self.get("/device").await
    }

    pub async fn screen(&self) -> Result<ScreenResponse> {
        self.get_retry("/screen", 4, Duration::from_millis(75))
            .await
    }

    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        self.screenshot(None, None, None).await
    }

    /// `GET /v1/screenshot.png` with optional server-side encoding controls.
    /// Unknown params are simply ignored by older servers (they always return
    /// a full-resolution PNG), so this stays backward-compatible.
    pub async fn screenshot(
        &self,
        format: Option<&str>,
        scale: Option<f32>,
        quality: Option<u32>,
    ) -> Result<Vec<u8>> {
        let mut q: Vec<String> = Vec::new();
        if let Some(f) = format {
            q.push(format!("format={f}"));
        }
        if let Some(s) = scale {
            q.push(format!("scale={s}"));
        }
        if let Some(qual) = quality {
            q.push(format!("quality={qual}"));
        }
        let query = if q.is_empty() {
            String::new()
        } else {
            format!("?{}", q.join("&"))
        };
        let resp = self
            .http
            .get(format!("{}/screenshot.png{query}", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// `POST /v1/pinch` — pinch in/out on the element matched by a selector
    /// (`UiObject2.pinchIn/Out` needs a real object handle, so pinch targets a
    /// selector rather than a dump element id).
    pub async fn pinch(
        &self,
        rid: Option<&str>,
        text: Option<&str>,
        desc: Option<&str>,
        direction: &str,
        percent: u32,
    ) -> Result<()> {
        let mut body = serde_json::Map::new();
        if let Some(v) = rid {
            body.insert("rid".into(), v.into());
        }
        if let Some(v) = text {
            body.insert("text".into(), v.into());
        }
        if let Some(v) = desc {
            body.insert("desc".into(), v.into());
        }
        body.insert("direction".into(), direction.into());
        body.insert("percent".into(), percent.into());
        let _: OkResponse = self
            .post("/pinch", &serde_json::Value::Object(body))
            .await?;
        Ok(())
    }

    pub async fn find(&self, query: &SelectorQuery) -> Result<FindResp> {
        self.post("/find", query).await
    }

    /// `POST /v1/scroll` — fast on-device scroll-to. Errors (notably 404 on an
    /// older server, or no scrollable container) signal the caller to fall back
    /// to the host scroll loop.
    #[allow(clippy::too_many_arguments)]
    pub async fn scroll(
        &self,
        rid: Option<&str>,
        text: Option<&str>,
        desc: Option<&str>,
        direction: &str,
        container_rid: Option<&str>,
        max_swipes: u32,
        tap: bool,
    ) -> Result<ScrollResp> {
        let mut body = serde_json::Map::new();
        if let Some(v) = rid {
            body.insert("rid".into(), v.into());
        }
        if let Some(v) = text {
            body.insert("text".into(), v.into());
        }
        if let Some(v) = desc {
            body.insert("desc".into(), v.into());
        }
        if let Some(v) = container_rid {
            body.insert("container_rid".into(), v.into());
        }
        body.insert("direction".into(), direction.into());
        body.insert("max_swipes".into(), max_swipes.into());
        body.insert("tap".into(), tap.into());
        self.post("/scroll", &serde_json::Value::Object(body)).await
    }

    pub async fn find_tap(&self, query: &SelectorQuery) -> Result<FindTapResp> {
        self.post("/find_tap", query).await
    }

    pub async fn xpath(&self, query: &str, tap: bool) -> Result<FindResp> {
        self.post(
            "/xpath",
            &XpathReq {
                query: query.to_string(),
                tap,
            },
        )
        .await
    }

    pub async fn xpath_tap(&self, query: &str) -> Result<FindTapResp> {
        let resp = self
            .http
            .post(format!("{}/xpath", self.base))
            .json(&XpathReq {
                query: query.to_string(),
                tap: true,
            })
            .send()
            .await?;
        check_then_json(resp).await
    }

    // ── gestures ────────────────────────────────────────────────

    pub async fn tap_xy(&self, x: i32, y: i32) -> Result<()> {
        let _: OkResponse = self
            .post("/tap", &serde_json::json!({"x": x, "y": y}))
            .await?;
        Ok(())
    }
    pub async fn double_tap(&self, x: i32, y: i32) -> Result<()> {
        let _: OkResponse = self
            .post("/double_tap", &serde_json::json!({"x": x, "y": y}))
            .await?;
        Ok(())
    }
    pub async fn long_tap(&self, x: i32, y: i32, duration_ms: u32) -> Result<()> {
        let _: OkResponse = self
            .post(
                "/long_tap",
                &serde_json::json!({"x": x, "y": y, "duration_ms": duration_ms}),
            )
            .await?;
        Ok(())
    }
    pub async fn swipe(&self, x1: i32, y1: i32, x2: i32, y2: i32, duration_ms: u32) -> Result<()> {
        let _: OkResponse = self
            .post(
                "/swipe",
                &serde_json::json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
            )
            .await?;
        Ok(())
    }
    pub async fn drag(&self, x1: i32, y1: i32, x2: i32, y2: i32, duration_ms: u32) -> Result<()> {
        let _: OkResponse = self
            .post(
                "/drag",
                &serde_json::json!({"from":[x1,y1],"to":[x2,y2],"duration_ms":duration_ms}),
            )
            .await?;
        Ok(())
    }
    pub async fn swipe_ext(&self, direction: &str, scale: f32, duration_ms: u32) -> Result<()> {
        let _: OkResponse = self
            .post(
                "/swipe_ext",
                &serde_json::json!({"direction":direction,"scale":scale,"duration_ms":duration_ms}),
            )
            .await?;
        Ok(())
    }

    // ── keys + text ────────────────────────────────────────────

    /// Press a named key. Returns the server's raw injection result: on
    /// Android 14+ `UiDevice.pressKey*` reports `false` even when the key was
    /// delivered, so this is advisory (`injected`) rather than a success flag —
    /// the call still resolves `Ok` for any valid key.
    pub async fn key(&self, name: &str) -> Result<bool> {
        // The on-device `/key` route accepts either a named key (`name`, mapped
        // server-side) or a raw Android KeyEvent keycode (`code`). The CLI exposes
        // a single `<NAME>` arg documented as "named key or keycode", so route a
        // bare integer to `code` and everything else to `name`.
        let body = match name.trim().parse::<i32>() {
            Ok(code) => serde_json::json!({ "code": code }),
            Err(_) => serde_json::json!({ "name": name }),
        };
        let r: OkResponse = self.post("/key", &body).await?;
        Ok(r.ok)
    }
    pub async fn text(&self, value: &str, clear: bool) -> Result<()> {
        self.text_with_target(value, clear, None).await
    }

    pub async fn text_with_target(
        &self,
        value: &str,
        clear: bool,
        target: Option<&SelectorQuery>,
    ) -> Result<()> {
        let mut body = serde_json::Map::new();
        body.insert("value".into(), value.into());
        body.insert("clear".into(), clear.into());
        if let Some(target) = target {
            if let Some(id) = target.id {
                body.insert("id".into(), id.into());
            }
            if let Some(v) = target.text.as_deref() {
                body.insert("text".into(), v.into());
            }
            if let Some(v) = target.rid.as_deref() {
                body.insert("rid".into(), v.into());
            }
            if let Some(v) = target.desc.as_deref() {
                body.insert("desc".into(), v.into());
            }
            if let Some(v) = target.klass.as_deref() {
                body.insert("klass".into(), v.into());
            }
            if let Some(v) = target.xpath.as_deref() {
                body.insert("xpath".into(), v.into());
            }
            if target.exact {
                body.insert("exact".into(), true.into());
            }
        }
        let _: OkResponse = self.post("/text", &serde_json::Value::Object(body)).await?;
        Ok(())
    }

    // ── app lifecycle ──────────────────────────────────────────

    pub async fn app_start(&self, package: &str, activity: Option<&str>) -> Result<AppStartResp> {
        let mut body = serde_json::json!({"package": package});
        if let Some(activity) = activity {
            body["activity"] = serde_json::json!(activity);
        }
        self.post("/app/start", &body).await
    }
    pub async fn app_stop(&self, package: &str) -> Result<()> {
        let _: OkResponse = self
            .post("/app/stop", &serde_json::json!({"package": package}))
            .await?;
        Ok(())
    }
    pub async fn app_clear(&self, package: &str) -> Result<()> {
        let _: OkResponse = self
            .post("/app/clear", &serde_json::json!({"package": package}))
            .await?;
        Ok(())
    }
    pub async fn app_wait(
        &self,
        package: &str,
        timeout_ms: u32,
        front: bool,
    ) -> Result<AppWaitResp> {
        self.post_with_timeout(
            "/app/wait",
            &serde_json::json!({"package": package, "timeout_ms": timeout_ms, "front": front}),
            Duration::from_millis(timeout_ms as u64 + 1_000),
        )
        .await
    }
    pub async fn app_info(&self, package: &str) -> Result<AppInfo> {
        self.get(&format!(
            "/app/info?package={}",
            urlencoding::encode(package)
        ))
        .await
    }
    pub async fn app_current(&self) -> Result<AppRef> {
        self.get("/app/current").await
    }

    // ── system ─────────────────────────────────────────────────

    pub async fn screen_on(&self) -> Result<()> {
        let _: OkResponse = self.post_empty("/screen/on").await?;
        Ok(())
    }
    pub async fn screen_off(&self) -> Result<()> {
        let _: OkResponse = self.post_empty("/screen/off").await?;
        Ok(())
    }
    pub async fn wakeup(&self) -> Result<()> {
        let _: OkResponse = self.post_empty("/wakeup").await?;
        Ok(())
    }
    pub async fn unlock(&self) -> Result<()> {
        let _: OkResponse = self.post_empty("/unlock").await?;
        Ok(())
    }

    pub async fn orientation_get(&self) -> Result<String> {
        let r: OrientationResp = self.get("/orientation").await?;
        Ok(r.value)
    }
    pub async fn orientation_set(&self, value: &str) -> Result<()> {
        let _: OkResponse = self
            .post("/orientation", &serde_json::json!({"value": value}))
            .await?;
        Ok(())
    }

    pub async fn clipboard_get(&self) -> Result<Option<String>> {
        let r: ClipResp = self.get("/clipboard").await?;
        Ok(r.value)
    }
    pub async fn clipboard_set(&self, value: &str) -> Result<()> {
        let _: OkResponse = self
            .post("/clipboard", &serde_json::json!({"value": value}))
            .await?;
        Ok(())
    }

    pub async fn open_notifications(&self) -> Result<()> {
        let _: OkResponse = self.post_empty("/notifications/open").await?;
        Ok(())
    }
    pub async fn open_quick_settings(&self) -> Result<()> {
        let _: OkResponse = self.post_empty("/quick_settings/open").await?;
        Ok(())
    }
    pub async fn open_url(&self, url: &str) -> Result<()> {
        let _: OkResponse = self
            .post("/url/open", &serde_json::json!({"url": url}))
            .await?;
        Ok(())
    }

    pub async fn shell(&self, cmd: &str, timeout_ms: u32) -> Result<ShellResp> {
        self.post_with_timeout(
            "/shell",
            &serde_json::json!({"cmd": cmd, "timeout_ms": timeout_ms}),
            Duration::from_millis(timeout_ms.max(1) as u64 + 1_000),
        )
        .await
    }

    pub async fn toast_start(&self, buffer_size: u32) -> Result<()> {
        let _: OkResponse = self
            .post(
                "/toast/start",
                &serde_json::json!({"buffer_size": buffer_size}),
            )
            .await?;
        Ok(())
    }

    pub async fn toast_recent(&self, since_ts: u64) -> Result<ToastRecentResp> {
        self.get(&format!("/toast/recent?since_ts={since_ts}"))
            .await
    }

    pub async fn push_file(
        &self,
        remote: &str,
        local: &Path,
        mode: Option<u32>,
    ) -> Result<FileWriteResp> {
        let mut path = file_path(remote);
        if let Some(mode) = mode {
            path.push_str(&format!("?mode={mode}"));
        }
        let file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("open {}", local.display()))?;
        let bytes = file
            .metadata()
            .await
            .with_context(|| format!("stat {}", local.display()))?
            .len();
        let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
        let resp = self
            .transfer_http
            .put(format!("{}{}", self.base, path))
            .header(reqwest::header::CONTENT_LENGTH, bytes)
            .body(body)
            .send()
            .await?;
        check_then_json(resp).await
    }

    pub async fn pull_file_response(&self, remote: &str) -> Result<Response> {
        let resp = self
            .transfer_http
            .get(format!("{}{}", self.base, file_path(remote)))
            .send()
            .await?;
        check_response(resp).await
    }

    /// `GET /v1/files/{dir}?list=true` — directory listing.
    pub async fn list_dir(&self, remote: &str) -> Result<FileListResp> {
        let resp = self
            .http
            .get(format!("{}{}?list=true", self.base, file_path(remote)))
            .send()
            .await?;
        check_then_json(resp).await
    }
}

pub(crate) fn is_transient_transport_error(err: &anyhow::Error) -> bool {
    if let Some(reqwest) = err.downcast_ref::<reqwest::Error>()
        && (reqwest.is_connect()
            || reqwest.is_timeout()
            || reqwest.is_request()
            || reqwest.is_body())
    {
        return true;
    }
    let message = err.to_string();
    message.contains("connection closed before message completed")
        || message.contains("error sending request")
        || message.contains("connection reset")
        || message.contains("connection refused")
        || message.contains("operation timed out")
}

/// A structured non-2xx response from the on-device server, carrying the wire
/// envelope's machine `code` (e.g. `element_not_found`) alongside the HTTP
/// status. It is surfaced as an `anyhow` error whose chain the CLI walks
/// (`cli::report_error`) to render a `{"type":"error","code":…}` object on
/// stdout instead of an opaque string. The `Display` form is kept identical to
/// the prior `bail!` message so log/string consumers see no change.
#[derive(Debug, thiserror::Error)]
#[error("server error {status}: {message} ({code})")]
pub struct ServerError {
    pub status: reqwest::StatusCode,
    pub code: String,
    pub message: String,
    /// The envelope's optional `detail` object (e.g. `ambiguous_match`'s
    /// candidate list), surfaced verbatim in the CLI's error JSON.
    pub detail: Option<serde_json::Value>,
}

/// Check response status; on non-2xx, try to parse our wire-error envelope
/// for a useful Rust error instead of just `error decoding response body`.
async fn check_then_json<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    Ok(check_response(resp).await?.json().await?)
}

async fn check_response(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    // Try to decode the structured error
    let text = resp.text().await?;
    if let Ok(env) = serde_json::from_str::<ErrorEnvelope>(&text) {
        return Err(ServerError {
            status,
            code: env.error.code,
            message: env.error.message,
            detail: env.error.detail,
        }
        .into());
    }
    Err(anyhow!("server returned {}: {}", status, text))
}

fn file_path(path: &str) -> String {
    let trimmed = path.trim();
    let normalized = trimmed.trim_start_matches('/');
    let encoded = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| urlencoding::encode(part).into_owned())
        .collect::<Vec<_>>()
        .join("/");
    format!("/files/{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_screen_transport_errors_as_transient() {
        let closed = anyhow!(
            "error sending request for url (http://127.0.0.1:7912/v1/screen): \
             connection closed before message completed"
        );
        assert!(is_transient_transport_error(&closed));

        let server = anyhow!("server error 500 Internal Server Error: boom");
        assert!(!is_transient_transport_error(&server));
    }
}
