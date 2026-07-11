//! `shadowdroid skill <agent> [--out PATH | --install [--scope user|project]]`
//! — generate an Agent Skills `SKILL.md` so a coding agent knows how to drive
//! Android with ShadowDroid. Agents: claude-code, cursor, codex, gemini, and
//! antigravity.
//!
//! One curated body (driving guidance, in the current grammar) is wrapped in
//! standard frontmatter and the right discovery location per agent/scope, with
//! an auto-generated command
//! reference appended from the live introspection catalog ([crate::cmd::introspect])
//! so it never drifts from the actual CLI. Prints to stdout by default;
//! `--out` writes a chosen path; `--install` writes the agent's conventional
//! location.
//!
//! Every generated file ends with a `<!-- shadowdroid-skill … -->` marker that
//! stamps the CLI version and a hash of the body. That lets `skill --sync` (and
//! a best-effort pass on `connect`) refresh installed skills after a CLI
//! upgrade — rewriting unmodified ones in place while leaving hand-edited ones
//! alone.

use anyhow::{anyhow, Context, Result};
use clap::CommandFactory;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::Cli;
use crate::hostenv::home_dir;

#[derive(clap::Args)]
pub struct SkillArgs {
    /// Target agent system. Omit together with `--sync` to refresh every
    /// already-installed skill instead.
    #[arg(
        value_parser = ["claude-code", "cursor", "codex", "gemini", "antigravity"],
        conflicts_with = "sync"
    )]
    pub agent: Option<String>,
    /// Write the generated SKILL.md to this path (default: print to stdout).
    #[arg(short = 'o', long, conflicts_with_all = ["install", "sync"])]
    pub out: Option<PathBuf>,
    /// Install to the agent's conventional skill directory instead of stdout.
    #[arg(long, conflicts_with = "sync")]
    pub install: bool,
    /// Installation/sync scope. User is global; project is relative to the CWD.
    #[arg(long, value_enum, value_name = "SCOPE", default_value = "user")]
    pub scope: SkillScope,
    /// Refresh every already-installed ShadowDroid skill to this CLI's version.
    /// Unmodified skills are rewritten in place; skills you've hand-edited are
    /// left alone and reported — unless you also pass --force.
    #[arg(long)]
    pub sync: bool,
    /// Allow overwriting a customized or markerless destination. Without this,
    /// every install/sync preserves user-authored content.
    #[arg(long)]
    pub force: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum SkillScope {
    #[default]
    User,
    Project,
}

impl SkillScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

const DESCRIPTION: &str = "Develop and debug Android apps with predictable structured JSON via the \
`shadowdroid` CLI: deploy and control apps/devices, inspect and automate UI, diagnose logs/crashes, \
debug and inspect layouts, capture/intercept OkHttp traffic, and manage permissions/files. Use for \
Android development, testing, reproduction, and debugging; Gradle or the Android CLI remains the \
underlying build engine.";

pub fn run(args: &SkillArgs) -> Result<()> {
    if args.sync {
        return sync_skills(args.scope, args.force);
    }
    let agent = args.agent.as_deref().ok_or_else(|| {
        anyhow!("specify an agent (claude-code|cursor|codex|gemini|antigravity), or pass --sync to refresh installed skills")
    })?;

    let path = if let Some(out) = &args.out {
        Some(out.clone())
    } else if args.install {
        Some(conventional_path(agent, args.scope)?)
    } else {
        None
    };

    let content = generated_content(agent)?;

    match path {
        Some(p) => {
            write_skill_checked(agent, &p, &content, args.force)?;
            let absolute_path = absolute_path(&p)?;
            let mut payload = json!({
                    "agent": agent, "path": p.display().to_string(),
                    "absolute_path": absolute_path.display().to_string(),
                    "scope": if args.install { Some(args.scope.as_str()) } else { None },
                    "bytes": content.len(),
            });
            if let Some(note) = install_note(agent, args.scope, args.install) {
                payload["note"] = Value::String(note.to_string());
            }
            crate::events::emit_action("skill", &payload);
        }
        None => print!("{content}"),
    }
    Ok(())
}

