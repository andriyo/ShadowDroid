//! Finite UI journeys with explicit destinations and observable lifecycle boundaries.
use super::{
    Status,
    configuration::{Configuration, Journal},
};
use crate::{
    device::{
        adb,
        client::{ActionGuard, ServerClient},
        installer,
    },
    ids::Serial,
    proto::{Element, SelectorQuery, SnapshotState},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journey {
    pub package: String,
    pub destinations: Vec<String>,
    pub steps: Vec<Step>,
    #[serde(default = "journey_timeout")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixCell {
    pub id: String,
    pub configuration: Configuration,
}

pub async fn matrix(
    journey: &Journey,
    cells: &[MatrixCell],
    reset_app_data: bool,
    serial: &Serial,
    out: &Path,
    cleanup: &mut Vec<String>,
) -> Result<(Status, Value, bool, bool)> {
    let mut results = Vec::new();
    let mut unknown = false;
    let mut interrupted = false;
    for cell in cells {
        if unknown || interrupted {
            results.push(json!({"id":cell.id,"status":"blocked","reason":"prior_outcome_unknown"}));
            continue;
        }
        let dir = out.join(&cell.id);
        std::fs::create_dir(&dir)?;
        if reset_app_data {
            let output =
                adb::shell_mutating(serial, format!("pm clear {}", journey.package)).await?;
            anyhow::ensure!(
                output.trim() == "Success",
                "matrix app-data reset not confirmed: {output}"
            );
        }
        let mut journey = journey.clone();
        let configuration_index =
            usize::from(matches!(journey.steps.first(), Some(Step::Start { .. })));
        journey.steps.insert(
            configuration_index,
            Step::Configure {
                configuration: cell.configuration.clone(),
            },
        );
        let (status, evidence, uncertain, stopped) = run(&journey, serial, &dir, cleanup).await?;
        unknown |= uncertain;
        interrupted |= stopped;
        results.push(json!({"id":cell.id,"status":status,"evidence":evidence}));
    }
    let status = super::runner::reduce(
        results
            .iter()
            .map(|v| serde_json::from_value(v["status"].clone()).unwrap_or(Status::Blocked)),
    );
    Ok((
        status,
        json!({"adapter":"matrix","cells":results,"reset_app_data_each_cell":reset_app_data,"animations":"unchanged","app_theme_overrides":"must_be_asserted_in_journey"}),
        unknown,
        interrupted,
    ))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "by", content = "value", rename_all = "snake_case")]
