//! `shadowdroid commands [--json]` — machine-readable self-introspection.
//!
//! Walks the live clap command tree (the single source of truth for the CLI
//! surface) and emits a catalog of every command, its help, nesting, and args.
//! `--help` is the human view; this is the *agent* view — a stable JSON shape an
//! agent can read once to discover the whole tool, and the source the `skill`
//! generator builds its command reference from.

use anyhow::Result;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgAction, Command, CommandFactory};

use crate::cli::Cli;

mod agent_metadata;
use agent_metadata::agent_metadata;

pub fn run(json: bool, depth: Option<usize>, describe: Option<&str>) -> Result<()> {
    let root = Cli::command();
    let catalog = if let Some(path) = describe {
        describe_catalog(&root, path).ok_or_else(|| {
            crate::diagnostic::DiagnosticError::new(
                "command_not_found",
                "commands",
                format!("no public command has path {path:?}"),
            )
            .detail(serde_json::json!({"path": path}))
            .next_actions(["shadowdroid commands --json --depth 1"])
        })?
    } else {
        catalog_with_depth(&root, depth)
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
    } else if describe.is_some() {
        print_command(&catalog["command"], 0);
    } else {
        print_tree(&catalog);
    }
    Ok(())
}

/// The full catalog as a JSON value (also used by the `skill` generator).
pub fn catalog(root: &Command) -> serde_json::Value {
    catalog_with_depth(root, None)
}

fn catalog_with_depth(root: &Command, depth: Option<usize>) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 2,
        "name": root.get_name(),
        "version": root.get_version().unwrap_or(""),
        "about": root.get_about().map(|s| s.to_string()),
        "global_args": args(root).into_iter().filter(|arg| arg["global"] == true).collect::<Vec<_>>(),
        "commands": subcommands(root, &[], depth),
    })
}

fn describe_catalog(root: &Command, raw_path: &str) -> Option<serde_json::Value> {
    let names = raw_path.split_whitespace().collect::<Vec<_>>();
    if names.is_empty() {
        return None;
    }
    let mut command = root;
    let mut parent = Vec::new();
    for name in &names {
        command = command
            .get_subcommands()
            .find(|candidate| candidate.get_name() == *name && !candidate.is_hide_set())?;
        parent.push((*name).to_string());
    }
    parent.pop();
    Some(serde_json::json!({
        "schema_version": 2,
        "path": names.join(" "),
        "global_args": args(root).into_iter().filter(|arg| arg["global"] == true).collect::<Vec<_>>(),
        "command": command_json(command, &parent, Some(0)),
    }))
}

fn subcommands(
    cmd: &Command,
    parent: &[String],
    remaining_depth: Option<usize>,
) -> Vec<serde_json::Value> {
    if remaining_depth == Some(0) {
        return Vec::new();
    }
    let child_depth = remaining_depth.map(|depth| depth.saturating_sub(1));
    cmd.get_subcommands()
        .filter(|c| c.get_name() != "help" && !c.is_hide_set())
        .map(|c| command_json(c, parent, child_depth))
        .collect()
}

fn command_json(
    cmd: &Command,
    parent: &[String],
    remaining_depth: Option<usize>,
) -> serde_json::Value {
    let mut path = parent.to_vec();
    path.push(cmd.get_name().to_string());
    let mut o = serde_json::Map::new();
    o.insert("name".into(), cmd.get_name().into());
    o.insert("path".into(), path.join(" ").into());
    let aliases = cmd.get_aliases().map(str::to_string).collect::<Vec<_>>();
    if !aliases.is_empty() {
        o.insert("aliases".into(), serde_json::json!(aliases));
    }
    if let Some(about) = cmd.get_about() {
        o.insert("about".into(), about.to_string().into());
    }
    if let Some(agent) = agent_metadata(&path) {
        o.insert("agent".into(), agent);
    }
    o.insert(
        "contract".into(),
        serde_json::json!({
            "output_mode": output_mode(&path),
            "success_condition": "process exit code 0; action envelopes also contain ok=true",
        }),
    );
    let command_args = args(cmd);
    if !command_args.is_empty() {
        o.insert("args".into(), serde_json::json!(command_args));
    }
    let groups = argument_groups(cmd);
    if !groups.is_empty() {
        o.insert("argument_groups".into(), serde_json::json!(groups));
    }
    let subs = subcommands(cmd, &path, remaining_depth);
    if !subs.is_empty() {
        o.insert("subcommands".into(), serde_json::json!(subs));
    }
    serde_json::Value::Object(o)
}
fn args(cmd: &Command) -> Vec<serde_json::Value> {
    cmd.get_arguments()
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .map(|arg| arg_json(cmd, arg))
        .collect()
}

