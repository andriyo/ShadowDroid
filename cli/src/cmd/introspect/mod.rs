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
use std::sync::LazyLock;

use crate::cli::Cli;

mod agent_metadata;
mod guides;
use agent_metadata::agent_metadata;

static CLI_COMMAND_TEMPLATE: LazyLock<Command> = LazyLock::new(Cli::command);

#[allow(clippy::too_many_arguments)]
pub fn run(
    json: bool,
    depth: Option<usize>,
    describe: Option<&str>,
    path: &[String],
    search: Option<&str>,
    guide: Option<&str>,
    compact: bool,
) -> Result<()> {
    let root = &*CLI_COMMAND_TEMPLATE;
    let positional_path = (!path.is_empty()).then(|| path.join(" "));
    let requested_path = describe.or(positional_path.as_deref());
    let mut catalog = if let Some(topic) = guide {
        guide_catalog(topic)?
    } else if let Some(query) = search {
        search_catalog(root, query, compact)?
    } else if let Some(path) = requested_path {
        describe_catalog(root, path).ok_or_else(|| command_not_found(root, path))?
    } else {
        catalog_with_depth(root, depth)
    };
    if compact && search.is_none() {
        compact_catalog(&mut catalog);
    }
    if json {
        crate::events::emit_result(&catalog);
    } else if guide.is_some() {
        print!("{}", catalog["content"].as_str().unwrap_or_default());
    } else if search.is_some() {
        print_search_results(&catalog);
    } else if requested_path.is_some() {
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
        "schema_version": 3,
        "name": root.get_name(),
        "version": root.get_version().unwrap_or(""),
        "about": root.get_about().map(|s| s.to_string()),
        "global_args": args(root).into_iter().filter(|arg| arg["global"] == true).collect::<Vec<_>>(),
        "commands": subcommands(root, &[], depth),
        "next_actions": next_actions_for_path("commands"),
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
        "schema_version": 3,
        "path": names.join(" "),
        "global_args": args(root).into_iter().filter(|arg| arg["global"] == true).collect::<Vec<_>>(),
        // Include one level of child names/contracts for namespace queries such
        // as `commands net`; leaf queries remain a single bounded command.
        "command": command_json(command, &parent, Some(1)),
        "next_actions": next_actions_for_path("commands"),
    }))
}

/// One driving guide as a bounded catalog response. Topic resolution accepts
/// the canonical topic or any covered command group (`--guide layout` returns
/// the debugger guide).
fn guide_catalog(topic: &str) -> Result<serde_json::Value> {
    let Some(guide) = guides::find_guide(topic) else {
        let available = guides::GUIDES
            .iter()
            .map(|guide| serde_json::json!({"topic": guide.topic, "covers": guide.covers}))
            .collect::<Vec<_>>();
        return Err(crate::diagnostic::DiagnosticError::new(
            "guide_not_found",
            "commands",
            format!("no driving guide covers {topic:?}"),
        )
        .detail(serde_json::json!({"topic": topic, "available": available}))
        .next_actions(
            guides::GUIDES
                .iter()
                .map(|guide| format!("shadowdroid commands --guide {} --json", guide.topic)),
        )
        .into());
    };
    let next_actions = guide
        .covers
        .iter()
        .map(|group| format!("shadowdroid commands {group} --json --compact"))
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "schema_version": 3,
        "guide": guide.topic,
        "covers": guide.covers,
        "content": guide.content,
        "next_actions": next_actions,
    }))
}

/// Canonical driving-guide topics; keeps the skill body's pointer stubs in
/// lockstep with the guides actually served.
#[cfg(test)]
pub(crate) fn guide_topics() -> Vec<&'static str> {
    guides::GUIDES.iter().map(|guide| guide.topic).collect()
}

fn command_not_found(root: &Command, path: &str) -> anyhow::Error {
    let nearest_paths = nearest_command_paths(root, path, 5);
    let next_actions = if nearest_paths.is_empty() {
        vec!["shadowdroid commands --json --depth 1".to_string()]
    } else {
        nearest_paths
            .iter()
            .take(3)
            .map(|candidate| format!("shadowdroid commands {candidate} --json --compact"))
            .collect()
    };
    crate::diagnostic::DiagnosticError::new(
        "command_not_found",
        "commands",
        format!("no public command has path {path:?}"),
    )
    .detail(serde_json::json!({"path": path, "nearest_paths": nearest_paths}))
    .next_actions(next_actions)
    .into()
}

const SEARCH_LIMIT: usize = 25;

