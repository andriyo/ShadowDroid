//! `shadowdroid fault`: put a device or app into a failure condition on
//! purpose — process death, no network, full storage, a moved clock, a
//! failing backend — and undo it reliably.
//!
//! Contract every kind follows, so an agent can predict the outcome:
//! - checks (app, emulator, API level, conflicts, `--allow-physical`) run
//!   before anything on the device changes;
//! - a state fault journals its restore plan before its first change, then
//!   verifies the fault took effect; if it can't, it is rolled back and the
//!   command fails with `fault_verification_failed`;
//! - a fault that would change nothing fails with `fault_no_effect`;
//! - `fault clear` runs the plan, verifies each step, and keeps any fault it
//!   could not restore listed as `restore_failed`;
//! - every inject, clear, expiry and rollback lands in the device's fault
//!   history, which `watch` streams and `why` reports.

pub mod args;
pub mod catalog;
mod device;
mod error;
pub mod journal;
mod kinds;
mod proxy;
mod restore;
mod scenario;

use crate::events::{emit_action, emit_result};
use crate::ids::Serial;
use anyhow::{Context, Result};
use args::{ClearArgs, ExpireArgs, FaultCmd, InjectCmd, SnapshotCmd};
use catalog::{Effect, KindInfo, Scope};
use journal::{FaultRecord, FaultState, Journal, now_ms};
use restore::RestoreStep;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Global flags a child `shadowdroid` process (timer, scenario step) must
/// share with this one to act on the same device ownership.
#[derive(Debug, Clone, Default)]
pub struct Forward {
    pub authority_dir: Option<PathBuf>,
}

impl Forward {
    fn args(&self) -> Vec<String> {
        match &self.authority_dir {
            Some(dir) => vec!["--authority-dir".into(), dir.display().to_string()],
            None => Vec::new(),
        }
    }
}

/// Journals a state fault's restore plan before its first device change.
pub struct Recorder {
    journal: Journal,
    record: FaultRecord,
    recorded: bool,
}

impl Recorder {
    pub async fn record(&mut self, params: &Value, restore: Vec<RestoreStep>) -> Result<()> {
        let mut faults = self.journal.load()?;
        self.record.params = params.clone();
        self.record.restore = restore;
        faults.retain(|fault| fault.id != self.record.id);
        faults.push(self.record.clone());
        self.journal.save(&faults)?;
        self.recorded = true;
        Ok(())
    }
}

fn new_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seed = format!(
        "{}:{}:{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    format!("flt_{}", &blake3::hash(seed.as_bytes()).to_hex()[..10])
}

/// `fault kinds` needs no device.
pub fn kinds() -> Result<()> {
    let kinds: Vec<Value> = catalog::KINDS
        .iter()
        .map(|info| {
            let mut value = serde_json::to_value(info).unwrap_or_else(|_| json!({}));
            value["command"] = json!(format!("shadowdroid fault inject {}", info.kind));
            value["describe"] = json!(format!(
                "shadowdroid commands --json --describe 'fault inject {}'",
                info.kind
            ));
            value
        })
        .collect();
    emit_result(&json!({
        "type": "fault_kinds",
        "count": kinds.len(),
        "kinds": kinds,
        "next_actions": [
            "shadowdroid fault inject <kind> …",
            "shadowdroid commands --guide faults",
        ],
    }));
    Ok(())
}

pub async fn run(cmd: FaultCmd, serial: &Serial, forward: &Forward) -> Result<()> {
    match cmd {
        FaultCmd::Kinds => kinds(),
        FaultCmd::Inject(inject_cmd) => inject(serial, &inject_cmd, forward).await,
        FaultCmd::List => list(serial).await,
        FaultCmd::Clear(args) => clear(serial, &args).await,
        FaultCmd::Snapshot(snapshot) => snapshot_cmd(serial, snapshot).await,
        FaultCmd::Run(args) => scenario::run(serial, &args, forward).await,
        FaultCmd::Expire(args) => run_expire(args).await,
    }
}

fn allow_physical(cmd: &InjectCmd) -> bool {
    matches!(
        cmd,
        InjectCmd::StorageFull {
            allow_physical: true,
            ..
        } | InjectCmd::Clock {
            allow_physical: true,
            ..
        }
    )
}

