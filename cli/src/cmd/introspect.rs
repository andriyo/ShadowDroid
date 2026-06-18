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
        "commands": subcommands(root, &[]),
    })
}

fn subcommands(cmd: &Command, parent: &[String]) -> Vec<serde_json::Value> {
    cmd.get_subcommands()
        .filter(|c| c.get_name() != "help")
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

fn agent_metadata(path: &[String]) -> Option<serde_json::Value> {
    let key = path.join(" ");
    match key.as_str() {
        "commands" => Some(serde_json::json!({
            "use_when": ["Discover ShadowDroid's command tree, flags, and agent decision hints without scraping human help text."],
            "output": "json catalog when --json is passed; human tree otherwise",
            "side_effects": ["none"],
            "next_commands": ["config schema --json", "ui dump", "watch"]
        })),
        "config" => Some(serde_json::json!({
            "use_when": ["Repeated app, package, device, project, or debugger parameters would cost tokens across commands."],
            "output": "json for schema/paths/validate when --json is passed",
            "side_effects": ["config init writes .shadowdroid.json or ~/.shadowdroid/config.json"],
            "next_commands": ["config paths --json", "config schema --json", "config init --project", "config validate --json", "debug auto"]
        })),
        "doctor" => Some(serde_json::json!({
            "use_when": ["ShadowDroid cannot connect, screen reads fail, adb/device state is unclear, or networking may be miswired."],
            "output": "diagnostic report; use --json for machine-readable status",
            "side_effects": ["--fix may reinstall the server, recreate forwards, restart components, and clear dangling device proxy state"],
            "next_commands": ["doctor --fix", "connect", "collect"]
        })),
        "collect" => Some(serde_json::json!({
            "use_when": ["Need a shareable evidence bundle after a failure or before handing off an investigation."],
            "output": "directory with doctor report, device info, logcat/crash context, and best-effort screen/screenshot/app state",
            "side_effects": ["writes files under --out or a generated collection directory"],
            "next_commands": ["doctor", "debug snapshot", "layout snapshot"]
        })),
        "watch" => Some(serde_json::json!({
            "use_when": [
                "Need one live timeline for screen changes, crashes, toasts, watcher actions, and HTTP(S) traffic when the net proxy is running.",
                "Need to correlate UI state with network responses, app crashes, or watcher automation during a flow."
            ],
            "avoid_when": ["Need one immediate actionable element list; use ui dump instead.", "Need a saved layout/source artifact; use layout snapshot instead."],
            "output": "jsonl event stream: ready, screen_compact/screen, crash, watcher_fired, http/http_intercept, warning, error",
            "side_effects": ["polls the screen", "tails logcat", "may run watcher actions", "auto-attaches to a running net proxy unless --no-net is passed"],
            "prerequisites": ["shadowdroid connect", "shadowdroid net start for HTTP(S) events"],
            "next_commands": ["ui tap", "ui text", "ui wait", "net start", "net show <id>", "debug snapshot"],
            "prefer_over": {
                "ui dump": "for long flows or correlation across multiple event types",
                "net log": "for live UI plus network correlation"
            }
        })),
        "ui" => Some(serde_json::json!({
            "use_when": ["Need to read or manipulate the currently visible UI."],
            "output": "one JSON object per read/action",
            "side_effects": ["action subcommands can tap, type, scroll, press keys, or navigate"],
            "next_commands": ["ui dump", "ui tap --text <label>", "ui text <value>", "ui wait --text <label>"]
        })),
        "ui dump" => Some(serde_json::json!({
            "use_when": ["Need the current actionable UI state for selector choice before tapping, typing, or waiting."],
            "avoid_when": ["Need Compose/source/layout inspection or a durable artifact; use layout snapshot."],
            "output": "compact screen JSON by default; --full adds bounds and every UIAutomator flag",
            "side_effects": ["none"],
            "next_commands": ["ui tap --id <id>", "ui tap --text <text>", "ui text --id <id> <value>", "ui wait"],
            "prefer_over": {
                "layout snapshot": "when the next step is acting on the UI rather than debugging layout/source structure"
            }
        })),
        "ui find" => Some(serde_json::json!({
            "use_when": ["Need to resolve a selector without tapping it."],
            "output": "matching elements, compact by default",
            "side_effects": ["none"],
            "next_commands": ["ui tap --id <id>", "ui text --id <id>"]
        })),
        "ui tap" => Some(serde_json::json!({
            "use_when": ["Need to activate a visible element by selector, fresh ui dump id, or coordinates."],
            "output": "action JSON with the chosen target/action",
            "side_effects": ["taps the device UI"],
            "prerequisites": ["prefer selectors or ids from a fresh ui dump over hard-coded coordinates"],
            "next_commands": ["ui wait", "ui dump", "watch"]
        })),
        "ui text" => Some(serde_json::json!({
            "use_when": ["Need to type into the focused field or a field selected by id/text/rid/desc/xpath."],
            "output": "action JSON",
            "side_effects": ["changes text in the app UI"],
            "next_commands": ["ui key enter", "ui wait", "ui dump"]
        })),
        "ui wait" => Some(serde_json::json!({
            "use_when": ["Need to block until an element, activity, or package appears or disappears."],
            "output": "JSON match result",
            "side_effects": ["polls current UI/app state"],
            "next_commands": ["ui dump", "ui tap", "watch"]
        })),
        "layout" => Some(serde_json::json!({
            "use_when": ["Need visual/layout/source structure artifacts rather than immediate UI actions."],
            "output": "layout JSON artifacts and diffs",
            "side_effects": ["snapshot can write files and screenshots"],
            "next_commands": ["layout snapshot", "layout diff", "layout source", "layout recompositions"]
        })),
        "layout snapshot" => Some(serde_json::json!({
            "use_when": ["Need a saved UI structure artifact, layout diff input, screenshot pairing, Compose semantics, or source mapping."],
            "avoid_when": ["Need to tap/type based on the current UI; use ui dump."],
            "output": "layout_snapshot JSON; --out writes it, --screenshot writes a sibling screenshot artifact",
            "side_effects": ["optional file writes with --out/--screenshot"],
            "prerequisites": ["Android Studio Layout Inspector bridge is needed for Compose/source enrichment; UIAutomator tree is still returned without it"],
            "next_commands": ["layout diff <before> <after>", "layout source --id <id>", "layout source --draw-id <id>"],
            "prefer_over": {
                "ui dump": "when preserving or debugging layout/source structure matters more than immediate action"
            }
        })),
        "layout source" => Some(serde_json::json!({
            "use_when": ["Need to map a current UIAutomator element or Studio Layout Inspector draw id back to source when available."],
            "output": "layout_source JSON with matched node and source availability",
            "side_effects": ["none"],
            "next_commands": ["debug break line", "debug auto", "layout snapshot --source-map"]
        })),
        "debug" => Some(serde_json::json!({
            "use_when": ["Need runtime causality, stack/variable state, breakpoints, replay, or Android Studio debugger control."],
            "output": "bounded JSON debug state or JSONL timelines depending on subcommand",
            "side_effects": ["attach/break/resume/step commands affect debugger/app execution"],
            "next_commands": ["debug auto", "debug snapshot", "debug record", "debug run-until-crash"]
        })),
        "debug auto" => Some(serde_json::json!({
            "use_when": ["Need the fastest agent entrypoint for launching/configuring the app, attaching the debugger when available, and returning a useful snapshot."],
            "output": "debug snapshot JSON",
            "side_effects": ["may launch the app and attach Android Studio debugger"],
            "next_commands": ["debug variables", "debug eval", "debug break line", "ui dump"]
        })),
        "debug snapshot" => Some(serde_json::json!({
            "use_when": ["Need current app/runtime/debugger/logcat/screen state for causality, not just visible UI."],
            "avoid_when": ["Need layout/source structure; use layout snapshot/source."],
            "output": "bounded debug state JSON",
            "side_effects": ["reads app/debugger/logcat/screen state"],
            "next_commands": ["debug variables", "debug eval", "layout source", "collect"]
        })),
        "net" => Some(serde_json::json!({
            "use_when": ["Need to enable, inspect, intercept, mutate, export, or replay HTTP(S) traffic."],
            "output": "one JSON object per command; live HTTP events appear on watch after net start",
            "side_effects": ["start/stop/trust/rule/intercept/resume/drop/respond change device proxy, trust, or flow behavior"],
            "next_commands": ["net check <pkg>", "net start", "watch", "net log", "net show <id>", "net intercept"]
        })),
        "net check" => Some(serde_json::json!({
            "use_when": ["Need to know whether a package is likely interceptable before relying on HTTP(S) events."],
            "output": "interceptability verdict JSON",
            "side_effects": ["none"],
            "next_commands": ["net trust", "net start", "watch"]
        })),
        "net start" => Some(serde_json::json!({
            "use_when": ["Need watch to include HTTP(S) events or need to intercept/modify traffic."],
            "output": "action JSON with proxy/device wiring details",
            "side_effects": ["starts host proxy daemon", "sets adb reverse", "sets device global http_proxy"],
            "next_commands": ["watch", "net status", "net check <pkg>", "net intercept"]
        })),
        "net status" => Some(serde_json::json!({
            "use_when": ["Need to verify whether the proxy daemon is running, the device points at it, or flows are held."],
            "output": "net_status action JSON",
            "side_effects": ["none"],
            "next_commands": ["net start", "net stop", "watch"]
        })),
        "net log" => Some(serde_json::json!({
            "use_when": ["Need recent HTTP flows from the session log without watching live UI."],
            "output": "http events followed by net_log summary",
            "side_effects": ["none"],
            "next_commands": ["net show <id>", "net export har <id>", "watch"]
        })),
        "net show" => Some(serde_json::json!({
            "use_when": ["Need headers, bodies, or full detail for a flow id seen in watch or net log."],
            "output": "flow detail JSON",
            "side_effects": ["none"],
            "next_commands": ["net resume <id>", "net respond <id>", "net export har <id>"]
        })),
        "net intercept" => Some(serde_json::json!({
            "use_when": ["Need the agent to pause matching HTTP flows and decide how to mutate, drop, or respond."],
            "output": "held flows appear as http_intercept events on watch",
            "side_effects": ["matching app HTTP calls block until released or timed out"],
            "next_commands": ["watch", "net show <id>", "net resume <id>", "net drop <id>", "net respond <id>"]
        })),
        "net resume" => Some(serde_json::json!({
            "use_when": ["Need to release a held flow, optionally with status/header/body/url mutations."],
            "output": "release result JSON",
            "side_effects": ["unblocks a held HTTP flow"],
            "next_commands": ["watch", "net log", "ui dump"]
        })),
        "net drop" => Some(serde_json::json!({
            "use_when": ["Need the app to experience a held request as a connection failure or explicit status."],
            "output": "release result JSON",
            "side_effects": ["unblocks a held HTTP flow with failure behavior"],
            "next_commands": ["watch", "ui dump"]
        })),
        "net respond" => Some(serde_json::json!({
            "use_when": ["Need to short-circuit a held request with a canned response without contacting upstream."],
            "output": "release result JSON",
            "side_effects": ["unblocks a held HTTP flow with a synthetic response"],
            "next_commands": ["watch", "ui dump"]
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;

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
