//! Content identity includes dirty and untracked source, not just HEAD.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 30_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inputs {
    pub root: PathBuf,
    pub source_scope: String,
    pub git_head: Option<String>,
    pub files: BTreeMap<PathBuf, String>,
}

pub fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
pub fn json_hash(value: &impl Serialize) -> Result<String> {
    Ok(hash(&serde_json::to_vec(value)?))
}

pub fn snapshot(root: &Path, explicit: &[PathBuf]) -> Result<Inputs> {
    let root = root
        .canonicalize()
        .with_context(|| format!("source root {}", root.display()))?;
    let listing = Command::new("git")
        .args(["-C"])
        .arg(&root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output();
    let mut files = BTreeMap::new();
    let mut bytes_read = 0;
    let mut git_head = None;
    let source_scope = if let Ok(output) = listing
        && output.status.success()
    {
        for name in output.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let relative =
                Path::new(std::str::from_utf8(name).context("source filenames must be UTF-8")?);
            anyhow::ensure!(
                !relative.is_absolute()
                    && !relative
                        .components()
                        .any(|p| p == std::path::Component::ParentDir),
                "unsafe git source path"
            );
            add(&root.join(relative), &mut files, &mut bytes_read)?;
        }
        if let Ok(head) = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            && head.status.success()
        {
            git_head = Some(String::from_utf8_lossy(&head.stdout).trim().to_owned());
        }
        "git_tracked_and_untracked_plus_declared_inputs"
    } else {
        "declared_inputs_only"
    }
    .to_owned();
    for path in explicit {
        add(&root.join(path), &mut files, &mut bytes_read)?;
    }
    Ok(Inputs {
        root,
        source_scope,
        git_head,
        files,
    })
}

fn add(path: &Path, files: &mut BTreeMap<PathBuf, String>, bytes_read: &mut u64) -> Result<()> {
    anyhow::ensure!(
        path.components().count() <= 256,
        "source path nesting exceeds 256 components"
    );
    if files.contains_key(path) {
        return Ok(());
    }
    if files.len() >= MAX_FILES {
        bail!("source snapshot exceeds {MAX_FILES} files");
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            files.insert(path.to_owned(), "missing".into());
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    if metadata.is_symlink() {
        // A link's target bytes affect the program too. Explicitly reject links
        // rather than silently treating the link text as complete provenance.
        bail!(
            "source input {} is a symlink; declare/materialize its resolved input explicitly",
            path.display()
        );
    }
    if metadata.is_dir() {
        files.insert(path.to_owned(), "directory".into());
        let mut entries = std::fs::read_dir(path)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            add(&entry, files, bytes_read)?;
        }
        return Ok(());
    }
    anyhow::ensure!(
        metadata.is_file(),
        "source input is not a regular file: {}",
        path.display()
    );
    *bytes_read = bytes_read
        .checked_add(metadata.len())
        .context("input byte count overflow")?;
    anyhow::ensure!(
        *bytes_read <= MAX_INPUT_BYTES,
        "source snapshot exceeds 512 MiB; narrow the source root/declared inputs"
    );
    files.insert(path.to_owned(), hash(&std::fs::read(path)?));
    Ok(())
}

pub fn verify_artifact(root: &Path, relative: &Path, digest: &str) -> Result<()> {
    anyhow::ensure!(
        !relative.is_absolute()
            && !relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "invalid evidence path"
    );
    let root = root.canonicalize()?;
    let path = root.join(relative).canonicalize()?;
    anyhow::ensure!(
        path.starts_with(&root),
        "evidence escaped its run directory"
    );
    anyhow::ensure!(
        hash(&std::fs::read(path)?) == digest,
        "evidence hash mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dirty_and_untracked_sources_change_identity() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.path().join("source"), "a").unwrap();
        let first = snapshot(root.path(), &[]).unwrap();
        std::fs::write(root.path().join("source"), "b").unwrap();
        assert_ne!(first, snapshot(root.path(), &[]).unwrap());
        std::fs::write(root.path().join("new-source"), "c").unwrap();
        assert_eq!(snapshot(root.path(), &[]).unwrap().files.len(), 2);
    }
    #[test]
    fn missing_or_changed_evidence_is_not_accepted() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("check.json"), b"original").unwrap();
        let digest = hash(b"original");
        verify_artifact(root.path(), Path::new("check.json"), &digest).unwrap();
        std::fs::write(root.path().join("check.json"), b"changed").unwrap();
        assert!(verify_artifact(root.path(), Path::new("check.json"), &digest).is_err());
        assert!(verify_artifact(root.path(), Path::new("../check.json"), &digest).is_err());
    }
}
