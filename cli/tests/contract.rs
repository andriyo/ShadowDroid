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
    assert!(value["next_actions"]
        .as_array()
        .is_some_and(|actions| !actions.is_empty()));
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
fn commands_json_is_one_valid_json_object() {
    let (out, code) = run(&["commands", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(out.trim()).expect("commands --json is valid JSON");
    assert!(v.is_object(), "catalog is a JSON object");
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
