//! A restart-readable ledger. Every check owns immutable evidence; the manifest is atomic.
use super::{
    Status, junit,
    plan::{Adapter, Plan},
    process,
    provenance::{self, Inputs},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub id: String,
    pub status: Status,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub outcome_unknown: bool,
    pub evidence: Value,
    pub artifacts: BTreeMap<PathBuf, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResultRef {
    path: PathBuf,
    hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub tool_version: String,
    pub plan_path: PathBuf,
    pub plan_hash: String,
    pub plan: Plan,
    pub inputs: Inputs,
    pub source_changed_during_run: bool,
    pub device: Option<String>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub execution_complete: bool,
    pub active_check: Option<String>,
    pub cleanup_errors: Vec<String>,
    checks: BTreeMap<String, ResultRef>,
}

fn save_manifest(out: &Path, manifest: &Manifest) -> Result<()> {
    crate::cmd::artifact::write_json(&out.join("manifest.json"), &serde_json::to_value(manifest)?)?;
    Ok(())
}

fn immutable_json(path: &Path, value: &impl Serialize) -> Result<String> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("artifact parent")?)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map_err(|e| e.error)
        .context("refusing to overwrite immutable verification evidence")?;
    Ok(provenance::hash(&std::fs::read(path)?))
}

