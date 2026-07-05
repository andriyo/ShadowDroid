//! `shadowdroid skill <agent> [--out PATH | --install]` — generate an
//! agent-integration file so a coding agent knows how to drive Android with
//! ShadowDroid. Agents: claude-code, cursor, codex, gemini, antigravity (the
//! last four match the set Android's own CLI installs skills for).
//!
//! One curated body (driving guidance, in the current grammar) is wrapped in
//! the right frontmatter/location per agent, with an auto-generated command
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
use std::path::{Path, PathBuf};

use crate::cli::Cli;
use crate::hostenv::home_dir;

#[derive(clap::Args)]
pub struct SkillArgs {
    /// Target agent system. Omit together with `--sync` to refresh every
    /// already-installed skill instead.
    #[arg(value_parser = ["claude-code", "cursor", "codex", "gemini", "antigravity"])]
    pub agent: Option<String>,
    /// Write the generated file to this path (default: print to stdout).
    #[arg(short = 'o', long)]
    pub out: Option<PathBuf>,
    /// Write to the agent's conventional location instead of stdout.
    ///
    /// claude-code / cursor / gemini / antigravity installs are global under
    /// $HOME; codex installs are project-scoped, relative to the CWD.
    #[arg(long)]
    pub install: bool,
    /// Refresh every already-installed ShadowDroid skill to this CLI's version.
    /// Unmodified skills are rewritten in place; skills you've hand-edited are
    /// left alone and reported — unless you also pass --force.
    #[arg(long)]
    pub sync: bool,
    /// With --sync, also overwrite skills you've customized and adopt
    /// legacy/markerless ones.
    #[arg(long)]
    pub force: bool,
}

const DESCRIPTION: &str = "Drive Android apps with structured JSON via the `shadowdroid` CLI — \
observe the screen as elements, tap/swipe/type by selector, scroll-to, wait for state, watch for \
crashes, and install/grant/profile a device. Use whenever a task involves the live UI of an Android \
app (navigate, test, screenshot, reproduce a bug, automate a flow) — not for building/compiling it.";

pub fn run(args: &SkillArgs) -> Result<()> {
    if args.sync {
        return sync_skills(args.force);
    }
    let agent = args.agent.as_deref().ok_or_else(|| {
        anyhow!("specify an agent (claude-code|cursor|codex|gemini|antigravity), or pass --sync to refresh installed skills")
    })?;

    let path = if let Some(out) = &args.out {
        Some(out.clone())
    } else if args.install {
        Some(conventional_path(agent)?)
    } else {
        None
    };

    let content = generated_content(agent, path.as_deref(), args.install)?;

    match path {
        Some(p) => {
            write_skill(&p, &content)?;
            let absolute_path = absolute_path(&p)?;
            let mut payload = json!({
                    "type": "action", "cmd": "skill",
                    "agent": agent, "path": p.display().to_string(),
                    "absolute_path": absolute_path.display().to_string(),
                    "bytes": content.len(),
            });
            if let Some(note) = install_note(agent, &p, args.install) {
                payload["note"] = Value::String(note.to_string());
            }
            println!("{payload}");
        }
        None => print!("{content}"),
    }
    Ok(())
}

/// Install the conventional ShadowDroid skill files that are safe to manage
/// automatically. Global skill locations are always created/updated. Codex's
/// `AGENTS.md` is project-scoped, so this only refreshes it when it already
/// exists and is ShadowDroid-generated.
pub fn install_default_skills() -> Value {
    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for agent in ["claude-code", "cursor", "gemini", "antigravity"] {
        match conventional_path(agent)
            .and_then(|path| install_skill_at(agent, &path, true).map(|bytes| (path, bytes)))
        {
            Ok((path, bytes)) => installed.push(json!({
                "agent": agent,
                "path": path.display().to_string(),
                "bytes": bytes,
            })),
            Err(err) => failed.push(json!({"agent": agent, "error": err.to_string()})),
        }
    }

    let codex_path = PathBuf::from("AGENTS.md");
    if codex_path.exists() {
        match inspect("codex", &codex_path) {
            Ok((Decision::UpToDate, _)) => installed.push(json!({
                "agent": "codex",
                "path": codex_path.display().to_string(),
                "bytes": std::fs::metadata(&codex_path).map(|m| m.len()).unwrap_or(0),
                "already_current": true,
            })),
            Ok((Decision::NormalizeMarker | Decision::StalePristine(_), expected)) => {
                match write_skill(&codex_path, &expected) {
                    Ok(()) => installed.push(json!({
                        "agent": "codex",
                        "path": codex_path.display().to_string(),
                        "bytes": expected.len(),
                    })),
                    Err(err) => failed.push(json!({"agent": "codex", "error": err.to_string()})),
                }
            }
            Ok((Decision::Customized | Decision::Untracked, _)) => skipped.push(json!({
                "agent": "codex",
                "path": codex_path.display().to_string(),
                "reason": "existing AGENTS.md is not ShadowDroid-generated",
            })),
            Err(err) => failed.push(json!({"agent": "codex", "error": err.to_string()})),
        }
    } else {
        skipped.push(json!({
            "agent": "codex",
            "path": codex_path.display().to_string(),
            "reason": "AGENTS.md is project-scoped; run `shadowdroid skill codex --install` from the repo root to create it",
        }));
    }

    json!({
        "type": "action",
        "cmd": "skill_install_defaults",
        "installed": installed,
        "skipped": skipped,
        "failed": failed,
    })
}

