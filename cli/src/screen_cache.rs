//! Recently seen screens, stored by `screen_hash`, so `ui dump --since <hash>`
//! can answer with only what changed since the screen an agent last saw.
//!
//! The hash is derived from the screen's content, so one store serves every
//! device. Files are private (0600 in a 0700 directory) because they hold
//! on-screen text; only the newest [`KEEP`] survive.

use crate::proto::{Element, ScreenResponse};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

const KEEP: usize = 64;
/// Per-dump identifiers: they differ between two reads of the same screen.
const VOLATILE_FIELDS: [&str; 2] = ["id", "handle"];

fn dir() -> Result<PathBuf> {
    Ok(crate::hostenv::shadowdroid_home()?.join("screens"))
}

/// Only hex hashes become file names.
fn file_for(dir: &Path, hash: &str) -> Option<PathBuf> {
    (!hash.is_empty() && hash.len() <= 128 && hash.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| dir.join(format!("{hash}.json")))
}

/// Remember `screen` under its hash. Best effort: a cache problem never fails
/// the command that read the screen.
pub fn remember(screen: &ScreenResponse) {
    if let Err(error) = dir().and_then(|dir| remember_in(&dir, screen)) {
        tracing::debug!("screen cache: {error:#}");
    }
}

fn remember_in(dir: &Path, screen: &ScreenResponse) -> Result<()> {
    let Some(path) = file_for(dir, &screen.screen_hash) else {
        return Ok(());
    };
    if path.exists() {
        // Same content already stored; refresh its age so pruning keeps it.
        let _ = std::fs::File::options()
            .append(true)
            .open(&path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()));
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).context("create screen cache")?;
    // NamedTempFile is created 0600.
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, &screen.elements)?;
    file.flush()?;
    file.persist(&path).map_err(|error| error.error)?;
    prune(dir);
    Ok(())
}

fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    if files.len() <= KEEP {
        return;
    }
    files.sort();
    for (_, path) in &files[..files.len() - KEEP] {
        let _ = std::fs::remove_file(path);
    }
}

/// The elements of a remembered screen.
pub fn load(hash: &str) -> Option<Vec<Element>> {
    load_in(&dir().ok()?, hash)
}

