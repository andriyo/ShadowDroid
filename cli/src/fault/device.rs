//! Shell, settings and emulator-console primitives the fault kinds share.
//! Every command reports its exit status: a device that ignores a command
//! must never look like one that obeyed it.

use crate::device::adb;
use anyhow::{Context, Result, anyhow, bail};

const RC_MARKER: &str = "__SD_RC=";

/// Output and exit status of one device shell command (stderr folded in).
#[derive(Debug)]
pub struct Ran {
    pub status: i32,
    pub output: String,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

fn wrap(command: &str) -> String {
    // A newline, not `;`, ends the group: `cmd & ;` would be a syntax error.
    format!("{{ {command}\n}} 2>&1; echo {RC_MARKER}$?")
}

fn unwrap_ran(raw: &str) -> Result<Ran> {
    let at = raw
        .rfind(RC_MARKER)
        .ok_or_else(|| anyhow!("device shell ended without reporting a status"))?;
    let status = raw[at + RC_MARKER.len()..]
        .trim()
        .parse()
        .context("parse device shell status")?;
    Ok(Ran {
        status,
        output: raw[..at].trim_end().to_string(),
    })
}

/// A command that changes the device.
pub async fn run(serial: &str, command: &str) -> Result<Ran> {
    unwrap_ran(&adb::shell_mutating(serial, wrap(command)).await?)
}

/// A command that only reads.
pub async fn read(serial: &str, command: &str) -> Result<Ran> {
    unwrap_ran(&adb::shell(serial, wrap(command)).await?)
}

/// `run`, failing with the device's own words when the command fails.
pub async fn run_ok(serial: &str, command: &str) -> Result<String> {
    let ran = run(serial, command).await?;
    if !ran.ok() {
        bail!(
            "device command failed ({}): {command}: {}",
            ran.status,
            ran.output.trim()
        );
    }
    Ok(ran.output)
}

pub async fn read_ok(serial: &str, command: &str) -> Result<String> {
    let ran = read(serial, command).await?;
    if !ran.ok() {
        bail!(
            "device command failed ({}): {command}: {}",
            ran.status,
            ran.output.trim()
        );
    }
    Ok(ran.output)
}

pub fn quote(value: &str) -> String {
    crate::config::quote_device_shell_arg(value)
}

/// A setting's value; `None` when it is unset.
pub async fn get_setting(serial: &str, namespace: &str, key: &str) -> Result<Option<String>> {
    let value = read_ok(serial, &format!("settings get {namespace} {}", quote(key))).await?;
    let value = value.trim();
    Ok((value != "null").then(|| value.to_string()))
}

/// Set (or with `None`, delete) a setting and verify it reads back.
pub async fn put_setting(
    serial: &str,
    namespace: &str,
    key: &str,
    value: Option<&str>,
) -> Result<()> {
    match value {
        Some(value) => {
            run_ok(
                serial,
                &format!("settings put {namespace} {} {}", quote(key), quote(value)),
            )
            .await?;
        }
        None => {
            run_ok(
                serial,
                &format!("settings delete {namespace} {}", quote(key)),
            )
            .await?;
        }
    }
    let now = get_setting(serial, namespace, key).await?;
    if now.as_deref() != value {
        bail!(
            "setting {namespace}/{key} reads {:?} after writing {:?}",
            now,
            value
        );
    }
    Ok(())
}

pub async fn sdk(serial: &str) -> Result<u32> {
    read_ok(serial, "getprop ro.build.version.sdk")
        .await?
        .trim()
        .parse()
        .context("read the device API level")
}

pub async fn is_emulator(serial: &str) -> bool {
    crate::device::target::avd_name(serial).await.is_some()
}

pub async fn pid_of(serial: &str, package: &str) -> Result<Option<u32>> {
    let ran = read(serial, &format!("pidof {}", quote(package))).await?;
    Ok(ran
        .output
        .split_whitespace()
        .next()
        .and_then(|pid| pid.parse().ok()))
}

/// Run an emulator console command (`adb emu …`) and return its reply
/// without the trailing `OK`. A `KO:` reply is an error.
pub async fn emu(serial: &str, args: &[&str]) -> Result<String> {
    let adb = crate::device::target::adb_program();
    let mut command = tokio::process::Command::new(&adb);
    command.arg("-s").arg(serial).arg("emu").args(args);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
        .await
        .map_err(|_| {
            anyhow!(
                "emulator console did not answer `{}` within 20s",
                args.join(" ")
            )
        })?
        .with_context(|| format!("run {} emu", adb.display()))?;
    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    if let Some(line) = text.lines().find(|line| line.starts_with("KO")) {
        bail!("emulator console refused `{}`: {line}", args.join(" "));
    }
    if !output.status.success() {
        bail!(
            "emulator console `{}` failed: {}",
            args.join(" "),
            text.trim()
        );
    }
    Ok(text
        .lines()
        .filter(|line| line.trim() != "OK")
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_status_is_read_from_the_marker() {
        let ran = unwrap_ran("line one\nerror: nope\n__SD_RC=3\n").unwrap();
        assert_eq!(ran.status, 3);
        assert_eq!(ran.output, "line one\nerror: nope");
        assert!(unwrap_ran("no marker").is_err());
        assert_eq!(
            wrap("pm revoke a b"),
            "{ pm revoke a b\n} 2>&1; echo __SD_RC=$?"
        );
    }
}
