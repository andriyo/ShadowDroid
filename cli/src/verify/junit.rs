//! JUnit is evidence only after a complete parse. Exit zero is never a test result.
use anyhow::{Context, Result, bail};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    pub id: String,
    pub suite: String,
    pub class: String,
    pub name: String,
    pub status: TestStatus,
    pub seconds: Option<f64>,
    pub file: Option<String>,
    pub line: Option<String>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReport {
    pub schema_version: u32,
    pub selection: String,
    pub tests: Vec<TestCase>,
    pub issues: Vec<String>,
}

fn attrs(e: &BytesStart<'_>) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for attr in e.attributes() {
        let attr = attr?;
        let name = std::str::from_utf8(attr.key.as_ref())?.to_owned();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
            .into_owned();
        result.insert(name, value);
    }
    Ok(result)
}

pub fn parse(bytes: &[u8]) -> Result<Vec<TestCase>> {
    if bytes.len() > 16 * 1024 * 1024 {
        bail!("JUnit report exceeds 16 MiB");
    }
    let mut reader = Reader::from_str(std::str::from_utf8(bytes)?);
    reader.config_mut().check_end_names = true;
    let mut stack: Vec<String> = Vec::new();
    let mut suites: Vec<(String, usize, BTreeMap<String, usize>)> = Vec::new();
    let mut tests: Vec<TestCase> = Vec::new();
    let mut current: Option<TestCase> = None;
    let mut root_seen = false;
    loop {
        let event = reader.read_event().context("malformed JUnit XML")?;
        match &event {
            Event::DocType(_) => bail!("JUnit DTDs are unsupported"),
            Event::Start(e) | Event::Empty(e) => {
                let name = std::str::from_utf8(e.name().as_ref())?.to_owned();
                let a = attrs(e)?;
                if stack.is_empty() {
                    if root_seen || !matches!(name.as_str(), "testsuite" | "testsuites") {
                        bail!("expected one testsuite/testsuites root");
                    }
                    root_seen = true;
                }
                if name == "testsuite" || name == "testsuites" {
                    if current.is_some() {
                        bail!("suite nested inside testcase");
                    }
                    let counts = ["tests", "failures", "errors", "skipped"]
                        .into_iter()
                        .filter_map(|key| a.get(key).map(|v| (key, v)))
                        .map(|(key, v)| {
                            Ok((
                                key.to_owned(),
                                v.parse::<usize>().context("invalid suite count")?,
                            ))
                        })
                        .collect::<Result<BTreeMap<_, _>>>()?;
                    suites.push((
                        a.get("name").cloned().unwrap_or_default(),
                        tests.len(),
                        counts,
                    ));
                }
                if name == "testcase" {
                    if current.is_some()
                        || suites.is_empty()
                        || stack.last().map(String::as_str) != Some("testsuite")
                    {
                        bail!("testcase must be a direct child of testsuite");
                    }
                    let test_name = a
                        .get("name")
                        .filter(|s| !s.is_empty())
                        .context("testcase lacks name")?
                        .clone();
                    let class = a.get("classname").cloned().unwrap_or_default();
                    let suite = suites
                        .iter()
                        .map(|(name, _, _)| name.as_str())
                        .filter(|n| !n.is_empty())
                        .collect::<Vec<_>>()
                        .join("/");
                    let seconds = a.get("time").map(|s| s.parse::<f64>()).transpose()?;
                    if seconds.is_some_and(|n| !n.is_finite() || n < 0.0) {
                        bail!("invalid testcase time");
                    }
                    current = Some(TestCase {
                        id: serde_json::to_string(&(&suite, &class, &test_name))?,
                        suite,
                        class,
                        name: test_name,
                        status: TestStatus::Passed,
                        seconds,
                        file: a.get("file").cloned(),
                        line: a.get("line").cloned(),
                        diagnostics: vec![],
                    });
                }
                if matches!(name.as_str(), "failure" | "error" | "skipped") {
                    let c = current.as_mut().context("test outcome outside testcase")?;
                    if stack.last().map(String::as_str) != Some("testcase") {
                        bail!("misplaced test outcome");
                    }
                    c.status = match (c.status, name.as_str()) {
                        (TestStatus::Error, _) | (_, "error") => TestStatus::Error,
                        (TestStatus::Failed, _) | (_, "failure") => TestStatus::Failed,
                        _ => TestStatus::Skipped,
                    };
                    if let Some(message) = a.get("message") {
                        c.diagnostics.push(message.clone());
                    }
                }
                stack.push(name);
            }
            Event::Text(e) if stack.is_empty() => {
                if !e.decode()?.trim().is_empty() {
                    bail!("text outside JUnit root");
                }
            }
            Event::Text(e) => {
                if stack
                    .last()
                    .is_some_and(|s| matches!(s.as_str(), "failure" | "error" | "skipped"))
                    && let Some(c) = &mut current
                {
                    c.diagnostics.push(e.decode()?.into_owned());
                }
            }
            Event::CData(e) => {
                if stack
                    .last()
                    .is_some_and(|s| matches!(s.as_str(), "failure" | "error" | "skipped"))
                    && let Some(c) = &mut current
                {
                    c.diagnostics.push(std::str::from_utf8(e)?.to_owned());
                }
            }
            Event::GeneralRef(e) => {
                let value =
                    quick_xml::escape::unescape(&format!("&{};", e.decode()?))?.into_owned();
                if stack
                    .last()
                    .is_some_and(|s| matches!(s.as_str(), "failure" | "error" | "skipped"))
                    && let Some(c) = &mut current
                {
                    c.diagnostics.push(value);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        if matches!(event, Event::Empty(_) | Event::End(_)) {
            let name = stack.pop().context("unexpected closing tag")?;
            if name == "testcase" {
                tests.push(current.take().context("unexpected testcase end")?);
            }
            if name == "testsuite" || name == "testsuites" {
                let (_, start, declared) = suites.pop().context("unexpected suite end")?;
                for (key, n) in declared {
                    let actual = tests[start..]
                        .iter()
                        .filter(|test| match key.as_str() {
                            "tests" => true,
                            "failures" => test.status == TestStatus::Failed,
                            "errors" => test.status == TestStatus::Error,
                            "skipped" => test.status == TestStatus::Skipped,
                            _ => false,
                        })
                        .count();
                    if n != actual {
                        bail!("suite declares {n} {key} but contains {actual}");
                    }
                }
            }
        }
    }
    if !root_seen || !stack.is_empty() || current.is_some() {
        bail!("empty or truncated JUnit report");
    }
    let mut ids = BTreeSet::new();
    for test in &tests {
        if !ids.insert(&test.id) {
            bail!("ambiguous duplicate test identity: {}", test.id);
        }
    }
    Ok(tests)
}

impl TestReport {
    pub fn new(selection: String) -> Self {
        Self {
            schema_version: 1,
            selection,
            tests: vec![],
            issues: vec![],
        }
    }
    pub fn add(&mut self, path: &Path) {
        match std::fs::read(path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| parse(&bytes))
        {
            Ok(tests) => self.tests.extend(tests),
            Err(error) => self.issues.push(format!("{}: {error:#}", path.display())),
        }
    }
    pub fn validate(&mut self, minimum: usize) {
        if self.tests.len() < minimum {
            self.issues.push(format!(
                "expected at least {minimum} tests; found {}",
                self.tests.len()
            ));
        }
        let mut ids = BTreeSet::new();
        for test in &self.tests {
            if !ids.insert(&test.id) {
                self.issues.push(format!(
                    "duplicate test identity across reports: {}",
                    test.id
                ));
            }
        }
    }
    pub fn status(&self) -> super::Status {
        if self
            .tests
            .iter()
            .any(|t| matches!(t.status, TestStatus::Failed | TestStatus::Error))
        {
            super::Status::Failed
        } else if !self.issues.is_empty()
            || self.tests.is_empty()
            || self.tests.iter().any(|t| t.status == TestStatus::Skipped)
        {
            super::Status::Blocked
        } else {
            super::Status::Passed
        }
    }
}

pub fn compare(baseline: &TestReport, candidate: &TestReport) -> serde_json::Value {
    let old: BTreeMap<_, _> = baseline.tests.iter().map(|t| (&t.id, t.status)).collect();
    let new: BTreeMap<_, _> = candidate.tests.iter().map(|t| (&t.id, t.status)).collect();
    let mut changes = Vec::new();
    let mut regressions = 0;
    let mut unresolved = !baseline.issues.is_empty()
        || !candidate.issues.is_empty()
        || baseline.selection != candidate.selection
        || old.is_empty()
        || new.is_empty();
    for id in old
        .keys()
        .chain(new.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let before = old.get(id).copied();
        let after = new.get(id).copied();
        let kind = match (before, after) {
            (Some(TestStatus::Passed), Some(TestStatus::Failed | TestStatus::Error)) => {
                regressions += 1;
                "regression"
            }
            (_, None) => {
                unresolved = true;
                "missing"
            }
            (_, Some(TestStatus::Skipped)) => {
                unresolved = true;
                "skipped"
            }
            (Some(TestStatus::Failed | TestStatus::Error), Some(TestStatus::Passed)) => "fixed",
            (None, Some(_)) => "added",
            _ => "unchanged",
        };
        changes
            .push(serde_json::json!({"id":id,"baseline":before,"candidate":after,"change":kind}));
    }
    serde_json::json!({"schema_version":1,"selection_matches":baseline.selection == candidate.selection,"regressions":regressions,"unresolved":unresolved,"regression_free":regressions==0 && !unresolved,"changes":changes})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outcomes_and_entities_are_not_lost() {
        let cases=parse(br#"<testsuites tests="3"><testsuite name="s" tests="3"><testcase name="ok"/><testcase name="bad"><failure message="wrong &amp; worse">trace</failure></testcase><testcase name="skip"><skipped/></testcase></testsuite></testsuites>"#).unwrap();
        assert_eq!(cases[1].status, TestStatus::Failed);
        assert_eq!(cases[1].diagnostics[0], "wrong & worse");
        assert_eq!(cases[2].status, TestStatus::Skipped);
    }
    #[test]
    fn no_partial_success_on_bad_evidence() {
        for xml in [
            "",
            "<testsuite><testcase name=\"x\"/>",
            "<testsuite tests=\"3\"><testcase name=\"x\"/></testsuite>",
            "<testsuite><testcase name=\"x\"/><testcase name=\"x\"/></testsuite>",
            "<testsuite/><testsuite/>",
            "<testsuite failures=\"1\"><testcase name=\"x\"/></testsuite>",
            "<!DOCTYPE testsuite><testsuite/>",
        ] {
            assert!(parse(xml.as_bytes()).is_err(), "{xml}");
        }
        let mut empty = TestReport::new("all".into());
        empty.validate(1);
        assert_eq!(empty.status(), super::super::Status::Blocked);
    }
    #[test]
    fn missing_test_cannot_improve_baseline() {
        let mut old = TestReport::new("all".into());
        old.tests=parse(br#"<testsuite><testcase name="x"/><testcase name="y"><failure/></testcase></testsuite>"#).unwrap();
        let mut new = old.clone();
        new.tests.pop();
        let report = compare(&old, &new);
        assert_eq!(report["regression_free"], false);
        assert_eq!(report["unresolved"], true);
    }
}