/// Everything that can refuse a fault without touching the device.
async fn preflight(
    serial: &str,
    info: &KindInfo,
    cmd: &InjectCmd,
    app: Option<&str>,
) -> Result<()> {
    if info.needs_app {
        let package = app.ok_or_else(|| error::requires_app(info.kind))?;
        let installed =
            crate::device::adb::shell(serial, format!("pm path {}", device::quote(package)))
                .await
                .unwrap_or_default();
        if !installed.contains("package:") {
            return Err(error::app_not_installed(serial, package));
        }
    }
    let emulator = device::is_emulator(serial).await;
    if info.scope == Scope::Emulator && !emulator {
        return Err(error::requires_emulator(info.kind));
    }
    if info.destructive && !emulator && !allow_physical(cmd) && info.scope != Scope::Emulator {
        return Err(error::requires_allow(info.kind));
    }
    let sdk = device::sdk(serial).await?;
    if sdk < info.min_sdk {
        return Err(error::unsupported_api(info.kind, sdk, info.min_sdk));
    }
    if info.effect == Effect::Action {
        return Ok(());
    }
    let wanted = info.resources_for(app);
    for active in Journal::for_device(serial)?.load()? {
        if let Some(resource) = active.resources.iter().find(|r| wanted.contains(r)) {
            return Err(error::conflict(
                serial,
                info.kind,
                &active.id,
                &active.kind,
                resource,
            ));
        }
    }
    Ok(())
}

fn view(record: &FaultRecord, info: Option<&KindInfo>) -> Value {
    let mut value = record.to_json(now_ms());
    if let Some(info) = info {
        value["category"] = json!(info.category);
        value["effect"] = json!(info.effect);
    }
    value
}

async fn inject(serial: &Serial, cmd: &InjectCmd, forward: &Forward) -> Result<()> {
    let serial_str = serial.as_str();
    let info = catalog::find(cmd.kind()).expect("every inject subcommand is cataloged");
    let app = {
        let mut cmd = cmd.clone();
        cmd.app_mut().and_then(|app| app.take())
    };
    preflight(serial_str, info, cmd, app.as_deref()).await?;
    let journal = Journal::for_device(serial_str)?;
    let id = new_id();
    let injected_at_ms = now_ms();
    let expires_at_ms = cmd.duration_ms().map(|duration| injected_at_ms + duration);
    let mut recorder = Recorder {
        journal,
        record: FaultRecord {
            id: id.clone(),
            kind: info.kind.to_string(),
            device: serial_str.to_string(),
            app: app.clone(),
            params: json!({}),
            state: FaultState::Active,
            injected_at_ms,
            expires_at_ms,
            observed: json!({}),
            restore: Vec::new(),
            resources: info.resources_for(app.as_deref()),
            expiry_pid: None,
            restore_errors: Vec::new(),
        },
        recorded: false,
    };
    let outcome = {
        let mut ctx = kinds::Ctx {
            serial: serial_str,
            id: &id,
            app: app.as_deref(),
            recorder: &mut recorder,
        };
        kinds::inject(&mut ctx, cmd).await
    };
    let journal = Journal::for_device(serial_str)?;
    let injected = match outcome {
        Ok(injected) => injected,
        Err(failure) => {
            if recorder.recorded {
                return Err(roll_back(serial_str, &journal, &recorder.record, failure).await);
            }
            return Err(failure);
        }
    };
    let mut record = recorder.record;
    record.params = injected.params.clone();
    record.observed = injected.observed.clone();
    let mut next_actions = injected.next_actions.clone();
    if info.effect == Effect::State {
        if !recorder.recorded {
            anyhow::bail!(
                "internal error: state fault {} did not journal its restore plan",
                info.kind
            );
        }
        if let Some(at_ms) = expires_at_ms {
            record.expiry_pid = spawn_expiry(serial_str, &id, at_ms, forward).ok();
        }
        let mut faults = journal.load()?;
        if let Some(slot) = faults.iter_mut().find(|fault| fault.id == id) {
            *slot = record.clone();
        }
        journal.save(&faults)?;
        journal.append_event(journal::event("injected", &record));
        next_actions.insert(0, format!("shadowdroid -d {serial_str} fault clear {id}"));
    } else {
        journal.append_event(journal::event("completed", &record));
    }
    next_actions.push(format!("shadowdroid -d {serial_str} why"));
    let mut fault = view(&record, Some(info));
    if info.effect == Effect::Action {
        fault["state"] = json!("completed");
    }
    emit_action(
        "fault_inject",
        &json!({
            "fault": fault,
            "warnings": injected.warnings,
            "next_actions": next_actions,
        }),
    );
    Ok(())
}