pub enum Target {
    Rid(String),
    Text(String),
    Description(String),
}
impl Target {
    fn query(&self) -> SelectorQuery {
        let mut query = SelectorQuery {
            exact: true,
            ..Default::default()
        };
        match self {
            Self::Rid(s) => query.rid = Some(s.clone()),
            Self::Text(s) => query.text = Some(s.clone()),
            Self::Description(s) => query.desc = Some(s.clone()),
        };
        query
    }
    fn matches(&self, e: &Element) -> bool {
        match self {
            Self::Rid(s) => e.rid.as_ref() == Some(s),
            Self::Text(s) => e.text.as_ref() == Some(s),
            Self::Description(s) => e.desc.as_ref() == Some(s),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Start {
        activity: String,
    },
    Tap {
        target: Target,
    },
    Text {
        target: Target,
        value: String,
    },
    Key {
        name: String,
    },
    Assert {
        target: Target,
        #[serde(default = "one")]
        count: usize,
        text: Option<String>,
        enabled: Option<bool>,
        selected: Option<bool>,
        checked: Option<bool>,
        destination: Option<String>,
    },
    Remember {
        target: Target,
        name: String,
    },
    Compare {
        target: Target,
        memory: String,
        #[serde(default)]
        different: bool,
    },
    Configure {
        configuration: Configuration,
    },
    Lifecycle {
        mode: Lifecycle,
        resume_activity: String,
    },
    Capture {
        name: String,
    },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    BackgroundResume,
    BackgroundKillRestore,
    ForceStopColdLaunch,
}
fn journey_timeout() -> u64 {
    120_000
}
fn one() -> usize {
    1
}

impl Journey {
    pub fn validate(&self) -> Result<()> {
        crate::config::validate_android_package(&self.package)?;
        anyhow::ensure!(
            (1..=3_600_000).contains(&self.timeout_ms),
            "journey timeout_ms must be 1..3600000"
        );
        anyhow::ensure!(
            !self.steps.is_empty() && self.steps.len() <= 200,
            "journeys need 1..200 steps"
        );
        let destinations = self.destinations.iter().collect::<BTreeSet<_>>();
        anyhow::ensure!(
            destinations.len() == self.destinations.len()
                && self.destinations.iter().all(|s| super::plan::valid_id(s)),
            "invalid/duplicate destination IDs"
        );
        let mut memories = BTreeSet::new();
        let mut captures = BTreeSet::new();
        for (index, step) in self.steps.iter().enumerate() {
            if matches!(step, Step::Key { .. }) {
                anyhow::ensure!(
                    matches!(
                        self.steps.get(index + 1),
                        Some(Step::Assert { .. } | Step::Compare { .. })
                    ),
                    "journey key needs an immediately following assert or compare postcondition"
                );
            }
            match step {
                Step::Start { activity }
                | Step::Lifecycle {
                    resume_activity: activity,
                    ..
                } => validate_activity(activity)?,
                Step::Configure { configuration } => configuration.validate()?,
                Step::Assert {
                    destination: Some(id),
                    ..
                } => anyhow::ensure!(
                    destinations.contains(id),
                    "assertion references undeclared destination {id}"
                ),
                Step::Remember { name, .. } => anyhow::ensure!(
                    super::plan::valid_id(name) && memories.insert(name),
                    "invalid/duplicate memory name"
                ),
                Step::Compare { memory, .. } => anyhow::ensure!(
                    memories.contains(memory),
                    "comparison needs an earlier memory {memory}"
                ),
                Step::Capture { name } => anyhow::ensure!(
                    super::plan::valid_id(name) && captures.insert(name),
                    "invalid/duplicate capture name"
                ),
                Step::Key { name } => anyhow::ensure!(
                    matches!(
                        name.as_str(),
                        "back"
                            | "home"
                            | "enter"
                            | "dpad_up"
                            | "dpad_down"
                            | "dpad_left"
                            | "dpad_right"
                            | "dpad_center"
                    ),
                    "unsupported journey key"
                ),
                _ => {}
            }
            if let Step::Assert {
                count,
                text,
                enabled,
                selected,
                checked,
                destination,
                ..
            } = step
            {
                anyhow::ensure!(*count <= 1000, "assertion count exceeds 1000");
                anyhow::ensure!(
                    *count != 0
                        || (text.is_none()
                            && enabled.is_none()
                            && selected.is_none()
                            && checked.is_none()
                            && destination.is_none()),
                    "absence assertions cannot assert properties or establish destinations"
                );
            }
        }
        Ok(())
    }
}
fn validate_activity(activity: &str) -> Result<()> {
    anyhow::ensure!(
        !activity.is_empty()
            && activity
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._$".contains(&c)),
        "invalid activity name"
    );
    Ok(())
}

pub async fn run(
    journey: &Journey,
    serial: &Serial,
    out: &Path,
    cleanup: &mut Vec<String>,
) -> Result<(Status, Value, bool, bool)> {
    journey.validate()?;
    let Some(client) = installer::probe_existing(serial, false).await? else {
        return Ok((
            Status::Blocked,
            json!({"reason":"server_unavailable","next_action":"connect explicitly before UI checks; instrumentation checks leave the service disconnected"}),
            false,
            false,
        ));
    };
    let mut journal = Journal::default();
    let journal_path = out.join("configuration-journal.json");
    let mut memories = BTreeMap::new();
    let mut visited = BTreeSet::new();
    let mut edges = vec![];
    let mut previous_destination = None;
    let mut results = vec![];
    let mut status = Status::Passed;
    let mut unknown = false;
    let mut interrupted = false;
    let configuration_before = super::configuration::metadata(serial).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(journey.timeout_ms);
    for (index, step) in journey.steps.iter().enumerate() {
        if status != Status::Passed {
            results.push(
                json!({"index":index,"status":"blocked","reason":"earlier_step_did_not_pass"}),
            );
            continue;
        }
        let started = crate::runtime::now_ms();
        let execution = tokio::select! {
            result=execute(step,journey,&client,serial,out,&mut journal,&journal_path,&mut memories)=>result,
            _=tokio::signal::ctrl_c()=> {interrupted=true; Err(anyhow::anyhow!("interrupted with action outcome unknown"))},
            _=tokio::time::sleep_until(deadline)=>Err(anyhow::anyhow!("journey deadline exceeded"))
        };
        let evidence = match execution {
            Ok((step_status, evidence)) => {
                status = step_status;
                evidence
            }
            Err(e) => {
                let terminal_server_error = e
                    .downcast_ref::<crate::device::client::ServerError>()
                    .is_some();
                status = if terminal_server_error {
                    Status::Failed
                } else {
                    Status::Blocked
                };
                unknown = interrupted
                    || (!terminal_server_error
                        && matches!(
                            step,
                            Step::Start { .. }
                                | Step::Tap { .. }
                                | Step::Text { .. }
                                | Step::Key { .. }
                                | Step::Configure { .. }
                                | Step::Lifecycle { .. }
                        ));
                json!({"error":format!("{e:#}"),"outcome_unknown":unknown})
            }
        };
        if status == Status::Passed
            && let Step::Assert {
                destination: Some(id),
                ..
            } = step
        {
            visited.insert(id.clone());
            if let Some(previous) = previous_destination.replace(id.clone()) {
                edges.push(json!({"from":previous,"to":id,"step":index}));
            }
        }
        results.push(json!({"index":index,"action":step,"status":status,"started_ms":started,"finished_ms":crate::runtime::now_ms(),"evidence":evidence}));
        save(
            &out.join("journey-progress.json"),
            &json!({"steps":results,"visited":visited,"edges":edges}),
        )?;
    }
    if !unknown {
        cleanup.extend(journal.restore(serial, &journal_path).await);
        unknown |= !cleanup.is_empty();
    }
    let unvisited = journey
        .destinations
        .iter()
        .filter(|id| !visited.contains(*id))
        .collect::<Vec<_>>();
    if status == Status::Passed
        && (!unvisited.is_empty()
            || !journey
                .steps
                .iter()
                .any(|s| matches!(s, Step::Assert { .. } | Step::Compare { .. })))
    {
        status = Status::Untested;
    }
    Ok((
        status,
        json!({"adapter":"journey","steps":results,"visited_destinations":visited,"unvisited_destinations":unvisited,"observed_edges":edges,"coverage":"declared_routes_only_not_exhaustive","configuration_before":configuration_before,"configuration_after":super::configuration::metadata(serial).await.ok(),"configuration_cleanup":if unknown {"unknown_or_failed"} else {"restored"}}),
        unknown,
        interrupted,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    step: &Step,
    journey: &Journey,
    client: &ServerClient,
    serial: &Serial,
    out: &Path,
    journal: &mut Journal,
    journal_path: &Path,
    memories: &mut BTreeMap<String, String>,
) -> Result<(Status, Value)> {
    let pass = |v| Ok((Status::Passed, v));
    match step {
        Step::Start { activity } => {
            let response = client.app_start(&journey.package, Some(activity)).await?;
            let wait = client.app_wait(&journey.package, 5000, true).await?;
            Ok((
                if response.ok && wait.matched {
                    Status::Passed
                } else {
                    Status::Failed
                },
                json!({"activity":response.activity,"foreground_matched":wait.matched}),
            ))
        }
        Step::Tap { target } | Step::Text { target, .. } => {
            let stable = client.stable_screen(150, 3000).await?;
            if !stable.stable || stable.screen.snapshot_state != SnapshotState::Consistent {
                return Ok((Status::Blocked, json!({"reason":"screen_not_stable"})));
            }
            let matches = stable
                .screen
                .elements
                .iter()
                .filter(|e| target.matches(e))
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Ok((
                    Status::Failed,
                    json!({"reason":"target_not_unique","matches":matches.len()}),
                ));
            }
            let guard = client.with_action_guard(ActionGuard {
                if_screen: Some(stable.screen.screen_hash),
                if_interaction: None,
                element_handle: matches[0].handle.clone(),
            });
            if let Step::Text { value, .. } = step {
                guard
                    .text_with_target(value, true, Some(&target.query()))
                    .await?;
            } else {
                let response = guard.find_tap(&target.query()).await?;
                if response.input_delivered == Some(false) {
                    return Ok((Status::Failed, json!({"reason":"input_not_delivered"})));
                }
            }
            pass(json!({"target":target,"guarded":true}))
        }
        Step::Key { name } => {
            let stable = client.stable_screen(150, 3000).await?;
            if !stable.stable || stable.screen.snapshot_state != SnapshotState::Consistent {
                return Ok((Status::Blocked, json!({"reason":"screen_not_stable"})));
            }
            let guard = client.with_action_guard(ActionGuard {
                if_screen: Some(stable.screen.screen_hash),
                if_interaction: None,
                element_handle: None,
            });
            let injected = guard.key(name).await?;
            // Android can report false even when the key changed the UI. Do
            // not repeat it or infer delivery from this advisory bit. Validation
            // requires the very next step to establish the requested outcome.
            pass(
                json!({"injected":injected,"injection_result":"advisory","guarded":true,"outcome_validation":"following_assert_or_compare","replayed":false}),
            )
        }
        Step::Assert {
            target,
            count,
            text,
            enabled,
            selected,
            checked,
            ..
        } => {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snapshot = client.stable_screen(150, 2000).await?;
                let matches = snapshot
                    .screen
                    .elements
                    .iter()
                    .filter(|e| target.matches(e))
                    .collect::<Vec<_>>();
                let complete = snapshot.stable
                    && snapshot.screen.snapshot_state == SnapshotState::Consistent
                    && snapshot.screen.current_app.package.as_deref() == Some(&journey.package);
                let matched = complete
                    && matches.len() == *count
                    && matches.iter().all(|e| {
                        text.as_ref().is_none_or(|v| e.text.as_ref() == Some(v))
                            && enabled.is_none_or(|v| e.enabled == v)
                            && selected.is_none_or(|v| e.selected == v)
                            && checked.is_none_or(|v| e.checked == v)
                    });
                if matched || Instant::now() >= deadline {
                    return Ok((
                        if matched {
                            Status::Passed
                        } else if complete {
                            Status::Failed
                        } else {
                            Status::Blocked
                        },
                        json!({"matches":matches,"screen_hash":snapshot.screen.screen_hash,"foreground":snapshot.screen.current_app,"consistent":complete}),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Step::Remember { target, name } => {
            let value = read_text(client, target).await?;
            memories.insert(name.clone(), value.clone());
            pass(json!({"name":name,"value":value}))
        }
        Step::Compare {
            target,
            memory,
            different,
        } => {
            let value = read_text(client, target).await?;
            let previous = memories.get(memory).context("missing remembered text")?;
            Ok((
                if (&value != previous) == *different {
                    Status::Passed
                } else {
                    Status::Failed
                },
                json!({"previous":previous,"current":value,"different":different}),
            ))
        }
        Step::Configure { configuration } => {
            let before = client.stable_screen(150, 3000).await?.screen.viewport;
            journal.apply(serial, journal_path, configuration).await?;
            let after = client.stable_screen(300, 5000).await?;
            pass(
                json!({"configuration":super::configuration::metadata(serial).await?,"viewport_before":before,"viewport_after":after.screen.viewport,"recreation":"not_inferred_from_configuration_change"}),
            )
        }
        Step::Lifecycle {
            mode,
            resume_activity,
        } => {
            lifecycle(
                serial,
                client,
                &journey.package,
                resume_activity,
                *mode,
                out,
            )
            .await
        }
        Step::Capture { name } => super::visual::capture(client, serial, out, name).await,
    }
}
async fn read_text(client: &ServerClient, target: &Target) -> Result<String> {
    let screen = client.stable_screen(200, 3000).await?;
    anyhow::ensure!(
        screen.stable && screen.screen.snapshot_state == SnapshotState::Consistent,
        "text observation is unstable"
    );
    let matches = screen
        .screen
        .elements
        .iter()
        .filter(|e| target.matches(e))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        matches.len() == 1,
        "text target must match exactly one element"
    );
    matches[0]
        .text
        .clone()
        .context("matched element has no text")
}
pub fn save(path: &Path, value: &Value) -> Result<()> {
    crate::cmd::artifact::write_json(
        path,
        &crate::redaction::redact_output_if_active(value.clone()),
    )?;
    Ok(())
}

async fn state(serial: &Serial, package: &str) -> Result<Value> {
    let dump = adb::shell(serial, "dumpsys activity activities").await?;
    let resumed = dump
        .lines()
        .find(|s| s.contains("ResumedActivity") && s.contains(&format!("{package}/")));
    let task = resumed.and_then(|line| {
        line.split_whitespace().find_map(|v| {
            v.strip_prefix('t')
                .and_then(|s| s.trim_end_matches('}').parse::<u32>().ok())
        })
    });
    let pid = adb::shell(serial, format!("pidof {package}"))
        .await?
        .trim()
        .to_owned();
    Ok(
        json!({"pid":pid,"task":task,"foreground":adb::foreground_activity(serial).await,"sampled_ms":crate::runtime::now_ms()}),
    )
}
async fn lifecycle(
    serial: &Serial,
    client: &ServerClient,
    package: &str,
    activity: &str,
    mode: Lifecycle,
    out: &Path,
) -> Result<(Status, Value)> {
    let before = state(serial, package).await?;
    if before["task"].is_null() || before["pid"] == "" {
        return Ok((
            Status::Blocked,
            json!({"reason":"target_must_be_foreground_with_known_task_and_pid","before":before}),
        ));
    }
    let mut observed_absent = false;
    match mode {
        Lifecycle::ForceStopColdLaunch => {
            adb::shell_mutating(serial, format!("am force-stop {package}")).await?;
        }
        _ => {
            client.key("home").await?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(foreground) = adb::foreground_activity(serial).await
                    && !foreground.starts_with(&format!("{package}/"))
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Ok((
                        Status::Blocked,
                        json!({"reason":"background_transition_not_observed"}),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if matches!(mode, Lifecycle::BackgroundKillRestore) {
                let component = before["foreground"]
                    .as_str()
                    .context("missing original activity")?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let dump = adb::shell(serial, "dumpsys activity activities").await?;
                    let saved = dump
                        .split("* Hist")
                        .skip(1)
                        .find(|part| {
                            part.lines()
                                .next()
                                .is_some_and(|line| line.contains(component))
                        })
                        .is_some_and(|part| {
                            part.contains("mHaveState=true") && part.contains("mAppStopped=true")
                        });
                    if saved {
                        break;
                    }
                    if Instant::now() >= deadline {
                        return Ok((
                            Status::Blocked,
                            json!({"reason":"stopped_saved_activity_not_observed","before":before}),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let pid = before["pid"].as_str().context("missing PID")?;
                if pid.parse::<u32>().is_err() {
                    return Ok((
                        Status::Blocked,
                        json!({"reason":"expected_one_main_process"}),
                    ));
                }
                let command = format!(
                    "run-as {package} sh -c {}",
                    crate::events::shell_token(&format!(
                        "kill -9 {pid} && echo __shadowdroid_killed__"
                    ))
                );
                let output = adb::shell_mutating(serial, command).await?;
                if !output.contains("__shadowdroid_killed__") {
                    return Ok((
                        Status::Blocked,
                        json!({"reason":"debuggable_process_kill_unavailable","output":output}),
                    ));
                }
            }
        }
    }
    if !matches!(mode, Lifecycle::BackgroundResume) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if adb::shell(serial, format!("pidof {package}"))
                .await?
                .trim()
                .is_empty()
            {
                observed_absent = true;
                break;
            }
            if Instant::now() >= deadline {
                return Ok((
                    Status::Blocked,
                    json!({"reason":"process_never_observed_dead","before":before}),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    // Reorder the existing top activity; launcher flags can create a new instance
    // even when PID, component and task ID stay the same.
    let command = if matches!(mode, Lifecycle::ForceStopColdLaunch) {
        format!("am start -W -n {package}/{activity}")
    } else {
        let component = before["foreground"]
            .as_str()
            .context("missing prior foreground")?;
        format!(
            "am start -W -f 0x10020000 -n {}",
            crate::events::shell_token(component)
        )
    };
    let launch = adb::shell_mutating(serial, command).await?;
    client.app_wait(package, 5000, true).await?;
    let after = state(serial, package).await?;
    let restored = before["task"] == after["task"]
        && before["foreground"] == after["foreground"]
        && !after["task"].is_null();
    let pid_changed = before["pid"] != after["pid"] && after["pid"] != "";
    let passed = match mode {
        Lifecycle::BackgroundResume => restored && before["pid"] == after["pid"],
        Lifecycle::BackgroundKillRestore => restored && observed_absent && pid_changed,
        Lifecycle::ForceStopColdLaunch => {
            observed_absent && pid_changed && !after["task"].is_null()
        }
    };
    let evidence = json!({"mode":mode,"before":before,"after":after,"process_absence_observed":observed_absent,"same_task_and_top_activity":restored,"launch_output":launch,"scope":"adb_simulation_not_all_low_memory_behavior","background_kill_mechanism":"run_as_SIGKILL_after_saved_stopped_activity"});
    save(
        &out.join(format!("lifecycle-{}.json", crate::runtime::now_ms())),
        &evidence,
    )?;
    Ok((
        if passed {
            Status::Passed
        } else {
            Status::Blocked
        },
        evidence,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_journey() -> Journey {
        serde_json::from_value(json!({
            "package":"example.app", "destinations":[], "steps":[
                {"action":"remember","target":{"by":"rid","value":"label"},"name":"before"},
                {"action":"key","name":"back"},
                {"action":"compare","target":{"by":"rid","value":"label"},"memory":"before","different":true}
            ]
        })).unwrap()
    }

    #[test]
    fn keys_require_immediate_postconditions() {
        let mut journey = key_journey();
        journey.validate().unwrap();
        journey.steps.pop();
        assert!(
            journey
                .validate()
                .unwrap_err()
                .to_string()
                .contains("postcondition")
        );
        journey.steps.push(Step::Capture {
            name: "after".into(),
        });
        assert!(
            journey.validate().is_err(),
            "a screenshot alone cannot prove key outcome"
        );
    }

    #[tokio::test]
    async fn keys_use_observed_postconditions_without_replay() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        for (injected, observed, expected) in [
            (false, "after", Status::Passed),
            (false, "before", Status::Failed),
            (true, "before", Status::Failed),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = ServerClient::new(listener.local_addr().unwrap().port()).unwrap();
            let server = tokio::spawn(async move {
                for index in 0..3 {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut socket = BufReader::new(socket);
                    let mut request = String::new();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                        request.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    socket.read_exact(&mut body).await.unwrap();
                    let response = if index == 1 {
                        assert!(request.starts_with("POST /v1/guarded/key "), "{request}");
                        assert!(request.to_ascii_lowercase().contains("x-shadowdroid-if-screen: before"));
                        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({"name":"back"}));
                        json!({"ok":injected})
                    } else {
                        assert!(request.starts_with("GET /v1/screen/stable?"), "keys must not be repeated: {request}");
                        json!({"stable":true,"settle_ms":0,"quiet_period_ms":150,"screen":{
                            "screen_hash":"before","snapshot_state":"consistent",
                            "viewport":{"w":320,"h":640},"current_app":{"package":"example.app"},
                            "element_count":1,"elements":[{"id":0,"rid":"label","text":if index==0 {"before"} else {observed}}]
                        }})
                    }.to_string();
                    socket.get_mut().write_all(format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response.len(), response
                    ).as_bytes()).await.unwrap();
                    socket.get_mut().shutdown().await.unwrap();
                }
            });
            let journey = key_journey();
            journey.validate().unwrap();
            let out = tempfile::tempdir().unwrap();
            let mut journal = Journal::default();
            let mut memories = BTreeMap::from([("before".into(), "before".into())]);
            for (index, step) in journey.steps.iter().skip(1).enumerate() {
                let (status, evidence) = execute(
                    step,
                    &journey,
                    &client,
                    &Serial::new("fixture"),
                    out.path(),
                    &mut journal,
                    &out.path().join("journal.json"),
                    &mut memories,
                )
                .await
                .unwrap();
                if index == 0 {
                    assert_eq!(status, Status::Passed);
                    assert_eq!(evidence["injected"], injected);
                    assert_eq!(evidence["injection_result"], "advisory");
                    assert_eq!(evidence["replayed"], false);
                } else {
                    assert_eq!(status, expected);
                }
            }
            server.await.unwrap();
        }
    }
}
