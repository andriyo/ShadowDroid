//! Write-ahead configuration ownership. Restore only values we still own.
use crate::{device::adb, ids::Serial};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub rotation: Option<u8>,
    pub font_scale: Option<f32>,
    pub night: Option<bool>,
    pub size: Option<[u32; 2]>,
    pub density: Option<u32>,
}

impl Configuration {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.rotation.is_none_or(|v| v <= 3),
            "rotation must be 0..3"
        );
        anyhow::ensure!(
            self.font_scale
                .is_none_or(|v| v.is_finite() && (0.5..=3.0).contains(&v)),
            "font_scale must be 0.5..3"
        );
        anyhow::ensure!(
            self.size
                .is_none_or(|v| v.iter().all(|n| (200..=4096).contains(n))),
            "size must be 200..4096 pixels per dimension"
        );
        anyhow::ensure!(
            self.density.is_none_or(|v| (72..=1000).contains(&v)),
            "density must be 72..1000"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Field {
    Setting { namespace: String, key: String },
    Wm(String),
    Night,
}
#[derive(Debug, Serialize, Deserialize)]
struct Change {
    field: Field,
    before: Option<String>,
    owned: Option<String>,
    restored: bool,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Journal {
    changes: Vec<Change>,
}

impl Journal {
    pub fn read(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
    fn save(&self, path: &Path) -> Result<()> {
        crate::cmd::artifact::write_json(path, &serde_json::to_value(self)?)?;
        Ok(())
    }
    async fn change(
        &mut self,
        serial: &Serial,
        path: &Path,
        field: Field,
        value: Option<String>,
    ) -> Result<()> {
        let before = read(serial, &field).await?;
        self.changes.push(Change {
            field: field.clone(),
            before,
            owned: value.clone(),
            restored: false,
        });
        self.save(path)?; // Intent is durable before the mutation, including its exact predecessor.
        write(serial, &field, value.as_deref()).await?;
        anyhow::ensure!(
            equivalent(
                &field,
                read(serial, &field).await?.as_deref(),
                value.as_deref()
            ),
            "configuration readback mismatch: {field:?}"
        );
        Ok(())
    }
    pub async fn apply(
        &mut self,
        serial: &Serial,
        path: &Path,
        config: &Configuration,
    ) -> Result<()> {
        config.validate()?;
        let setting = |key: &str| Field::Setting {
            namespace: "system".into(),
            key: key.into(),
        };
        if let Some(value) = config.font_scale {
            self.change(
                serial,
                path,
                setting("font_scale"),
                Some(format!("{value:?}")),
            )
            .await?;
        }
        if let Some(value) = config.rotation {
            self.change(
                serial,
                path,
                setting("accelerometer_rotation"),
                Some("0".into()),
            )
            .await?;
            self.change(
                serial,
                path,
                setting("user_rotation"),
                Some(format!("{value:?}")),
            )
            .await?;
        }
        if let Some([w, h]) = config.size {
            self.change(
                serial,
                path,
                Field::Wm("size".into()),
                Some(format!("{w}x{h}")),
            )
            .await?;
        }
        if let Some(value) = config.density {
            self.change(
                serial,
                path,
                Field::Wm("density".into()),
                Some(value.to_string()),
            )
            .await?;
        }
        if let Some(value) = config.night {
            // cmd uimode also writes ui_night_mode. Capture the raw presence separately,
            // restoring the mode first and then the original unset/explicit setting.
            let raw = Field::Setting {
                namespace: "secure".into(),
                key: "ui_night_mode".into(),
            };
            let before = read(serial, &raw).await?;
            self.changes.push(Change {
                field: raw,
                before,
                owned: Some(if value { "2" } else { "1" }.into()),
                restored: false,
            });
            self.save(path)?;
            self.change(
                serial,
                path,
                Field::Night,
                Some(if value { "yes" } else { "no" }.into()),
            )
            .await?;
        }
        Ok(())
    }
    pub async fn restore(&mut self, serial: &Serial, path: &Path) -> Vec<String> {
        let mut errors = vec![];
        for index in (0..self.changes.len()).rev() {
            if self.changes[index].restored {
                continue;
            }
            let change = &self.changes[index];
            let result = async {
                let current = read(serial,&change.field).await?;
                if equivalent(&change.field,current.as_deref(),change.before.as_deref()) {
                    if matches!(&change.field,Field::Setting{key,..} if key=="font_scale") {write(serial,&change.field,change.before.as_deref()).await?;}
                    return Ok(());
                }
                anyhow::ensure!(equivalent(&change.field,current.as_deref(),change.owned.as_deref()), "configuration ownership conflict for {:?}: current={current:?}, last_owned={:?}",change.field,change.owned);
                write(serial,&change.field,change.before.as_deref()).await?;
                anyhow::ensure!(equivalent(&change.field,read(serial,&change.field).await?.as_deref(),change.before.as_deref()),"configuration restore readback mismatch");
                Ok::<(),anyhow::Error>(())
            }.await;
            match result {
                Ok(()) => {
                    self.changes[index].restored = true;
                    if let Err(e) = self.save(path) {
                        errors.push(format!("{e:#}"));
                    }
                }
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        errors
    }
}

fn equivalent(field: &Field, a: Option<&str>, b: Option<&str>) -> bool {
    if a == b {
        return true;
    }
    if matches!(field,Field::Setting{key,..} if key=="font_scale")
        && let (Some(a), Some(b)) = (a, b)
        && let (Ok(a), Ok(b)) = (a.parse::<f32>(), b.parse::<f32>())
    {
        return a.is_finite() && b.is_finite() && a == b;
    }
    false
}

async fn read(serial: &Serial, field: &Field) -> Result<Option<String>> {
    Ok(match field {
        Field::Setting { namespace, key } => {
            validate_field(namespace, key)?;
            let output = adb::shell(serial, format!("settings list {namespace}")).await?;
            output
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{key}=")).map(str::to_owned))
        }
        Field::Wm(kind) => {
            anyhow::ensure!(
                matches!(kind.as_str(), "size" | "density"),
                "invalid wm field"
            );
            adb::shell(serial, format!("wm {kind}"))
                .await?
                .lines()
                .find_map(|line| {
                    line.strip_prefix(&format!("Override {kind}: "))
                        .map(str::to_owned)
                })
        }
        Field::Night => {
            let output = adb::shell(serial, "cmd uimode night").await?;
            let mode = output
                .trim()
                .strip_prefix("Night mode: ")
                .context("night mode query unsupported")?;
            anyhow::ensure!(
                matches!(mode, "yes" | "no" | "auto" | "custom"),
                "unsupported night mode: {mode}"
            );
            Some(mode.into())
        }
    })
}
fn validate_field(namespace: &str, key: &str) -> Result<()> {
    anyhow::ensure!(
        (namespace == "system"
            && matches!(
                key,
                "font_scale" | "accelerometer_rotation" | "user_rotation"
            ))
            || (namespace == "secure" && key == "ui_night_mode"),
        "unsupported configuration field"
    );
    Ok(())
}
async fn write(serial: &Serial, field: &Field, value: Option<&str>) -> Result<()> {
    let command = match field {
        Field::Setting { namespace, key } => {
            validate_field(namespace, key)?;
            match value {
                Some(v) => format!(
                    "settings put {namespace} {key} {}",
                    crate::events::shell_token(v)
                ),
                None => format!("settings delete {namespace} {key}"),
            }
        }
        Field::Wm(kind) => {
            anyhow::ensure!(
                matches!(kind.as_str(), "size" | "density"),
                "invalid wm field"
            );
            format!(
                "wm {kind} {}",
                crate::events::shell_token(value.unwrap_or("reset"))
            )
        }
        Field::Night => {
            let v = value.context("missing night mode")?;
            anyhow::ensure!(
                matches!(v, "yes" | "no" | "auto" | "custom"),
                "invalid night mode"
            );
            format!("cmd uimode night {v}")
        }
    };
    if matches!(field,Field::Setting{namespace,key} if namespace=="system" && key=="accelerometer_rotation")
        && value == Some("1")
    {
        adb::shell_mutating(serial, "wm user-rotation free").await?;
    } else {
        adb::shell_mutating(serial, command).await?;
    }
    if matches!(field,Field::Setting{key,..} if key=="user_rotation" || key=="accelerometer_rotation")
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut stable_since = None;
        loop {
            let mut matched = read(serial, field).await?.as_deref() == value;
            if matches!(field,Field::Setting{key,..} if key=="user_rotation")
                && adb::shell(serial, "settings get system accelerometer_rotation")
                    .await?
                    .trim()
                    == "0"
            {
                let dump = adb::shell(serial, "dumpsys input").await?;
                let actual = dump
                    .lines()
                    .find(|line| line.contains("displayId=0,") && line.contains("Viewport"))
                    .and_then(|line| {
                        line.split("orientation=")
                            .nth(1)?
                            .split(',')
                            .next()?
                            .parse::<u8>()
                            .ok()
                    });
                if let Some(expected) = value.and_then(|s| s.parse::<u8>().ok()) {
                    matched &= actual == Some(expected);
                }
            }
            if matched {
                let since = stable_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= std::time::Duration::from_millis(300) {
                    break;
                }
            } else {
                stable_since = None;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "rotation setting/effective display did not settle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    if matches!(field,Field::Setting{key,..} if key=="font_scale") {
        let expected = value
            .unwrap_or("1.0")
            .parse::<f32>()
            .context("invalid font-scale predecessor")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let dump = adb::shell(serial, "dumpsys activity processes").await?;
            let effective = dump.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("mGlobalConfiguration: {")?
                    .split_whitespace()
                    .next()?
                    .parse::<f32>()
                    .ok()
            });
            if effective == Some(expected) {
                if value.is_none() && read(serial, field).await?.as_deref() == Some("1.0") {
                    adb::shell_mutating(serial, "settings delete system font_scale").await?;
                }
                if equivalent(field, read(serial, field).await?.as_deref(), value) {
                    break;
                }
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "effective font scale did not converge to requested setting"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    Ok(())
}

pub async fn metadata(serial: &Serial) -> Result<serde_json::Value> {
    Ok(
        serde_json::json!({"font_scale":read(serial,&Field::Setting{namespace:"system".into(),key:"font_scale".into()}).await?,"night":read(serial,&Field::Night).await?,"size":adb::shell(serial,"wm size").await?,"density":adb::shell(serial,"wm density").await?,"rotation":read(serial,&Field::Setting{namespace:"system".into(),key:"user_rotation".into()}).await?,"accelerometer_rotation":read(serial,&Field::Setting{namespace:"system".into(),key:"accelerometer_rotation".into()}).await?}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn font_normalization_preserves_unset_and_different_values() {
        let field = Field::Setting {
            namespace: "system".into(),
            key: "font_scale".into(),
        };
        assert!(equivalent(&field, Some("1"), Some("1.0")));
        assert!(!equivalent(&field, None, Some("1.0")));
        assert!(!equivalent(&field, Some("1.3"), Some("1.4")));
    }
}