fn search_catalog(root: &Command, raw_query: &str, compact: bool) -> Result<serde_json::Value> {
    let query = raw_query.trim();
    if query.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "search_query_required",
            "commands",
            "--search needs at least one non-whitespace search term",
        )
        .next_actions(["shadowdroid commands --search 'response body' --json"])
        .into());
    }
    let terms = query
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect::<Vec<_>>();
    let mut matches = all_command_values(root)
        .into_iter()
        .filter(|command| {
            let searchable = command.to_string().to_lowercase();
            terms.iter().all(|term| searchable.contains(term))
        })
        .collect::<Vec<_>>();
    let total = matches.len();
    matches.truncate(SEARCH_LIMIT);
    if compact {
        for command in &mut matches {
            compact_command(command);
        }
    }
    Ok(serde_json::json!({
        "schema_version": 3,
        "query": query,
        "count": matches.len(),
        "total_matches": total,
        "truncated": total > SEARCH_LIMIT,
        "commands": matches,
        "next_actions": next_actions_for_path("commands"),
    }))
}

fn all_command_values(root: &Command) -> Vec<serde_json::Value> {
    fn collect(command: &Command, parent: &[String], out: &mut Vec<serde_json::Value>) {
        for child in command
            .get_subcommands()
            .filter(|child| child.get_name() != "help" && !child.is_hide_set())
        {
            out.push(command_json(child, parent, Some(0)));
            let mut path = parent.to_vec();
            path.push(child.get_name().to_string());
            collect(child, &path, out);
        }
    }

    let mut commands = Vec::new();
    collect(root, &[], &mut commands);
    commands
}

fn nearest_command_paths(root: &Command, query: &str, limit: usize) -> Vec<String> {
    let query = query.trim().to_lowercase();
    let mut scored = all_command_values(root)
        .into_iter()
        .filter_map(|command| command["path"].as_str().map(str::to_string))
        .map(|path| {
            let score = crate::fusion::similarity(&query, &path.to_lowercase());
            (path, score)
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(left_path, left_score), (right_path, right_score)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| left_path.len().cmp(&right_path.len()))
            .then_with(|| left_path.cmp(right_path))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(path, _)| path)
        .collect()
}

fn compact_catalog(catalog: &mut serde_json::Value) {
    if let Some(command) = catalog.get_mut("command") {
        compact_command(command);
    }
    if let Some(commands) = catalog
        .get_mut("commands")
        .and_then(|value| value.as_array_mut())
    {
        for command in commands {
            compact_command(command);
        }
    }
}

fn compact_command(command: &mut serde_json::Value) {
    let Some(object) = command.as_object_mut() else {
        return;
    };
    let examples = object
        .remove("agent")
        .and_then(|agent| agent.get("examples").cloned());
    if let Some(examples) = examples {
        object.insert("examples".into(), examples);
    }
    if let Some(subcommands) = object
        .get_mut("subcommands")
        .and_then(|value| value.as_array_mut())
    {
        for child in subcommands {
            compact_command(child);
        }
    }
}

