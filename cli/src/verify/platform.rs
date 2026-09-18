//! Named framework assertions at Android boundaries; never infer absence from dumpsys.
use super::{
    Status,
    junit::{TestReport, TestStatus},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    OutboundIntent,
    MediaSession,
    WidgetUpdate,
    PictureInPicture,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observation {
    Actual,
    Stubbed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub boundary: Boundary,
    pub class: String,
    pub test: String,
    pub observation: Observation,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformTest {
    pub package: String,
    pub min_api: u32,
    pub max_api: u32,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub reports: Vec<PathBuf>,
    pub selection: String,
    pub contracts: Vec<Contract>,
}
impl PlatformTest {
    pub fn validate(&self) -> Result<()> {
        crate::config::validate_android_package(&self.package)?;
        anyhow::ensure!(
            (21..=100).contains(&self.min_api)
                && self.min_api <= self.max_api
                && self.max_api <= 100,
            "invalid declared API interval"
        );
        anyhow::ensure!(
            !self.argv.is_empty()
                && !self.argv[0].is_empty()
                && !self.cwd.as_os_str().is_empty()
                && !self.reports.is_empty()
                && !self.selection.trim().is_empty()
                && (1..=3_600_000).contains(&self.timeout_ms),
            "platform tests need explicit argv, cwd, reports, selection and bounded timeout"
        );
        anyhow::ensure!(
            !self.contracts.is_empty() && self.contracts.len() <= 100,
            "platform test requires 1..100 named contracts"
        );
        let mut ids = BTreeSet::new();
        for contract in &self.contracts {
            anyhow::ensure!(
                !contract.class.trim().is_empty()
                    && !contract.test.trim().is_empty()
                    && ids.insert((&contract.class, &contract.test)),
                "platform contract identities must be nonempty and unique"
            );
            anyhow::ensure!(
                matches!(contract.observation, Observation::Actual)
                    || matches!(contract.boundary, Boundary::OutboundIntent),
                "only intent contracts can declare stubbed observation"
            );
        }
        Ok(())
    }
    pub fn evaluate(&self, report: &TestReport) -> (Status, Value) {
        let mut statuses = vec![];
        let mut observations = vec![];
        for contract in &self.contracts {
            let tests = report
                .tests
                .iter()
                .filter(|t| t.class == contract.class && t.name == contract.test)
                .collect::<Vec<_>>();
            let status = if tests.len() != 1 {
                Status::Blocked
            } else {
                match tests[0].status {
                    TestStatus::Passed => Status::Passed,
                    TestStatus::Failed | TestStatus::Error => Status::Failed,
                    TestStatus::Skipped => Status::Blocked,
                }
            };
            statuses.push(status);
            observations.push(json!({"contract":contract,"status":status,"matching_cases":tests,"observation_source":"project_instrumentation_assertions"}));
        }
        (
            super::runner::reduce(statuses),
            json!({"contracts":observations,"scope":"explicit_named_framework_assertions","limitations":["Project test code determines observation completeness; inspect it independently.","Stubbed intents do not establish actual delivery.","MediaSession release does not establish every audio, decoder or hardware resource was released.","A widget host fixture does not cover every launcher.","Emulator PiP does not establish physical camera or Wear OS behavior."]}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unrelated_passing_tests_cannot_satisfy_a_boundary_contract() {
        let spec = PlatformTest {
            package: "example.app".into(),
            min_api: 29,
            max_api: 36,
            argv: vec!["gradlew".into()],
            cwd: ".".into(),
            timeout_ms: 1000,
            reports: vec!["reports".into()],
            selection: "boundary".into(),
            contracts: vec![Contract {
                boundary: Boundary::OutboundIntent,
                class: "Boundary".into(),
                test: "delivered".into(),
                observation: Observation::Actual,
            }],
        };
        let mut report = TestReport::new("boundary".into());
        report.tests = super::super::junit::parse(
            b"<testsuite><testcase classname='Boundary' name='other'/></testsuite>",
        )
        .unwrap();
        assert_eq!(spec.evaluate(&report).0, Status::Blocked);
        report.tests[0].name = "delivered".into();
        assert_eq!(spec.evaluate(&report).0, Status::Passed);
        report.tests[0].status = TestStatus::Skipped;
        assert_eq!(spec.evaluate(&report).0, Status::Blocked);
    }
}