fn install_skill_at(agent: &str, path: &Path, install: bool) -> Result<usize> {
    let content = generated_content(agent, Some(path), install)?;
    let bytes = content.len();
    write_skill(path, &content)?;
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

fn install_note(agent: &str, path: &Path, install: bool) -> Option<&'static str> {
    match agent {
        "claude-code" => Some(
            "Claude Code skills are global; restart or reload Claude Code if it was already running.",
        ),
        "cursor" if install || is_cursor_skill_path(path) => Some(
            "Cursor personal skills are global. Restart or reload Cursor if it was already running. For a project rule instead, pass --out <project>/.cursor/rules/shadowdroid.mdc.",
        ),
        "cursor" => Some(
            "Cursor project rules are workspace-scoped. Open the matching project folder in Cursor, or use --install for a global personal skill.",
        ),
        "codex" => Some(
            "Codex AGENTS.md instructions are project-scoped; place the file at the repo root opened by Codex.",
        ),
        "gemini" => Some(
            "Gemini CLI skills are global (~/.gemini/skills). Restart Gemini CLI if it was already running.",
        ),
        "antigravity" => Some(
            "Antigravity skills are global (~/.gemini/antigravity*). Restart Antigravity if it was already running.",
        ),
        _ => None,
    }
}

/// The agent's conventional integration location (relative to $HOME or $CWD).
fn conventional_path(agent: &str) -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(match agent {
        "claude-code" => home.join(".claude/skills/shadowdroid/SKILL.md"),
        "cursor" => home.join(".cursor/skills/shadowdroid/SKILL.md"),
        "gemini" => home.join(".gemini/skills/shadowdroid/SKILL.md"),
        "antigravity" => antigravity_skill_path(&home),
        "codex" => PathBuf::from("AGENTS.md"),
        other => return Err(anyhow!("unknown agent '{other}'")),
    })
}

/// Antigravity's global skills dir is cited as both `.gemini/antigravity-cli`
/// (Antigravity guides) and `.gemini/antigravity` (Android's CLI docs); use
/// whichever already exists, else default to the former.
fn antigravity_skill_path(home: &Path) -> PathBuf {
    for sub in [".gemini/antigravity-cli", ".gemini/antigravity"] {
        let dir = home.join(sub);
        if dir.is_dir() {
            return dir.join("skills/shadowdroid/SKILL.md");
        }
    }
    home.join(".gemini/antigravity-cli/skills/shadowdroid/SKILL.md")
}

fn content_for_destination(
    agent: &str,
    body: &str,
    path: Option<&Path>,
    install: bool,
) -> Result<String> {
    if agent == "cursor" {
        if install || path.is_some_and(is_cursor_skill_path) {
            return Ok(wrap_skill_md(body));
        }
        if path.is_some_and(is_cursor_project_rule_path) {
            return Ok(wrap_cursor_project_rule(body));
        }
    }
    wrap_for_agent(agent, body)
}

fn is_cursor_skill_path(path: &Path) -> bool {
    let raw = normalized_path(path);
    raw.contains(".cursor/skills") || path.file_name().is_some_and(|name| name == "SKILL.md")
}

fn is_cursor_project_rule_path(path: &Path) -> bool {
    let raw = normalized_path(path);
    raw.contains(".cursor/rules")
        || path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("mdc"))
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn wrap_for_agent(agent: &str, body: &str) -> Result<String> {
    Ok(match agent {
        // SKILL.md (name + description frontmatter) — Claude Code, Gemini CLI,
        // and Antigravity all consume this identical shape.
        "claude-code" | "gemini" | "antigravity" => wrap_skill_md(body),
        // Cursor stdout defaults to a project rule because that is the most
        // copy-paste friendly format for an arbitrary destination.
        "cursor" => wrap_cursor_project_rule(body),
        // Codex / generic AGENTS.md: a self-contained section, no frontmatter.
        "codex" => format!("# ShadowDroid — driving Android\n\n{body}\n"),
        other => {
            return Err(anyhow!(
                "unknown agent '{other}' (claude-code|cursor|codex|gemini|antigravity)"
            ));
        }
    })
}