fn event(out: &Path, value: Value) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join("events.jsonl"))?;
    serde_json::to_writer(&mut file, &value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub async fn run(plan_path: &Path, out: &Path, serial: Option<&crate::ids::Serial>) -> Result<()> {
    let plan_path = plan_path.canonicalize()?;
    let plan = Plan::read(&plan_path)?;
    let base = plan_path.parent().context("plan parent")?;
    let root = base.join(&plan.source_root).canonicalize()?;
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let out = parent
        .canonicalize()?
        .join(out.file_name().context("run directory name")?);
    anyhow::ensure!(
        !out.starts_with(&root),
        "run evidence must be outside the source root so it cannot invalidate source fingerprints"
    );
    // Validate every adapter before making changes or creating a partial run.
    if serial.is_none() {
        for check in &plan.checks {
            anyhow::ensure!(
                matches!(
                    &check.adapter,
                    Adapter::Junit {
                        instrumentation: false,
                        ..
                    }
                ),
                "--host-only cannot run device/instrumentation checks"
            );
        }
    }
    let inputs = provenance::snapshot(&root, &plan.inputs)?;
    anyhow::ensure!(
        !inputs.files.is_empty() && !inputs.files.values().any(|v| v == "missing"),
        "verification needs present source/fixture inputs; use a Git source root or explicit inputs"
    );
    let source_lock = crate::runtime::build_lock(&root)?;
    std::fs::create_dir(&out).context("run output must be a new directory")?;
    crate::video::paths::protect_dir(&out)?;
    std::fs::create_dir(out.join("checks"))?;
    immutable_json(&out.join("plan.json"), &plan)?;
    let mut manifest = Manifest {
        schema_version: 1,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        plan_hash: provenance::json_hash(&plan)?,
        plan_path,
        plan,
        inputs,
        source_changed_during_run: false,
        device: serial.map(ToString::to_string),
        started_ms: crate::runtime::now_ms(),
        finished_ms: None,
        execution_complete: false,
        active_check: None,
        cleanup_errors: vec![],
        checks: BTreeMap::new(),
    };
    save_manifest(&out, &manifest)?;
    event(
        &out,
        json!({"event":"run_started","at_ms":manifest.started_ms,"plan_hash":manifest.plan_hash}),
    )?;
    source_lock.begin(&out)?;
    let mut statuses = BTreeMap::new();
    let mut unknown = false;
    let mut interrupted = false;
    for index in manifest.plan.execution_order()? {
        let check = &manifest.plan.checks[index];
        let start = crate::runtime::now_ms();
        manifest.active_check = Some(check.id.clone());
        save_manifest(&out, &manifest)?;
        event(
            &out,
            json!({"event":"check_started","id":check.id,"at_ms":start}),
        )?;
        let dir = out.join("checks").join(&check.id);
        std::fs::create_dir(&dir)?;
        let unmet = check
            .depends_on
            .iter()
            .filter(|id| statuses.get(*id) != Some(&Status::Passed))
            .cloned()
            .collect::<Vec<_>>();
        let mut result = if unknown || interrupted || !unmet.is_empty() {
            CheckResult {
                id: check.id.clone(),
                status: Status::Blocked,
                started_ms: start,
                finished_ms: start,
                outcome_unknown: false,
                evidence: json!({"reason":"dependency_or_execution_blocked","unmet_dependencies":unmet,"earlier_outcome_unknown":unknown,"interrupted":interrupted}),
                artifacts: BTreeMap::new(),
            }
        } else {
            match &check.adapter {
                Adapter::Junit {
                    argv,
                    cwd,
                    timeout_ms,
                    reports,
                    selection,
                    minimum_tests,
                    instrumentation,
                    baseline,
                } => {
                    let cwd = root.join(cwd);
                    let arguments = JunitArgs {
                        argv,
                        cwd: &cwd,
                        timeout_ms: *timeout_ms,
                        reports,
                        selection,
                        minimum_tests: *minimum_tests,
                        instrumentation: *instrumentation,
                        baseline: baseline.as_deref().map(|p| root.join(p)),
                    };
                    match execute_junit(&arguments, &dir, serial, &mut manifest.cleanup_errors)
                        .await
                    {
                        Ok((status, evidence, uncertain, was_interrupted)) => {
                            interrupted |= was_interrupted;
                            CheckResult {
                                id: check.id.clone(),
                                status,
                                started_ms: start,
                                finished_ms: crate::runtime::now_ms(),
                                outcome_unknown: uncertain,
                                evidence,
                                artifacts: BTreeMap::new(),
                            }
                        }
                        Err(e) => CheckResult {
                            id: check.id.clone(),
                            status: Status::Blocked,
                            started_ms: start,
                            finished_ms: crate::runtime::now_ms(),
                            outcome_unknown: true,
                            evidence: json!({"reason":"execution_or_evidence_error","error":format!("{e:#}")}),
                            artifacts: BTreeMap::new(),
                        },
                    }
                }
            }
        };
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_file() {
                result.artifacts.insert(
                    path.strip_prefix(&out)?.to_owned(),
                    provenance::hash(&std::fs::read(&path)?),
                );
            }
        }
        unknown |= result.outcome_unknown;
        statuses.insert(check.id.clone(), result.status);
        let path = PathBuf::from("checks").join(&check.id).join("result.json");
        let hash = immutable_json(&out.join(&path), &result)?;
        manifest
            .checks
            .insert(check.id.clone(), ResultRef { path, hash });
        event(
            &out,
            json!({"event":"check_finished","id":check.id,"status":result.status,"at_ms":result.finished_ms,"outcome_unknown":result.outcome_unknown}),
        )?;
        manifest.active_check = None;
        save_manifest(&out, &manifest)?;
    }
    manifest.source_changed_during_run =
        provenance::snapshot(&root, &manifest.plan.inputs)? != manifest.inputs;
    manifest.finished_ms = Some(crate::runtime::now_ms());
    manifest.execution_complete = !unknown && !interrupted;
    save_manifest(&out, &manifest)?;
    if !unknown && !interrupted {
        source_lock.complete()?;
    }
    let report = report_value(&out, &manifest, true)?;
    immutable_json(&out.join("report.json"), &report)?;
    event(
        &out,
        json!({"event":"run_finished","at_ms":manifest.finished_ms,"execution_complete":manifest.execution_complete,"requirements_satisfied_at_run":report["requirements_satisfied_at_run"]}),
    )?;
    if report["requirements_satisfied_at_run"] != true
        || !manifest.cleanup_errors.is_empty()
        || unknown
        || interrupted
    {
        return Err(crate::diagnostic::DiagnosticError::new(
            if unknown || interrupted {
                "verification_outcome_unknown"
            } else {
                "verification_unresolved"
            },
            "verify",
            "verification has failed, unresolved or stale requirements",
        )
        .detail(json!({"run":out,"report":report}))
        .next_actions([
            format!(
                "shadowdroid verify report {}",
                crate::events::shell_token(&out.display().to_string())
            ),
            "inspect check evidence before rerunning; never replay an unknown action automatically"
                .into(),
        ])
        .process_exit_code(if interrupted { 130 } else { 1 })
        .into());
    }
    crate::events::emit_action("verify_run", &json!({"run":out,"report":report}));
    Ok(())
}

struct JunitArgs<'a> {
    argv: &'a [String],
    cwd: &'a Path,
    timeout_ms: u64,
    reports: &'a [PathBuf],
    selection: &'a str,
    minimum_tests: usize,
    instrumentation: bool,
    baseline: Option<PathBuf>,
}

