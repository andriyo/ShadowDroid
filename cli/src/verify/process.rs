//! Bounded subprocesses with captured output and explicit interruption semantics.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

const LOG_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub argv: Vec<String>,
    pub cwd: String,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub exit_code: Option<i32>,
    pub interrupted: bool,
    pub timed_out: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub logs_truncated: bool,
    pub termination: Option<String>,
}

struct Capture {
    bytes: Vec<u8>,
    path: std::path::PathBuf,
    saved: bool,
}
impl Capture {
    fn save(&mut self) -> Result<()> {
        let text = String::from_utf8_lossy(&self.bytes);
        crate::cmd::artifact::write_bytes(
            &self.path,
            crate::redaction::redact_text_if_active(&text).as_bytes(),
        )?;
        self.saved = true;
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if !self.saved {
            let _ = self.save();
        }
    }
}

async fn capture(mut reader: impl AsyncRead + Unpin, path: &Path) -> Result<u64> {
    let mut capture = Capture {
        bytes: Vec::new(),
        path: path.to_owned(),
        saved: false,
    };
    let mut total = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        let retain = count.min(LOG_LIMIT.saturating_sub(capture.bytes.len()));
        capture.bytes.extend_from_slice(&buffer[..retain]);
    }
    capture.save()?;
    Ok(total)
}

/// Use the filesystem's own clock granularity for freshness comparisons.
/// Linux inode mtimes can lag CLOCK_REALTIME by a clock tick, even for a file
/// just created by the child. Comparing against a userspace instant falsely
/// rejects these reports. The marker is removed before the child is spawned.
pub fn filesystem_time(cwd: &Path) -> Result<std::time::SystemTime> {
    let marker = tempfile::Builder::new()
        .prefix(".shadowdroid-freshness-")
        .tempfile_in(cwd)
        .context("create filesystem freshness marker")?;
    marker
        .as_file()
        .metadata()?
        .modified()
        .context("read filesystem freshness clock")
}

pub async fn run(
    argv: &[String],
    cwd: &Path,
    timeout_ms: u64,
    out: &Path,
    serial: Option<&crate::ids::Serial>,
) -> Result<Execution> {
    anyhow::ensure!(
        !argv.is_empty() && !argv[0].is_empty(),
        "external command is empty"
    );
    anyhow::ensure!(
        (1..=3_600_000).contains(&timeout_ms),
        "external command timeout must be in 1..3600000 ms"
    );
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(serial) = serial {
        command
            .env("ANDROID_SERIAL", serial.as_str())
            .env("SHADOWDROID_DEVICE", serial.as_str());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let started_ms = crate::runtime::now_ms();
    let mut child = command.spawn().context("launching verification command")?;
    let pid = child.id().context("child has no process ID")?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let stderr = child.stderr.take().context("missing child stderr")?;
    let stdout_path = out.join("stdout.log");
    let stderr_path = out.join("stderr.log");
    let stdout_task = tokio::spawn(async move { capture(stdout, &stdout_path).await });
    let stderr_task = tokio::spawn(async move { capture(stderr, &stderr_path).await });
    let mut timed_out = false;
    let mut interrupted = false;
    let mut termination = None;
    let status = tokio::select! {
        status=child.wait()=>Some(status.context("waiting for verification child")?),
        _=tokio::time::sleep(Duration::from_millis(timeout_ms))=>{timed_out=true;None},
        signal=tokio::signal::ctrl_c()=>{signal?;interrupted=true;None},
    };
    let status = if let Some(status) = status {
        status
    } else {
        termination = Some(stop_tree(pid).await);
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("verification child did not stop; outcome unknown")?
            .context("reaping verification child")?
    };
    // A detached worker may inherit a pipe after its parent exits. Bound the
    // drain and surface that uncertainty rather than hanging after the timeout.
    let stdout_bytes = drain(stdout_task).await?;
    let stderr_bytes = drain(stderr_task).await?;
    Ok(Execution {
        argv: argv.to_vec(),
        cwd: cwd.display().to_string(),
        started_ms,
        finished_ms: crate::runtime::now_ms(),
        exit_code: status.code(),
        interrupted,
        timed_out,
        stdout_bytes,
        stderr_bytes,
        logs_truncated: stdout_bytes > LOG_LIMIT as u64 || stderr_bytes > LOG_LIMIT as u64,
        termination,
    })
}

async fn drain(mut task: tokio::task::JoinHandle<Result<u64>>) -> Result<u64> {
    match tokio::time::timeout(Duration::from_secs(3), &mut task).await {
        Ok(value) => value.context("capturing verification output")?,
        Err(_) => {
            task.abort();
            let _ = task.await;
            anyhow::bail!(
                "child exited but an external worker retained its output pipe; execution outcome is unknown"
            )
        }
    }
}

async fn stop_tree(pid: u32) -> String {
    #[cfg(unix)]
    let result = tokio::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .output()
        .await;
    #[cfg(windows)]
    let result = tokio::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output()
        .await;
    #[cfg(not(any(unix, windows)))]
    let result: std::io::Result<std::process::Output> = Err(std::io::Error::other(
        "process-tree termination unavailable",
    ));
    match result {
        Ok(output) if output.status.success() => "process_tree_stop_requested".into(),
        Ok(output) => format!(
            "process_tree_stop_unconfirmed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => format!("process_tree_stop_unavailable: {e}"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn timeout_reaps_the_child_and_prevents_delayed_group_writes() {
        let dir = tempfile::tempdir().unwrap();
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "(sleep 1; echo late > marker) & wait".into(),
        ];
        let result = run(&command, dir.path(), 100, dir.path(), None)
            .await
            .unwrap();
        assert!(result.timed_out);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!dir.path().join("marker").exists());
        assert!(dir.path().join("stdout.log").is_file());
    }
}
