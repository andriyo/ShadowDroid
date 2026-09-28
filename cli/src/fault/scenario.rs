//! `fault run <scenario.json>`: a scripted chaos run.
//!
//! ```json
//! {"name": "offline checkout", "seed": 7, "steps": [
//!   {"run": ["app", "start", "com.example"]},
//!   {"fault": ["inject", "airplane-mode", "--duration-ms", "5000"]},
//!   {"run": ["ui", "tap", "--rid", "checkout"]},
//!   {"wait_ms": 1000},
//!   {"pick": [["fault", "inject", "process-death", "--app", "com.example"],
//!             ["fault", "inject", "low-memory", "--app", "com.example"]]},
//!   {"run": ["ui", "wait", "--text", "Offline", "--timeout-ms", "5000"]},
//!   {"run": ["ui", "tap", "--rid", "retry"], "allow_failure": true}
//! ]}
//! ```
//!
//! Each step is an ordinary `shadowdroid` command run as a child process
//! against the same device, so it takes the device lock itself; this runner
//! holds none. A step fails when its command exits non-zero (unless
//! `allow_failure`); the run stops there. Every fault the scenario injected
//! is cleared at the end, pass or fail.

use super::{Forward, args::RunArgs, error};
use crate::events::emit_result;
use crate::ids::Serial;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    seed: Option<u64>,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    #[serde(default)]
    run: Option<Vec<String>>,
    /// Shorthand for `run: ["fault", …]`.
    #[serde(default)]
    fault: Option<Vec<String>>,
    #[serde(default)]
    wait_ms: Option<u64>,
    /// Run one of these commands, chosen from the seed.
    #[serde(default)]
    pick: Option<Vec<Vec<String>>>,
    #[serde(default)]
    allow_failure: bool,
}

const PROXY_KINDS: [&str; 6] = [
    "http-errors",
    "http-latency",
    "bandwidth",
    "connection-reset",
    "truncated-response",
    "tls-failure",
];

/// Global flags a step may not set: the runner owns device selection.
const RESERVED: [&str; 6] = [
    "-d",
    "--device",
    "--target",
    "--authority-dir",
    "--takeover",
    "--session",
];

fn choose(seed: u64, index: usize, options: usize) -> usize {
    let hash = blake3::hash(format!("{seed}:{index}").as_bytes());
    (u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("8 bytes")) % options as u64)
        as usize
}