fn print_search_results(catalog: &serde_json::Value) {
    println!(
        "{} command(s) matched {:?}:",
        catalog["count"].as_u64().unwrap_or(0),
        catalog["query"].as_str().unwrap_or_default(),
    );
    if let Some(commands) = catalog["commands"].as_array() {
        for command in commands {
            println!(
                "  {:<32} {}",
                command["path"].as_str().unwrap_or_default(),
                command["about"].as_str().unwrap_or_default(),
            );
        }
    }
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
    if let Some(agent) = catalog_agent_metadata(&path) {
        o.insert("agent".into(), agent);
    }
    o.insert(
        "contract".into(),
        serde_json::json!({
            "output_mode": output_mode(&path),
            "success_condition": "process exit code 0; action envelopes also contain ok=true",
            "next_actions": "every terminal JSON success/error contains a non-empty next_actions array; streaming events omit it until their terminal summary",
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

/// Return parseable follow-up command templates for a public command path.
///
/// The hand-curated agent metadata is the sole source of these actions. Runtime
/// envelopes specialize observed placeholders (or replace unresolved templates
/// with an exact `commands --describe` action), while `commands --json` exposes
/// the same templates for planning.
pub(crate) fn next_actions_for_path(path: &str) -> Vec<String> {
    let parts = path
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    agent_metadata(&parts)
        .and_then(|value| value.get("next_actions").cloned())
        .and_then(|value| value.as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(normalize_next_command))
        .collect()
}

fn catalog_agent_metadata(path: &[String]) -> Option<serde_json::Value> {
    let mut metadata = agent_metadata(path)?;
    let object = metadata.as_object_mut()?;
    object.remove("next_actions");
    object.insert(
        "next_actions".into(),
        serde_json::json!(next_actions_for_path(&path.join(" "))),
    );
    Some(metadata)
}

fn normalize_next_command(command: &str) -> String {
    let command = command.trim();
    let normalized = if command.starts_with("shadowdroid ") || command == "shadowdroid" {
        command.to_string()
    } else {
        format!("shadowdroid {command}")
    };
    if normalized
        .split_whitespace()
        .any(|token| token.contains('|'))
        && let Some(path) = command_path_for_invocation(&normalized)
    {
        return format!("shadowdroid commands --json --describe '{path}'");
    }
    if !command.contains('<')
        && let Some(path) = incomplete_command_path(command)
    {
        return format!("shadowdroid commands --json --describe '{path}'");
    }
    if !action_template_parses(&normalized)
        && let Some(path) = command_path_for_invocation(&normalized)
    {
        return format!("shadowdroid commands --json --describe '{path}'");
    }
    normalized
}

/// A catalog hint such as bare `debug eval` or `ui tap` is a dead end: clap (or
/// the command's semantic validator) immediately rejects it. Replace those
/// incomplete hints with an exact, runnable describe command. Templates that
/// already name `<placeholders>` remain templates because they tell the agent
/// precisely which observed value must be supplied.
fn incomplete_command_path(command: &str) -> Option<String> {
    let command = command.strip_prefix("shadowdroid ").unwrap_or(command);
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    let root = &*CLI_COMMAND_TEMPLATE;
    let mut node = root;
    let mut path = Vec::new();
    let mut consumed = 0usize;
    for token in &tokens {
        let Some(child) = node.get_subcommands().find(|candidate| {
            candidate.get_name() == *token
                || candidate.get_all_aliases().any(|alias| alias == *token)
        }) else {
            break;
        };
        path.push(child.get_name().to_string());
        node = child;
        consumed += 1;
    }
    if path.is_empty() || consumed < tokens.len() {
        return None;
    }
    let runtime_required = matches!(
        path.join(" ").as_str(),
        "ui tap"
            | "ui text"
            | "ui wait"
            | "ui scroll-to"
            | "layout source"
            | "profile apply"
            | "debug inspect"
    );
    let clap_required = node
        .get_arguments()
        .any(|argument| argument.is_required_set());
    let needs_subcommand = node.has_subcommands();
    (runtime_required || clap_required || needs_subcommand).then(|| path.join(" "))
}

/// Resolve the canonical public command path at the front of a suggested
/// invocation, ignoring global `-d/--device` scoping. Used when a runtime
/// template still has unresolved placeholders and must become an exact
/// `commands --describe` action instead of a command that is guaranteed to
/// fail parsing or semantic validation.
pub(crate) fn command_path_for_invocation(command: &str) -> Option<String> {
    let command = command.strip_prefix("shadowdroid ")?;
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    let root = &*CLI_COMMAND_TEMPLATE;
    let mut node = root;
    let mut path = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        let token = tokens[index];
        if path.is_empty() && matches!(token, "-d" | "--device") {
            index += 2;
            continue;
        }
        let child = node.get_subcommands().find(|candidate| {
            candidate.get_name() == token || candidate.get_all_aliases().any(|alias| alias == token)
        });
        let Some(child) = child else {
            if path.is_empty() {
                index += 1;
                continue;
            }
            break;
        };
        path.push(child.get_name().to_string());
        node = child;
        index += 1;
    }
    (!path.is_empty()).then(|| path.join(" "))
}

fn action_template_parses(command: &str) -> bool {
    let command = materialize_action_template(command);
    let Some(argv) = split_shell_words(&command) else {
        return false;
    };
    CLI_COMMAND_TEMPLATE
        .clone()
        .try_get_matches_from(argv)
        .is_ok()
}

fn materialize_action_template(command: &str) -> String {
    let mut output = String::with_capacity(command.len());
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '<' {
            output.push(ch);
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '>' {
                closed = true;
                break;
            }
            name.push(next);
        }
        if !closed {
            output.push('<');
            output.push_str(&name);
            break;
        }
        let replacement = match name.as_str() {
            "pkg" | "app" => "com.example.app",
            "id" | "n" | "frame" => "1",
            "op" => "CAMERA",
            "mode" => "allow",
            "permission" => "android.permission.CAMERA",
            "remote" | "remote-dir" => "/sdcard/example",
            "local" | "before" | "after" => "/tmp/example",
            "label" | "text" | "value" => "Example",
            "cmd" | "command" => "true",
            "recommended verb" => "ui dump",
            _ => "example",
        };
        output.push_str(replacement);
    }
    output
}

fn split_shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                word.push(ch);
            }
            continue;
        }
        if ch.is_whitespace() && quote.is_none() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    (!words.is_empty()).then_some(words)
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
        "watch" | "log" | "net log" | "net ws" | "debug replay" => "jsonl",
        "test" => "passthrough_with_json_trailer",
        "ui gen" => "source_text",
        "skill" => "source_text_or_json_action",
        "net export" => "json_action_with_artifact",
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

    fn collect_commands_with_invalid_next_action_templates(
        commands: &[Value],
        prefix: &mut Vec<String>,
        missing: &mut Vec<String>,
    ) {
        for command in commands {
            if let Some(name) = command["name"].as_str() {
                prefix.push(name.to_string());
                let actions = command["agent"]["next_actions"].as_array();
                if !actions.is_some_and(|actions| {
                    !actions.is_empty()
                        && actions
                            .iter()
                            .all(|action| action.as_str().is_some_and(action_template_parses))
                }) {
                    missing.push(prefix.join(" "));
                }
                if command["agent"].get("next_commands").is_some() {
                    missing.push(format!("{} (legacy next_commands)", prefix.join(" ")));
                }
                if let Some(subcommands) = command["subcommands"].as_array() {
                    collect_commands_with_invalid_next_action_templates(
                        subcommands,
                        prefix,
                        missing,
                    );
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

        let net_export = find_command(commands, &["net", "export"]).unwrap();
        assert_eq!(
            net_export["contract"]["output_mode"],
            "json_action_with_artifact"
        );
    }

    #[test]
    fn compact_catalog_and_describe_keep_complete_construction_data() {
        let root = Cli::command();
        let compact = catalog_with_depth(&root, Some(1));
        let commands = compact["commands"].as_array().unwrap();
        assert!(
            commands
                .iter()
                .all(|command| command.get("subcommands").is_none())
        );
        assert!(
            compact["global_args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg["long"] == "device" && arg["short"] == "d")
        );

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
        assert!(
            !permission_policy["default_values"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn scoped_compact_leaf_is_small_fast_and_keeps_the_invocation_contract() {
        let root = Cli::command();
        let started = std::time::Instant::now();
        let mut described = describe_catalog(&root, "net rule add").unwrap();
        compact_catalog(&mut described);
        let elapsed = started.elapsed();
        let bytes = serde_json::to_vec(&described).unwrap();

        assert_eq!(described["path"], "net rule add");
        assert!(described["command"].get("agent").is_none());
        assert!(
            described["command"]["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|argument| argument["long"] == "host")
        );
        assert!(
            bytes.len() < 32 * 1024,
            "leaf contract was {} bytes",
            bytes.len()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "leaf lookup took {elapsed:?}"
        );
    }

    #[test]
    fn search_covers_argument_help_and_examples_and_stays_bounded() {
        let root = Cli::command();
        let response_body = search_catalog(&root, "response body", true).unwrap();
        let paths = response_body["commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|command| command["path"].as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"net respond"), "{paths:?}");
        assert!(
            response_body["commands"]
                .as_array()
                .unwrap()
                .iter()
                .all(|command| command.get("agent").is_none())
        );

        let example = search_catalog(&root, "set-status 503", false).unwrap();
        assert!(
            example["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command["path"] == "net rule add")
        );
        assert!(response_body["count"].as_u64().unwrap() <= SEARCH_LIMIT as u64);
    }

    #[test]
    fn unknown_paths_suggest_the_nearest_public_contracts() {
        let root = Cli::command();
        let nearest = nearest_command_paths(&root, "net ruel add", 5);
        assert_eq!(nearest.first().map(String::as_str), Some("net rule add"));
        assert!(nearest.iter().all(|path| path != "net daemon"));
    }

    #[test]
    fn driving_guides_resolve_by_topic_and_by_covered_group() {
        // Canonical topics resolve and carry their moved skill-body depth.
        for (topic, marker) in [
            ("net", "net check"),
            ("debugger", "debug sessions"),
            ("state", "--scope uid"),
        ] {
            let guide = guide_catalog(topic).unwrap();
            assert_eq!(guide["guide"], topic);
            let content = guide["content"].as_str().unwrap();
            assert!(content.contains(marker), "{topic}: {content}");
        }

        // Every covered command group is accepted as an alias for its guide.
        for (alias, topic) in [("aar", "net"), ("layout", "debugger"), ("files", "state")] {
            assert_eq!(guide_catalog(alias).unwrap()["guide"], topic, "{alias}");
        }

        // Guide next_actions are runnable catalog commands.
        let guide = guide_catalog("net").unwrap();
        for action in guide["next_actions"].as_array().unwrap() {
            let action = action.as_str().unwrap();
            assert!(action_template_parses(action), "{action}");
        }
    }

    #[test]
    fn guide_covers_stay_in_lockstep_with_public_command_groups() {
        let root = Cli::command();
        let mut seen = std::collections::HashSet::new();
        for guide in guides::GUIDES {
            for group in guide.covers {
                assert!(
                    root.get_subcommands()
                        .any(|command| command.get_name() == *group && !command.is_hide_set()),
                    "guide {:?} covers {group:?}, which is not a public top-level command",
                    guide.topic
                );
                assert!(
                    seen.insert(*group),
                    "group {group:?} is claimed by more than one guide"
                );
            }
        }
    }

    #[test]
    fn unknown_guide_topic_fails_typed_and_lists_available_guides() {
        let err = guide_catalog("networking").unwrap_err();
        assert_eq!(crate::cli::error_code_of(&err), "guide_not_found");
        let diagnostic = err
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        let available = serde_json::to_string(&diagnostic.detail).unwrap();
        for topic in ["net", "debugger", "state"] {
            assert!(available.contains(topic), "{available}");
        }
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
        assert!(
            depth["conflicts_with"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == "describe")
        );

        let init = describe_catalog(&root, "config init").unwrap();
        let user = init["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["long"] == "user")
            .unwrap();
        assert!(
            user["conflicts_with"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == "project")
        );

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
    fn every_public_command_advertises_parseable_next_action_templates() {
        let catalog = catalog(&Cli::command());
        assert_eq!(catalog["schema_version"], 3);
        assert!(
            catalog["next_actions"]
                .as_array()
                .is_some_and(|actions| !actions.is_empty())
        );
        let mut missing = Vec::new();
        collect_commands_with_invalid_next_action_templates(
            catalog["commands"].as_array().unwrap(),
            &mut Vec::new(),
            &mut missing,
        );
        assert!(
            missing.is_empty(),
            "public commands with missing or invalid next_action templates: {missing:?}"
        );
    }

    #[test]
    fn incomplete_bare_hints_become_exact_discovery_commands() {
        assert_eq!(
            normalize_next_command("ui tap"),
            "shadowdroid commands --json --describe 'ui tap'"
        );
        assert_eq!(
            normalize_next_command("debug eval"),
            "shadowdroid commands --json --describe 'debug eval'"
        );
        assert_eq!(
            normalize_next_command("debug inspect"),
            "shadowdroid commands --json --describe 'debug inspect'"
        );
        assert_eq!(
            normalize_next_command("ui scroll-to"),
            "shadowdroid commands --json --describe 'ui scroll-to'"
        );
        assert_eq!(
            normalize_next_command("ui tap --id <id>"),
            "shadowdroid ui tap --id <id>"
        );
        assert_eq!(
            normalize_next_command("ui text --id <id>"),
            "shadowdroid commands --json --describe 'ui text'"
        );
        assert_eq!(
            normalize_next_command("app current"),
            "shadowdroid app current"
        );
        assert_eq!(
            normalize_next_command("appops set <pkg> <op> <mode> --scope uid|package"),
            "shadowdroid commands --json --describe 'appops set'"
        );
    }

    #[test]
    fn action_template_validation_checks_the_complete_clap_invocation() {
        assert!(action_template_parses("shadowdroid app start <pkg>"));
        assert!(action_template_parses(
            "shadowdroid commands --json --describe '<recommended verb>'"
        ));
        assert!(!action_template_parses(
            "shadowdroid debug sessions --definitely-invalid"
        ));
        assert!(!action_template_parses("shadowdroid net export"));
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
        assert!(
            watch["agent"]["use_when"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str().unwrap_or("").contains("HTTP(S) traffic"))
        );
        assert!(
            watch["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg["long"] == "no-net")
        );

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
        assert!(
            recompositions["agent"]["use_when"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value
                    .as_str()
                    .unwrap_or("")
                    .contains("Compose recomposition"))
        );
        assert!(
            recompositions["agent"]["next_actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("shadowdroid layout recompositions --reset"))
        );
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
        assert!(
            !net["subcommands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command["name"] == "watch")
        );
    }
}