/// Install the conventional user-scoped ShadowDroid skills that are safe to
/// manage automatically. Project skills stay explicit because `init` may run
/// from an arbitrary directory.
pub fn install_default_skills() -> Value {
    let mut installed = Vec::new();
    let skipped: Vec<Value> = Vec::new();
    let mut failed = Vec::new();

    for agent in ["claude-code", "cursor", "codex", "gemini", "antigravity"] {
        match conventional_path(agent, SkillScope::User)
            .and_then(|path| install_skill_at(agent, &path).map(|bytes| (path, bytes)))
        {
            Ok((path, bytes)) => installed.push(json!({
                "agent": agent,
                "path": path.display().to_string(),
                "bytes": bytes,
            })),
            Err(err) => failed.push(json!({"agent": agent, "error": err.to_string()})),
        }
    }

    json!({
        "type": "action",
        "ok": failed.is_empty(),
        "cmd": "skill_install_defaults",
        "installed": installed,
        "skipped": skipped,
        "failed": failed,
    })
}

fn install_skill_at(agent: &str, path: &Path) -> Result<usize> {
    let content = generated_content(agent)?;
    let bytes = content.len();
    write_skill_checked(agent, path, &content, false)?;
    Ok(bytes)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("resolve current working directory")?
        .join(path))
}

fn install_note(agent: &str, scope: SkillScope, install: bool) -> Option<&'static str> {
    if !install {
        return Some(
            "This is an Agent Skills SKILL.md; place it in a supported <skills>/<name>/SKILL.md directory.",
        );
    }
    match (agent, scope) {
        ("claude-code", SkillScope::User) => Some(
            "Claude Code personal skill installed. A newly created top-level skills directory may require restarting Claude Code.",
        ),
        ("cursor", SkillScope::User) => Some(
            "Cursor personal skill installed. Restart Cursor if it does not appear in the skills list.",
        ),
        ("codex", SkillScope::User) => Some(
            "Codex user skill installed. Codex detects changes automatically; restart if it does not appear in /skills.",
        ),
        ("gemini", SkillScope::User) => Some(
            "Gemini CLI user skill installed. Use `/skills reload` or restart Gemini CLI.",
        ),
        ("antigravity", SkillScope::User) => Some(
            "Antigravity global skill installed. Use `/skills` to verify discovery.",
        ),
        ("claude-code", SkillScope::Project) => Some(
            "Claude Code project skill installed relative to the current directory.",
        ),
        (_, SkillScope::Project) => Some(
            "Shared project skill installed under .agents/skills; Codex, Cursor, Gemini CLI, and Antigravity can discover it.",
        ),
        _ => None,
    }
}

/// The agent's conventional skill location (relative to $HOME or the CWD).
fn conventional_path(agent: &str, scope: SkillScope) -> Result<PathBuf> {
    if scope == SkillScope::Project {
        return Ok(match agent {
            "claude-code" => PathBuf::from(".claude/skills/shadowdroid/SKILL.md"),
            "cursor" | "codex" | "gemini" | "antigravity" => {
                PathBuf::from(".agents/skills/shadowdroid/SKILL.md")
            }
            other => return Err(anyhow!("unknown agent '{other}'")),
        });
    }

    let home = home_dir()?;
    Ok(match agent {
        "claude-code" => home.join(".claude/skills/shadowdroid/SKILL.md"),
        "cursor" => home.join(".cursor/skills/shadowdroid/SKILL.md"),
        "codex" => home.join(".agents/skills/shadowdroid/SKILL.md"),
        "gemini" => home.join(".gemini/skills/shadowdroid/SKILL.md"),
        "antigravity" => home.join(".gemini/config/skills/shadowdroid/SKILL.md"),
        other => return Err(anyhow!("unknown agent '{other}'")),
    })
}