fn validate(scenario: &Scenario) -> Result<()> {
    if scenario.steps.is_empty() {
        return Err(error::scenario_invalid("the scenario has no steps"));
    }
    for (index, step) in scenario.steps.iter().enumerate() {
        let forms = [
            step.run.is_some(),
            step.fault.is_some(),
            step.wait_ms.is_some(),
            step.pick.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        if forms != 1 {
            return Err(error::scenario_invalid(format!(
                "step {index} must have exactly one of run, fault, wait_ms, pick"
            )));
        }
        let fault_argv: Option<Vec<String>> = step.fault.as_ref().map(|fault| {
            std::iter::once("fault".to_string())
                .chain(fault.iter().cloned())
                .collect()
        });
        let commands: Vec<&Vec<String>> = step
            .run
            .iter()
            .chain(fault_argv.iter())
            .chain(step.pick.iter().flatten())
            .collect();
        if step.pick.as_ref().is_some_and(Vec::is_empty) {
            return Err(error::scenario_invalid(format!(
                "step {index}: pick has no options"
            )));
        }
        for argv in commands {
            if argv.is_empty() {
                return Err(error::scenario_invalid(format!(
                    "step {index}: empty command"
                )));
            }
            if argv.first().is_some_and(|word| word == "fault")
                && argv.get(1).is_some_and(|word| word == "run")
            {
                return Err(error::scenario_invalid(format!(
                    "step {index}: scenarios cannot nest `fault run`"
                )));
            }
            if let Some(flag) = argv.iter().find(|arg| {
                RESERVED
                    .iter()
                    .any(|reserved| *arg == reserved || arg.starts_with(&format!("{reserved}=")))
            }) {
                return Err(error::scenario_invalid(format!(
                    "step {index}: {flag} is set by `fault run`, not by steps"
                )));
            }
        }
        if step.wait_ms.is_some_and(|ms| ms > 600_000) {
            return Err(error::scenario_invalid(format!(
                "step {index}: wait_ms is over 10 minutes"
            )));
        }
    }
    Ok(())
}

/// The command a step runs, with the scenario seed added to proxy faults
/// that don't set their own.
fn command_for(step: &Step, index: usize, seed: u64) -> Option<(String, Vec<String>)> {
    let (form, mut argv) = if let Some(run) = &step.run {
        ("run", run.clone())
    } else if let Some(fault) = &step.fault {
        (
            "fault",
            std::iter::once("fault".to_string())
                .chain(fault.iter().cloned())
                .collect(),
        )
    } else if let Some(options) = &step.pick {
        ("pick", options[choose(seed, index, options.len())].clone())
    } else {
        return None;
    };
    let injects_proxy_fault = argv.first().is_some_and(|w| w == "fault")
        && argv.get(1).is_some_and(|w| w == "inject")
        && argv
            .get(2)
            .is_some_and(|kind| PROXY_KINDS.contains(&kind.as_str()));
    if injects_proxy_fault
        && !argv
            .iter()
            .any(|arg| arg == "--seed" || arg.starts_with("--seed="))
    {
        argv.push("--seed".into());
        argv.push(seed.to_string());
    }
    Some((form.to_string(), argv))
}

struct StepRun {
    exit_code: i32,
    output: Value,
}

async fn run_child(serial: &str, forward: &Forward, argv: &[String]) -> Result<StepRun> {
    let exe = std::env::current_exe()?;
    let output = tokio::process::Command::new(exe)
        .args(forward.args())
        .args(["-d", serial])
        .args(argv)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("run scenario step")?;
    // Each command ends with one JSON object; streams (watch) are not steps.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or_else(|| json!({"stdout": stdout.chars().take(2_000).collect::<String>()}));
    Ok(StepRun {
        exit_code: output.status.code().unwrap_or(-1),
        output: last,
    })
}

pub async fn run(serial: &Serial, args: &RunArgs, forward: &Forward) -> Result<()> {
    let serial_str = serial.as_str();
    let text = std::fs::read_to_string(&args.file)
        .map_err(|e| error::scenario_invalid(format!("read {}: {e}", args.file.display())))?;
    let scenario: Scenario = serde_json::from_str(&text)
        .map_err(|e| error::scenario_invalid(format!("parse {}: {e}", args.file.display())))?;
    validate(&scenario)?;
    let seed = args.seed.or(scenario.seed).unwrap_or(0);
    let mut steps = Vec::new();
    let mut injected = Vec::<String>::new();
    let mut failed_step = None;
    for (index, step) in scenario.steps.iter().enumerate() {
        let started = std::time::Instant::now();
        if let Some(wait_ms) = step.wait_ms {
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            steps.push(json!({"index": index, "form": "wait", "wait_ms": wait_ms, "ok": true}));
            continue;
        }
        let (form, argv) = command_for(step, index, seed).expect("validated step");
        let ran = run_child(serial_str, forward, &argv).await?;
        let ok = ran.exit_code == 0;
        if ok
            && ran.output["cmd"] == "fault_inject"
            && ran.output["fault"]["state"] == "active"
            && let Some(id) = ran.output["fault"]["id"].as_str()
        {
            injected.push(id.to_string());
        }
        steps.push(json!({
            "index": index,
            "form": form,
            "argv": argv,
            "exit_code": ran.exit_code,
            "ok": ok,
            "allow_failure": step.allow_failure,
            "duration_ms": started.elapsed().as_millis() as u64,
            "output": ran.output,
        }));
        if !ok && !step.allow_failure {
            failed_step = Some(index);
            break;
        }
    }
    // Clear what this scenario injected (and nothing else); already-expired
    // faults are fine.
    let cleanup = if injected.is_empty() {
        json!({"cleared": []})
    } else {
        let argv: Vec<String> = ["fault", "clear"]
            .into_iter()
            .map(String::from)
            .chain(injected.iter().cloned())
            .chain(["--expired".to_string()])
            .collect();
        let ran = run_child(serial_str, forward, &argv).await?;
        json!({"exit_code": ran.exit_code, "ok": ran.exit_code == 0, "output": ran.output})
    };
    let cleanup_ok = cleanup.get("ok").is_none_or(|ok| ok == true);
    let result = json!({
        "type": "fault_scenario",
        "name": scenario.name,
        "file": args.file.display().to_string(),
        "device": serial_str,
        "seed": seed,
        "ok": failed_step.is_none() && cleanup_ok,
        "failed_step": failed_step,
        "steps": steps,
        "injected": injected,
        "cleanup": cleanup,
    });
    if failed_step.is_some() || !cleanup_ok {
        return Err(crate::diagnostic::DiagnosticError::new(
            "fault_scenario_failed",
            "fault",
            match failed_step {
                Some(index) => format!("scenario step {index} failed"),
                None => "the scenario's faults could not all be cleared".to_string(),
            },
        )
        .retryable(false)
        .detail(result)
        .next_actions([
            format!("shadowdroid -d {serial_str} why"),
            format!("shadowdroid -d {serial_str} fault list"),
        ])
        .into());
    }
    emit_result(&result);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Scenario {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn scenarios_are_validated_before_anything_runs() {
        assert!(validate(&parse(r#"{"steps": []}"#)).is_err());
        assert!(
            validate(&parse(
                r#"{"steps": [{"run": ["ui","dump"], "wait_ms": 5}]}"#
            ))
            .is_err()
        );
        assert!(validate(&parse(r#"{"steps": [{"run": ["-d","x","ui","dump"]}]}"#)).is_err());
        assert!(validate(&parse(r#"{"steps": [{"fault": ["run","x.json"]}]}"#)).is_err());
        assert!(validate(&parse(r#"{"steps": [{"pick": []}]}"#)).is_err());
        assert!(serde_json::from_str::<Scenario>(r#"{"steps": [{"sleep": 1}]}"#).is_err());
        assert!(
            validate(&parse(
                r#"{"steps": [{"run": ["ui","dump"]}, {"wait_ms": 10}]}"#
            ))
            .is_ok()
        );
    }

    #[test]
    fn picks_and_proxy_seeds_follow_the_scenario_seed() {
        let scenario = parse(
            r#"{"steps": [{"pick": [["a"],["b"],["c"],["d"]]},
                          {"fault": ["inject","http-errors","--percent","50"]},
                          {"fault": ["inject","http-errors","--seed","9"]}]}"#,
        );
        let pick = |seed| command_for(&scenario.steps[0], 0, seed).unwrap().1;
        assert_eq!(pick(3), pick(3));
        let spread: std::collections::BTreeSet<_> = (0..40).map(pick).collect();
        assert!(spread.len() > 1, "different seeds pick differently");
        let (form, argv) = command_for(&scenario.steps[1], 1, 42).unwrap();
        assert_eq!(form, "fault");
        assert_eq!(argv[0], "fault");
        assert_eq!(&argv[argv.len() - 2..], ["--seed", "42"]);
        let (_, argv) = command_for(&scenario.steps[2], 2, 42).unwrap();
        assert_eq!(argv.iter().filter(|arg| *arg == "--seed").count(), 1);
    }
}
