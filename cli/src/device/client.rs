//! HTTP client to the on-device ShadowDroid server.
//!
//! Sensible timeouts:
//!   - 2s connect
//!   - 30s for UI operations (most return in <100ms; some shell commands may
//!     legitimately take longer)
//!
//! On a 4xx/5xx response, we try to deserialise the wire error envelope so the
//! caller gets the structured `code` + `message` rather than just a raw status.

#![allow(dead_code)]

use crate::proto::*;
use anyhow::{anyhow, bail, Result};
use reqwest::{Client, Response};
use serde::Serialize;
use std::time::Duration;

pub struct ServerClient {
    base: String, // e.g. "http://127.0.0.1:7912/v1"
    http: Client,
}

impl ServerClient {
    pub fn new(port: u16) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base: format!("http://127.0.0.1:{port}/v1"),
            http,
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

    pub async fn screen(&self) -> Result<ScreenResponse> {
        self.get("/screen").await
    }

    pub async fn screen_xml(&self) -> Result<String> {
        let resp = self
            .http
            .get(format!("{}/screen?format=xml", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.text().await?)
    }

    pub async fn screenshot_png(&self) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(format!("{}/screenshot.png", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.bytes().await?.to_vec())
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

    pub async fn key(&self, name: &str) -> Result<()> {
        let _: OkResponse = self
            .post("/key", &serde_json::json!({"name": name}))
            .await?;
        Ok(())
    }
    pub async fn key_code(&self, code: i32) -> Result<()> {
        let _: OkResponse = self
            .post("/key", &serde_json::json!({"code": code}))
            .await?;
        Ok(())
    }
    pub async fn text(&self, value: &str, clear: bool) -> Result<()> {
        let _: OkResponse = self
            .post(
                "/text",
                &serde_json::json!({"value": value, "clear": clear}),
            )
            .await?;
        Ok(())
    }

    // ── app lifecycle ──────────────────────────────────────────

    pub async fn app_start(&self, package: &str) -> Result<()> {
        let _: OkResponse = self
            .post("/app/start", &serde_json::json!({"package": package}))
            .await?;
        Ok(())
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
        self.post(
            "/app/wait",
            &serde_json::json!({"package": package, "timeout_ms": timeout_ms, "front": front}),
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
        self.post(
            "/shell",
            &serde_json::json!({"cmd": cmd, "timeout_ms": timeout_ms}),
        )
        .await
    }
}

/// Check response status; on non-2xx, try to parse our wire-error envelope
/// for a useful Rust error instead of just `error decoding response body`.
async fn check_then_json<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp.json().await?);
    }
    // Try to decode the structured error
    let text = resp.text().await?;
    if let Ok(env) = serde_json::from_str::<ErrorEnvelope>(&text) {
        bail!(
            "server error {}: {} ({})",
            status,
            env.error.message,
            env.error.code
        );
    }
    Err(anyhow!("server returned {}: {}", status, text))
}