fn arg_json(cmd: &Command, a: &Arg) -> serde_json::Value {
    let takes_value = !matches!(
        a.get_action(),
        ArgAction::SetTrue
            | ArgAction::SetFalse
            | ArgAction::Count
            | ArgAction::Help
            | ArgAction::Version
    );
    let possible_values = a
        .get_value_parser()
        .possible_values()
        .map(|values| {
            values
                .map(|value| value.get_name().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let defaults = a
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let value_names = a
        .get_value_names()
        .map(|names| names.iter().map(ToString::to_string).collect::<Vec<_>>())
        .unwrap_or_default();
    let conflicts_with = cmd
        .get_arg_conflicts_with(a)
        .into_iter()
        .map(|other| other.get_id().as_str().to_string())
        .filter(|id| !matches!(id.as_str(), "help" | "version"))
        .collect::<std::collections::BTreeSet<_>>();
    let requires = probe_requires(cmd, a);
    serde_json::json!({
        "name": a.get_id().as_str(),
        "positional": a.is_positional(),
        "long": a.get_long(),
        "short": a.get_short(),
        "global": a.is_global_set(),
        "required": a.is_required_set(),
        "takes_value": takes_value,
        "action": format!("{:?}", a.get_action()),
        "num_args": a.get_num_args().map(|range| format!("{range:?}")),
        "value_names": value_names,
        "default_values": defaults,
        "possible_values": possible_values,
        "aliases": a.get_all_aliases().unwrap_or_default(),
        "conflicts_with": conflicts_with,
        "requires": requires,
        "env": a.get_env().map(|value| value.to_string_lossy().into_owned()),
        "value_delimiter": a.get_value_delimiter(),
        "value_terminator": a.get_value_terminator().map(ToString::to_string),
        "value_hint": format!("{:?}", a.get_value_hint()),
        "allow_hyphen_values": a.is_allow_hyphen_values_set(),
        "allow_negative_numbers": a.is_allow_negative_numbers_set(),
        "require_equals": a.is_require_equals_set(),
        "exclusive": a.is_exclusive_set(),
        "trailing_var_arg": a.is_trailing_var_arg_set(),
        "last": a.is_last_set(),
        "ignore_case": a.is_ignore_case_set(),
        "help": a.get_help().map(|s| s.to_string()),
    })
}

fn argument_groups(cmd: &Command) -> Vec<serde_json::Value> {
    cmd.get_groups()
        .filter_map(|group| {
            let arguments = group
                .get_args()
                .map(|id| id.as_str().to_string())
                .collect::<Vec<_>>();
            if arguments.len() < 2 && !group.is_required_set() {
                return None;
            }
            let mut clone = group.clone();
            Some(serde_json::json!({
                "name": group.get_id().as_str(),
                "args": arguments,
                "required": group.is_required_set(),
                "multiple": clone.is_multiple(),
            }))
        })
        .collect()
}

/// Clap exposes conflicts directly but currently has no public reflection API
/// for `requires`. Probe the already-built parser with one argument plus the
/// command's unconditional required arguments, then map Clap's own
/// MissingRequiredArgument context back to live argument ids. This keeps the
/// catalog derived from the parser instead of a second hand-maintained table.
fn probe_requires(cmd: &Command, arg: &Arg) -> Vec<String> {
    if arg.is_required_set() || matches!(arg.get_id().as_str(), "help" | "version") {
        return Vec::new();
    }
    let mut probe = cmd.clone();
    probe.build();
    let Some(probe_arg) = probe
        .get_arguments()
        .find(|candidate| candidate.get_id() == arg.get_id())
        .cloned()
    else {
        return Vec::new();
    };
    let mut argv = vec![probe.get_name().to_string()];
    for required in probe
        .get_arguments()
        .filter(|candidate| candidate.is_required_set() && candidate.get_id() != arg.get_id())
    {
        argv.extend(example_tokens(required));
    }
    argv.extend(example_tokens(&probe_arg));

    let Err(error) = probe.clone().try_get_matches_from(argv) else {
        return Vec::new();
    };
    if error.kind() != ErrorKind::MissingRequiredArgument {
        return Vec::new();
    }
    let displays = match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(value)) => vec![value.as_str()],
        Some(ContextValue::Strings(values)) => values.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    };
    let mut required = std::collections::BTreeSet::new();
    for candidate in probe.get_arguments() {
        if candidate.get_id() == arg.get_id() || candidate.is_required_set() {
            continue;
        }
        let long = candidate.get_long().map(|name| format!("--{name}"));
        let short = candidate.get_short().map(|name| format!("-{name}"));
        if displays.iter().any(|display| {
            long.as_ref().is_some_and(|token| display.contains(token))
                || short.as_ref().is_some_and(|token| display.contains(token))
                || display.contains(candidate.get_id().as_str())
        }) {
            required.insert(candidate.get_id().as_str().to_string());
        }
    }
    required.into_iter().collect()
}

fn example_tokens(arg: &Arg) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(long) = arg.get_long() {
        tokens.push(format!("--{long}"));
    } else if let Some(short) = arg.get_short() {
        tokens.push(format!("-{short}"));
    }
    if !matches!(
        arg.get_action(),
        ArgAction::SetTrue
            | ArgAction::SetFalse
            | ArgAction::Count
            | ArgAction::Help
            | ArgAction::Version
    ) {
        let count = arg
            .get_num_args()
            .map(|range| range.min_values().max(1))
            .unwrap_or(1);
        let value = arg
            .get_value_parser()
            .possible_values()
            .and_then(|mut values| values.next().map(|value| value.get_name().to_string()))
            .unwrap_or_else(|| "1".to_string());
        tokens.extend(std::iter::repeat_n(value, count));
    }
    tokens
}