fn wrap_skill_md(body: &str) -> String {
    format!(
        "---\nname: shadowdroid\ndescription: {desc}\n---\n\n# ShadowDroid\n\n{body}\n",
        desc = DESCRIPTION,
    )
}

fn wrap_cursor_project_rule(body: &str) -> String {
    format!(
        "---\ndescription: {desc}\nglobs:\nalwaysApply: false\n---\n\n# ShadowDroid\n\n{body}\n",
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

/// Build the full file: curated body + per-destination wrapper + version marker.
fn generated_content(agent: &str, dest: Option<&Path>, install: bool) -> Result<String> {
    let body = format!("{}\n\n{}", SKILL_BODY.trim(), command_reference());
    let core = content_for_destination(agent, &body, dest, install)?;
    Ok(append_marker(&core))
}

fn append_marker(core: &str) -> String {
    format!(
        "{core}{MARKER_PREFIX}v={v} h={h} · auto-generated by `shadowdroid skill`; run `shadowdroid skill sync` to update. Hand-edits are detected and preserved. -->\n",
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

// ── detect + refresh installed skills ─────────────────────────────────────

/// The auto-refreshable global skill locations (single-purpose SKILL.md files
/// under $HOME). Codex's AGENTS.md and Cursor project `.mdc` rules are
/// project-scoped and may hold unrelated content, so they're excluded.
fn global_skill_targets() -> Vec<(&'static str, PathBuf)> {
    let Ok(home) = home_dir() else {
        return Vec::new();
    };
    vec![
        (
            "claude-code",
            home.join(".claude/skills/shadowdroid/SKILL.md"),
        ),
        ("cursor", home.join(".cursor/skills/shadowdroid/SKILL.md")),
        ("gemini", home.join(".gemini/skills/shadowdroid/SKILL.md")),
        ("antigravity", antigravity_skill_path(&home)),
    ]
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
    let expected = generated_content(agent, Some(path), true)?;
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
fn sync_skills(force: bool) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let mut refreshed = Vec::new();
    let mut up_to_date = Vec::new();
    let mut skipped_customized = Vec::new();
    let mut skipped_untracked = Vec::new();

    for (agent, path) in global_skill_targets() {
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
        "type": "action", "cmd": "skill_sync", "version": version,
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
    println!("{payload}");
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

    for (agent, path) in global_skill_targets() {
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
        o["hint"] = Value::String("run `shadowdroid skill sync --force` to update".into());
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
    fn cursor_install_uses_global_skill_format() {
        let content = content_for_destination(
            "cursor",
            "body",
            Some(Path::new("/home/user/.cursor/skills/shadowdroid/SKILL.md")),
            true,
        )
        .unwrap();

        assert!(content.starts_with("---\nname: shadowdroid\n"));
        assert!(!content.contains("alwaysApply: false"));
    }

    #[test]
    fn cursor_project_rule_output_uses_mdc_format() {
        let content = content_for_destination(
            "cursor",
            "body",
            Some(Path::new(".cursor/rules/shadowdroid.mdc")),
            false,
        )
        .unwrap();

        assert!(content.starts_with("---\ndescription: "));
        assert!(content.contains("globs:\nalwaysApply: false"));
        assert!(!content.contains("\nname: shadowdroid\n"));
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
    fn new_agents_use_skill_md_format() {
        for agent in ["gemini", "antigravity"] {
            let c = wrap_for_agent(agent, "body").unwrap();
            assert!(c.starts_with("---\nname: shadowdroid\n"), "{agent}: {c}");
            assert!(c.contains("# ShadowDroid"), "{agent}");
        }
    }

    #[test]
    fn conventional_paths_for_new_agents() {
        // Tolerant of an unset $HOME (e.g. minimal CI), since the path is built
        // from it.
        if let Ok(g) = conventional_path("gemini") {
            assert!(
                normalized_path(&g).ends_with(".gemini/skills/shadowdroid/SKILL.md"),
                "{}",
                g.display()
            );
        }
        if let Ok(a) = conventional_path("antigravity") {
            assert!(
                normalized_path(&a).contains(".gemini/antigravity"),
                "{}",
                a.display()
            );
        }
    }

    #[test]
    fn skill_marker_round_trips_and_flags_edits() {
        let path = Path::new("/tmp/.claude/skills/shadowdroid/SKILL.md");
        let content = generated_content("claude-code", Some(path), true).unwrap();

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
}
