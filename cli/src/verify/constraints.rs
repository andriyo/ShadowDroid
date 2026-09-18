//! Task-specific source heuristics and actual Gradle resolution observations.
use super::{Status, process};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRules {
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub required_patterns: Vec<String>,
    #[serde(default)]
    pub forbidden_patterns: Vec<String>,
    #[serde(default)]
    pub forbidden_extensions: Vec<String>,
}
impl SourceRules {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.files.is_empty() && self.files.len() <= 1000,
            "source constraints need 1..1000 explicit files"
        );
        anyhow::ensure!(
            !self.required_patterns.is_empty()
                || !self.forbidden_patterns.is_empty()
                || !self.forbidden_extensions.is_empty(),
            "source constraints need an explicit rule"
        );
        for pattern in self
            .required_patterns
            .iter()
            .chain(&self.forbidden_patterns)
        {
            anyhow::ensure!(pattern.len() <= 4096, "constraint regex exceeds 4096 bytes");
            regex::Regex::new(pattern)?;
        }
        Ok(())
    }
}
pub fn source(root: &Path, rules: &SourceRules) -> Result<(Status, Value, bool, bool)> {
    rules.validate()?;
    let mut documents = vec![];
    let mut bytes = 0;
    for path in &rules.files {
        let full = root.join(path);
        let meta = std::fs::symlink_metadata(&full)?;
        anyhow::ensure!(
            meta.is_file() && !meta.is_symlink(),
            "constraint input must be a regular source file"
        );
        bytes += meta.len();
        anyhow::ensure!(
            bytes <= 16 * 1024 * 1024,
            "constraint source exceeds 16 MiB"
        );
        documents.push((path, std::fs::read_to_string(full)?));
    }
    let mut violations = vec![];
    for pattern in &rules.required_patterns {
        let regex = regex::Regex::new(pattern)?;
        if !documents.iter().any(|(_, text)| regex.is_match(text)) {
            violations.push(json!({"rule":"required_pattern_missing","pattern":pattern}));
        }
    }
    for (path, text) in &documents {
        if rules.forbidden_extensions.iter().any(|extension| {
            path.extension()
                .is_some_and(|ext| ext == extension.as_str())
        }) {
            violations.push(json!({"rule":"forbidden_extension","file":path}));
        }
        for pattern in &rules.forbidden_patterns {
            let regex = regex::Regex::new(pattern)?;
            for (index, line) in text
                .lines()
                .enumerate()
                .filter(|(_, line)| regex.is_match(line))
                .take(100)
            {
                violations.push(json!({"rule":"forbidden_pattern","pattern":pattern,"file":path,"line":index+1,"text":line}));
            }
        }
    }
    Ok((
        if violations.is_empty() {
            Status::Passed
        } else {
            Status::Failed
        },
        json!({"adapter":"source_constraints","scope":"heuristic_text_search_over_explicit_files","architecture_compliance":"not_proven","violations":violations,"files":rules.files}),
        false,
        false,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coordinate {
    pub group: String,
    pub name: String,
    pub version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependencies {
    /// Gradle executable and optional project-specific flags; task/init script are appended.
    pub argv: Vec<String>,
    pub module: String,
    pub configuration: String,
    pub timeout_ms: u64,
    pub required: Vec<Coordinate>,
    #[serde(default)]
    pub forbidden: Vec<Coordinate>,
}
impl Dependencies {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.argv.is_empty()
                && !self.argv[0].is_empty()
                && (1..=3_600_000).contains(&self.timeout_ms),
            "dependency resolution needs Gradle argv and a bounded timeout"
        );
        anyhow::ensure!(
            self.module.starts_with(':')
                && self
                    .module
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b":_-".contains(&b)),
            "module must be an absolute Gradle project path"
        );
        anyhow::ensure!(
            !self.configuration.is_empty()
                && self
                    .configuration
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid Gradle configuration"
        );
        anyhow::ensure!(
            !self.required.is_empty() || !self.forbidden.is_empty(),
            "declare dependency requirements"
        );
        Ok(())
    }
}
pub async fn dependencies(
    root: &Path,
    spec: &Dependencies,
    out: &Path,
) -> Result<(Status, Value, bool, bool)> {
    spec.validate()?;
    let script = format!(
        r#"gradle.projectsEvaluated {{
  def selected = gradle.rootProject.findProject('{}')
  if (selected == null) throw new GradleException('Selected project is missing')
  selected.tasks.register('shadowdroidVerifyDependencies') {{
    doLast {{
      def configuration = selected.configurations.getByName('{}')
      if (!configuration.canBeResolved) throw new GradleException('Configuration is not resolvable')
      def modules = configuration.incoming.resolutionResult.allComponents.findResults {{ component ->
        def id = component.id
        id instanceof org.gradle.api.artifacts.component.ModuleComponentIdentifier ? [group:id.group,name:id.module,version:id.version] : null
      }}
      println('SHADOWDROID_RESOLVED_V1=' + groovy.json.JsonOutput.toJson([module:selected.path,configuration:configuration.name,modules:modules]))
    }}
  }}
}}
"#,
        spec.module, spec.configuration
    );
    let path = out.join("resolution.init.gradle");
    std::fs::write(&path, script)?;
    let mut argv = spec.argv.clone();
    argv.extend([
        "--init-script".into(),
        path.display().to_string(),
        "--no-configuration-cache".into(),
        "--console=plain".into(),
        format!(
            "{}{}shadowdroidVerifyDependencies",
            spec.module,
            if spec.module == ":" { "" } else { ":" }
        ),
    ]);
    let execution = process::run(&argv, root, spec.timeout_ms, out, None).await?;
    let interrupted = execution.interrupted;
    let unknown = interrupted || execution.timed_out;
    if unknown || execution.exit_code != Some(0) {
        return Ok((
            Status::Blocked,
            json!({"execution":execution,"reason":"dependency_resolution_failed"}),
            unknown,
            interrupted,
        ));
    }
    let stdout = std::fs::read_to_string(out.join("stdout.log"))?;
    let records = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("SHADOWDROID_RESOLVED_V1="))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        records.len() == 1,
        "expected one complete resolved dependency graph"
    );
    let graph: Value = serde_json::from_str(records[0])?;
    anyhow::ensure!(
        graph["module"] == spec.module && graph["configuration"] == spec.configuration,
        "resolved graph selection mismatch"
    );
    let modules: Vec<Coordinate> = serde_json::from_value(
        graph
            .get("modules")
            .context("missing resolved modules")?
            .clone(),
    )?;
    let missing = spec
        .required
        .iter()
        .filter(|c| !modules.contains(c))
        .collect::<Vec<_>>();
    let forbidden = spec
        .forbidden
        .iter()
        .filter(|c| modules.contains(c))
        .collect::<Vec<_>>();
    Ok((
        if missing.is_empty() && forbidden.is_empty() {
            Status::Passed
        } else {
            Status::Failed
        },
        json!({"adapter":"resolved_dependencies","execution":execution,"graph":graph,"missing_required":missing,"present_forbidden":forbidden,"scope":"selected_gradle_configuration_not_every_build_variant"}),
        false,
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_specific_rules_find_locations_without_claiming_architecture() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Screen.kt"),
            "fun screen() {\n findViewById(42)\n}",
        )
        .unwrap();
        let rules = SourceRules {
            files: vec!["Screen.kt".into()],
            required_patterns: vec!["fun screen".into()],
            forbidden_patterns: vec!["findViewById".into()],
            forbidden_extensions: vec![],
        };
        let (status, evidence, _, _) = source(temp.path(), &rules).unwrap();
        assert_eq!(status, Status::Failed);
        assert_eq!(evidence["violations"][0]["line"], 2);
        std::fs::write(temp.path().join("Screen.kt"), "fun screen() {}").unwrap();
        assert_eq!(source(temp.path(), &rules).unwrap().0, Status::Passed);
    }
}
