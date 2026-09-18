//! Deterministic requirement verification and evidence, independent of a model provider.
pub mod junit;
pub mod plan;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Failed,
    Untested,
    Blocked,
    Stale,
    NotApplicable,
}

#[derive(Args)]
pub struct VerifyArgs {
    #[command(subcommand)]
    pub command: VerifyCmd,
}

#[derive(Subcommand)]
pub enum VerifyCmd {
    /// Validate requirement coverage, adapter inputs and dependency ordering offline.
    Plan {
        #[command(subcommand)]
        command: PlanCmd,
    },
    /// Parse saved JUnit reports. This is an observation, not proof of a fresh execution.
    Junit {
        /// XML reports; each expected report must be present and complete.
        #[arg(long, required = true)]
        report: Vec<PathBuf>,
        /// Stable description of the test selection, used for baseline comparisons.
        #[arg(long)]
        selection: String,
        /// Minimum expected number of test cases; empty suites never pass.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        minimum_tests: u32,
        /// Save the normalized report for later baseline comparison.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compare two normalized reports without silently discarding missing tests.
    Compare {
        #[arg(long)]
        baseline: PathBuf,
        #[arg(long)]
        candidate: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum PlanCmd {
    /// Validate schema version 1 without connecting to a device or running tests.
    Validate { plan: PathBuf },
}

pub fn run(args: &VerifyArgs) -> Result<()> {
    run_inner(args).map_err(|error| {
        crate::diagnostic::DiagnosticError::new(
            "verification_input_invalid",
            "verify",
            format!("{error:#}"),
        )
        .next_actions([
            "shadowdroid commands verify --json",
            "validate the plan and inspect the referenced evidence files",
        ])
        .into()
    })
}

fn run_inner(args: &VerifyArgs) -> Result<()> {
    match &args.command {
        VerifyCmd::Plan {
            command: PlanCmd::Validate { plan },
        } => {
            let p = plan::Plan::read(plan)?;
            crate::events::emit_action(
                "verify_plan_validate",
                &json!({
                    "schema_version":1, "valid":true, "requirements":p.requirements.len(), "checks":p.checks.len(),
                    "unmapped_requirements":p.requirements.iter().filter(|r|r.checks.is_empty() && r.not_applicable.is_none()).map(|r|&r.id).collect::<Vec<_>>(),
                    "excluded_requirements":p.requirements.iter().filter(|r|r.not_applicable.is_some()).map(|r|&r.id).collect::<Vec<_>>(),
                    "execution_order":p.execution_order()?.iter().map(|i|&p.checks[*i].id).collect::<Vec<_>>(),
                    "limitations":["Plan validity does not establish completeness against the original task.","Only explicitly declared inputs can establish freshness."]
                }),
            );
        }
        VerifyCmd::Junit {
            report,
            selection,
            minimum_tests,
            out,
        } => {
            let mut parsed = junit::TestReport::new(selection.clone());
            for path in report {
                parsed.add(path);
            }
            parsed.validate(*minimum_tests as usize);
            if let Some(path) = out {
                crate::cmd::artifact::write_json(path, &serde_json::to_value(&parsed)?)?;
            }
            crate::events::emit_action(
                "verify_junit",
                &json!({
                    "schema_version":1,"status":parsed.status(),"report":parsed,"artifact":out,
                    "freshness":"unknown", "execution":"not_observed",
                    "limitations":["Saved XML alone cannot prove which source or APK produced the results."]
                }),
            );
        }
        VerifyCmd::Compare {
            baseline,
            candidate,
        } => {
            let read = |path: &PathBuf| -> Result<junit::TestReport> {
                let mut report: junit::TestReport =
                    serde_json::from_slice(&std::fs::read(path)?)
                        .context("expected a normalized JUnit report")?;
                anyhow::ensure!(
                    report.schema_version == 1,
                    "unsupported JUnit report schema"
                );
                report.validate(1);
                Ok(report)
            };
            crate::events::emit_action(
                "verify_compare",
                &junit::compare(&read(baseline)?, &read(candidate)?),
            );
        }
    }
    Ok(())
}
