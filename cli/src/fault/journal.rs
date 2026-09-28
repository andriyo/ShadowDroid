//! Host-side record of faults a device currently carries, written before the
//! device is touched so a crash of this CLI never loses the way back.
//!
//! `~/.shadowdroid/faults/<device>.json` holds the active faults (and ones
//! whose restore failed); `<device>.events.jsonl` is the append-only history
//! `watch` and `why` read. Both are private: they can name apps and hosts.

use super::restore::RestoreStep;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultState {
    Active,
    /// `fault clear` could not verify every restore step; clear it again.
    RestoreFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultRecord {
    pub id: String,
    pub kind: String,
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub params: Value,
    pub state: FaultState,
    pub injected_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    pub observed: Value,
    /// Executed in order by `fault clear`.
    pub restore: Vec<RestoreStep>,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restore_errors: Vec<Value>,
}

impl FaultRecord {
    /// The agent-facing view: everything but the internal helper pid.
    pub fn to_json(&self, now_ms: u64) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Value::Object(map) = &mut value {
            map.remove("expiry_pid");
            map.insert(
                "restore_plan".into(),
                json!(
                    self.restore
                        .iter()
                        .map(RestoreStep::describe)
                        .collect::<Vec<_>>()
                ),
            );
            map.remove("restore");
            if let Some(expires) = self.expires_at_ms {
                map.insert("overdue".into(), json!(now_ms > expires + 5_000));
            }
        }
        value
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JournalFile {
    schema_version: u32,
    faults: Vec<FaultRecord>,
}

pub struct Journal {
    path: PathBuf,
    events: PathBuf,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn dir() -> Result<PathBuf> {
    Ok(crate::hostenv::shadowdroid_home()?.join("faults"))
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))
}

impl Journal {
    pub fn for_device(serial: &str) -> Result<Self> {
        Ok(Self::in_dir(&dir()?, serial))
    }

    fn in_dir(dir: &Path, serial: &str) -> Self {
        let component = crate::ids::stable_file_component(serial);
        Self {
            path: dir.join(format!("{component}.json")),
            events: dir.join(format!("{component}.events.jsonl")),
        }
    }

    pub fn load(&self) -> Result<Vec<FaultRecord>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let file: JournalFile = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse fault journal {}", self.path.display()))?;
                Ok(file.faults)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => {
                Err(error).with_context(|| format!("read fault journal {}", self.path.display()))
            }
        }
    }

    pub fn save(&self, faults: &[FaultRecord]) -> Result<()> {
        let parent = self.path.parent().context("fault journal has no parent")?;
        if faults.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        ensure_private_dir(parent)?;
        // NamedTempFile is created 0600, and persist() replaces atomically.
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(
            &mut file,
            &JournalFile {
                schema_version: 1,
                faults: faults.to_vec(),
            },
        )?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }

    /// Append one history event. Best effort: history never fails a fault.
    pub fn append_event(&self, event: Value) {
        let result = (|| -> Result<()> {
            ensure_private_dir(self.events.parent().context("no parent")?)?;
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&self.events)?;
            let mut line = serde_json::to_vec(&event)?;
            line.push(b'\n');
            file.write_all(&line)?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::debug!("fault event log: {error:#}");
        }
    }

    /// History events at or after `since_ms`, oldest first, at most `limit`.
    pub fn events_since(&self, since_ms: u64, limit: usize) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(&self.events) else {
            return Vec::new();
        };
        let mut events: Vec<Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| event["ts_ms"].as_u64().unwrap_or(0) >= since_ms)
            .collect();
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        events
    }

    /// Size of the history file, for tailing it.
    pub fn events_len(&self) -> u64 {
        std::fs::metadata(&self.events)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// History events appended after byte `offset`, with the new offset.
    pub fn events_after(&self, offset: u64) -> (Vec<Value>, u64) {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(&self.events) else {
            return (Vec::new(), offset);
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        // The log was rotated or replaced: start over.
        let start = if len < offset { 0 } else { offset };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return (Vec::new(), offset);
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return (Vec::new(), offset);
        }
        // Only complete lines; a line still being written is read next time.
        let complete = text.rfind('\n').map(|end| end + 1).unwrap_or(0);
        let events = text[..complete]
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect();
        (events, start + complete as u64)
    }
}

/// A history event with the fields every fault event carries.
pub fn event(name: &str, record: &FaultRecord) -> Value {
    json!({
        "type": "fault",
        "event": name,
        "id": record.id,
        "kind": record.kind,
        "app": record.app,
        "device": record.device,
        "params": record.params,
        "ts_ms": now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> FaultRecord {
        FaultRecord {
            id: id.into(),
            kind: "airplane-mode".into(),
            device: "emulator-5554".into(),
            app: None,
            params: json!({}),
            state: FaultState::Active,
            injected_at_ms: 1,
            expires_at_ms: Some(10),
            observed: json!({}),
            restore: vec![RestoreStep::Setting {
                namespace: "global".into(),
                key: "airplane_mode_on".into(),
                value: Some("0".into()),
            }],
            resources: vec!["connectivity".into()],
            expiry_pid: Some(42),
            restore_errors: vec![],
        }
    }

    #[test]
    fn journal_round_trips_privately_and_disappears_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::in_dir(&dir.path().join("faults"), "emulator-5554");
        assert!(journal.load().unwrap().is_empty());
        journal.save(&[record("flt_1")]).unwrap();
        assert_eq!(journal.load().unwrap()[0].id, "flt_1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&journal.path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        journal.save(&[]).unwrap();
        assert!(!journal.path.exists());
    }

    #[test]
    fn events_are_tailed_by_offset() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::in_dir(dir.path(), "emulator-5554");
        journal.append_event(event("injected", &record("flt_1")));
        let (first, offset) = journal.events_after(0);
        assert_eq!(first.len(), 1);
        journal.append_event(event("cleared", &record("flt_1")));
        let (second, _) = journal.events_after(offset);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["event"], "cleared");
        assert_eq!(journal.events_since(0, 1).len(), 1);
    }

    #[test]
    fn agent_view_hides_the_helper_pid_and_describes_the_restore() {
        let view = record("flt_1").to_json(100_000);
        assert!(view.get("expiry_pid").is_none());
        assert!(view.get("restore").is_none());
        assert_eq!(view["overdue"], true);
        assert!(
            view["restore_plan"][0]
                .as_str()
                .unwrap()
                .contains("airplane_mode_on")
        );
    }
}