/// Undo a half-applied fault, then report the original failure — or, if
/// even the rollback failed, that the device needs `fault clear`.
async fn roll_back(
    serial: &str,
    journal: &Journal,
    record: &FaultRecord,
    failure: anyhow::Error,
) -> anyhow::Error {
    let errors = apply_plan(serial, &record.restore).await;
    let mut faults = journal.load().unwrap_or_default();
    if errors.is_empty() {
        faults.retain(|fault| fault.id != record.id);
        let _ = journal.save(&faults);
        journal.append_event(journal::event("rolled_back", record));
        failure.context(format!("fault {} was rolled back", record.id))
    } else {
        if let Some(slot) = faults.iter_mut().find(|fault| fault.id == record.id) {
            slot.state = FaultState::RestoreFailed;
            slot.restore_errors = errors.clone();
        }
        let _ = journal.save(&faults);
        journal.append_event(journal::event("restore_failed", record));
        error::restore_failed(
            serial,
            json!([{"id": record.id, "kind": record.kind, "errors": errors, "inject_error": format!("{failure:#}")}]),
        )
    }
}

/// Run every step; return the failures (empty: fully restored).
async fn apply_plan(serial: &str, plan: &[RestoreStep]) -> Vec<Value> {
    let mut errors = Vec::new();
    for step in plan {
        if let Err(error) = step.apply(serial).await {
            errors.push(json!({"step": step.describe(), "error": format!("{error:#}")}));
        }
    }
    errors
}

async fn list(serial: &Serial) -> Result<()> {
    let serial_str = serial.as_str();
    let faults = Journal::for_device(serial_str)?.load()?;
    let stats = if faults
        .iter()
        .any(|fault| catalog::find(&fault.kind).is_some_and(|info| info.scope == Scope::Proxy))
    {
        proxy::stats(serial_str).await
    } else {
        None
    };
    let views: Vec<Value> = faults
        .iter()
        .map(|record| {
            let mut value = view(record, catalog::find(&record.kind));
            if let Some(hits) = stats.as_ref().and_then(|stats| stats.get(&record.id)) {
                value["proxy_hits"] = hits.clone();
            }
            value
        })
        .collect();
    let mut next_actions = Vec::new();
    if !views.is_empty() {
        next_actions.push(format!("shadowdroid -d {serial_str} fault clear --all"));
    }
    next_actions.push(format!("shadowdroid -d {serial_str} fault inject <kind> …"));
    emit_result(&json!({
        "type": "fault_list",
        "device": serial_str,
        "count": views.len(),
        "faults": views,
        "next_actions": next_actions,
    }));
    Ok(())
}

/// Clear every fault on the device, for `disconnect`. Never fails the caller.
pub async fn clear_all_quietly(serial: &str) -> Option<Value> {
    let journal = Journal::for_device(serial).ok()?;
    let faults = journal.load().ok()?;
    if faults.is_empty() {
        return None;
    }
    let (cleared, failed) = clear_records(serial, &journal, faults, "cleared_by_disconnect").await;
    Some(json!({"cleared": cleared, "failed": failed}))
}

async fn clear_records(
    serial: &str,
    journal: &Journal,
    mut targets: Vec<FaultRecord>,
    event_name: &str,
) -> (Vec<Value>, Vec<Value>) {
    // Newest first: a later fault may have been layered on an earlier one.
    targets.sort_by_key(|record| std::cmp::Reverse(record.injected_at_ms));
    let mut cleared = Vec::new();
    let mut failed = Vec::new();
    for record in targets {
        let errors = apply_plan(serial, &record.restore).await;
        let mut faults = journal.load().unwrap_or_default();
        if errors.is_empty() {
            faults.retain(|fault| fault.id != record.id);
            journal.append_event(journal::event(event_name, &record));
            stop_expiry(&record);
            cleared.push(json!({
                "id": record.id,
                "kind": record.kind,
                "restored": record.restore.iter().map(RestoreStep::describe).collect::<Vec<_>>(),
            }));
        } else {
            if let Some(slot) = faults.iter_mut().find(|fault| fault.id == record.id) {
                slot.state = FaultState::RestoreFailed;
                slot.restore_errors = errors.clone();
            }
            journal.append_event(journal::event("restore_failed", &record));
            failed.push(json!({"id": record.id, "kind": record.kind, "errors": errors}));
        }
        if let Err(error) = journal.save(&faults) {
            tracing::warn!("saving the fault journal failed: {error:#}");
        }
    }
    (cleared, failed)
}

