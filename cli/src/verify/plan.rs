//! Versioned, deliberately small verification vocabulary. Unknown fields are errors.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema_version: u32,
    /// Original task, retained so an independent reviewer can check plan completeness.
    pub task: String,
    pub requirements: Vec<Requirement>,
    pub checks: Vec<Check>,
    /// Source root relative to the plan file. Git inputs include dirty and untracked files.
    #[serde(default = "dot")]
    pub source_root: PathBuf,
    /// Explicit source/reference/fixture inputs. Empty input sets cannot establish freshness.
    #[serde(default)]
    pub inputs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub id: String,
    pub text: String,
    pub source: String,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub not_applicable: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub adapter: Adapter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Adapter {
    /// Runs the project's existing test framework; passing requires fresh JUnit evidence.
    Junit {
        argv: Vec<String>,
        cwd: PathBuf,
        timeout_ms: u64,
        reports: Vec<PathBuf>,
        selection: String,
        #[serde(default = "one")]
        minimum_tests: usize,
        /// Release the device's UiAutomation slot around this test command.
        #[serde(default)]
        instrumentation: bool,
        /// Optional normalized report from the same test selection.
        #[serde(default)]
        baseline: Option<PathBuf>,
    },
}

fn dot() -> PathBuf {
    PathBuf::from(".")
}

fn one() -> usize {
    1
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

impl Plan {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if bytes.len() > 4 * 1024 * 1024 {
            bail!("plan exceeds 4 MiB");
        }
        let plan: Self = serde_json::from_slice(&bytes).context("invalid verification plan")?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported plan schema_version: {}", self.schema_version);
        }
        if self.task.trim().is_empty() || self.requirements.is_empty() {
            bail!("task and at least one explicit requirement are required");
        }
        if self.checks.len() > 1000 || self.requirements.len() > 1000 {
            bail!("plan exceeds 1000 checks/requirements");
        }
        let mut ids = BTreeSet::new();
        for check in &self.checks {
            if !valid_id(&check.id) || !ids.insert(&check.id) {
                bail!("invalid or duplicate check ID: {}", check.id);
            }
            match &check.adapter {
                Adapter::Junit {
                    argv,
                    cwd,
                    timeout_ms,
                    reports,
                    selection,
                    minimum_tests,
                    ..
                } => {
                    if argv.is_empty()
                        || argv[0].is_empty()
                        || cwd.as_os_str().is_empty()
                        || !(1..=3_600_000).contains(timeout_ms)
                        || reports.is_empty()
                        || selection.trim().is_empty()
                        || *minimum_tests == 0
                    {
                        bail!(
                            "{}: junit needs argv, cwd, reports, selection, positive minimum_tests and timeout_ms in 1..3600000",
                            check.id
                        );
                    }
                    if reports.iter().collect::<BTreeSet<_>>().len() != reports.len() {
                        bail!("{}: duplicate report paths", check.id);
                    }
                }
            }
        }
        let mut requirements = BTreeSet::new();
        for requirement in &self.requirements {
            if !valid_id(&requirement.id)
                || !requirements.insert(&requirement.id)
                || requirement.text.trim().is_empty()
                || requirement.source.trim().is_empty()
            {
                bail!(
                    "invalid/duplicate requirement ID or missing source text: {}",
                    requirement.id
                );
            }
            if let Some(reason) = &requirement.not_applicable
                && (reason.trim().is_empty() || !requirement.checks.is_empty())
            {
                bail!(
                    "{}: exclusions need a reason and cannot also have checks",
                    requirement.id
                );
            }
            for id in &requirement.checks {
                if !ids.contains(id) {
                    bail!("{} references unknown check {id}", requirement.id);
                }
            }
        }
        self.execution_order()?;
        Ok(())
    }

    /// Stable topological ordering; never hide a dependency or implicitly discard a cycle.
    pub fn execution_order(&self) -> Result<Vec<usize>> {
        let indices: BTreeMap<_, _> = self
            .checks
            .iter()
            .enumerate()
            .map(|(i, c)| (&c.id, i))
            .collect();
        let mut done = BTreeSet::new();
        let mut order = Vec::new();
        while order.len() < self.checks.len() {
            let before = order.len();
            for (index, check) in self.checks.iter().enumerate() {
                if done.contains(&check.id) {
                    continue;
                }
                for dep in &check.depends_on {
                    if !indices.contains_key(dep) {
                        bail!("{} depends on unknown check {dep}", check.id);
                    }
                }
                if check.depends_on.iter().all(|id| done.contains(id)) {
                    done.insert(&check.id);
                    order.push(index);
                }
            }
            if before == order.len() {
                bail!("check dependencies contain a cycle");
            }
        }
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn omitted_requirement_is_retained_and_unknown_fields_rejected() {
        let json = r#"{"schema_version":1,"task":"task","requirements":[{"id":"r","text":"secondary route","source":"task"}],"checks":[]}"#;
        let plan: Plan = serde_json::from_str(json).unwrap();
        plan.validate().unwrap();
        assert!(plan.requirements[0].checks.is_empty());
        assert!(
            serde_json::from_str::<Plan>(
                &json.replace("\"checks\":[]", "\"checks\":[],\"passed\":true")
            )
            .is_err()
        );
    }
    #[test]
    fn cycles_and_unknown_references_rejected() {
        let mut plan = Plan {
            schema_version: 1,
            task: "t".into(),
            requirements: vec![Requirement {
                id: "r".into(),
                text: "t".into(),
                source: "s".into(),
                checks: vec![],
                not_applicable: None,
            }],
            inputs: vec![],
            source_root: dot(),
            checks: vec![],
        };
        let adapter = Adapter::Junit {
            argv: vec!["test".into()],
            cwd: ".".into(),
            timeout_ms: 1000,
            reports: vec!["a.xml".into()],
            selection: "all".into(),
            minimum_tests: 1,
            instrumentation: false,
            baseline: None,
        };
        plan.checks.push(Check {
            id: "a".into(),
            depends_on: vec!["b".into()],
            adapter: adapter.clone(),
        });
        assert!(plan.validate().is_err());
        plan.checks.push(Check {
            id: "b".into(),
            depends_on: vec!["a".into()],
            adapter,
        });
        assert!(plan.validate().is_err());
        plan.checks[1].depends_on.clear();
        assert_eq!(plan.execution_order().unwrap(), vec![1, 0]);
    }
}
