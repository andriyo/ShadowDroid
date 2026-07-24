//! Line-delimited JSON control plane for the per-device video daemon.

use super::paths;
use crate::ids::Serial;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub async fn request(serial: &Serial, mut value: Value) -> Result<Value> {
    if let Value::Object(fields) = &mut value {
        fields.insert("serial".into(), serial.as_str().into());
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        request_unbounded(serial, value),
    )
    .await
    .map_err(|_| {
        crate::diagnostic::DiagnosticError::new(
            "video_control_timeout",
            "video",
            "video daemon did not reply within 5 seconds",
        )
        .retryable(true)
        .detail(json!({"device": serial.as_str(), "timeout_ms": 5000}))
        .next_actions([
            "shadowdroid video status",
            "shadowdroid video stop",
            "shadowdroid doctor --json",
        ])
    })?
}

async fn request_unbounded(serial: &Serial, value: Value) -> Result<Value> {
    let port = control_port(serial)?;
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .with_context(|| format!("connect to video daemon for {serial}"))?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(&value)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await?;
    let mut lines = BufReader::new(read).lines();
    let response = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("video daemon closed without a response"))?;
    serde_json::from_str(&response).context("parse video daemon response")
}

pub fn control_port(serial: &Serial) -> Result<u16> {
    let path = paths::control(serial)?;
    let value =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    value
        .trim()
        .parse()
        .with_context(|| format!("parse video control port from {}", path.display()))
}

pub enum StatusProbe {
    Absent,
    Running(Value),
    Unreachable(String),
}

pub async fn probe_status(serial: &Serial) -> StatusProbe {
    let path = match paths::control(serial) {
        Ok(path) => path,
        Err(error) => return StatusProbe::Unreachable(error.to_string()),
    };
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return StatusProbe::Absent;
        }
        Err(error) => return StatusProbe::Unreachable(error.to_string()),
    }
    match request(serial, json!({"op": "status"})).await {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            StatusProbe::Running(value)
        }
        Ok(value) => {
            StatusProbe::Unreachable(format!("video daemon rejected status request: {value}"))
        }
        Err(error) => StatusProbe::Unreachable(format!("{error:#}")),
    }
}