async fn clear(serial: &Serial, args: &ClearArgs) -> Result<()> {
    let serial_str = serial.as_str();
    let journal = Journal::for_device(serial_str)?;
    let faults = journal.load()?;
    let mut already_gone = Vec::new();
    let targets: Vec<FaultRecord> = if args.all {
        faults
    } else {
        let mut targets = Vec::new();
        // Resolve every id before restoring any: a typo never half-clears.
        for id in &args.ids {
            match faults.iter().find(|fault| &fault.id == id) {
                Some(record) => targets.push(record.clone()),
                None if args.expired => already_gone.push(id.clone()),
                None => return Err(error::not_found(serial_str, id)),
            }
        }
        targets
    };
    let event_name = if args.expired { "expired" } else { "cleared" };
    let (cleared, failed) = clear_records(serial_str, &journal, targets, event_name).await;
    if !failed.is_empty() {
        return Err(error::restore_failed(serial_str, json!(failed)));
    }
    let remaining = journal.load()?.len();
    emit_action(
        "fault_clear",
        &json!({
            "device": serial_str,
            "cleared": cleared,
            "already_cleared": already_gone,
            "remaining": remaining,
            "next_actions": [format!("shadowdroid -d {serial_str} fault list")],
        }),
    );
    Ok(())
}

fn expiry_log(serial: &str) -> Result<PathBuf> {
    Ok(crate::hostenv::shadowdroid_home()?
        .join("faults")
        .join(format!(
            "{}.expire.log",
            crate::ids::stable_file_component(serial)
        )))
}

/// Start the timer process that clears `id` at `at_ms`.
fn spawn_expiry(serial: &str, id: &str, at_ms: u64, forward: &Forward) -> Result<u32> {
    let exe = std::env::current_exe().context("locate the shadowdroid executable")?;
    let log_path = expiry_log(serial)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["fault", "expire", "--serial", serial, "--id", id, "--at-ms"])
        .arg(at_ms.to_string());
    if let Some(dir) = &forward.authority_dir {
        command.arg("--forward-authority-dir").arg(dir);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Ok(command
        .spawn()
        .context("start the fault expiry timer")?
        .id())
}

/// Stop a still-sleeping expiry timer, if the pid is still that timer.
fn stop_expiry(record: &FaultRecord) {
    #[cfg(unix)]
    if let Some(pid) = record.expiry_pid {
        let is_timer = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .map(|out| {
                let command = String::from_utf8_lossy(&out.stdout);
                command.contains("fault expire") && command.contains(&record.id)
            })
            .unwrap_or(false);
        if is_timer {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
    }
    #[cfg(not(unix))]
    let _ = record;
}

/// The hidden timer: sleep, then clear the fault through a normal command so
/// it takes the device lock like any other client.
pub async fn run_expire(args: ExpireArgs) -> Result<()> {
    let wait = args.at_ms.saturating_sub(now_ms());
    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    let exe = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(exe);
    if let Some(dir) = &args.forward_authority_dir {
        command.arg("--authority-dir").arg(dir);
    }
    let status = command
        .args(["--lock-timeout-ms", "30000", "-d", &args.serial])
        .args(["fault", "clear", &args.id, "--expired"])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("clearing expired fault {} exited with {status}", args.id);
    }
    Ok(())
}

// ── emulator snapshots ────────────────────────────────────────────────────