async fn execute_junit(
    args: &JunitArgs<'_>,
    out: &Path,
    serial: Option<&crate::ids::Serial>,
    cleanup_errors: &mut Vec<String>,
) -> Result<(Status, Value, bool, bool)> {
    let paths = args
        .reports
        .iter()
        .map(|p| args.cwd.join(p))
        .collect::<Vec<_>>();
    let previous = paths.iter().map(|p| stamp(p).ok()).collect::<Vec<_>>();
    if args.instrumentation {
        let serial = serial.context("instrumentation needs a selected device")?;
        let _guard = crate::device::installer::acquire_lifecycle_lock(serial)?;
        crate::cli::free_ui_automation_slot(serial).await?;
    }
    let started = SystemTime::now();
    let execution = process::run(args.argv, args.cwd, args.timeout_ms, out, serial).await;
    if args.instrumentation {
        let serial = serial.unwrap();
        let cleanup = async {
            let _guard = crate::device::installer::acquire_lifecycle_lock(serial)?;
            crate::cli::free_ui_automation_slot(serial).await
        }
        .await;
        if let Err(e) = cleanup {
            cleanup_errors.push(format!("instrumentation cleanup: {e:#}"));
        }
    }
    let execution = execution?;
    let mut report = junit::TestReport::new(args.selection.to_owned());
    let mut stale = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let current = stamp(path).ok();
        if current
            .as_ref()
            .is_some_and(|(modified, _)| *modified < started)
            || (current.is_some() && current == previous[index])
        {
            stale.push(path.display().to_string());
        }
        if path.exists() {
            anyhow::ensure!(
                std::fs::metadata(path)?.len() <= 16 * 1024 * 1024,
                "JUnit XML exceeds 16 MiB"
            );
            let bytes = std::fs::read(path)?;
            let text = String::from_utf8_lossy(&bytes);
            crate::cmd::artifact::write_bytes(
                &out.join(format!("report-{index}.xml")),
                crate::redaction::redact_text_if_active(&text).as_bytes(),
            )?;
        }
        report.add(path);
    }
    report.validate(args.minimum_tests);
    let comparison = if let Some(path) = &args.baseline {
        let mut baseline: junit::TestReport = serde_json::from_slice(&std::fs::read(path)?)?;
        anyhow::ensure!(baseline.schema_version == 1, "unsupported baseline schema");
        baseline.validate(1);
        Some(junit::compare(&baseline, &report))
    } else {
        None
    };
    let status = if execution.interrupted || execution.timed_out {
        Status::Blocked
    } else if !stale.is_empty() {
        Status::Stale
    } else if execution.exit_code != Some(0) {
        if report.status() == Status::Failed {
            Status::Failed
        } else {
            Status::Blocked
        }
    } else if comparison
        .as_ref()
        .is_some_and(|c| c["regression_free"] != true)
    {
        Status::Blocked
    } else {
        report.status()
    };
    immutable_json(&out.join("junit.json"), &report)?;
    let interrupted = execution.interrupted;
    let uncertain = interrupted || execution.timed_out || !cleanup_errors.is_empty();
    Ok((
        status,
        json!({"adapter":"junit","execution":execution,"report":report,"baseline_comparison":comparison,"stale_reports":stale,"instrumentation":args.instrumentation,"source_to_apk":"not_established_by_junit_alone"}),
        uncertain,
        interrupted,
    ))
}

fn stamp(path: &Path) -> Result<(SystemTime, String)> {
    let metadata = std::fs::metadata(path)?;
    anyhow::ensure!(
        metadata.len() <= 16 * 1024 * 1024,
        "JUnit XML exceeds 16 MiB"
    );
    Ok((
        metadata.modified()?,
        provenance::hash(&std::fs::read(path)?),
    ))
}

pub fn recover(out: &Path, external_workers_stopped: bool) -> Result<()> {
    anyhow::ensure!(
        external_workers_stopped,
        "review/stop all external workers, then explicitly pass --external-workers-stopped; no action will be replayed"
    );
    let out = out.canonicalize()?;
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(out.join("manifest.json"))?)?;
    anyhow::ensure!(manifest.schema_version == 1, "unsupported run schema");
    crate::runtime::recover_build(&manifest.inputs.root, &out)?;
    event(
        &out,
        json!({"event":"external_workers_stopped_attested","at_ms":crate::runtime::now_ms(),"device_recovery_still_required":manifest.device.is_some()}),
    )?;
    crate::events::emit_action(
        "verify_recover",
        &json!({"run":out,"build_ownership":"released","external_workers_stopped_attested":true,"prior_outcomes":"unchanged","device_recovery_still_required":manifest.device.is_some(),"next_actions":["inspect session status for device recovery; start a new run after owned-state recovery"]}),
    );
    Ok(())
}

pub fn report(out: &Path) -> Result<()> {
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(out.join("manifest.json"))?)?;
    anyhow::ensure!(manifest.schema_version == 1, "unsupported run schema");
    crate::events::emit_action("verify_report", &report_value(out, &manifest, false)?);
    Ok(())
}