fn output_mode(path: &[String]) -> &'static str {
    let joined = path.join(" ");
    match joined.as_str() {
        "watch" | "log" | "net log" | "debug replay" => "jsonl",
        "test" => "passthrough_with_json_trailer",
        "ui gen" => "source_text",
        "skill" => "source_text_or_json_action",
        "net export" => "selected_format",
        "update" | "init" | "doctor" | "commands" => "human_or_json",
        _ if matches!(
            path.first().map(String::as_str),
            Some("config" | "studio" | "aar")
        ) =>
        {
            "human_or_json"
        }
        _ => "json",
    }
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
    fn catalog_avoids_duplicate_summary_and_has_contract() {
        let root = Cli::command();
        let catalog = catalog(&root);
        let commands = catalog["commands"].as_array().unwrap();
        let ui = commands
            .iter()
            .find(|command| command["name"] == "ui")
            .unwrap();
        assert!(ui.get("summary").is_none());
        assert_eq!(ui["contract"]["output_mode"], "json");

        let watch = commands
            .iter()
            .find(|command| command["name"] == "watch")
            .unwrap();
        assert!(watch.get("summary").is_none());
        assert_eq!(watch["contract"]["output_mode"], "jsonl");
    }

    #[test]
    fn compact_catalog_and_describe_keep_complete_construction_data() {
        let root = Cli::command();
        let compact = catalog_with_depth(&root, Some(1));
        let commands = compact["commands"].as_array().unwrap();
        assert!(commands
            .iter()
            .all(|command| command.get("subcommands").is_none()));
        assert!(compact["global_args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg["long"] == "device" && arg["short"] == "d"));

        let described = describe_catalog(&root, "watch").unwrap();
        let permission_policy = described["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["long"] == "permission-dialogs")
            .unwrap();
        assert_eq!(
            permission_policy["possible_values"],
            serde_json::json!(["ignore", "allow", "deny"])
        );
        assert!(!permission_policy["default_values"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn catalog_exposes_live_conflicts_requirements_and_trailing_values() {
        let root = Cli::command();

        let commands = describe_catalog(&root, "commands").unwrap();
        let depth = commands["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["long"] == "depth")
            .unwrap();
        assert!(depth["conflicts_with"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "describe"));

        let init = describe_catalog(&root, "config init").unwrap();
        let user = init["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["long"] == "user")
            .unwrap();
        assert!(user["conflicts_with"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "project"));

        let wait = describe_catalog(&root, "debug continue-until").unwrap();
        let file = wait["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["long"] == "file")
            .unwrap();
        assert_eq!(file["requires"], serde_json::json!(["line"]));

        let aar = describe_catalog(&root, "aar install").unwrap();
        let okhttp_from = aar["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["long"] == "okhttp-from")
            .unwrap();
        assert_eq!(okhttp_from["requires"], serde_json::json!(["okhttp"]));

        let test = describe_catalog(&root, "test").unwrap();
        let command = test["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["name"] == "command")
            .unwrap();
        assert_eq!(command["trailing_var_arg"], true);
        assert_eq!(command["allow_hyphen_values"], true);
    }

    #[test]
    fn nested_human_json_commands_advertise_their_real_mode() {
        assert_eq!(
            output_mode(&["config".into(), "validate".into()]),
            "human_or_json"
        );
        assert_eq!(
            output_mode(&["studio".into(), "status".into()]),
            "human_or_json"
        );
        assert_eq!(
            output_mode(&["aar".into(), "capture".into()]),
            "human_or_json"
        );
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
