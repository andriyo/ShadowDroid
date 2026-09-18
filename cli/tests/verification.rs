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

#[test]
fn external_test_fixture() {
    if std::fs::read_to_string("fixture-mode.txt").is_ok_and(|mode| mode == "backdated") {
        std::fs::write(
            "tests.xml",
            "<testsuite><testcase name=\"state\"/></testsuite>",
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open("tests.xml")
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
    }
    if std::fs::read_to_string("fixture-mode.txt").is_ok_and(|mode| mode == "directory") {
        std::fs::write(
            "reports/device/tests.xml",
            "<testsuite><testcase name=\"state\"/></testsuite>",
        )
        .unwrap();
    }
    if std::fs::read_to_string("fixture-mode.txt").is_ok_and(|mode| mode == "sleep") {
        std::thread::sleep(std::time::Duration::from_secs(10));
    }
    if std::fs::read_to_string("fixture-mode.txt").is_ok_and(|mode| mode == "write") {
        std::fs::write(
            "tests.xml",
            "<testsuite><testcase name=\"state\"/></testsuite>",
        )
        .unwrap();
    }
}

fn host_plan(source: &Path, mode: &str) -> Value {
    std::fs::write(source.join("input.txt"), "candidate").unwrap();
    std::fs::write(source.join("fixture-mode.txt"), mode).unwrap();
    json!({"schema_version":1,"task":"keep state and include secondary route","inputs":["input.txt","fixture-mode.txt"],
        "requirements":[{"id":"state","text":"keep state","source":"task","checks":["unit"]},{"id":"route","text":"include secondary route","source":"task"}],
        "checks":[{"id":"unit","adapter":{"kind":"junit","argv":[std::env::current_exe().unwrap(),"--exact","external_test_fixture","--nocapture"],"cwd":".","timeout_ms":10000,"reports":["tests.xml"],"selection":"all"}}]})
}

#[test]
fn executable_ledger_preserves_omitted_requirements_and_detects_staleness_and_tampering() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let plan = host_plan(&source, "write");
    std::fs::write(source.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = temp.path().join("run");
    let (code, value) = run(
        &source,
        &[
            "verify",
            "run",
            "plan.json",
            "--host-only",
            "--out",
            output.to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0, "omitted route must not pass: {value}");
    assert_eq!(value["code"], "verification_unresolved", "{value}");
    let (_, report) = run(&source, &["verify", "report", output.to_str().unwrap()]);
    assert_eq!(report["check_statuses"]["unit"], "passed", "{report}");
    assert_eq!(report["requirements"][1]["status"], "untested");
    std::fs::write(source.join("input.txt"), "different candidate").unwrap();
    let (_, report) = run(&source, &["verify", "report", output.to_str().unwrap()]);
    assert_eq!(report["source_inputs_current"], false);
    assert_eq!(report["requirements"][0]["status"], "stale");
    std::fs::write(output.join("checks/unit/report-0.xml"), "tampered").unwrap();
    let (_, report) = run(&source, &["verify", "report", output.to_str().unwrap()]);
    assert_eq!(report["check_statuses"]["unit"], "blocked");
    assert!(!report["evidence_issues"].as_array().unwrap().is_empty());
}

#[test]
fn zero_exit_cannot_reuse_old_xml_and_failed_prerequisites_block_dependents() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let mut plan = host_plan(&source, "noop");
    std::fs::write(
        source.join("tests.xml"),
        "<testsuite><testcase name=\"old\"/></testsuite>",
    )
    .unwrap();
    let mut downstream = plan["checks"][0].clone();
    downstream["id"] = json!("dependent");
    downstream["depends_on"] = json!(["unit"]);
    plan["checks"].as_array_mut().unwrap().push(downstream);
    std::fs::write(source.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = temp.path().join("run");
    let (code, value) = run(
        &source,
        &[
            "verify",
            "run",
            "plan.json",
            "--host-only",
            "--out",
            output.to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0, "{value}");
    let (_, report) = run(&source, &["verify", "report", output.to_str().unwrap()]);
    assert_eq!(report["check_statuses"]["unit"], "stale", "{report}");
    assert_eq!(report["check_statuses"]["dependent"], "blocked");
}

#[test]
fn interrupted_build_requires_explicit_recovery_and_new_runs_can_pass() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let mut plan = host_plan(&source, "sleep");
    plan["requirements"].as_array_mut().unwrap().pop();
    plan["checks"][0]["adapter"]["timeout_ms"] = json!(100);
    std::fs::write(source.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let output = temp.path().join("interrupted");
    let next = temp.path().join("next");
    let args = [
        "verify",
        "run",
        "plan.json",
        "--host-only",
        "--out",
        output.to_str().unwrap(),
    ];
    let (code, value) = run(&source, &args);
    assert_ne!(code, 0, "{value}");
    assert_eq!(value["code"], "verification_outcome_unknown", "{value}");
    let (_, report) = run(&source, &["verify", "report", output.to_str().unwrap()]);
    assert_eq!(report["execution_complete"], false);
    let (_, value) = run(
        &source,
        &[
            "verify",
            "run",
            "plan.json",
            "--host-only",
            "--out",
            next.to_str().unwrap(),
        ],
    );
    assert_eq!(value["code"], "build_recovery_required", "{value}");
    assert_ne!(
        run(&source, &["verify", "recover", output.to_str().unwrap()]).0,
        0
    );
    assert_eq!(
        run(
            &source,
            &[
                "verify",
                "recover",
                output.to_str().unwrap(),
                "--external-workers-stopped"
            ]
        )
        .0,
        0
    );
    std::fs::write(source.join("fixture-mode.txt"), "write").unwrap();
    plan["checks"][0]["adapter"]["timeout_ms"] = json!(10000);
    std::fs::write(source.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let (code, value) = run(
        &source,
        &[
            "verify",
            "run",
            "plan.json",
            "--host-only",
            "--out",
            next.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        value["report"]["requirements_satisfied_at_run"], true,
        "{value}"
    );
}

#[test]
fn recursive_junit_directories_preserve_empty_selection_and_find_fresh_reports() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("reports/device/empty")).unwrap();
    let mut plan = host_plan(&source, "nothing");
    plan["requirements"] = json!([{"id":"state","text":"state","source":"task","checks":["unit"]}]);
    plan["checks"][0]["adapter"]["reports"] = json!(["reports"]);
    std::fs::write(source.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let out = temp.path().join("empty");
    let (code, _) = run(
        &source,
        &[
            "verify",
            "run",
            "plan.json",
            "--host-only",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0);
    let (_, report) = run(&source, &["verify", "report", out.to_str().unwrap()]);
    assert_eq!(report["check_statuses"]["unit"], "blocked", "{report}");
    // Missing reports must not quarantine a finished host process.
    std::fs::write(source.join("fixture-mode.txt"), "directory").unwrap();
    let out = temp.path().join("fresh");
    let (code, result) = run(
        &source,
        &[
            "verify",
            "run",
            "plan.json",
            "--host-only",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{result}");
}

#[test]
fn newly_copied_but_backdated_reports_remain_stale() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let mut plan = host_plan(&source, "backdated");
    plan["requirements"].as_array_mut().unwrap().pop();
    std::fs::write(source.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
    let out = temp.path().join("run");
    let (code, _) = run(
        &source,
        &[
            "verify",
            "run",
            "plan.json",
            "--host-only",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0);
    let (_, report) = run(&source, &["verify", "report", out.to_str().unwrap()]);
    assert_eq!(report["check_statuses"]["unit"], "stale", "{report}");
}