#[cfg(test)]
fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn wrap_for_agent(agent: &str, body: &str) -> Result<String> {
    match agent {
        "claude-code" | "cursor" | "codex" | "gemini" | "antigravity" => Ok(wrap_skill_md(body)),
        other => Err(anyhow!(
            "unknown agent '{other}' (claude-code|cursor|codex|gemini|antigravity)"
        )),
    }
}

fn wrap_skill_md(body: &str) -> String {
    format!(
        "---\nname: shadowdroid\ndescription: {desc}\n---\n\n# ShadowDroid\n\n{body}\n",
        desc = DESCRIPTION,
    )
}

// ── generation + version marker ───────────────────────────────────────────
//
// Every written skill ends with one marker line stamping the CLI version and a
// hash of the body above it. The hash makes "has the user edited this?" a
// version-independent question: recompute it and compare. That powers the
// pristine-only refresh in `--sync` and on `connect`.

const MARKER_PREFIX: &str = "<!-- shadowdroid-skill ";

/// Build the full file: curated body + standard wrapper + version marker.
fn generated_content(agent: &str) -> Result<String> {
    let body = format!("{}\n\n{}", SKILL_BODY.trim(), command_reference());
    let core = wrap_for_agent(agent, &body)?;
    Ok(append_marker(&core))
}

fn append_marker(core: &str) -> String {
    format!(
        "{core}{MARKER_PREFIX}v={v} h={h} · auto-generated by `shadowdroid skill`; run `shadowdroid skill --sync` to update. Hand-edits are detected and preserved. -->\n",
        v = env!("CARGO_PKG_VERSION"),
        h = body_hash(core),
    )
}

/// The body (everything before the marker line), unchanged if there is none.
fn strip_marker(content: &str) -> &str {
    match content.rfind(MARKER_PREFIX) {
        Some(i) => &content[..i],
        None => content,
    }
}

/// Returns `(body, version, body_hash)` from a marked file, or `None` if the
/// marker (or either field) is absent.
fn split_marker(content: &str) -> Option<(&str, String, String)> {
    let idx = content.rfind(MARKER_PREFIX)?;
    let body = &content[..idx];
    let mut version = None;
    let mut hash = None;
    for tok in content[idx..].split_whitespace() {
        if let Some(v) = tok.strip_prefix("v=") {
            version = Some(v.to_string());
        } else if let Some(h) = tok.strip_prefix("h=") {
            hash = Some(h.to_string());
        }
    }
    Some((body, version?, hash?))
}

