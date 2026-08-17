//! Executable enforcement of the agent-facing output contract:
//!   - every failure prints exactly one `{"type":"error","code":…,"msg":…}` line;
//!   - one-shot results carry no `ts` (only streamed `watch` events do);
//!   - introspection commands print valid JSON and exit 0.
//!
//! These run the real binary (no device needed), so they catch contract drift
//! that unit tests on the emitter can't — e.g. a command that prints to stdout
//! around the envelope, or a non-JSON error path.

use std::process::Command;

/// Run the built binary with `SHADOWDROID_QUIET=1` so stderr tracing never
/// bleeds into the stdout we assert on. Returns (stdout, exit_code).
fn run(args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
        .args(args)
        .env("SHADOWDROID_QUIET", "1")
        .output()
        .expect("spawn shadowdroid");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Assert stdout is exactly one non-empty line of JSON and return it parsed.
fn one_json_line(stdout: &str) -> serde_json::Value {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one JSON line, got {}:\n{stdout}",
        lines.len()
    );
    serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?}", lines[0]))
}

#[test]
fn unknown_flag_is_a_structured_usage_error() {
    let (out, code) = run(&["ui", "wait", "--definitely-not-a-flag"]);
    let v = one_json_line(&out);
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "usage", "{v}");
    assert!(v["msg"].is_string(), "error must carry a string msg: {v}");
    assert!(
        v.get("arg").is_some(),
        "unknown-flag error should name the offending flag: {v}"
    );
    assert!(
        v.get("ts").is_none(),
        "one-shot error must not carry ts: {v}"
    );
    assert_eq!(code, 2, "usage errors exit 2");
    assert!(
        v["next_actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty())
    );
}

#[test]
fn invalid_subcommand_is_a_structured_error() {
    let (out, _) = run(&["ui", "frobnicate"]);
    let v = one_json_line(&out);
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["code"], "usage", "{v}");
}

