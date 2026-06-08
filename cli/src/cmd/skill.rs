//! `shadowdroid skill <agent> [--out PATH | --install]` — generate an
//! agent-integration file so Claude Code / Cursor / Codex know how to drive
//! Android with ShadowDroid.
//!
//! One curated body (driving guidance, in the current grammar) is wrapped in
//! the right frontmatter/location per agent, with an auto-generated command
//! reference appended from the live introspection catalog ([crate::cmd::introspect])
//! so it never drifts from the actual CLI. Prints to stdout by default;
//! `--out` writes a chosen path; `--install` writes the agent's conventional
//! location.

use anyhow::{anyhow, Context, Result};
use clap::CommandFactory;
use std::path::{Path, PathBuf};

use crate::cli::Cli;

#[derive(clap::Args)]
pub struct SkillArgs {
    /// Target agent system.
    #[arg(value_parser = ["claude-code", "cursor", "codex"])]
    pub agent: String,
    /// Write the generated file to this path (default: print to stdout).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Write to the agent's conventional location instead of stdout.
    ///
    /// Claude Code and Cursor installs are global under $HOME. Codex installs
    /// are project-scoped and relative to the current working directory.
    #[arg(long)]
    pub install: bool,
}

const DESCRIPTION: &str = "Drive Android apps with structured JSON via the `shadowdroid` CLI — \
observe the screen as elements, tap/swipe/type by selector, scroll-to, wait for state, watch for \
crashes, and install/grant/profile a device. Use whenever a task involves the live UI of an Android \
app (navigate, test, screenshot, reproduce a bug, automate a flow) — not for building/compiling it.";

pub fn run(args: &SkillArgs) -> Result<()> {
    let body = format!("{}\n\n{}", SKILL_BODY.trim(), command_reference());

    let path = if let Some(out) = &args.out {
        Some(out.clone())
    } else if args.install {
        Some(conventional_path(&args.agent)?)
    } else {
        None
    };

    let content = content_for_destination(&args.agent, &body, path.as_deref(), args.install)?;

    match path {
        Some(p) => {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::write(&p, &content).with_context(|| format!("writing {}", p.display()))?;
            let absolute_path = absolute_path(&p)?;
            let mut payload = serde_json::json!({
                    "type": "action", "cmd": "skill",
                    "agent": args.agent, "path": p.display().to_string(),
                    "absolute_path": absolute_path.display().to_string(),
                    "bytes": content.len(),
            });
            if let Some(note) = install_note(&args.agent, &p, args.install) {
                payload["note"] = serde_json::Value::String(note.to_string());
            }
            println!("{payload}");
        }
        None => print!("{content}"),
    }
    Ok(())
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
        "claude-code" => Some("Claude Code skills are global; restart or reload Claude Code if it was already running."),
        "cursor" if install || is_cursor_skill_path(path) => Some("Cursor personal skills are global. Restart or reload Cursor if it was already running. For a project rule instead, pass --out <project>/.cursor/rules/shadowdroid.mdc."),
        "cursor" => Some("Cursor project rules are workspace-scoped. Open the matching project folder in Cursor, or use --install for a global personal skill."),
        "codex" => Some("Codex AGENTS.md instructions are project-scoped; place the file at the repo root opened by Codex."),
        _ => None,
    }
}

/// The agent's conventional integration location (relative to $HOME or $CWD).
fn conventional_path(agent: &str) -> Result<PathBuf> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("$HOME not set"))
    };
    Ok(match agent {
        "claude-code" => home()?.join(".claude/skills/shadowdroid/SKILL.md"),
        "cursor" => home()?.join(".cursor/skills/shadowdroid/SKILL.md"),
        "codex" => PathBuf::from("AGENTS.md"),
        other => return Err(anyhow!("unknown agent '{other}'")),
    })
}

fn content_for_destination(
    agent: &str,
    body: &str,
    path: Option<&Path>,
    install: bool,
) -> Result<String> {
    if agent == "cursor" {
        if install || path.is_some_and(is_cursor_skill_path) {
            return Ok(wrap_cursor_skill(body));
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
        // Claude Code skill: YAML frontmatter (name + description).
        "claude-code" => format!(
            "---\nname: shadowdroid\ndescription: {desc}\n---\n\n# ShadowDroid\n\n{body}\n",
            desc = DESCRIPTION,
        ),
        // Cursor stdout defaults to a project rule because that is the most
        // copy-paste friendly format for an arbitrary destination.
        "cursor" => wrap_cursor_project_rule(body),
        // Codex / generic AGENTS.md: a self-contained section, no frontmatter.
        "codex" => format!("# ShadowDroid — driving Android\n\n{body}\n"),
        other => {
            return Err(anyhow!(
                "unknown agent '{other}' (claude-code|cursor|codex)"
            ))
        }
    })
}

fn wrap_cursor_skill(body: &str) -> String {
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
}

