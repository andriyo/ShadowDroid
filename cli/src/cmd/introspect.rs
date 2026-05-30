//! `shadowdroid commands [--json]` — machine-readable self-introspection.
//!
//! Walks the live clap command tree (the single source of truth for the CLI
//! surface) and emits a catalog of every command, its help, nesting, and args.
//! `--help` is the human view; this is the *agent* view — a stable JSON shape an
//! agent can read once to discover the whole tool, and the source the `skill`
//! generator builds its command reference from.

use anyhow::Result;
use clap::{Arg, ArgAction, Command, CommandFactory};

use crate::cli::Cli;

pub fn run(json: bool) -> Result<()> {
    let root = Cli::command();
    let catalog = catalog(&root);
    if json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
    } else {
        print_tree(&catalog);
    }
    Ok(())
}

/// The full catalog as a JSON value (also used by the `skill` generator).
pub fn catalog(root: &Command) -> serde_json::Value {
    serde_json::json!({
        "name": root.get_name(),
        "version": root.get_version().unwrap_or(""),
        "about": root.get_about().map(|s| s.to_string()),
        "commands": subcommands(root),
    })
}

fn subcommands(cmd: &Command) -> Vec<serde_json::Value> {
    cmd.get_subcommands()
        .filter(|c| c.get_name() != "help")
        .map(command_json)
        .collect()
}

fn command_json(cmd: &Command) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("name".into(), cmd.get_name().into());
    if let Some(about) = cmd.get_about() {
        o.insert("about".into(), about.to_string().into());
    }
    let subs = subcommands(cmd);
    if subs.is_empty() {
        o.insert("args".into(), serde_json::json!(args(cmd)));
    } else {
        o.insert("subcommands".into(), serde_json::json!(subs));
    }
    serde_json::Value::Object(o)
}

fn args(cmd: &Command) -> Vec<serde_json::Value> {
    cmd.get_arguments()
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .map(arg_json)
        .collect()
}

fn arg_json(a: &Arg) -> serde_json::Value {
    let takes_value = !matches!(
        a.get_action(),
        ArgAction::SetTrue
            | ArgAction::SetFalse
            | ArgAction::Count
            | ArgAction::Help
            | ArgAction::Version
    );
    serde_json::json!({
        "name": a.get_id().as_str(),
        "positional": a.is_positional(),
        "long": a.get_long(),
        "required": a.is_required_set(),
        "takes_value": takes_value,
        "help": a.get_help().map(|s| s.to_string()),
    })
}

// ── human tree ────────────────────────────────────────────────

fn print_tree(catalog: &serde_json::Value) {
    println!(
        "{} {}",
        catalog["name"].as_str().unwrap_or("shadowdroid"),
        catalog["version"].as_str().unwrap_or("")
    );
    if let Some(cmds) = catalog["commands"].as_array() {
        for c in cmds {
            print_command(c, 1);
        }
    }
}

fn print_command(c: &serde_json::Value, depth: usize) {
    let indent = "  ".repeat(depth);
    let name = c["name"].as_str().unwrap_or("");
    let about = c["about"].as_str().unwrap_or("");
    println!("{indent}{name:<14} {about}");
    if let Some(subs) = c["subcommands"].as_array() {
        for s in subs {
            print_command(s, depth + 1);
        }
    }
}