fn report_value(out: &Path, manifest: &Manifest, at_run: bool) -> Result<Value> {
    let mut statuses = BTreeMap::new();
    let mut evidence_issues = Vec::new();
    for check in &manifest.plan.checks {
        let Some(reference) = manifest.checks.get(&check.id) else {
            statuses.insert(check.id.clone(), Status::Untested);
            continue;
        };
        let read = || -> Result<CheckResult> {
            provenance::verify_artifact(out, &reference.path, &reference.hash)?;
            let result: CheckResult =
                serde_json::from_slice(&std::fs::read(out.join(&reference.path))?)?;
            anyhow::ensure!(result.id == check.id, "check identity mismatch");
            for (path, hash) in &result.artifacts {
                provenance::verify_artifact(out, path, hash)?;
            }
            Ok(result)
        };
        match read() {
            Ok(result) => {
                statuses.insert(check.id.clone(), result.status);
            }
            Err(e) => {
                statuses.insert(check.id.clone(), Status::Blocked);
                evidence_issues.push(json!({"check":check.id,"error":format!("{e:#}")}));
            }
        }
    }
    let current = provenance::snapshot(&manifest.inputs.root, &manifest.plan.inputs);
    let current_plan = Plan::read(&manifest.plan_path).and_then(|p| provenance::json_hash(&p));
    let stale = manifest.source_changed_during_run
        || current
            .as_ref()
            .map_or(true, |inputs| inputs != &manifest.inputs)
        || current_plan
            .as_ref()
            .map_or(true, |hash| hash != &manifest.plan_hash);
    let requirements=manifest.plan.requirements.iter().map(|requirement|{
        let status=if requirement.not_applicable.is_some(){Status::NotApplicable}
            else if requirement.checks.is_empty(){Status::Untested}
            else if stale {Status::Stale}
            else {reduce(requirement.checks.iter().map(|id|*statuses.get(id).unwrap_or(&Status::Untested)))};
        json!({"id":requirement.id,"text":requirement.text,"source":requirement.source,"status":status,"checks":requirement.checks,"not_applicable_reason":requirement.not_applicable})
    }).collect::<Vec<_>>();
    let applicable = requirements
        .iter()
        .filter(|r| r["status"] != "not_applicable")
        .count();
    let passed = requirements
        .iter()
        .filter(|r| r["status"] == "passed")
        .count();
    Ok(
        json!({"schema_version":1,"execution_complete":manifest.execution_complete,"active_check":manifest.active_check,"requirements_satisfied_at_run":manifest.execution_complete && applicable>0 && passed==applicable && !stale && evidence_issues.is_empty(),"scope":if at_run {"run_end"}else{"historical_results_with_current_source_and_evidence_checks"},"source_inputs_current":!stale,"device_state_currentness":if at_run {"observed_by_executed_checks"}else{"not_reobserved"},"applicable_requirements":applicable,"passed_requirements":passed,"excluded_requirements":requirements.len()-applicable,"requirements":requirements,"check_statuses":statuses,"cleanup_status":if !manifest.cleanup_errors.is_empty(){"failed"}else if !manifest.execution_complete{"unknown"}else{"complete"},"cleanup_errors":manifest.cleanup_errors,"evidence_issues":evidence_issues,"started_ms":manifest.started_ms,"finished_ms":manifest.finished_ms,"source_scope":manifest.inputs.source_scope,"limitations":["Plan completeness requires independent review against the original task.","External commands are not sandboxed; isolated worktrees, device selection and backend namespaces remain required.","A historical report does not prove the current installed APK or live device state."]}),
    )
}

pub fn reduce(statuses: impl IntoIterator<Item = Status>) -> Status {
    let values = statuses.into_iter().collect::<Vec<_>>();
    for priority in [
        Status::Failed,
        Status::Stale,
        Status::Blocked,
        Status::Untested,
    ] {
        if values.contains(&priority) {
            return priority;
        }
    }
    if !values.is_empty() && values.iter().all(|s| *s == Status::Passed) {
        Status::Passed
    } else {
        Status::Untested
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requirement_needs_all_current_checks() {
        assert_eq!(reduce([]), Status::Untested);
        assert_eq!(reduce([Status::Passed, Status::Untested]), Status::Untested);
        assert_eq!(reduce([Status::Passed, Status::Blocked]), Status::Blocked);
        assert_eq!(reduce([Status::Passed, Status::Stale]), Status::Stale);
        assert_eq!(reduce([Status::Passed, Status::Failed]), Status::Failed);
        assert_eq!(reduce([Status::Passed, Status::Passed]), Status::Passed);
    }
}