/// Render the live command catalog as a grouped markdown reference.
fn command_reference() -> String {
    let root = Cli::command();
    let catalog = crate::cmd::introspect::catalog(&root);
    let mut out =
        String::from("## Command reference\n\nGenerated from `shadowdroid commands --json`.\n\n");
    if let Some(cmds) = catalog["commands"].as_array() {
        for c in cmds {
            let name = c["name"].as_str().unwrap_or("");
            let about = c["about"].as_str().unwrap_or("");
            if let Some(subs) = c["subcommands"].as_array() {
                out.push_str(&format!("- **`{name}`** — {about}\n"));
                for s in subs {
                    let sn = s["name"].as_str().unwrap_or("");
                    let sa = s["about"].as_str().unwrap_or("");
                    out.push_str(&format!("  - `{name} {sn}` — {sa}\n"));
                }
            } else {
                out.push_str(&format!("- **`{name}`** — {about}\n"));
            }
        }
    }
    out
}

const SKILL_BODY: &str = r#"
`shadowdroid` is a single static binary that drives Android apps and emits one
JSON line per action. A tiny on-device UI Automator service makes screen dumps
~25 ms, so the observe→act→observe loop stays responsive. It talks to the device
over `adb`; no Appium, no Python.

## When to use it

Reach for ShadowDroid whenever a task touches the **live UI of an Android app**:
navigate to a screen, tap/type, take a screenshot, reproduce a crash, exercise a
flow, or inspect what's on screen. It is *not* for building/compiling the app
(Gradle, Kotlin source) — only for observing or acting on a running app.

## First contact

```bash
shadowdroid devices                 # attached devices/emulators
shadowdroid connect                 # install + start the on-device service
shadowdroid commands --json         # the full command catalog, for discovery
shadowdroid screen | jq             # current UI as a flat element list
```

If no device is attached, ask the user to start one — don't boot an emulator
silently. With multiple devices, pass `-d <serial>`.

## The driving loop

Read the screen, act by **selector** (don't hard-code coordinates), confirm.

```bash
shadowdroid screen | jq '.elements[] | {id, text, rid, tap}'
shadowdroid tap --text "Sign in"        # or --rid / --desc / --xpath, or `tap <id>`
shadowdroid text "alice@example.com"    # types into the focused field
shadowdroid key enter
shadowdroid scroll-to --text "Privacy" --tap   # scroll a list until found, then tap
shadowdroid wait --text "Welcome" --timeout-ms 8000   # block until it appears
shadowdroid screenshot /tmp/after.png
```

For a long flow, stream every change and watch for crashes:

```bash
shadowdroid watch --app com.example.app | jq -c .
# emits ready → screen_compact → … and a structured `crash` event on a stack trace
```

## Debugging for agents

Use a bounded snapshot when you need causality, not just the screen:

```bash
shadowdroid debug snapshot --app com.example.app --depth 1 | jq
```

With the Android Studio plugin installed and Studio restarted, use `debugger`
for attach, breakpoints, stack, threads, variables, deterministic eval, and
watches:

```bash
shadowdroid debugger attach --project /path/to/app --package com.example.app
shadowdroid debugger break line --file app/src/main/java/Foo.kt --line 42
shadowdroid debugger variables --thread 0 --frame 0 --depth 2 --timeout-ms 2500
shadowdroid debugger eval 'this.state' --thread 0 --frame 0 --depth 2 --timeout-ms 5000
shadowdroid debugger watch add 'this.state'
```

Prefer `debug record` for longer investigations; it writes a JSONL timeline of
screen changes, logcat, debugger snapshots, screenshots, and app lifecycle.
Use `layout snapshot --compose --semantics --source-map` when the question is
visual structure or Compose source; use `layout source` to map a node back to
code (`--id` for UIAutomator nodes, `--draw-id` for Android Studio Layout
Inspector nodes) and `layout recompositions --reset` to isolate Compose
recomposition counts during one interaction. Debugger read commands are bounded
and return structured JSON warnings/errors instead of waiting indefinitely when
the app is running or stopped on a frame without debug information.

## Make a device deterministic before driving

```bash
shadowdroid app install ./app-debug.apk --grant-all --wait-front  # install + grant + launch
shadowdroid profile apply --preset automation                     # animations off (+ stylus tutorial)
shadowdroid perm grant com.example.app android.permission.CAMERA
```

`connect` already disables the Android 14+ stylus-handwriting tutorial that
otherwise hijacks the first text-field focus.

## When something breaks

```bash
shadowdroid doctor            # device state / APK / forward / server / owners / clock
shadowdroid doctor --fix      # repair (reinstall, re-forward, restart)
shadowdroid collect --app com.example.app   # bundle logs+screen+screenshot+diagnostics
```

## Output contract

Every action prints one JSON object with `type` and `cmd`; reads print their
payload. Parse stdout; never scrape human text. Tap by selector and re-read the
screen rather than trusting fixed coordinates across layouts.
"#;
