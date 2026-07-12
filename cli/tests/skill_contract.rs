use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
        .arg("skill")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("USERPROFILE")
        .env("SHADOWDROID_QUIET", "1")
        .output()
        .expect("spawn shadowdroid skill")
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
        "skill install must emit one JSON line; stdout={stdout}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(lines[0]).expect("skill stdout line is JSON")
}

#[test]
fn user_scope_installs_real_skills_for_every_supported_agent() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    for (agent, relative) in [
        ("claude-code", ".claude/skills/shadowdroid/SKILL.md"),
        ("cursor", ".cursor/skills/shadowdroid/SKILL.md"),
        ("codex", ".agents/skills/shadowdroid/SKILL.md"),
        ("gemini", ".gemini/skills/shadowdroid/SKILL.md"),
        ("antigravity", ".gemini/config/skills/shadowdroid/SKILL.md"),
    ] {
        let output = run(&home, &project, &[agent, "--install"]);
        let value = one_json_line(&output);
        assert!(output.status.success(), "{agent}: {value}");
        assert_eq!(value["agent"], agent);
        assert_eq!(value["scope"], "user");

        let content = std::fs::read_to_string(home.join(relative)).unwrap();
        assert!(content.starts_with("---\nname: shadowdroid\ndescription: "));
        assert!(content.contains("# ShadowDroid"));
    }

    assert!(
        !project.join("AGENTS.md").exists(),
        "installing a Codex skill must not create AGENTS.md"
    );
}

#[test]
fn project_scope_uses_claude_native_and_shared_agent_skills_paths() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    for agent in ["codex", "cursor", "gemini", "antigravity"] {
        let output = run(&home, &project, &[agent, "--install", "--scope", "project"]);
        let value = one_json_line(&output);
        assert!(output.status.success(), "{agent}: {value}");
        assert_eq!(value["path"], ".agents/skills/shadowdroid/SKILL.md");
        assert_eq!(value["scope"], "project");
    }

    let output = run(
        &home,
        &project,
        &["claude-code", "--install", "--scope", "project"],
    );
    let value = one_json_line(&output);
    assert!(output.status.success(), "{value}");
    assert_eq!(value["path"], ".claude/skills/shadowdroid/SKILL.md");

    assert!(
        project
            .join(".agents/skills/shadowdroid/SKILL.md")
            .is_file()
    );
    assert!(
        project
            .join(".claude/skills/shadowdroid/SKILL.md")
            .is_file()
    );
    assert!(!project.join("AGENTS.md").exists());
}

#[test]
fn dry_run_prints_a_skill_md_for_codex() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    let output = run(&home, &project, &["codex"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("---\nname: shadowdroid\ndescription: "));
    assert!(!stdout.starts_with("# ShadowDroid — driving Android"));
    assert!(!project.join("AGENTS.md").exists());
}
