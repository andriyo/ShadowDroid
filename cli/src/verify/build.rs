//! An observed build/install chain, with installed bytes checked independently.
use super::{Status, process, provenance};
use crate::{
    device::{adb, installer},
    ids::Serial,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub apk: PathBuf,
    pub package: String,
}
impl Build {
    pub fn validate(&self) -> Result<()> {
        crate::config::validate_android_package(&self.package)?;
        anyhow::ensure!(
            !self.argv.is_empty()
                && !self.argv[0].is_empty()
                && (1..=3_600_000).contains(&self.timeout_ms),
            "build needs argv and timeout_ms in 1..3600000"
        );
        anyhow::ensure!(
            !self.cwd.as_os_str().is_empty() && !self.apk.as_os_str().is_empty(),
            "build needs cwd and APK path"
        );
        Ok(())
    }
}
pub async fn connect(serial: &Serial, apk: Option<&Path>) -> Result<(Status, Value, bool, bool)> {
    let client = installer::ensure_ready(serial, apk, false).await?;
    let screen = client.screen().await?;
    Ok((
        Status::Passed,
        json!({"adapter":"connect","screen":screen,"application_requirements":"not_evaluated"}),
        false,
        false,
    ))
}
pub async fn run(
    build: &Build,
    root: &Path,
    serial: &Serial,
    out: &Path,
) -> Result<(Status, Value, bool, bool)> {
    let cwd = root.join(&build.cwd);
    let apk = cwd.join(&build.apk);
    let prior = std::fs::metadata(&apk).ok().and_then(|m| m.modified().ok());
    let started = SystemTime::now();
    let execution = process::run(&build.argv, &cwd, build.timeout_ms, out, Some(serial)).await?;
    if execution.timed_out || execution.interrupted {
        let interrupted = execution.interrupted;
        return Ok((
            Status::Blocked,
            json!({"adapter":"build_install","execution":execution,"reason":"build_outcome_unknown"}),
            true,
            interrupted,
        ));
    }
    if execution.exit_code != Some(0) {
        return Ok((
            Status::Failed,
            json!({"adapter":"build_install","execution":execution,"reason":"build_failed","installed":false}),
            false,
            false,
        ));
    }
    let meta = match std::fs::metadata(&apk) {
        Ok(m) => m,
        Err(e) => {
            return Ok((
                Status::Blocked,
                json!({"reason":"apk_missing","error":e.to_string(),"execution":execution}),
                false,
                false,
            ));
        }
    };
    anyhow::ensure!(
        meta.is_file() && meta.len() <= 256 * 1024 * 1024,
        "APK must be a regular file at most 256 MiB"
    );
    let modified = meta.modified()?;
    if modified < started || prior == Some(modified) {
        return Ok((
            Status::Stale,
            json!({"reason":"build_reused_old_apk","apk":apk,"execution":execution,"next_action":"run the project build with its appropriate rerun option"}),
            false,
            false,
        ));
    }
    let hash = provenance::hash(&std::fs::read(&apk)?);
    adb::install(serial, apk.clone()).await?;
    let installed = installed_apks(serial, &build.package).await?;
    let matched = installed.len() == 1 && installed[0]["blake3"] == hash;
    Ok((
        if matched {
            Status::Passed
        } else {
            Status::Blocked
        },
        json!({"adapter":"build_install","execution":execution,"apk":apk,"apk_hash":hash,"installed_apks":installed,"installed_bytes_match_build_output":matched,"provenance":"observed_build_install_chain_not_hermetic_build_attestation","supported_artifacts":"single_apk; split sets are observed but cannot pass this installer"}),
        false,
        false,
    ))
}
pub async fn installed_apks(serial: &Serial, package: &str) -> Result<Vec<Value>> {
    crate::config::validate_android_package(package)?;
    let paths = adb::shell(serial, format!("pm path {package}")).await?;
    let paths = paths
        .lines()
        .map(|p| {
            p.strip_prefix("package:")
                .context("unexpected package path response")
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        !paths.is_empty() && paths.len() <= 100,
        "missing or excessive installed APKs"
    );
    let mut result = vec![];
    let mut total=0u64;
    for path in paths {
        let quoted = crate::events::shell_token(path);
        let size = adb::shell(serial, format!("stat -c %s {quoted}"))
            .await?
            .trim()
            .parse::<u64>()
            .context("installed APK size unavailable")?;
        anyhow::ensure!(size <= 256 * 1024 * 1024, "installed APK exceeds 256 MiB");
        total+=size;
        anyhow::ensure!(total<=512*1024*1024,"installed APK set exceeds 512 MiB");
        let bytes = adb::shell_bytes(serial, format!("cat {quoted}")).await?;
        anyhow::ensure!(
            bytes.len() as u64 == size,
            "installed APK changed while reading"
        );
        result.push(json!({"path":path,"bytes":size,"blake3":provenance::hash(&bytes)}));
    }
    Ok(result)
}
