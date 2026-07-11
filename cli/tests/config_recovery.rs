use serde_json::Value;
use std::process::{Command, Output};

fn run(home: &std::path::Path, cwd: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("SHADOWDROID_QUIET", "1")
        .output()
        .unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}; status={:?}; stdout={}; stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

#[test]
fn malformed_config_does_not_block_config_recovery_commands() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(home.join(".shadowdroid")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(home.join(".shadowdroid/config.json"), "{\n  \"app\":\n}\n").unwrap();

    for (args, expected_type) in [
        (
            ["config", "schema", "--json"].as_slice(),
            "shadowdroid_config_schema",
        ),
        (
            ["config", "paths", "--json"].as_slice(),
            "shadowdroid_config_paths",
        ),
        (
            ["config", "explain", "--json"].as_slice(),
            "shadowdroid_config_explain",
        ),
    ] {
        let output = run(&home, &project, args);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(json(&output)["type"], expected_type);
    }

    let validate = run(&home, &project, &["config", "validate", "--json"]);
    assert_ne!(
        validate.status.code(),
        Some(2),
        "validate hit CLI usage parsing"
    );
    let validate_json = json(&validate);
    assert!(!validate.status.success());
    assert_eq!(validate_json["type"], "error");
    assert_eq!(validate_json["ok"], false);
    assert_eq!(validate_json["code"], "config_invalid");
    assert_eq!(validate_json["detail"]["files"][0]["ok"], false);
    assert_eq!(validate_json["detail"]["files"][0]["code"], "config_parse");
    assert_eq!(validate_json["detail"]["files"][0]["detail"]["line"], 3);
    assert!(validate_json["detail"]["errors"][0]
        .as_str()
        .unwrap()
        .contains(":3:"));

    // Normal commands still fail closed and preserve the typed parse location.
    let devices = run(&home, &project, &["devices"]);
    assert!(!devices.status.success());
    let error = json(&devices);
    assert_eq!(error["code"], "config_parse");
    assert_eq!(error["stage"], "config");
    assert_eq!(error["detail"]["line"], 3);
}