fn body_hash(body: &str) -> String {
    let digest = blake3::hash(body.as_bytes());
    digest.as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn write_skill(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let existing_permissions = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary skill beside {}", path.display()))?;
    temp.write_all(content.as_bytes())
        .with_context(|| format!("write temporary skill for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("flush temporary skill for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync temporary skill for {}", path.display()))?;
    if let Some(permissions) = existing_permissions {
        temp.as_file()
            .set_permissions(permissions)
            .with_context(|| format!("preserve permissions for {}", path.display()))?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))
                .with_context(|| format!("set permissions for {}", path.display()))?;
        }
    }
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace skill {}", path.display()))?;
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Shared safety gate for explicit installs and first-run/default installs.
/// Marker-aware sync already had this behavior; using it for every writer keeps
/// `init` and `skill --install` from erasing unrelated project/global guidance.
fn write_skill_checked(agent: &str, path: &Path, content: &str, force: bool) -> Result<()> {
    if !path.exists() {
        return write_skill(path, content);
    }
    let (decision, _) = inspect(agent, path)?;
    match decision {
        Decision::UpToDate => Ok(()),
        Decision::NormalizeMarker | Decision::StalePristine(_) => write_skill(path, content),
        Decision::Customized | Decision::Untracked if force => write_skill(path, content),
        Decision::Customized => Err(crate::diagnostic::DiagnosticError::new(
            "skill_customized",
            "skill",
            format!("refusing to overwrite customized skill {}", path.display()),
        )
        .detail(serde_json::json!({"path": path.display().to_string(), "agent": agent}))
        .next_actions(["rerun with --force only if replacing the customization is intended"])
        .into()),
        Decision::Untracked => Err(crate::diagnostic::DiagnosticError::new(
            "skill_destination_untracked",
            "skill",
            format!("refusing to overwrite untracked file {}", path.display()),
        )
        .detail(serde_json::json!({"path": path.display().to_string(), "agent": agent}))
        .next_actions(["choose --out <new-path>, or rerun with --force after reviewing the file"])
        .into()),
    }
}

// ── detect + refresh installed skills ─────────────────────────────────────

/// Auto-refreshable skill locations for one scope. Project targets are
/// de-duplicated because four agents share `.agents/skills`.
fn skill_targets(scope: SkillScope) -> Vec<(&'static str, PathBuf)> {
    let mut seen = std::collections::HashSet::new();
    ["claude-code", "codex", "cursor", "gemini", "antigravity"]
        .into_iter()
        .filter_map(|agent| {
            let path = conventional_path(agent, scope).ok()?;
            seen.insert(path.clone()).then_some((agent, path))
        })
        .collect()
}

enum Decision {
    /// Already byte-identical to current output.
    UpToDate,
    /// Body matches current output; only the marker is absent or stale-stamped.
    NormalizeMarker,
    /// Unmodified since ShadowDroid wrote it, but an older version.
    StalePristine(Option<String>),
    /// Hand-edited (marker hash no longer matches the body).
    Customized,
    /// No marker — can't prove it's unmodified.
    Untracked,
}

fn inspect(agent: &str, path: &Path) -> Result<(Decision, String)> {
    let installed =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let expected = generated_content(agent)?;
    if installed == expected {
        return Ok((Decision::UpToDate, expected));
    }
    if strip_marker(&installed) == strip_marker(&expected) {
        return Ok((Decision::NormalizeMarker, expected));
    }
    let decision = match split_marker(&installed) {
        Some((body, ver, stored)) if body_hash(body) == stored => {
            Decision::StalePristine(Some(ver))
        }
        Some(_) => Decision::Customized,
        None => Decision::Untracked,
    };
    Ok((decision, expected))
}

/// `skill --sync`: refresh installed skills to this CLI's version.
fn sync_skills(scope: SkillScope, force: bool) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let mut refreshed = Vec::new();
    let mut up_to_date = Vec::new();
    let mut skipped_customized = Vec::new();
    let mut skipped_untracked = Vec::new();

    for (agent, path) in skill_targets(scope) {
        if !path.exists() {
            continue;
        }
        let (decision, expected) = inspect(agent, &path)?;
        let here = || json!({"agent": agent, "path": path.display().to_string()});
        match decision {
            Decision::UpToDate => up_to_date.push(here()),
            Decision::NormalizeMarker => {
                write_skill(&path, &expected)?;
                up_to_date.push(here());
            }
            Decision::StalePristine(from) => {
                write_skill(&path, &expected)?;
                refreshed
                    .push(json!({"agent": agent, "path": path.display().to_string(), "from": from, "to": version}));
            }
            Decision::Customized => {
                if force {
                    write_skill(&path, &expected)?;
                    refreshed.push(json!({"agent": agent, "path": path.display().to_string(), "to": version, "was": "customized"}));
                } else {
                    skipped_customized.push(here());
                }
            }
            Decision::Untracked => {
                if force {
                    write_skill(&path, &expected)?;
                    refreshed.push(json!({"agent": agent, "path": path.display().to_string(), "to": version, "was": "untracked"}));
                } else {
                    skipped_untracked.push(here());
                }
            }
        }
    }

    let mut payload = json!({
        "version": version,
        "scope": scope.as_str(),
        "refreshed": refreshed, "up_to_date": up_to_date,
    });
    if !skipped_customized.is_empty() {
        payload["skipped_customized"] = json!(skipped_customized);
    }
    if !skipped_untracked.is_empty() {
        payload["skipped_untracked"] = json!(skipped_untracked);
    }
    if !skipped_customized.is_empty() || !skipped_untracked.is_empty() {
        payload["hint"] =
            Value::String("re-run with --force to overwrite customized/untracked skills".into());
    }
    crate::events::emit_action("skill_sync", &payload);
    Ok(())
}

