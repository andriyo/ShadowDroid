//! How to undo a fault, recorded as data before the fault is applied, so a
//! later `fault clear` — from another process, after a crash — can finish it.

use super::device;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verify {
    /// A read-only command whose output must contain `contains`.
    pub command: String,
    pub contains: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum RestoreStep {
    /// Put the setting back (`None`: it was unset, so delete it).
    Setting {
        namespace: String,
        key: String,
        value: Option<String>,
    },
    Shell {
        command: String,
        describe: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verify: Option<Verify>,
    },
    Emu {
        args: Vec<String>,
        describe: String,
    },
    /// Set the clock to the true time: the device time at inject plus the
    /// uptime that has passed since (independent of the host's clock).
    Clock {
        epoch_ms: u64,
        uptime_ms: u64,
    },
    /// Stop every process whose pid file is in `dir`, then remove it.
    KillPids {
        dir: String,
    },
    RemoveFile {
        path: String,
    },
    /// Remove a fault from the ShadowDroid proxy.
    ProxyFault {
        id: String,
    },
}

impl RestoreStep {
    pub fn describe(&self) -> String {
        match self {
            Self::Setting {
                namespace,
                key,
                value: Some(value),
            } => format!("set {namespace} setting {key} back to {value}"),
            Self::Setting {
                namespace,
                key,
                value: None,
            } => format!("delete {namespace} setting {key} (it was unset)"),
            Self::Shell { describe, .. } | Self::Emu { describe, .. } => describe.clone(),
            Self::Clock { .. } => "set the clock back to the true time".into(),
            Self::KillPids { dir } => format!("stop the background processes recorded in {dir}"),
            Self::RemoveFile { path } => format!("delete {path}"),
            Self::ProxyFault { id } => format!("remove proxy fault {id}"),
        }
    }

    pub async fn apply(&self, serial: &str) -> Result<()> {
        match self {
            Self::Setting {
                namespace,
                key,
                value,
            } => device::put_setting(serial, namespace, key, value.as_deref()).await,
            Self::Shell {
                command, verify, ..
            } => {
                device::run_ok(serial, command).await?;
                if let Some(verify) = verify {
                    wait_for(serial, verify).await?;
                }
                Ok(())
            }
            Self::Emu { args, .. } => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                device::emu(serial, &args).await.map(|_| ())
            }
            Self::Clock {
                epoch_ms,
                uptime_ms,
            } => {
                let now_uptime = uptime_ms_now(serial).await?;
                let true_now = epoch_ms + now_uptime.saturating_sub(*uptime_ms);
                device::run_ok(serial, &format!("cmd alarm set-time {true_now}")).await?;
                let device_now = device_epoch_ms(serial).await?;
                if device_now.abs_diff(true_now) > 10_000 {
                    bail!("the clock reads {device_now} ms after being set to {true_now} ms");
                }
                Ok(())
            }
            Self::KillPids { dir } => {
                let dir = device::quote(dir);
                device::run_ok(
                    serial,
                    &format!(
                        "for f in {dir}/*.pid; do [ -f \"$f\" ] && kill $(cat \"$f\") 2>/dev/null; done; rm -rf {dir}"
                    ),
                )
                .await
                .map(|_| ())
            }
            Self::RemoveFile { path } => {
                device::run_ok(serial, &format!("rm -f {}", device::quote(path)))
                    .await
                    .map(|_| ())
            }
            Self::ProxyFault { id } => super::proxy::remove(serial, id).await,
        }
    }
}

/// Poll a read-only check for up to 5 s: many device changes settle
/// asynchronously (power state, connectivity).
pub async fn wait_for(serial: &str, verify: &Verify) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let ran = device::read(serial, &verify.command).await?;
        if ran.output.contains(&verify.contains) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "`{}` did not show `{}` (last output: {})",
                verify.command,
                verify.contains,
                ran.output.trim().chars().take(200).collect::<String>()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

pub async fn uptime_ms_now(serial: &str) -> Result<u64> {
    let text = device::read_ok(serial, "cat /proc/uptime").await?;
    let seconds: f64 = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| anyhow::anyhow!("unreadable /proc/uptime: {text}"))?;
    Ok((seconds * 1000.0) as u64)
}

pub async fn device_epoch_ms(serial: &str) -> Result<u64> {
    let text = device::read_ok(serial, "date +%s").await?;
    let seconds: u64 = text
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("unreadable device date: {text}"))?;
    Ok(seconds * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_round_trip_and_describe_themselves() {
        let steps = vec![
            RestoreStep::Setting {
                namespace: "global".into(),
                key: "private_dns_mode".into(),
                value: None,
            },
            RestoreStep::Clock {
                epoch_ms: 1,
                uptime_ms: 2,
            },
        ];
        let json = serde_json::to_string(&steps).unwrap();
        assert!(json.contains("\"step\":\"setting\""));
        let back: Vec<RestoreStep> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, steps);
        assert_eq!(
            steps[0].describe(),
            "delete global setting private_dns_mode (it was unset)"
        );
    }
}