/// Wait until adb lists the device again and it reports boot completed.
async fn wait_until_booted(serial: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let online = crate::device::adb::list_devices()
            .await
            .map(|devices| devices.iter().any(|device| device == serial))
            .unwrap_or(false);
        if online
            && device::read(serial, "getprop sys.boot_completed")
                .await
                .is_ok_and(|ran| ran.output.trim() == "1")
        {
            return true;
        }
    }
    false
}

fn valid_snapshot_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(error::invalid_param(format!(
            "snapshot name {name:?} must be 1-64 letters, digits, '_', '-' or '.'"
        )));
    }
    Ok(())
}

async fn snapshot_cmd(serial: &Serial, cmd: SnapshotCmd) -> Result<()> {
    let serial_str = serial.as_str();
    if !device::is_emulator(serial_str).await {
        return Err(error::requires_emulator("snapshot"));
    }
    match cmd {
        SnapshotCmd::Save { name } => {
            valid_snapshot_name(&name)?;
            device::emu(serial_str, &["avd", "snapshot", "save", &name]).await?;
            emit_action(
                "fault_snapshot_save",
                &json!({
                    "device": serial_str,
                    "snapshot": name,
                    "saved": true,
                    "next_actions": [format!("shadowdroid -d {serial_str} fault snapshot load {name}")],
                }),
            );
        }
        SnapshotCmd::Load { name } => {
            valid_snapshot_name(&name)?;
            device::emu(serial_str, &["avd", "snapshot", "load", &name]).await?;
            // Loading resets the adb transport; answer only once the device
            // is back, so the next command (and this one's cleanup) reach it.
            let back = wait_until_booted(serial_str, std::time::Duration::from_secs(60)).await;
            let active = Journal::for_device(serial_str)?.load()?.len();
            let mut warnings = Vec::new();
            if active > 0 {
                warnings.push(format!(
                    "{active} fault(s) injected before the load are still listed; the snapshot may already have undone them, and `fault clear --all` reconciles"
                ));
            }
            if !back {
                warnings
                    .push("the emulator did not report booted within 60 s after the load".into());
            }
            emit_action(
                "fault_snapshot_load",
                &json!({
                    "device": serial_str,
                    "snapshot": name,
                    "loaded": true,
                    "device_back": back,
                    "warnings": warnings,
                    "next_actions": [
                        format!("shadowdroid -d {serial_str} connect"),
                        format!("shadowdroid -d {serial_str} fault list"),
                    ],
                }),
            );
        }
        SnapshotCmd::List => {
            let text = device::emu(serial_str, &["avd", "snapshot", "list"]).await?;
            let snapshots: Vec<Value> = text
                .lines()
                .skip_while(|line| !line.trim_start().starts_with("--"))
                .skip(1)
                .filter_map(|line| {
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    (fields.len() >= 4).then(|| {
                        json!({"id": fields[0], "name": fields[1], "size": fields[2], "saved_at": fields[3..].join(" ")})
                    })
                })
                .collect();
            emit_result(&json!({
                "type": "fault_snapshot_list",
                "device": serial_str,
                "snapshots": snapshots,
            }));
        }
        SnapshotCmd::Delete { name } => {
            valid_snapshot_name(&name)?;
            device::emu(serial_str, &["avd", "snapshot", "delete", &name]).await?;
            emit_action(
                "fault_snapshot_delete",
                &json!({"device": serial_str, "snapshot": name, "deleted": true}),
            );
        }
    }
    Ok(())
}

/// Active faults, as `<id> (<kind>)`, split into deliberate ones and
/// leftovers (overdue timer, failed restore). Read-only.
pub struct Summary {
    pub active: Vec<String>,
    pub stale: Vec<String>,
}

fn is_stale(record: &FaultRecord, now: u64) -> bool {
    record.state == FaultState::RestoreFailed
        || record
            .expires_at_ms
            .is_some_and(|expires| now > expires + 5_000)
}

pub fn summary(serial: &str) -> Summary {
    let now = now_ms();
    let faults = Journal::for_device(serial)
        .and_then(|journal| journal.load())
        .unwrap_or_default();
    let (stale, active): (Vec<_>, Vec<_>) = faults.iter().partition(|record| is_stale(record, now));
    let label = |record: &&FaultRecord| format!("{} ({})", record.id, record.kind);
    Summary {
        active: active.iter().map(label).collect(),
        stale: stale.iter().map(label).collect(),
    }
}

