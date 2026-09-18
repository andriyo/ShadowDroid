//! Cooperative same-host device ownership. Reservations never expire by time or PID.
//! A persistent operation journal fails closed if its process disappears mid-command.
use crate::ids::Serial;
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static OPTIONS: OnceLock<Options> = OnceLock::new();
static ACTIVE: Mutex<Option<Operation>> = Mutex::new(None);
static TERMINAL: Mutex<Vec<Value>> = Mutex::new(Vec::new());

pub fn defer_terminal(value: &Value) -> bool {
    if crate::events::current_command_path() == Some("watch")
        || value["type"] == "error"
        || ACTIVE.lock().unwrap().is_none()
    {
        return false;
    }
    TERMINAL.lock().unwrap().push(value.clone());
    true
}

struct Options {
    token: Option<String>,
    root: PathBuf,
    wait_ms: u32,
}

#[derive(Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCmd,
}

#[derive(Subcommand)]
pub enum SessionCmd {
    /// Reserve an online device across commands. Ownership has no automatic expiry.
    Open {
        /// Human-readable agent identity (not an authentication credential).
        #[arg(long)]
        agent: String,
    },
    /// Read owner and interrupted-operation metadata without changing ownership.
    Status,
    /// Release an idle reservation after stopping its proxy/recorder.
    Close,
    /// Clear an interrupted operation only after a verified device reboot and external-worker review.
    Recover {
        /// Attest that all external test/build/ADB workers from the interrupted command are stopped.
        #[arg(long)]
        external_workers_stopped: bool,
    },
    /// Transfer an idle device, revoking the old token; retain a handoff context artifact.
    Handoff {
        #[arg(long)]
        agent: String,
        /// JSON containing candidate, plan, backend and starting-state references.
        #[arg(long)]
        context: PathBuf,
    },
    /// Observe existing UI and independently consume crashes; never installs or reconnects.
    Observe {
        /// Independent event consumer ID; use a different ID for each adviser.
        #[arg(long)]
        subscription: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Owner {
    token: String,
    agent: String,
    opened_ms: u64,
    boot_id: String,
    generation: u64,
    #[serde(default)]
    needs_observation: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InFlight {
    request_id: String,
    command: String,
    pid: u32,
    started_ms: u64,
    boot_id: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    schema_version: u32,
    serial: String,
    generation: u64,
    owner: Option<Owner>,
    in_flight: Option<InFlight>,
    last_completion: Option<Value>,
    last_cleanup: Option<Value>,
    handoff: Option<Value>,
}

struct Operation {
    _lock: File,
    release_anchor: bool,
    recovery_cleanup: bool,
    path: PathBuf,
    state: State,
}

fn fail(code: &str, message: &str) -> anyhow::Error {
    crate::diagnostic::DiagnosticError::new(code,"session",message)
        .next_actions(["shadowdroid session status", "use the current owner's --session token; interrupted operations require recovery, never steal a lease by PID or timeout"])
        .into()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn configure(token: Option<String>, root: Option<PathBuf>, wait_ms: u32) -> Result<()> {
    let root = root.unwrap_or(crate::hostenv::shadowdroid_home()?.join("authority"));
    OPTIONS
        .set(Options {
            token,
            root,
            wait_ms,
        })
        .map_err(|_| anyhow::anyhow!("authority already configured"))?;
    Ok(())
}
fn options() -> Result<&'static Options> {
    OPTIONS.get().context("runtime authority is not configured")
}
pub fn token() -> Option<&'static str> {
    OPTIONS.get().and_then(|o| o.token.as_deref())
}
pub fn subscription_scope() -> String {
    OPTIONS
        .get()
        .map(|o| {
            format!(
                "{}:{}",
                o.root.display(),
                o.token.as_deref().unwrap_or("legacy")
            )
        })
        .unwrap_or_else(|| "legacy".into())
}
const ANCHOR: &str = "/data/local/tmp/shadowdroid-authority";

fn authority_id(root: &Path) -> Result<String> {
    let path = root.join("authority-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        return Ok(id);
    }
    let id = blake3::hash(fresh_id(root)?.as_bytes())
        .to_hex()
        .to_string();
    use std::io::Write;
    let mut temp = tempfile::NamedTempFile::new_in(root)?;
    temp.write_all(id.as_bytes())?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(&path) {
        Ok(_) => Ok(id),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(std::fs::read_to_string(path)?)
        }
        Err(e) => Err(e.error.into()),
    }
}

fn owner_index(serial: &Serial) -> Result<PathBuf> {
    Ok(crate::hostenv::shadowdroid_home()?
        .join("authority-owners")
        .join(format!("{}.json", key(serial))))
}
fn check_local_owner(serial: &Serial, root: &Path) -> Result<()> {
    match std::fs::read(owner_index(serial)?) {
        Ok(bytes) => {
            let owner: Value = serde_json::from_slice(&bytes)?;
            if owner["authority_id"] != authority_id(root)? {
                return Err(fail(
                    "authority_conflict",
                    "the local service owner uses another authority; share its --authority-dir",
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn publish_local_owner(serial: &Serial, root: &Path) -> Result<()> {
    let path = owner_index(serial)?;
    crate::cmd::artifact::write_json(
        &path,
        &json!({"authority_id":authority_id(root)?,"root":root.canonicalize()?}),
    )?;
    crate::video::paths::protect_file(&path)?;
    Ok(())
}
fn remove_local_owner(serial: &Serial, root: &Path) -> Result<()> {
    check_local_owner(serial, root)?;
    match std::fs::remove_file(owner_index(serial)?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn allows_offline_control(path: &str) -> bool {
    (path.starts_with("net ") && !matches!(path, "net start" | "net check" | "net trust"))
        || matches!(
            path,
            "video status" | "video stop" | "video mark" | "disconnect"
        )
}

async fn anchor(serial: &Serial, root: &Path, claim: bool) -> Result<()> {
    let id = authority_id(root)?;
    anyhow::ensure!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid authority identity"
    );
    let binding = format!("{id}:{}", key(serial));
    let command = if claim {
        format!(
            "if mkdir {ANCHOR} 2>/dev/null; then printf '%s' '{binding}' > {ANCHOR}/owner; fi; cat {ANCHOR}/owner 2>/dev/null"
        )
    } else {
        format!("if [ -d {ANCHOR} ]; then cat {ANCHOR}/owner 2>/dev/null; else printf absent; fi")
    };
    let observed = if claim {
        crate::device::adb::shell_mutating(serial, command).await?
    } else {
        crate::device::adb::shell(serial, command).await?
    };
    if observed.trim() == binding || (!claim && observed.trim() == "absent") {
        Ok(())
    } else {
        Err(fail(
            "authority_conflict",
            "device belongs to a different (or incomplete) authority; use the same shared --authority-dir, never private registries for a shared device",
        ))
    }
}

async fn release_anchor(serial: &Serial, root: &Path) -> Result<()> {
    let id = authority_id(root)?;
    anyhow::ensure!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid authority identity"
    );
    let binding = format!("{id}:{}", key(serial));
    let result=crate::device::adb::shell_mutating(serial,format!("if [ \"$(cat {ANCHOR}/owner 2>/dev/null)\" = '{binding}' ]; then rm {ANCHOR}/owner && rmdir {ANCHOR} && printf released; elif [ ! -d {ANCHOR} ]; then printf released; fi")).await?;
    anyhow::ensure!(
        result.trim() == "released",
        "authority anchor cleanup conflict; do not remove another authority's device marker"
    );
    Ok(())
}

pub struct BuildGuard {
    _lock: File,
    journal: PathBuf,
}
impl BuildGuard {
    pub fn begin(&self, run: &Path) -> Result<()> {
        crate::cmd::artifact::write_json(
            &self.journal,
            &json!({"run":run,"pid":std::process::id(),"started_ms":now_ms(),"outcome":"running"}),
        )?;
        Ok(())
    }
    pub fn complete(&self) -> Result<()> {
        if self.journal.exists() {
            std::fs::remove_file(&self.journal)?;
        }
        Ok(())
    }
}

fn build_guard(root: &Path) -> Result<BuildGuard> {
    let options = options()?;
    let root_dir = options.root.join("builds");
    let identity = Serial::new(root.canonicalize()?.display().to_string());
    let lock=lock_at(&root_dir,&identity,options.wait_ms)
        .map_err(|e|crate::diagnostic::DiagnosticError::new("build_output_busy","verify",format!("another verifier owns this source/build root: {e}")).next_actions(["use an isolated worktree and build output directory, or wait for the active verification to finish"]))?;
    Ok(BuildGuard {
        _lock: lock,
        journal: root_dir.join(format!("{}.operation.json", key(&identity))),
    })
}

pub fn build_lock(root: &Path) -> Result<BuildGuard> {
    let guard = build_guard(root)?;
    if guard.journal.exists() {
        return Err(crate::diagnostic::DiagnosticError::new("build_recovery_required","verify","a prior verifier did not finish; stop/review external workers before releasing its build ownership")
        .detail(serde_json::from_slice::<Value>(&std::fs::read(&guard.journal)?)?)
        .next_actions(["shadowdroid verify recover <interrupted-run> --external-workers-stopped"]).into());
    }
    Ok(guard)
}

pub fn recover_build(root: &Path, run: &Path) -> Result<()> {
    let guard = build_guard(root)?;
    if !guard.journal.exists() {
        return Ok(());
    }
    let record: Value = serde_json::from_slice(&std::fs::read(&guard.journal)?)?;
    anyhow::ensure!(
        record["run"].as_str() == run.to_str(),
        "build ownership belongs to a different run; inspect that run first"
    );
    guard.complete()
}

fn key(serial: &Serial) -> String {
    blake3::hash(format!("adb:127.0.0.1:5037:{}", serial.as_str()).as_bytes())
        .to_hex()
        .to_string()
}
fn state_path(root: &Path, serial: &Serial) -> PathBuf {
    root.join(format!("{}.json", key(serial)))
}
fn read_state(path: &Path, serial: &Serial) -> Result<State> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let state: State = serde_json::from_slice(&bytes)
                .context("authority journal is corrupt; refusing device mutation")?;
            anyhow::ensure!(
                state.schema_version == 1 && state.serial == serial.as_str(),
                "authority journal identity/schema mismatch"
            );
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State {
            schema_version: 1,
            serial: serial.to_string(),
            ..Default::default()
        }),
        Err(e) => Err(e.into()),
    }
}
fn write_state(path: &Path, state: &State) -> Result<()> {
    crate::cmd::artifact::write_json(path, &serde_json::to_value(state)?)?;
    crate::video::paths::protect_file(path)?;
    Ok(())
}
fn fresh_id(root: &Path) -> Result<String> {
    let file = tempfile::Builder::new()
        .prefix("sd-")
        .rand_bytes(24)
        .tempfile_in(root)?;
    Ok(file
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned())
}
fn lock_at(root: &Path, serial: &Serial, wait_ms: u32) -> Result<File> {
    std::fs::create_dir_all(root)?;
    crate::video::paths::protect_dir(root)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(root.join(format!("{}.lock", key(serial))))?;
    let start = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock)
                if start.elapsed() < Duration::from_millis(wait_ms.into()) =>
            {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(fail(
                    "device_operation_busy",
                    "another command still holds this device; retry after it completes",
                ));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}
fn verify_owner(state: &State, token: Option<&str>) -> Result<()> {
    match (&state.owner, token) {
        (Some(owner), Some(token)) if owner.token == token => Ok(()),
        (Some(_), _) => Err(fail(
            "device_reserved",
            "device is reserved by another session",
        )),
        (None, Some(_)) => Err(fail(
            "session_stale",
            "session is closed or belongs to another device/authority",
        )),
        (None, None) => Ok(()),
    }
}
fn require_idle(state: &State) -> Result<()> {
    if state.in_flight.is_some() {
        return Err(fail(
            "session_recovery_required",
            "a prior operation has no durable completion; ownership cannot be reassigned",
        ));
    }
    Ok(())
}
async fn boot_id(serial: &Serial) -> Result<String> {
    let id = crate::device::adb::shell(serial, "cat /proc/sys/kernel/random/boot_id").await?;
    let id = id.trim();
    anyhow::ensure!(
        id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "device boot identity unavailable"
    );
    Ok(id.to_owned())
}

/// Called exactly once after canonical device resolution and before device work.
pub async fn admit(serial: &Serial) -> Result<()> {
    if let Some(active) = ACTIVE.lock().unwrap().as_ref() {
        anyhow::ensure!(
            active.state.serial == serial.as_str(),
            "one command cannot silently change its selected device"
        );
        return Ok(());
    }
    let options = options()?;
    let lock = lock_at(&options.root, serial, options.wait_ms)?;
    let path = state_path(&options.root, serial);
    let mut state = read_state(&path, serial)?;
    verify_owner(&state, options.token.as_deref())?;
    let command = crate::events::current_command_path().unwrap_or("unknown");
    let recovery_cleanup =
        state.in_flight.is_some() && matches!(command, "net stop" | "video stop" | "disconnect");
    if !recovery_cleanup {
        require_idle(&state)?;
    }
    check_local_owner(serial, &options.root)?;
    let command = crate::events::current_command_path().unwrap_or("unknown");
    let changes_device = crate::cmd::introspect::changes_device(command);
    let (boot, online) = match boot_id(serial).await {
        Ok(boot) => (boot, true),
        Err(_) if allows_offline_control(command) => (
            state
                .owner
                .as_ref()
                .map(|o| o.boot_id.clone())
                .unwrap_or_else(|| "unavailable".into()),
            false,
        ),
        Err(error) => return Err(error),
    };
    if online {
        anchor(
            serial,
            &options.root,
            state.owner.is_some() || changes_device,
        )
        .await?;
    }
    if let Some(owner) = &state.owner {
        if owner.boot_id != boot && !recovery_cleanup {
            return Err(fail(
                "device_instance_changed",
                "device rebooted since session open; inspect and close the old session",
            ));
        }
        if owner.needs_observation && !recovery_cleanup {
            return Err(fail(
                "session_observation_required",
                "new driver must run session observe with its token before taking control",
            ));
        }
    }
    if !recovery_cleanup {
        state.in_flight = Some(InFlight {
            request_id: fresh_id(&options.root)?,
            command: crate::events::current_command_path()
                .unwrap_or("unknown")
                .into(),
            pid: std::process::id(),
            started_ms: now_ms(),
            boot_id: boot,
        });
    }
    write_state(&path, &state)?;
    let release_anchor = state.owner.is_none() && changes_device && online;
    *ACTIVE.lock().unwrap() = Some(Operation {
        _lock: lock,
        release_anchor,
        recovery_cleanup,
        path,
        state,
    });
    Ok(())
}

/// Only an observed terminal completion clears the journal. Transport uncertainty stays quarantined.
pub async fn finish(result: &Result<()>) -> Result<()> {
    let pending = std::mem::take(&mut *TERMINAL.lock().unwrap());
    let Some(mut operation) = ACTIVE.lock().unwrap().take() else {
        return Ok(());
    };
    let uncertain = result.as_ref().err().is_some_and(|error| {
        (!error.chain().any(|cause| {
            cause
                .downcast_ref::<crate::diagnostic::DiagnosticError>()
                .is_some()
                || cause
                    .downcast_ref::<crate::device::client::ServerError>()
                    .is_some()
        })) || error
            .chain()
            .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some())
            || matches!(
                crate::cli::error_code_of(error).as_str(),
                "adb_timeout"
                    | "test_command_runner_failed"
                    | "test_command_interrupted"
                    | "verification_outcome_unknown"
            )
    });
    if uncertain {
        // Retain the OS lock until process exit as well as the durable journal.
        // Pending ADB workers must not overlap a recovery command.
        *ACTIVE.lock().unwrap() = Some(operation);
        return Ok(());
    }
    {
        if operation.recovery_cleanup {
            operation.state.last_cleanup = Some(
                json!({"command":crate::events::current_command_path(),"completed_ms":now_ms(),"ok":result.is_ok()}),
            );
        } else {
            operation.state.last_completion = Some(
                json!({"request":operation.state.in_flight.take(),"completed_ms":now_ms(),"ok":result.is_ok()}),
            );
        }
        if let Err(error) = write_state(&operation.path, &operation.state) {
            *ACTIVE.lock().unwrap() = Some(operation);
            return Err(error.context(
                "could not persist operation completion; reservation remains quarantined",
            ));
        }
    }
    if operation.release_anchor && !operation.recovery_cleanup {
        release_anchor(&Serial::new(&operation.state.serial), &options()?.root).await?;
    }
    if result.is_ok() {
        for value in pending {
            crate::events::emit(&value);
        }
    }
    Ok(())
}

pub fn has_reservations() -> Result<bool> {
    let root = &options()?.root;
    if !root.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            let state: State = serde_json::from_slice(&std::fs::read(path)?)?;
            if state.owner.is_some() || state.in_flight.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn public_state(state: &State) -> Value {
    json!({"schema_version":1,"device":state.serial,"generation":state.generation,"owner":state.owner.as_ref().map(|o|json!({"agent":o.agent,"opened_ms":o.opened_ms,"boot_id":o.boot_id,"generation":o.generation,"needs_observation":o.needs_observation})),"in_flight":state.in_flight,"last_completion":state.last_completion,"last_cleanup":state.last_cleanup,"handoff":state.handoff,"automatic_expiry":false})
}
fn services_stopped(serial: &Serial) -> Result<()> {
    for path in [
        crate::net::paths::pid_path(serial)?,
        crate::video::paths::pid(serial)?,
    ] {
        if path.exists() {
            return Err(fail(
                "session_services_active",
                "stop the device's network daemon and video recorder before closing or handing off",
            ));
        }
    }
    Ok(())
}

pub async fn run(serial: &Serial, args: &SessionArgs) -> Result<()> {
    let options = options()?;
    let path = state_path(&options.root, serial);
    if matches!(args.command, SessionCmd::Status) {
        crate::events::emit_action("session_status", &public_state(&read_state(&path, serial)?));
        return Ok(());
    }
    if let SessionCmd::Observe { subscription } = &args.command {
        anyhow::ensure!(
            crate::verify::plan::valid_id(subscription),
            "subscription must be a short alphanumeric ID"
        );
        crate::crashscan::set_subscription(format!("observer:{subscription}"));
        let observation_lock = if token().is_some() {
            Some(lock_at(&options.root, serial, options.wait_ms)?)
        } else {
            None
        };
        let observed_boot = boot_id(serial).await?;
        let foreground = crate::device::adb::foreground_activity(serial).await;
        let before = now_ms();
        let probe = crate::crashscan::spawn_probe(serial);
        let screen = match crate::device::installer::probe_existing(serial, false).await {
            Ok(Some(client)) => match client.screen().await {
                Ok(screen) => json!({"status":"observed","value":screen}),
                Err(e) => json!({"status":"unavailable","error":e.to_string()}),
            },
            Ok(None) => json!({"status":"unavailable","reason":"server is not running"}),
            Err(e) => json!({"status":"unavailable","error":e.to_string()}),
        };
        let crashes = crate::crashscan::finish_probe(probe).await;
        if observation_lock.is_some() {
            let mut state = read_state(&path, serial)?;
            verify_owner(&state, token())?;
            if let Some(owner) = &mut state.owner
                && owner.boot_id == observed_boot
            {
                owner.needs_observation = false;
                write_state(&path, &state)?;
            }
        }
        crate::events::emit_action(
            "session_observe",
            &json!({"schema_version":1,"subscription":subscription,"boot_id":observed_boot,"foreground":foreground,"capture_started_ms":before,"capture_finished_ms":now_ms(),"screen":screen,"crashes":crashes,"ownership":public_state(&read_state(&path,serial)?),"consistency":"bounded_observation_window","server_repaired":false}),
        );
        return Ok(());
    }
    let _lock = lock_at(&options.root, serial, options.wait_ms)?;
    let mut state = read_state(&path, serial)?;
    if !matches!(args.command, SessionCmd::Recover { .. }) {
        require_idle(&state)?;
    }
    match &args.command {
        SessionCmd::Open { agent } => {
            anyhow::ensure!(
                !agent.trim().is_empty() && agent.len() <= 200,
                "agent identity must contain 1..200 characters"
            );
            if state.owner.is_some() {
                return Err(fail(
                    "device_reserved",
                    "device already has a driver; use an explicit handoff",
                ));
            }
            services_stopped(serial)?;
            anchor(serial, &options.root, true).await?;
            publish_local_owner(serial, &options.root)?;
            state.generation += 1;
            let owner = Owner {
                token: fresh_id(&options.root)?,
                agent: agent.clone(),
                opened_ms: now_ms(),
                boot_id: boot_id(serial).await?,
                generation: state.generation,
                needs_observation: true,
            };
            state.owner = Some(owner.clone());
            write_state(&path, &state)?;
            crate::events::emit_action(
                "session_open",
                &json!({"session":owner.token,"ownership":public_state(&state),"next_actions":["pass --session <session> to each driver command; advisers use session observe --subscription <unique-id>"]}),
            );
        }
        SessionCmd::Recover {
            external_workers_stopped,
        } => {
            verify_owner(&state, token())?;
            anyhow::ensure!(
                *external_workers_stopped,
                "recovery requires --external-workers-stopped after reviewing all external workers; no PID or timeout can prove they stopped"
            );
            let previous = state
                .in_flight
                .as_ref()
                .context("there is no interrupted operation")?;
            let current_boot = boot_id(serial).await?;
            anyhow::ensure!(
                current_boot != previous.boot_id,
                "recovery requires a verified device reboot; observe the unknown outcome before rebooting and never replay it automatically"
            );
            services_stopped(serial)?;
            state.last_completion = Some(
                json!({"request":state.in_flight.take(),"outcome":"unknown","recovered_ms":now_ms(),"new_boot_id":current_boot,"external_workers_stopped_attested":true}),
            );
            if let Some(owner) = &mut state.owner {
                owner.boot_id = current_boot;
                owner.needs_observation = true;
            }
            write_state(&path, &state)?;
            crate::events::emit_action("session_recover", &public_state(&state));
        }
        SessionCmd::Close => {
            verify_owner(&state, token())?;
            if state.owner.is_none() {
                bail!("device has no open session");
            }
            services_stopped(serial)?;
            state.owner = None;
            state.generation += 1;
            write_state(&path, &state)?;
            remove_local_owner(serial, &options.root)?;
            release_anchor(serial, &options.root).await?;
            crate::events::emit_action("session_close", &public_state(&state));
        }
        SessionCmd::Handoff { agent, context } => {
            verify_owner(&state, token())?;
            anyhow::ensure!(state.owner.is_some(), "device has no open session");
            anyhow::ensure!(
                !agent.trim().is_empty() && agent.len() <= 200,
                "invalid recipient agent identity"
            );
            services_stopped(serial)?;
            let bytes = std::fs::read(context)?;
            anyhow::ensure!(bytes.len() <= 1024 * 1024, "handoff context exceeds 1 MiB");
            let context: Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(context.is_object(), "handoff context must be a JSON object");
            state.handoff = Some(
                json!({"context":context,"hash":blake3::hash(&bytes).to_hex().to_string(),"previous_generation":state.generation,"captured_ms":now_ms(),"recipient_must_observe":true}),
            );
            state.generation += 1;
            let owner = Owner {
                token: fresh_id(&options.root)?,
                agent: agent.clone(),
                opened_ms: now_ms(),
                boot_id: boot_id(serial).await?,
                generation: state.generation,
                needs_observation: true,
            };
            state.owner = Some(owner.clone());
            write_state(&path, &state)?;
            crate::events::emit_action(
                "session_handoff",
                &json!({"session":owner.token,"ownership":public_state(&state),"next_actions":["recipient: observe fresh device state before taking the next action"]}),
            );
        }
        SessionCmd::Status | SessionCmd::Observe { .. } => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn driver_tokens_are_fenced_and_journal_never_expires() {
        let mut state = State {
            owner: Some(Owner {
                token: "new".into(),
                agent: "b".into(),
                opened_ms: 0,
                boot_id: "b".into(),
                generation: 2,
                needs_observation: false,
            }),
            ..Default::default()
        };
        assert!(verify_owner(&state, Some("old")).is_err());
        assert!(verify_owner(&state, None).is_err());
        verify_owner(&state, Some("new")).unwrap();
        state.in_flight = Some(InFlight {
            request_id: "r".into(),
            command: "ui tap".into(),
            pid: 0,
            started_ms: 0,
            boot_id: "b".into(),
        });
        assert!(require_idle(&state).is_err());
    }
    #[test]
    fn locks_share_one_inode_and_independent_devices_do_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let a = Serial::new("a");
        let b = Serial::new("b");
        let guard = lock_at(dir.path(), &a, 0).unwrap();
        assert!(lock_at(dir.path(), &a, 0).is_err());
        let other = lock_at(dir.path(), &b, 0).unwrap();
        drop(other);
        drop(guard);
        lock_at(dir.path(), &a, 0).unwrap();
        assert!(dir.path().join(format!("{}.lock", key(&a))).exists());
    }
}