/// Best-effort skill refresh during `connect`: silently rewrites pristine, stale
/// skills to this CLI's version and reports anything that needs a manual
/// `skill sync`. Returns `None` when there's nothing to say. Never errors — a
/// skill problem must not fail connect.
pub fn refresh_for_connect() -> Option<Value> {
    let version = env!("CARGO_PKG_VERSION");
    let mut refreshed = Vec::new();
    let mut need_sync: Vec<&str> = Vec::new();

    for (agent, path) in skill_targets(SkillScope::User) {
        if !path.exists() {
            continue;
        }
        let Ok((decision, expected)) = inspect(agent, &path) else {
            continue;
        };
        match decision {
            Decision::UpToDate => {}
            Decision::NormalizeMarker => {
                let _ = write_skill(&path, &expected);
            }
            Decision::StalePristine(from) => {
                if write_skill(&path, &expected).is_ok() {
                    refreshed.push(json!({"agent": agent, "from": from, "to": version}));
                }
            }
            Decision::Customized | Decision::Untracked => {
                if !need_sync.contains(&agent) {
                    need_sync.push(agent);
                }
            }
        }
    }

    if refreshed.is_empty() && need_sync.is_empty() {
        return None;
    }
    let mut o = json!({});
    if !refreshed.is_empty() {
        o["refreshed"] = json!(refreshed);
    }
    if !need_sync.is_empty() {
        o["need_sync"] = json!(need_sync);
        o["hint"] = Value::String("run `shadowdroid skill --sync --force` to update".into());
    }
    Some(o)
}

/// Render the live command catalog as a grouped markdown reference.
///
/// Only the verbs an agent reaches for in the observe→act loop are expanded in
/// full. The advanced long tail (`debug`, `layout`, `appops`, …) is named
/// with a pointer to `commands --json`, so the skill stays lean in context
/// without losing discoverability. Still generated from the live catalog, so it
/// never drifts from the actual CLI.
fn command_reference() -> String {
    // The core driving surface, expanded with subcommands. Anything else is
    // listed by name only. Matched against the live catalog, so a renamed verb
    // falls through to the pointer line rather than silently vanishing.
    const CORE: &[&str] = &[
        "devices",
        "connect",
        "disconnect",
        "doctor",
        "collect",
        "log",
        "why",
        "config",
        "ui",
        "watch",
        "app",
        "perm",
        "device",
        "net",
    ];

    let root = Cli::command();
    let catalog = crate::cmd::introspect::catalog(&root);
    let mut core = String::new();
    let mut tail: Vec<String> = Vec::new();

    if let Some(cmds) = catalog["commands"].as_array() {
        for c in cmds {
            let name = c["name"].as_str().unwrap_or("");
            if !CORE.contains(&name) {
                tail.push(name.to_string());
                continue;
            }
            let about = c["about"].as_str().unwrap_or("");
            core.push_str(&format!("- **`{name}`** — {about}\n"));
            if let Some(subs) = c["subcommands"].as_array() {
                for s in subs {
                    let sn = s["name"].as_str().unwrap_or("");
                    let sa = s["about"].as_str().unwrap_or("");
                    core.push_str(&format!("  - `{name} {sn}` — {sa}\n"));
                }
            }
        }
    }

    let mut out = String::from(
        "## Command reference\n\nCore commands below — run \
         `shadowdroid commands --json` for the full catalog (every command, \
         subcommand, flag, and agent decision hint).\n\n",
    );
    out.push_str(&core);
    if !tail.is_empty() {
        out.push_str(&format!(
            "\nOther commands (`shadowdroid commands --json` for details): {}.\n",
            tail.join(", ")
        ));
    }
    out
}