/// Clear leftover faults for `doctor --fix`; returns what happened, one line each.
pub async fn clear_stale(serial: &str) -> Vec<String> {
    let Ok(journal) = Journal::for_device(serial) else {
        return Vec::new();
    };
    let now = now_ms();
    let stale: Vec<FaultRecord> = journal
        .load()
        .unwrap_or_default()
        .into_iter()
        .filter(|record| is_stale(record, now))
        .collect();
    if stale.is_empty() {
        return Vec::new();
    }
    let (cleared, failed) = clear_records(serial, &journal, stale, "cleared_by_doctor").await;
    cleared
        .iter()
        .map(|fault| format!("cleared leftover fault {} ({})", fault["id"], fault["kind"]))
        .chain(failed.iter().map(|fault| {
            format!("could not restore leftover fault {} ({}); run `fault clear` once the device is reachable", fault["id"], fault["kind"])
        }))
        .collect()
}

/// Clear faults whose --duration-ms has passed, as their timer would. For a
/// long-running command (`watch`) that holds the device the timer needs.
pub async fn clear_due(serial: &str) {
    let Ok(journal) = Journal::for_device(serial) else {
        return;
    };
    let now = now_ms();
    let due: Vec<FaultRecord> = journal
        .load()
        .unwrap_or_default()
        .into_iter()
        .filter(|record| {
            record.state == FaultState::Active
                && record.expires_at_ms.is_some_and(|expires| now >= expires)
        })
        .collect();
    if !due.is_empty() {
        clear_records(serial, &journal, due, "expired").await;
    }
}

/// `watch`'s `{"cmd":"fault","args":[…]}`: the `fault` subcommands, in process.
pub async fn run_in_watch(serial: &Serial, args: Vec<String>) -> Result<()> {
    #[derive(clap::Parser)]
    #[command(name = "fault", no_binary_name = false)]
    struct Argv {
        #[command(subcommand)]
        cmd: FaultCmd,
    }
    let parsed =
        <Argv as clap::Parser>::try_parse_from(std::iter::once("fault".to_string()).chain(args))
            .map_err(|error| {
                error::invalid_param(error.to_string().lines().next().unwrap_or("").to_string())
            })?;
    match parsed.cmd {
        FaultCmd::Run(_) | FaultCmd::Expire(_) => Err(error::invalid_param(
            "watch runs single fault commands; run scenarios with `fault run` outside watch",
        )),
        cmd => run(cmd, serial, &Forward::default()).await,
    }
}

/// Recent fault history and active faults for `why`/`watch`: read-only.
pub fn history(serial: &str, since_ms: u64) -> Value {
    let Ok(journal) = Journal::for_device(serial) else {
        return json!({"active": [], "recent": []});
    };
    let active: Vec<Value> = journal
        .load()
        .unwrap_or_default()
        .iter()
        .map(|record| view(record, catalog::find(&record.kind)))
        .collect();
    json!({
        "active": active,
        "recent": journal.events_since(since_ms, 20),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[derive(clap::Parser)]
    struct Probe {
        #[command(subcommand)]
        cmd: FaultCmd,
    }

    #[test]
    fn every_inject_subcommand_is_cataloged_and_vice_versa() {
        let command = Probe::command();
        let inject = command.find_subcommand("inject").unwrap();
        let names: std::collections::BTreeSet<&str> =
            inject.get_subcommands().map(|sub| sub.get_name()).collect();
        let cataloged: std::collections::BTreeSet<&str> =
            catalog::KINDS.iter().map(|info| info.kind).collect();
        assert_eq!(names, cataloged);
        // The effect matches whether the subcommand takes --duration-ms.
        for sub in inject.get_subcommands() {
            let info = catalog::find(sub.get_name()).unwrap();
            let has_duration = sub.get_arguments().any(|arg| arg.get_id() == "duration_ms");
            assert_eq!(
                has_duration,
                info.effect == Effect::State,
                "{} duration flag vs effect",
                info.kind
            );
            let has_app = sub.get_arguments().any(|arg| arg.get_id() == "app");
            assert_eq!(has_app, info.needs_app, "{} --app vs needs_app", info.kind);
        }
    }

    #[test]
    fn ids_are_distinct_and_file_safe() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(a.starts_with("flt_") && a[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
