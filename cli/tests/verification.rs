//! Exercise the shipped CLI boundary, including malformed config independence.
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

fn run(cwd: &Path, args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
        .current_dir(cwd)
        .env("SHADOWDROID_QUIET", "1")
        .args(args)
        .output()
        .unwrap();
    let lines = String::from_utf8(out.stdout).unwrap();
    assert_eq!(lines.lines().count(), 1, "{lines}");
    (
        out.status.code().unwrap(),
        serde_json::from_str(&lines).unwrap(),
    )
}

#[test]
fn offline_verification_survives_bad_project_config_and_preserves_unmapped_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".shadowdroid")).unwrap();
    std::fs::write(dir.path().join(".shadowdroid/config.json"), "{invalid").unwrap();
    let plan = json!({"schema_version":1,"task":"Add a secondary route","requirements":[{"id":"route","text":"Add a secondary route","source":"task"}],"checks":[]});
    std::fs::write(
        dir.path().join("plan.json"),
        serde_json::to_vec(&plan).unwrap(),
    )
    .unwrap();
    let (code, result) = run(dir.path(), &["verify", "plan", "validate", "plan.json"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["unmapped_requirements"], json!(["route"]));
    assert_eq!(result["checks"], 0);
}

#[test]
fn missing_truncated_and_skipped_evidence_never_passes() {
    let dir = tempfile::tempdir().unwrap();
    for (name, xml) in [
        ("empty.xml", "<testsuite/>"),
        ("truncated.xml", "<testsuite><testcase name=\"x\"/>"),
        (
            "skipped.xml",
            "<testsuite><testcase name=\"x\"><skipped/></testcase></testsuite>",
        ),
    ] {
        std::fs::write(dir.path().join(name), xml).unwrap();
    }
    for file in ["missing.xml", "empty.xml", "truncated.xml", "skipped.xml"] {
        let (code, result) = run(
            dir.path(),
            &["verify", "junit", "--selection", "all", "--report", file],
        );
        assert_eq!(code, 0, "parser observation should be readable: {result}");
        assert_eq!(result["status"], "blocked", "{result}");
        assert_eq!(result["freshness"], "unknown");
    }
}

#[test]
fn baseline_comparison_preserves_failure_and_missing_tests() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("old.xml"),
        "<testsuite><testcase name=\"a\"/><testcase name=\"b\"/></testsuite>",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("new.xml"),
        "<testsuite><testcase name=\"a\"><failure message=\"regression\"/></testcase></testsuite>",
    )
    .unwrap();
    for (xml, out) in [("old.xml", "old.json"), ("new.xml", "new.json")] {
        assert_eq!(
            run(
                dir.path(),
                &[
                    "verify",
                    "junit",
                    "--selection",
                    "all",
                    "--report",
                    xml,
                    "--out",
                    out
                ]
            )
            .0,
            0
        );
    }
    let (code, result) = run(
        dir.path(),
        &[
            "verify",
            "compare",
            "--baseline",
            "old.json",
            "--candidate",
            "new.json",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(result["regressions"], 1);
    assert_eq!(result["unresolved"], true);
    assert_eq!(result["regression_free"], false);
}
