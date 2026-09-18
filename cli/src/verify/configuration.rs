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
            read(serial, &field).await? == value,
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
            self.change(serial, path, setting("font_scale"), Some(value.to_string()))
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
                Some(value.to_string()),
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
                if current == change.before { return Ok(()); }
                anyhow::ensure!(current == change.owned, "configuration ownership conflict for {:?}: current={current:?}, last_owned={:?}",change.field,change.owned);
                write(serial,&change.field,change.before.as_deref()).await?;
                anyhow::ensure!(read(serial,&change.field).await? == change.before,"configuration restore readback mismatch");
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
    adb::shell_mutating(serial, command).await?;
    Ok(())
}

pub async fn metadata(serial: &Serial) -> Result<serde_json::Value> {
    Ok(
        serde_json::json!({"font_scale":read(serial,&Field::Setting{namespace:"system".into(),key:"font_scale".into()}).await?,"night":read(serial,&Field::Night).await?,"size":adb::shell(serial,"wm size").await?,"density":adb::shell(serial,"wm density").await?,"rotation":read(serial,&Field::Setting{namespace:"system".into(),key:"user_rotation".into()}).await?,"accelerometer_rotation":read(serial,&Field::Setting{namespace:"system".into(),key:"accelerometer_rotation".into()}).await?}),
    )
}
