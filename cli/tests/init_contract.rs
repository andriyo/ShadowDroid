use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn run(home: &Path, cwd: &Path, missing_studio: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shadowdroid"));
    command
        .arg("init")
        .args(extra)
        .arg("--studio")
        .arg(missing_studio)
        .arg("--json")
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("USERPROFILE")
        .env("SHADOWDROID_QUIET", "1");
    command.output().expect("spawn shadowdroid init")
}

fn one_json_line(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "init must emit exactly one terminal JSON line; status={:?}; stdout={stdout}; stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_str(lines[0]).expect("init stdout line is JSON")
}

#[test]
fn json_success_with_skill_install_is_one_compact_terminal_action() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let missing_studio = temp.path().join("missing-studio");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    let output = run(&home, &project, &missing_studio, &["--no-studio-plugin"]);
    let value = one_json_line(&output);

    assert!(output.status.success(), "{value}");
    assert_eq!(value["type"], "action");
    assert_eq!(value["cmd"], "init");
    assert_eq!(value["ok"], true);
    assert_eq!(value["steps"]["studio_plugin"]["requested"], false);
    assert_eq!(value["steps"]["agent_skills"]["requested"], true);
    assert_eq!(value["steps"]["agent_skills"]["ok"], true);
    assert!(
        value["steps"]["agent_skills"]["report"]["installed"]
            .as_array()
            .unwrap()
            .len()
            >= 5
    );
    assert!(home.join(".agents/skills/shadowdroid/SKILL.md").is_file());
}

#[test]
fn plugin_failure_is_one_nonzero_typed_error_with_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let missing_studio = temp.path().join("missing-studio");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    let output = run(&home, &project, &missing_studio, &["--no-skills"]);
    let value = one_json_line(&output);

    assert!(!output.status.success(), "{value}");
    assert_eq!(value["type"], "error");
    assert_eq!(value["ok"], false);
    assert_eq!(value["stage"], "init");
    assert_eq!(value["code"], "init_failed");
    assert_eq!(
        value["detail"]["failed_steps"],
        serde_json::json!(["studio_plugin"])
    );
    assert!(
        value["detail"]["steps"]["studio_plugin"]["error"]
            .as_str()
            .unwrap()
            .contains("Android Studio was not detected")
    );
    assert!(
        value["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action.as_str().unwrap_or("").contains("--no-studio-plugin"))
    );
}

#[test]
fn skill_failure_is_one_nonzero_typed_error_with_full_report() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let missing_studio = temp.path().join("missing-studio");
    let claude_skill = home.join(".claude/skills/shadowdroid/SKILL.md");
    std::fs::create_dir_all(claude_skill.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(&claude_skill, "personal instructions\n").unwrap();

    let output = run(&home, &project, &missing_studio, &["--no-studio-plugin"]);
    let value = one_json_line(&output);

    assert!(!output.status.success(), "{value}");
    assert_eq!(value["code"], "init_failed");
    assert_eq!(
        value["detail"]["failed_steps"],
        serde_json::json!(["agent_skills"])
    );
    let failures = value["detail"]["steps"]["agent_skills"]["report"]["failed"]
        .as_array()
        .unwrap();
    assert!(failures.iter().any(|failure| {
        failure["agent"] == "claude-code"
            && failure["error"]
                .as_str()
                .unwrap_or("")
                .contains("refusing to overwrite untracked file")
    }));
    assert_eq!(
        std::fs::read_to_string(&claude_skill).unwrap(),
        "personal instructions\n"
    );
    assert!(
        value["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action
                .as_str()
                .unwrap_or("")
                .contains("review each destination"))
    );
}
