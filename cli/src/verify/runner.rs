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
    #[serde(default)]
    pub session_context: Option<Value>,
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
                    } | Adapter::SourceConstraints { .. }
                        | Adapter::ResolvedDependencies { .. }
                        | Adapter::VisualComparison { .. }
                ),
                "--host-only cannot run device/instrumentation checks"
            );
        }
    }
    let inputs = provenance::snapshot(&root, &plan.tracked_inputs())?;
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
        session_context: crate::runtime::evidence_context(),
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
    let mut built_packages: BTreeMap<String, (String, String)> = BTreeMap::new();
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
        let package = match &check.adapter {
            Adapter::Journey { journey } | Adapter::Matrix { journey, .. } => {
                Some(journey.package.as_str())
            }
            Adapter::Sqlite { package, .. } => Some(package.as_str()),
            Adapter::PlatformTest { test } => Some(test.package.as_str()),
            _ => None,
        };
        let before_app = if let (Some(package), Some(serial)) = (package, serial) {
            super::build::installed_apks(serial, package).await
        } else {
            Ok(Vec::new())
        };
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
        } else if let Err(error) = &before_app {
            CheckResult {
                id: check.id.clone(),
                status: Status::Blocked,
                started_ms: start,
                finished_ms: crate::runtime::now_ms(),
                outcome_unknown: false,
                evidence: json!({"reason":"installed_apk_identity_unavailable","error":format!("{error:#}")}),
                artifacts: BTreeMap::new(),
            }
        } else {
            match &check.adapter {
                Adapter::PlatformTest { test } => {
                    let execution = execute_platform(
                        test,
                        &root,
                        &dir,
                        serial.context("platform checks require a device")?,
                        &mut manifest.cleanup_errors,
                    )
                    .await;
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::VisualComparison { comparison } => {
                    let execution=super::visual::run(&root,&out,&dir,comparison).or_else(|e|Ok((Status::Blocked,json!({"reason":"visual_evidence_unavailable","error":format!("{e:#}")}),false,false)));
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::Connect { server_apk } => {
                    let apk = server_apk.as_deref().map(|p| root.join(p));
                    let execution = super::build::connect(
                        serial.context("connect needs a device")?,
                        apk.as_deref(),
                    )
                    .await;
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::BuildInstall { build } => {
                    let execution = super::build::run(
                        build,
                        &root,
                        serial.context("build/install needs a device")?,
                        &dir,
                    )
                    .await;
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::SourceConstraints { rules } => {
                    let execution=super::constraints::source(&root,rules).or_else(|e|Ok((Status::Blocked,json!({"reason":"source_constraint_unavailable","error":format!("{e:#}")}),false,false)));
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::ResolvedDependencies { dependencies } => {
                    let execution =
                        super::constraints::dependencies(&root, dependencies, &dir).await;
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::Matrix {
                    journey,
                    cells,
                    reset_app_data,
                } => {
                    let execution = super::journey::matrix(
                        journey,
                        cells,
                        *reset_app_data,
                        serial.context("matrix requires a device")?,
                        &dir,
                        &mut manifest.cleanup_errors,
                    )
                    .await;
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::Sqlite {
                    package,
                    database,
                    query,
                } => {
                    let execution = super::sqlite::run(
                        serial.context("SQLite snapshot requires a device")?,
                        package,
                        database,
                        query,
                        &dir,
                    )
                    .await;
                    from_execution(&check.id, start, execution, &mut interrupted)
                }
                Adapter::Journey { journey } => {
                    let execution = super::journey::run(
                        journey,
                        serial.context("journey requires a device")?,
                        &dir,
                        &mut manifest.cleanup_errors,
                    )
                    .await;
                    match execution {
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
                            evidence: json!({"reason":"journey_error","error":format!("{e:#}")}),
                            artifacts: BTreeMap::new(),
                        },
                    }
                }
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
        if let (Some(package), Some(serial), Ok(before)) = (package, serial, &before_app) {
            let after = super::build::installed_apks(serial, package).await;
            let stable = after
                .as_ref()
                .is_ok_and(|after| super::build::same_installed_apks(before, after));
            let bound = built_packages.get(package).is_some_and(|(id, hash)| {
                before.len() == 1
                    && before[0]["blake3"] == *hash
                    && manifest.plan.is_ancestor(id, &check.id)
            }) && stable;
            result.evidence["installed_apks_before"] = json!(before);
            result.evidence["installed_apks_after"] = json!(after.as_ref().ok());
            result.evidence["apk_identity_stable"] = json!(stable);
            result.evidence["bound_to_observed_build"] = json!(bound);
            if result.status == Status::Passed && !stable {
                result.status = Status::Stale;
            }
        }
        if result.status == Status::Passed
            && let Adapter::BuildInstall { build } = &check.adapter
            && let Some(hash) = result.evidence["apk_hash"].as_str()
        {
            built_packages.insert(build.package.clone(), (check.id.clone(), hash.into()));
        }
        hash_artifacts(&out, &dir, &mut result.artifacts)?;
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
        provenance::snapshot(&root, &manifest.plan.tracked_inputs())? != manifest.inputs;
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
    let declared = args
        .reports
        .iter()
        .map(|p| args.cwd.join(p))
        .collect::<Vec<_>>();
    let previous = report_paths(&declared)?
        .into_iter()
        .map(|p| {
            let value = stamp(&p).ok();
            (p, value)
        })
        .collect::<BTreeMap<_, _>>();
    if args.instrumentation {
        let serial = serial.context("instrumentation needs a selected device")?;
        let _guard = crate::device::installer::acquire_lifecycle_lock(serial)?;
        crate::cli::free_ui_automation_slot(serial).await?;
    }
    let started = process::filesystem_time(args.cwd)?;
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
    let paths = report_paths(&declared)?;
    let mut report = junit::TestReport::new(args.selection.to_owned());
    let mut stale = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let current = stamp(path).ok();
        if current
            .as_ref()
            .is_some_and(|(modified, _)| *modified < started)
            || (current.is_some() && Some(&current) == previous.get(path))
        {
            stale.push(path.display().to_string());
        }
        if path.is_file() {
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
        json!({"adapter":"junit","execution":execution,"report":report,"baseline_comparison":comparison,"stale_reports":stale,"freshness_clock":"filesystem_marker_in_command_cwd","instrumentation":args.instrumentation,"source_to_apk":"not_established_by_junit_alone"}),
        uncertain,
        interrupted,
    ))
}

async fn execute_platform(
    spec: &super::platform::PlatformTest,
    root: &Path,
    out: &Path,
    serial: &crate::ids::Serial,
    cleanup: &mut Vec<String>,
) -> Result<(Status, Value, bool, bool)> {
    let api = crate::device::adb::shell(serial, "getprop ro.build.version.sdk")
        .await?
        .trim()
        .parse::<u32>()?;
    if !(spec.min_api..=spec.max_api).contains(&api) {
        return Ok((
            Status::Blocked,
            json!({"reason":"outside_declared_api_support","api":api,"min_api":spec.min_api,"max_api":spec.max_api}),
            false,
            false,
        ));
    }
    let cwd = root.join(&spec.cwd);
    let args = JunitArgs {
        argv: &spec.argv,
        cwd: &cwd,
        timeout_ms: spec.timeout_ms,
        reports: &spec.reports,
        selection: &spec.selection,
        minimum_tests: spec.contracts.len(),
        instrumentation: true,
        baseline: None,
    };
    let (execution_status, mut evidence, unknown, interrupted) =
        execute_junit(&args, out, Some(serial), cleanup).await?;
    let report: junit::TestReport = serde_json::from_value(evidence["report"].clone())?;
    let (contract_status, contracts) = spec.evaluate(&report);
    evidence["adapter"] = json!("platform_test");
    evidence["api"] = json!(api);
    evidence["boundaries"] = contracts;
    Ok((
        reduce([execution_status, contract_status]),
        evidence,
        unknown,
        interrupted,
    ))
}

fn report_paths(declared: &[PathBuf]) -> Result<Vec<PathBuf>> {
    fn visit(
        path: &Path,
        depth: usize,
        files: &mut Vec<PathBuf>,
        visited: &mut usize,
    ) -> Result<()> {
        *visited += 1;
        anyhow::ensure!(
            *visited <= 5000,
            "JUnit report discovery exceeds directory bounds"
        );
        anyhow::ensure!(
            depth <= 8 && files.len() < 1000,
            "JUnit report discovery exceeds bounds"
        );
        anyhow::ensure!(!path.is_symlink(), "JUnit report symlinks are unsupported");
        if path.is_dir() {
            let mut entries = std::fs::read_dir(path)?
                .take(1001)
                .map(|e| e.map(|v| v.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            anyhow::ensure!(
                entries.len() <= 1000,
                "JUnit directory exceeds 1000 entries"
            );
            entries.sort();
            let before = files.len();
            for child in entries {
                if child.is_dir() || child.extension().is_some_and(|e| e == "xml") {
                    visit(&child, depth + 1, files, visited)?;
                }
            }
            if before == files.len() && depth == 0 {
                files.push(path.to_owned());
            } // Empty selections stay unresolved.
        } else {
            files.push(path.to_owned());
        }
        Ok(())
    }
    let mut paths = vec![];
    let mut visited = 0;
    for path in declared {
        visit(path, 0, &mut paths, &mut visited)?;
    }
    Ok(paths)
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

pub async fn recover(out: &Path, external_workers_stopped: bool) -> Result<()> {
    anyhow::ensure!(
        external_workers_stopped,
        "review/stop all external workers, then explicitly pass --external-workers-stopped; no action will be replayed"
    );
    let out = out.canonicalize()?;
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(out.join("manifest.json"))?)?;
    anyhow::ensure!(manifest.schema_version == 1, "unsupported run schema");
    manifest.plan.validate()?;
    let guard = crate::runtime::recover_build(&manifest.inputs.root, &out)?;
    let mut cleanup_errors = Vec::new();
    if let Some(serial) = &manifest.device {
        let serial = crate::ids::Serial::new(serial);
        crate::runtime::admit(&serial).await?;
        for check in &manifest.plan.checks {
            let dir = out.join("checks").join(&check.id);
            let mut paths = vec![dir.join("configuration-journal.json")];
            if let Adapter::Matrix { cells, .. } = &check.adapter {
                paths.extend(
                    cells
                        .iter()
                        .map(|cell| dir.join(&cell.id).join("configuration-journal.json")),
                );
            }
            for path in paths {
                if path.exists() {
                    let mut journal = super::configuration::Journal::read(&path)?;
                    cleanup_errors.extend(journal.restore(&serial, &path).await);
                }
            }
        }
    }
    anyhow::ensure!(
        cleanup_errors.is_empty(),
        "configuration recovery failed; ownership retained: {cleanup_errors:?}"
    );
    guard.complete()?;
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
    let mut build_bindings = BTreeMap::new();
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
                build_bindings.insert(
                    check.id.clone(),
                    result.evidence["bound_to_observed_build"] == true,
                );
                statuses.insert(check.id.clone(), result.status);
            }
            Err(e) => {
                statuses.insert(check.id.clone(), Status::Blocked);
                evidence_issues.push(json!({"check":check.id,"error":format!("{e:#}")}));
            }
        }
    }
    let current = provenance::snapshot(&manifest.inputs.root, &manifest.plan.tracked_inputs());
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
    let build_verified = manifest.plan.checks.iter().any(|c| {
        matches!(c.adapter, Adapter::BuildInstall { .. })
            && statuses.get(&c.id) == Some(&Status::Passed)
    });
    let app_checks_bound = manifest.plan.checks.iter().all(|c| match c.adapter {
        Adapter::Journey { .. }
        | Adapter::Matrix { .. }
        | Adapter::Sqlite { .. }
        | Adapter::PlatformTest { .. } => build_bindings.get(&c.id) == Some(&true),
        Adapter::Junit {
            instrumentation: true,
            ..
        } => false,
        _ => true,
    });
    let applicable = requirements
        .iter()
        .filter(|r| r["status"] != "not_applicable")
        .count();
    let passed = requirements
        .iter()
        .filter(|r| r["status"] == "passed")
        .count();
    Ok(
        json!({"schema_version":1,"execution_complete":manifest.execution_complete,"active_check":manifest.active_check,"requirements_satisfied_at_run":manifest.execution_complete && applicable>0 && passed==applicable && !stale && evidence_issues.is_empty(),"scope":if at_run {"run_end"}else{"historical_results_with_current_source_and_evidence_checks"},"current_edits_verified_at_run":build_verified && app_checks_bound && passed==applicable && applicable>0 && manifest.execution_complete && !stale && evidence_issues.is_empty(),"build_provenance_observed":build_verified,"session_context":manifest.session_context,"source_inputs_current":!stale,"device_state_currentness":if at_run {"observed_by_executed_checks"}else{"not_reobserved"},"applicable_requirements":applicable,"passed_requirements":passed,"excluded_requirements":requirements.len()-applicable,"requirements":requirements,"check_statuses":statuses,"cleanup_status":if !manifest.cleanup_errors.is_empty(){"failed"}else if !manifest.execution_complete{"unknown"}else{"complete"},"cleanup_errors":manifest.cleanup_errors,"evidence_issues":evidence_issues,"started_ms":manifest.started_ms,"finished_ms":manifest.finished_ms,"source_scope":manifest.inputs.source_scope,"limitations":["Plan completeness requires independent review against the original task.","External commands are not sandboxed; isolated worktrees, device selection and backend namespaces remain required.","A historical report does not prove the current installed APK or live device state."]}),
    )
}

pub(super) fn reduce(statuses: impl IntoIterator<Item = Status>) -> Status {
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

fn from_execution(
    id: &str,
    start: u64,
    execution: Result<(Status, Value, bool, bool)>,
    interrupted: &mut bool,
) -> CheckResult {
    match execution {
        Ok((status, evidence, unknown, was_interrupted)) => {
            *interrupted |= was_interrupted;
            CheckResult {
                id: id.into(),
                status,
                started_ms: start,
                finished_ms: crate::runtime::now_ms(),
                outcome_unknown: unknown,
                evidence,
                artifacts: BTreeMap::new(),
            }
        }
        Err(e) => CheckResult {
            id: id.into(),
            status: Status::Blocked,
            started_ms: start,
            finished_ms: crate::runtime::now_ms(),
            outcome_unknown: true,
            evidence: json!({"reason":"adapter_error","error":format!("{e:#}")}),
            artifacts: BTreeMap::new(),
        },
    }
}
fn hash_artifacts(root: &Path, dir: &Path, hashes: &mut BTreeMap<PathBuf, String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        anyhow::ensure!(!path.is_symlink(), "evidence symlinks are not supported");
        if path.is_dir() {
            hash_artifacts(root, &path, hashes)?;
        } else if path.is_file() {
            hashes.insert(
                path.strip_prefix(root)?.to_owned(),
                provenance::hash(&std::fs::read(path)?),
            );
        }
    }
    Ok(())
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
