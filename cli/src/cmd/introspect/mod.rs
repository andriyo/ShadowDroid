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

mod agent_metadata;
use agent_metadata::agent_metadata;

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
        "commands": subcommands(root, &[]),
    })
}

fn subcommands(cmd: &Command, parent: &[String]) -> Vec<serde_json::Value> {
    cmd.get_subcommands()
        .filter(|c| c.get_name() != "help" && !c.is_hide_set())
        .map(|c| command_json(c, parent))
        .collect()
}

fn command_json(cmd: &Command, parent: &[String]) -> serde_json::Value {
    let mut path = parent.to_vec();
    path.push(cmd.get_name().to_string());
    let mut o = serde_json::Map::new();
    o.insert("name".into(), cmd.get_name().into());
    if let Some(about) = cmd.get_about() {
        o.insert("about".into(), about.to_string().into());
        o.insert("summary".into(), about.to_string().into());
    }
    if let Some(agent) = agent_metadata(&path) {
        o.insert("agent".into(), agent);
    }
    let subs = subcommands(cmd, &path);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use serde_json::Value;

    fn find_command<'a>(commands: &'a [Value], path: &[&str]) -> Option<&'a Value> {
        let (name, rest) = path.split_first()?;
        let command = commands
            .iter()
            .find(|command| command["name"].as_str() == Some(*name))?;
        if rest.is_empty() {
            Some(command)
        } else {
            find_command(command["subcommands"].as_array()?, rest)
        }
    }

    fn collect_commands_without_agent(
        commands: &[Value],
        prefix: &mut Vec<String>,
        missing: &mut Vec<String>,
    ) {
        for command in commands {
            if let Some(name) = command["name"].as_str() {
                prefix.push(name.to_string());
                if command.get("agent").is_none() {
                    missing.push(prefix.join(" "));
                }
                if let Some(subcommands) = command["subcommands"].as_array() {
                    collect_commands_without_agent(subcommands, prefix, missing);
                }
                prefix.pop();
            }
        }
    }

    #[test]
    fn catalog_includes_summary_alias_for_agent_discovery() {
        let root = Cli::command();
        let catalog = catalog(&root);
        let commands = catalog["commands"].as_array().unwrap();
        let ui = commands
            .iter()
            .find(|command| command["name"] == "ui")
            .unwrap();
        assert_eq!(ui["summary"], ui["about"]);

        let watch = commands
            .iter()
            .find(|command| command["name"] == "watch")
            .unwrap();
        assert_eq!(watch["summary"], watch["about"]);
    }

    #[test]
    fn catalog_hides_internal_commands_from_agent_catalog() {
        let root = Cli::command();
        let catalog = catalog(&root);
        let commands = catalog["commands"].as_array().unwrap();

        assert!(find_command(commands, &["net", "daemon"]).is_none());
    }

    #[test]
    fn catalog_advertises_agent_metadata_for_every_public_command() {
        let root = Cli::command();
        let catalog = catalog(&root);
        let commands = catalog["commands"].as_array().unwrap();
        let mut missing = Vec::new();
        collect_commands_without_agent(commands, &mut Vec::new(), &mut missing);

        assert!(
            missing.is_empty(),
            "public commands without agent metadata: {missing:?}"
        );
    }

    #[test]
    fn catalog_exposes_agent_decision_hints_for_common_tool_choices() {
        let root = Cli::command();
        let catalog = catalog(&root);
        let commands = catalog["commands"].as_array().unwrap();

        let watch = commands
            .iter()
            .find(|command| command["name"] == "watch")
            .unwrap();
        assert!(watch["agent"]["use_when"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str().unwrap_or("").contains("HTTP(S) traffic")));
        assert!(watch["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg["long"] == "no-net"));

        let ui = commands
            .iter()
            .find(|command| command["name"] == "ui")
            .unwrap();
        let ui_dump = ui["subcommands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == "dump")
            .unwrap();
        assert_eq!(
            ui_dump["agent"]["prefer_over"]["layout snapshot"],
            "when the next step is acting on the UI rather than debugging layout/source structure"
        );

        let layout = commands
            .iter()
            .find(|command| command["name"] == "layout")
            .unwrap();
        let snapshot = layout["subcommands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == "snapshot")
            .unwrap();
        assert_eq!(
            snapshot["agent"]["prefer_over"]["ui dump"],
            "when preserving or debugging layout/source structure matters more than immediate action"
        );

        let recompositions = layout["subcommands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == "recompositions")
            .unwrap();
        assert!(recompositions["agent"]["use_when"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value
                .as_str()
                .unwrap_or("")
                .contains("Compose recomposition")));
        assert!(recompositions["agent"]["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("layout recompositions --reset")));
    }

    #[test]
    fn catalog_does_not_advertise_duplicate_net_watch() {
        let root = Cli::command();
        let catalog = catalog(&root);
        let commands = catalog["commands"].as_array().unwrap();
        let net = commands
            .iter()
            .find(|command| command["name"] == "net")
            .unwrap();
        assert!(!net["subcommands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "watch"));
    }
}