fn load_in(dir: &Path, hash: &str) -> Option<Vec<Element>> {
    let bytes = std::fs::read(file_for(dir, hash)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A stable identity per element: its resource id, else description or text
/// with its class, else its class, numbered by occurrence in document order
/// so repeated list rows stay distinct.
fn element_keys(elements: &[Value]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    elements
        .iter()
        .map(|element| {
            let field = |name: &str| element[name].as_str().filter(|value| !value.is_empty());
            let class = field("klass").unwrap_or("");
            let base = if let Some(rid) = field("rid") {
                format!("rid:{rid}")
            } else if let Some(desc) = field("desc") {
                format!("desc:{class}:{desc}")
            } else if let Some(text) = field("text") {
                format!("text:{class}:{text}")
            } else {
                format!("class:{class}")
            };
            let occurrence = seen.entry(base.clone()).or_default();
            *occurrence += 1;
            if *occurrence == 1 {
                base
            } else {
                format!("{base}#{occurrence}")
            }
        })
        .collect()
}

fn comparable(element: &Value) -> BTreeMap<String, Value> {
    element
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| !VOLATILE_FIELDS.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// What changed from `before` to `after` (elements in the same output shape).
/// Added and changed elements are given in full as they are now, so an agent
/// can act on them; changed ones also carry the previous values of the fields
/// that differ.
pub fn diff(before: &[Value], after: &[Value]) -> Value {
    let before_keyed: HashMap<String, &Value> =
        element_keys(before).into_iter().zip(before).collect();
    let after_keys = element_keys(after);
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for (key, element) in after_keys.iter().zip(after) {
        match before_keyed.get(key) {
            None => added.push(element.clone()),
            Some(previous) => {
                let (was, now) = (comparable(previous), comparable(element));
                if was != now {
                    let fields: Vec<&String> = was
                        .keys()
                        .chain(now.keys())
                        .filter(|name| was.get(*name) != now.get(*name))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    let previous_values: BTreeMap<&String, Value> = fields
                        .iter()
                        .map(|name| (*name, was.get(*name).cloned().unwrap_or(Value::Null)))
                        .collect();
                    changed.push(json!({
                        "key": key,
                        "fields": fields,
                        "before": previous_values,
                        "element": element,
                    }));
                }
            }
        }
    }
    let after_set: std::collections::HashSet<&String> = after_keys.iter().collect();
    let removed: Vec<Value> = element_keys(before)
        .into_iter()
        .zip(before)
        .filter(|(key, _)| !after_set.contains(key))
        .map(|(key, element)| {
            let mut summary = serde_json::Map::new();
            summary.insert("key".into(), json!(key));
            for name in ["text", "desc", "rid", "klass"] {
                if let Some(value) = element.get(name).filter(|value| !value.is_null()) {
                    summary.insert(name.into(), value.clone());
                }
            }
            Value::Object(summary)
        })
        .collect();
    json!({
        "counts": {"added": added.len(), "removed": removed.len(), "changed": changed.len()},
        "added": added,
        "removed": removed,
        "changed": changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(hash: &str, texts: &[&str]) -> ScreenResponse {
        let elements: Vec<Value> = texts
            .iter()
            .enumerate()
            .map(|(id, text)| json!({"id": id, "text": text, "klass": "android.widget.TextView"}))
            .collect();
        serde_json::from_value(json!({
            "screen_hash": hash,
            "viewport": {"w": 1, "h": 1},
            "current_app": {},
            "element_count": elements.len(),
            "elements": elements,
        }))
        .unwrap()
    }

    #[test]
    fn screens_are_remembered_by_hash_and_pruned_to_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        remember_in(dir.path(), &screen("abc123", &["Hello"])).unwrap();
        let loaded = load_in(dir.path(), "abc123").unwrap();
        assert_eq!(loaded[0].text.as_deref(), Some("Hello"));
        assert!(load_in(dir.path(), "ffff").is_none());
        // Anything that isn't a hex hash never becomes a path.
        assert!(file_for(dir.path(), "../etc/passwd").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("abc123.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        for i in 0..KEEP + 5 {
            remember_in(dir.path(), &screen(&format!("{i:04x}"), &["x"])).unwrap();
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), KEEP);
    }

    #[test]
    fn diff_reports_added_removed_and_changed_elements() {
        let before = vec![
            json!({"id": 0, "rid": "title", "text": "Inbox", "tap": [10, 10]}),
            json!({"id": 1, "text": "Row", "klass": "T"}),
            json!({"id": 2, "text": "Row", "klass": "T"}),
            json!({"id": 3, "text": "Old banner", "klass": "T"}),
        ];
        let after = vec![
            json!({"id": 7, "handle": "h1", "rid": "title", "text": "Inbox (2)", "tap": [10, 10]}),
            json!({"id": 8, "text": "Row", "klass": "T"}),
            json!({"id": 9, "text": "Row", "klass": "T"}),
            json!({"id": 10, "text": "Row", "klass": "T"}),
        ];
        let diff = diff(&before, &after);
        assert_eq!(
            diff["counts"],
            json!({"added": 1, "removed": 1, "changed": 1})
        );
        // Ids and handles differ on every read and are not changes.
        assert_eq!(diff["changed"][0]["key"], "rid:title");
        assert_eq!(diff["changed"][0]["fields"], json!(["text"]));
        assert_eq!(diff["changed"][0]["before"], json!({"text": "Inbox"}));
        assert_eq!(diff["changed"][0]["element"]["handle"], "h1");
        // The third repeated row is new; the first two match by occurrence.
        assert_eq!(diff["added"][0]["id"], 10);
        assert_eq!(
            diff["removed"][0],
            json!({"key": "text:T:Old banner", "text": "Old banner", "klass": "T"})
        );

        let same = super::diff(&before, &before);
        assert_eq!(
            same["counts"],
            json!({"added": 0, "removed": 0, "changed": 0})
        );
    }
}