#[test]
fn bare_invocation_is_a_structured_discovery_error() {
    let (out, code) = run(&[]);
    let value = one_json_line(&out);
    assert_eq!(value["type"], "error", "{value}");
    assert_eq!(value["code"], "missing_subcommand", "{value}");
    assert!(
        value["next_actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty())
    );
    assert_eq!(code, 2);
}

#[test]
fn help_exits_zero_and_is_not_an_error_envelope() {
    let (out, code) = run(&["ui", "wait", "--help"]);
    assert_eq!(code, 0, "--help exits 0");
    assert!(
        !out.trim_start().starts_with('{'),
        "--help renders human text, not the JSON envelope"
    );
}

#[test]
fn help_exposes_named_target_and_takeover_controls() {
    let (out, code) = run(&["connect", "--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("--target <TARGET>"), "{out}");
    assert!(out.contains("--takeover"), "{out}");
}

#[test]
fn breakpoint_commands_expose_validation_bypass() {
    let (out, code) = run(&["debug", "break", "line", "--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("--force"), "{out}");
    let (out, code) = run(&["debug", "break", "update", "--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("--force"), "{out}");
}

#[test]
fn logpoint_add_requires_explicit_output_and_hides_suspend_policy() {
    let (out, code) = run(&[
        "debug",
        "logpoint",
        "add",
        "--file",
        "/tmp/Foo.kt",
        "--line",
        "42",
    ]);
    let value = one_json_line(&out);
    assert_eq!(code, 2);
    assert_eq!(value["code"], "usage");
    let arg = value["arg"].as_str().unwrap_or_default();
    assert!(arg.contains("--expression"), "{value}");
    assert!(arg.contains("--log-message"), "{value}");
    assert!(arg.contains("--log-stack"), "{value}");

    let (help, code) = run(&["debug", "logpoint", "add", "--help"]);
    assert_eq!(code, 0);
    for option in [
        "--expression",
        "--log-message",
        "--log-stack",
        "--condition",
        "--pass-count",
        "--owner",
        "--max-events-per-second",
        "--max-message-chars",
    ] {
        assert!(help.contains(option), "missing {option}:\n{help}");
    }
    assert!(
        !help.contains("--suspend"),
        "logpoint suspension must stay fixed and hidden:\n{help}"
    );

    let (out, code) = run(&[
        "debug",
        "logpoint",
        "add",
        "--file",
        "/tmp/Foo.kt",
        "--line",
        "42",
        "--expression",
        "   ",
    ]);
    let value = one_json_line(&out);
    assert_eq!(code, 2);
    assert_eq!(value["code"], "usage");
    assert_eq!(value["arg"], "--expression <EXPRESSION>");
}

#[test]
fn logpoint_numeric_limits_match_the_studio_bridge() {
    for (option, value) in [
        ("--line", "2147483648"),
        ("--pass-count", "2147483648"),
        ("--max-message-chars", "65537"),
    ] {
        let line = if option == "--line" { value } else { "42" };
        let mut args = vec![
            "debug",
            "logpoint",
            "add",
            "--file",
            "/tmp/Foo.kt",
            "--line",
            line,
            "--expression",
            "counter",
        ];
        if option != "--line" {
            args.extend([option, value]);
        }
        let (out, code) = run(&args);
        let error = one_json_line(&out);
        assert_eq!(code, 2, "{option} unexpectedly accepted {value}: {error}");
        assert_eq!(error["code"], "usage", "{option}: {error}");
    }
}

#[test]
fn logpoint_resume_cursor_requires_its_stream_id() {
    for args in [
        vec!["debug", "logpoint", "events", "--after", "7"],
        vec!["debug", "logpoint", "events", "--stream-id", "stream-a"],
        vec!["debug", "logpoint", "follow", "--after", "7"],
        vec!["debug", "logpoint", "follow", "--stream-id", "stream-a"],
    ] {
        let (out, code) = run(&args);
        let error = one_json_line(&out);
        assert_eq!(code, 2, "unexpectedly accepted {args:?}: {error}");
        assert_eq!(error["code"], "usage", "{args:?}: {error}");
    }

    for command in ["events", "follow"] {
        let (help, code) = run(&["debug", "logpoint", command, "--help"]);
        assert_eq!(code, 0, "{help}");
        assert!(help.contains("--stream-id <STREAM_ID>"), "{help}");

        let path = format!("debug logpoint {command}");
        let (catalog, code) = run(&["commands", "--json", "--describe", &path]);
        let catalog = one_json_line(&catalog);
        assert_eq!(code, 0, "{catalog}");
        let args = catalog["command"]["args"].as_array().unwrap();
        let after = args.iter().find(|arg| arg["name"] == "after").unwrap();
        let stream_id = args.iter().find(|arg| arg["name"] == "stream_id").unwrap();
        assert_eq!(after["requires"], serde_json::json!(["stream_id"]));
        assert_eq!(stream_id["requires"], serde_json::json!(["after"]));
    }
}

#[test]
fn logpoint_catalog_describes_transactional_add_and_jsonl_follow() {
    let (out, code) = run(&["commands", "--json", "--describe", "debug logpoint add"]);
    let add = one_json_line(&out);
    assert_eq!(code, 0, "{add}");
    assert_eq!(add["path"], "debug logpoint add");
    assert_eq!(add["command"]["contract"]["output_mode"], "json");
    assert_eq!(
        add["command"]["agent"]["output"],
        "logpoint creation JSON with stable id, ownership, limits, and fixed suspend_policy=NONE"
    );
    let groups = add["command"]["argument_groups"].as_array().unwrap();
    assert!(groups.iter().any(|group| {
        group["name"] == "logpoint_output"
            && group["required"] == true
            && group["args"] == serde_json::json!(["expression", "log_message", "log_stack"])
    }));

    let (out, code) = run(&["commands", "--json", "--describe", "debug logpoint follow"]);
    let follow = one_json_line(&out);
    assert_eq!(code, 0, "{follow}");
    assert_eq!(follow["path"], "debug logpoint follow");
    assert_eq!(follow["command"]["contract"]["output_mode"], "jsonl");
    assert_eq!(
        follow["command"]["agent"]["output"],
        "JSONL logpoint events and cursor-gap/stream-reset warnings followed by a terminal action summary"
    );
}

#[test]
fn video_help_and_catalog_expose_the_agent_recording_contract() {
    let (help, code) = run(&["video", "record", "--help"]);
    assert_eq!(code, 0);
    for option in [
        "--out",
        "--duration",
        "--backend",
        "--size",
        "--bit-rate",
        "--display-id",
        "--bugreport",
        "--segment-seconds",
    ] {
        assert!(help.contains(option), "missing {option}:\n{help}");
    }

    let (out, code) = run(&["commands", "--json", "--describe", "video start"]);
    let value = one_json_line(&out);
    assert_eq!(code, 0);
    assert_eq!(value["path"], "video start");
    assert_eq!(value["command"]["contract"]["output_mode"], "json");
    assert!(value["command"]["agent"]["use_when"].is_array());
}

#[test]
fn video_record_missing_bundle_is_a_structured_usage_error() {
    let (out, code) = run(&["video", "record", "--duration", "30s"]);
    let value = one_json_line(&out);
    assert_eq!(code, 2);
    assert_eq!(value["code"], "usage");
    assert_eq!(value["arg"], "--out <DIR>");
    assert!(
        value["next_actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty())
    );
}

#[test]
fn config_schema_describes_stable_device_targets() {
    let (out, code) = run(&["config", "schema", "--json"]);
    let value = one_json_line(&out);
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        value["target_entry"]["start"]["enum"],
        serde_json::json!(["never", "if-needed"])
    );
    assert_eq!(value["fields"]["targets"]["type"], "object");
    assert_eq!(value["app_entry"]["target"]["type"], "string");
}

#[test]
fn catalog_exposes_typed_effect_contracts_and_their_vocabulary() {
    let (out, code) = run(&["commands", "--json", "--depth", "1"]);
    let catalog = one_json_line(&out);
    assert_eq!(code, 0, "{catalog}");
    assert_eq!(catalog["effect_model"]["schema_version"], 1);
    assert_eq!(catalog["effect_model"]["mode"], "conservative_upper_bound");
    assert_eq!(
        catalog["effect_model"]["wildcard_semantics"]["effect"],
        "unbounded_external_command"
    );
    assert_eq!(
        catalog["effect_model"]["wildcard_semantics"]["scope"],
        "transitive_external_subprocess_effects"
    );
    let vocabulary = catalog["effect_model"]["effects"]
        .as_array()
        .expect("effect vocabulary");
    for required in [
        "host_read",
        "host_write",
        "device_read",
        "device_mutate",
        "package_install",
        "process_start",
        "port_mapping_mutate",
        "network_download",
    ] {
        assert!(
            vocabulary.iter().any(|effect| effect["name"] == required),
            "missing {required}: {vocabulary:?}"
        );
    }
    let wildcard = vocabulary
        .iter()
        .find(|effect| effect["name"] == "unbounded_external_command")
        .expect("external-command wildcard");
    assert_eq!(wildcard["wildcard"], true);
    assert!(
        catalog["commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| {
                let contract = &command["contract"]["effect_contract"];
                contract["effects"].is_array()
                    && contract["effect_coverage"]["kind"].is_string()
                    && contract["effect_coverage"]["wildcard_effects"].is_array()
                    && contract["conditional_effects"]
                        .as_array()
                        .is_some_and(|effects| {
                            effects.iter().any(|effect| {
                                effect["effect"] == "host_write"
                                    && effect["when"]
                                        .as_str()
                                        .is_some_and(|when| when.contains("usage logging"))
                            })
                        })
            })
    );
}

#[test]
fn external_subprocess_effect_wildcard_is_explicit_per_command() {
    for path in ["aar install", "device shell", "test", "update"] {
        let (out, code) = run(&["commands", "--json", "--describe", path]);
        let described = one_json_line(&out);
        assert_eq!(code, 0, "{described}");
        let contract = &described["command"]["contract"]["effect_contract"];
        assert_eq!(
            contract["effect_coverage"]["kind"], "enumerated_plus_unbounded_external_subprocess",
            "{path}: {contract}"
        );
        assert_eq!(
            contract["effect_coverage"]["wildcard_effects"],
            serde_json::json!(["unbounded_external_command"]),
            "{path}: {contract}"
        );
    }

    let (out, code) = run(&["commands", "--json", "--describe", "commands"]);
    let described = one_json_line(&out);
    assert_eq!(code, 0, "{described}");
    let contract = &described["command"]["contract"]["effect_contract"];
    assert!(
        contract["effects"]
            .as_array()
            .is_some_and(|effects| effects.iter().any(|effect| effect == "host_read")),
        "commands must declare the usage-config read: {contract}"
    );
    assert_eq!(contract["effect_coverage"]["kind"], "enumerated");
    assert_eq!(
        contract["effect_coverage"]["wildcard_effects"],
        serde_json::json!([])
    );
}

#[test]
fn collect_effect_contract_is_passive_and_server_reads_advertise_bring_up() {
    let (out, code) = run(&["commands", "--json", "--describe", "collect"]);
    let collect = one_json_line(&out);
    assert_eq!(code, 0, "{collect}");
    let effect_contract = &collect["command"]["contract"]["effect_contract"];
    assert_eq!(effect_contract["scope"], "invocation");
    assert_eq!(
        effect_contract["effects"],
        serde_json::json!(["host_read", "host_write", "device_read"])
    );
    assert!(
        effect_contract["conditional_effects"]
            .as_array()
            .is_some_and(|effects| effects
                .iter()
                .any(|effect| effect["effect"] == "host_write")),
        "usage-log exception must be explicit on collect: {effect_contract}"
    );
    let dependencies = effect_contract["effectful_dependencies"]
        .as_array()
        .unwrap();
    assert!(
        dependencies
            .iter()
            .any(|value| value == "existing_server_probe")
    );
    assert!(
        dependencies
            .iter()
            .any(|value| value == "target_resolve_online"),
        "collect must reject offline targets without booting them: {effect_contract}"
    );
    for forbidden in [
        "server_ensure_ready",
        "release_asset_download",
        "package_installer",
        "managed_process_start",
        "port_mapping_mutation",
        "target_resolve_may_start",
        "target_resolve_existing",
    ] {
        assert!(
            dependencies.iter().all(|value| value != forbidden),
            "collect gained {forbidden}: {effect_contract}"
        );
    }

    let (out, code) = run(&["commands", "--json", "--describe", "ui dump"]);
    let ui_dump = one_json_line(&out);
    assert_eq!(code, 0, "{ui_dump}");
    let effects = ui_dump["command"]["contract"]["effect_contract"]["effects"]
        .as_array()
        .unwrap();
    for lifecycle_effect in [
        "package_install",
        "process_start",
        "port_mapping_mutate",
        "network_download",
    ] {
        assert!(
            effects.iter().any(|effect| effect == lifecycle_effect),
            "ui dump must advertise ensure-ready's {lifecycle_effect}: {effects:?}"
        );
    }
}

#[test]
fn commands_json_is_one_valid_json_object() {
    let (out, code) = run(&["commands", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).expect("commands --json is valid JSON");
    assert!(v.is_object(), "catalog is a JSON object");
    assert_eq!(v["schema_version"], 3);
    assert!(
        v["next_actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty())
    );
    assert_eq!(code, 0);
}

#[test]
fn early_closing_stdout_consumer_does_not_panic_the_cli() {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
        .args(["commands", "--json"])
        .env("SHADOWDROID_QUIET", "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn shadowdroid");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut prefix = [0u8; 64];
    stdout.read_exact(&mut prefix).expect("read catalog prefix");
    drop(stdout);
    let status = child.wait().expect("wait for shadowdroid");
    assert_ne!(status.code(), Some(101), "broken pipe must not panic");
}

#[test]
fn config_paths_json_is_valid_json() {
    let (out, code) = run(&["config", "paths", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).expect("config paths --json is valid JSON");
    assert_eq!(code, 0);
    // The project config is the folder form.
    assert!(
        v["project_config"]
            .as_str()
            .is_some_and(|p| p.ends_with(".shadowdroid/config.json")),
        "project_config should be the folder form: {v}"
    );
    assert!(
        v["next_actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty())
    );
}

#[test]
fn missing_required_argument_points_to_the_exact_command_contract() {
    let (out, code) = run(&["layout", "diff", "only-before.json"]);
    let value = one_json_line(&out);
    assert_eq!(code, 2);
    assert_eq!(value["code"], "usage");
    let actions = value["next_actions"].as_array().unwrap();
    assert!(
        actions
            .iter()
            .any(|action| action == "shadowdroid layout diff --help")
    );
    assert!(
        actions
            .iter()
            .any(|action| action == "shadowdroid commands --json --describe 'layout diff'")
    );
}

#[test]
fn host_action_success_has_nonempty_next_actions() {
    let (out, code) = run(&["usage", "status"]);
    let value = one_json_line(&out);
    assert_eq!(code, 0);
    assert_eq!(value["type"], "action");
    assert!(
        value["next_actions"]
            .as_array()
            .is_some_and(|actions| !actions.is_empty())
    );
}

#[test]
fn net_daemon_help_exposes_ca_flags() {
    // The detached daemon is spawned with individual flags; the CA must be one of
    // them (regression guard for the parent→daemon CA threading).
    let (out, code) = run(&["net", "daemon", "--help"]);
    assert_eq!(code, 0, "--help exits 0");
    assert!(
        out.contains("--ca-cert"),
        "net daemon should accept --ca-cert:\n{out}"
    );
    assert!(
        out.contains("--ca-key"),
        "net daemon should accept --ca-key:\n{out}"
    );
}