/// The curated skill body (driving guidance). Kept as markdown beside this
/// file so it edits and diffs as prose; trimmed and joined with the live
/// command reference at generation time.
const SKILL_BODY: &str = include_str!("skill_body.md");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_uses_standard_skill_md_format() {
        for agent in ["claude-code", "cursor", "codex", "gemini", "antigravity"] {
            let content = wrap_for_agent(agent, "body").unwrap();
            assert!(
                content.starts_with("---\nname: shadowdroid\ndescription: "),
                "{agent}: {content}"
            );
            assert!(content.contains("# ShadowDroid"), "{agent}: {content}");
            assert!(!content.contains("alwaysApply"), "{agent}: {content}");
        }
    }

    #[test]
    fn command_reference_expands_core_and_points_to_catalog_for_tail() {
        let r = command_reference();
        // Core driving verbs are expanded with detail…
        assert!(r.contains("- **`ui`**"), "{r}");
        assert!(r.contains("  - `ui dump`"), "{r}");
        assert!(r.contains("  - `ui tap`"), "{r}");
        // …and a previously blank gesture now carries help text.
        assert!(r.contains("  - `ui swipe` — Swipe"), "{r}");
        assert!(r.contains("- **`watch`**"), "{r}");
        // The advanced long tail is not expanded inline (no subcommands)…
        assert!(!r.contains("debug variables"), "{r}");
        // …but stays discoverable via the pointer line.
        assert!(r.contains("commands --json"));
        assert!(r.contains("debug"));
    }

    #[test]
    fn user_paths_follow_current_agent_conventions() {
        // Tolerant of an unset $HOME (e.g. minimal CI), since the path is built
        // from it.
        for (agent, suffix) in [
            ("claude-code", ".claude/skills/shadowdroid/SKILL.md"),
            ("cursor", ".cursor/skills/shadowdroid/SKILL.md"),
            ("codex", ".agents/skills/shadowdroid/SKILL.md"),
            ("gemini", ".gemini/skills/shadowdroid/SKILL.md"),
            ("antigravity", ".gemini/config/skills/shadowdroid/SKILL.md"),
        ] {
            if let Ok(path) = conventional_path(agent, SkillScope::User) {
                assert!(
                    normalized_path(&path).ends_with(suffix),
                    "{agent}: {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn project_paths_use_native_claude_and_shared_agent_skills() {
        assert_eq!(
            conventional_path("claude-code", SkillScope::Project).unwrap(),
            PathBuf::from(".claude/skills/shadowdroid/SKILL.md")
        );
        for agent in ["cursor", "codex", "gemini", "antigravity"] {
            assert_eq!(
                conventional_path(agent, SkillScope::Project).unwrap(),
                PathBuf::from(".agents/skills/shadowdroid/SKILL.md"),
                "{agent}"
            );
        }
    }

    #[test]
    fn skill_marker_round_trips_and_flags_edits() {
        let content = generated_content("claude-code").unwrap();

        let (body, version, stored) = split_marker(&content).expect("marker present");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            body_hash(body),
            stored,
            "freshly generated skill is pristine"
        );

        // A hand edit changes the body hash → no longer matches the stored hash.
        let edited = content.replace("# ShadowDroid", "# ShadowDroid (my notes)");
        let (ebody, _v, estored) = split_marker(&edited).unwrap();
        assert_ne!(
            body_hash(ebody),
            estored,
            "edited skill reads as customized"
        );
    }

    #[test]
    fn checked_writer_preserves_untracked_content_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "personal instructions\n").unwrap();
        let generated = generated_content("claude-code").unwrap();

        let err = write_skill_checked("claude-code", &path, &generated, false).unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&err),
            "skill_destination_untracked"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "personal instructions\n"
        );

        write_skill_checked("claude-code", &path, &generated, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), generated);
    }
}
