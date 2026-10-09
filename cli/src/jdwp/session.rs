//! The debugger session a `debugd` owns: breakpoint bookkeeping, deferred
//! binding through ClassPrepare, the suspension state machine, and the event
//! loop that turns `Event.Composite` packets into state changes.
//!
//! Reads of frames and values live in [`super::inspect`]; both are `impl
//! Session` so they share the per-connection type cache and handle pins.
//!
//! Invariants:
//! * suspension state is cleared *before* the VM is resumed, so an event that
//!   arrives right after `Resume` is never overwritten by the resume path;
//! * every composite is handled event by event (one class load can carry one
//!   ClassPrepare per matching request), and whatever the VM suspended is
//!   resumed unless some event in it produced a user-visible stop;
//! * object handles are pinned with DisableCollection when handed out and
//!   released on resume and detach.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, watch};

use super::breakpoints::BreakpointOptions;
use super::codec::{Location, Value};
use super::conn::{Incoming, JdwpError};
use super::events::{Composite, Event};
use super::protocol::{self, event_kind, step, suspend_policy};
use super::resolve::{self, SourceTarget};
use super::vm::{FieldInfo, Jdwp, LineTable, MethodInfo, Modifier, Variable};

/// Step-into filters (design §5.5): never land in framework or stdlib code.
pub const DEFAULT_STEP_FILTERS: &[&str] = &[
    "java.*",
    "javax.*",
    "jdk.*",
    "sun.*",
    "kotlin.*",
    "kotlinx.coroutines.*",
    "android.*",
    "androidx.*",
    "com.android.*",
    "dalvik.*",
    "libcore.*",
];

/// Whether `class` (dotted name) belongs to the packages
/// [`DEFAULT_STEP_FILTERS`] treat as framework or library code.
pub fn is_framework_class(class: &str) -> bool {
    DEFAULT_STEP_FILTERS.iter().any(|pattern| {
        pattern
            .strip_suffix('*')
            .is_some_and(|prefix| class.starts_with(prefix))
    })
}

/// The class-loading machinery a runtime class resolution runs through.
fn is_class_loading_class(class: &str) -> bool {
    matches!(
        class,
        "java.lang.Class" | "java.lang.ClassLoader" | "java.lang.BootClassLoader"
    ) || (class.starts_with("dalvik.system.") && class.ends_with("ClassLoader"))
}

/// Shown while a real field watch is armed.
pub(super) const SLOW_WATCH_WARNING: &str = "a field watch makes ART interpret the whole app (about 10x slower UI on the emulator) until it is cleared; it auto-clears after --duration-ms";

/// After this long suspended, an attach-to-running session warns about ANRs.
const ANR_WARNING_SECS: f64 = 4.0;

/// Cap on objects pinned with DisableCollection while suspended.
pub const MAX_LIVE_HANDLES: usize = 512;
const RECENT_EVENTS: usize = 64;

/// A structured failure the daemon returns over RPC; the CLI turns it into a
/// `DiagnosticError` with the same code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub detail: Json,
    #[serde(default)]
    pub next_actions: Vec<String>,
}

impl RpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            detail: json!({}),
            next_actions: Vec::new(),
        }
    }

    pub fn detail(mut self, detail: Json) -> Self {
        self.detail = detail;
        self
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn next(mut self, actions: &[&str]) -> Self {
        self.next_actions = actions.iter().map(|a| a.to_string()).collect();
        self
    }

    pub fn not_suspended() -> Self {
        RpcError::new(
            "debugger_not_suspended",
            "the debugged process is running; pause it or wait for a breakpoint",
        )
        .next(&[
            "shadowdroid debug pause --backend jdwp",
            "shadowdroid debug break line --backend jdwp --file <File.kt> --line <n>",
        ])
    }
}

impl From<JdwpError> for RpcError {
    fn from(error: JdwpError) -> Self {
        let message = error.to_string();
        match &error {
            JdwpError::Timeout {
                command,
                timeout_ms,
            } => RpcError::new("debugger_timeout", message)
                .retryable()
                .detail(json!({"command": command, "timeout_ms": timeout_ms}))
                .next(&["shadowdroid debug status --backend jdwp"]),
            JdwpError::Closed(reason) => RpcError::new("debugger_disconnected", message)
                .detail(json!({"reason": reason}))
                .next(&["shadowdroid debug attach --backend jdwp --package <pkg>"]),
            JdwpError::HandshakeClosed { read } => {
                RpcError::new("debugger_already_attached", message)
                    .detail(json!({"handshake_bytes_read": read}))
            }
            JdwpError::Handshake(_) => RpcError::new("daemon_unreachable", message).retryable(),
            JdwpError::Codec { command, .. } => {
                RpcError::new("jdwp_protocol_error", message).detail(json!({"command": command}))
            }
            JdwpError::Vm {
                command,
                code,
                name,
            } => {
                let detail =
                    json!({"command": command, "jdwp_error": code, "jdwp_error_name": name});
                let code_str = match *code {
                    13 => "debugger_not_suspended",
                    20 => "stale_object_handle",
                    24 => "breakpoint_unresolved",
                    30 => "stale_frame",
                    35 => "variable_not_in_scope",
                    101 => "debug_info_absent",
                    112 => "debuggee_exited",
                    _ => "jdwp_error",
                };
                RpcError::new(code_str, message).detail(detail)
            }
        }
    }
}

pub type RpcResult<T> = Result<T, RpcError>;

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub serial: String,
    pub pid: u32,
    pub package: Option<String>,
    pub attached_at: f64,
    pub vm: Json,
    pub capabilities: Json,
    /// Started under `am set-debug-app -w`: the system runs no ANR timers.
    pub launched_under_debugger: bool,
}

#[derive(Clone, Debug)]
pub(super) enum Owner {
    Breakpoint(String),
    Step,
    /// Deferred line binding for one source file name.
    LinePrepare(String),
    /// Deferred exception-class binding for one breakpoint.
    ExceptionPrepare(String),
    /// Deferred method/field binding: stays armed for every match.
    MemberPrepare(String),
    /// SUSPEND_NONE class-load watch that keeps the coroutine class cache
    /// current.
    CoroutinePrepare,
}

/// What a bound location re-arms with after a config change.
#[derive(Clone, Copy, Debug)]
pub(super) enum Arm {
    Line(Location),
    Exception(u64),
    /// FieldAccess/FieldModification with FieldOnly (slow on ART).
    Field {
        type_id: u64,
        field_id: u64,
        modification: bool,
    },
}

impl Arm {
    pub(super) fn event_kind(&self) -> u8 {
        match self {
            Arm::Line(_) => event_kind::BREAKPOINT,
            Arm::Exception(_) => event_kind::EXCEPTION,
            Arm::Field {
                modification: true, ..
            } => event_kind::FIELD_MODIFICATION,
            Arm::Field { .. } => event_kind::FIELD_ACCESS,
        }
    }

    /// ART interprets the whole app while any field-watch request exists.
    pub(super) fn is_slow(&self) -> bool {
        matches!(self, Arm::Field { .. })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BoundLocation {
    /// `None` while the breakpoint is disabled.
    pub request_id: Option<i32>,
    pub class: String,
    pub method: String,
    pub code_index: u64,
    /// What this location is for: `entry`, `exit`, `setter`, `getter`,
    /// `field_access`, `field_modification` (method/field breakpoints).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    /// The same as `role`, under the name the Studio bridge's method
    /// breakpoints use (`entry`/`exit`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// Inside a Kotlin lambda (`$lambda$` body, `invoke`/`invokeSuspend`
    /// of a lambda class) rather than the outer call site.
    pub lambda: bool,
    /// Lambda nesting depth (0 for the outer method).
    pub lambda_depth: u32,
    #[serde(skip)]
    pub class_id: u64,
    #[serde(skip)]
    pub(super) arm: Arm,
}

#[derive(Clone, Debug)]
pub(super) enum BreakpointKind {
    Line {
        target: SourceTarget,
        line: u32,
    },
    Exception {
        class: String,
        caught: bool,
        uncaught: bool,
    },
    /// Line breakpoints at each matching method's first line (entry) and
    /// at its return instructions (exit). Never MethodEntry/MethodExit.
    Method {
        class: String,
        method: String,
        entry: bool,
        exit: bool,
    },
    /// A property/field watch: setter/getter line breakpoints by default,
    /// a real FieldAccess/FieldModification watch only when opted in.
    Field {
        class: String,
        field: String,
        access: bool,
        modification: bool,
        watch: bool,
    },
}

#[derive(Clone, Debug)]
pub(super) struct Breakpoint {
    pub(super) id: String,
    pub(super) kind: BreakpointKind,
    pub(super) locations: Vec<BoundLocation>,
    pub(super) pending_reason: Option<&'static str>,
    pub(super) hit_count: u64,
    pub(super) created_at: f64,
    pub(super) opts: BreakpointOptions,
    /// Parsed condition; `Err` keeps a `--force`d unparseable one.
    pub(super) condition: Option<Result<super::expr::Expr, String>>,
    pub(super) log_expression: Option<Result<super::expr::Expr, String>>,
    pub(super) last_evaluation_error: Option<Json>,
    pub(super) last_hit_at: Option<f64>,
    pub(super) rate: super::logpoints::RateWindow,
    pub(super) dropped: u64,
    /// A pass-count request fired and is spent.
    pub(super) expired: bool,
    /// Disarmed by the rate limit until this time (epoch seconds).
    pub(super) throttled_until: Option<f64>,
    /// A slow field watch auto-clears at this time (epoch seconds).
    pub(super) slow_until: Option<f64>,
    pub(super) expired_reason: Option<&'static str>,
    /// Why the daemon bound something other than what was asked for.
    pub(super) note: Option<&'static str>,
    /// Field reads/writes found in the APK's dex files (`break field`
    /// without an accessor): bound per class as each loads.
    pub(super) dex_sites: Option<Vec<super::members::SiteState>>,
    /// Why the dex files could not be read, when they could not.
    pub(super) dex_error: Option<String>,
}

impl Breakpoint {
    pub(super) fn new(id: String, kind: BreakpointKind, opts: BreakpointOptions) -> Self {
        let mut breakpoint = Breakpoint {
            id,
            kind,
            locations: Vec::new(),
            pending_reason: None,
            hit_count: 0,
            created_at: crate::events::now_ts(),
            opts: BreakpointOptions::default(),
            condition: None,
            log_expression: None,
            last_evaluation_error: None,
            last_hit_at: None,
            rate: Default::default(),
            dropped: 0,
            expired: false,
            throttled_until: None,
            slow_until: None,
            expired_reason: None,
            note: None,
            dex_sites: None,
            dex_error: None,
        };
        breakpoint.set_opts(opts);
        breakpoint
    }

    pub(super) fn set_opts(&mut self, opts: BreakpointOptions) {
        self.condition = opts.condition.as_deref().map(super::expr::parse);
        self.log_expression = opts.log_expression.as_deref().map(super::expr::parse);
        self.opts = opts;
    }

    /// The `(target basename or path, line)` a line breakpoint sits on.
    pub(super) fn line_key(&self) -> Option<(String, u32)> {
        match &self.kind {
            BreakpointKind::Line { target, line } => Some((target_key(target), *line)),
            _ => None,
        }
    }

    pub(super) fn to_json(&self) -> Json {
        let logpoint = self.opts.is_logpoint();
        let mut value = json!({
            "id": self.id,
            "backend": "jdwp",
            "kind": if logpoint { "logpoint" } else { "breakpoint" },
            "enabled": self.opts.enabled,
            "temporary": self.opts.temporary,
            "bound": !self.locations.is_empty(),
            "locations": self.locations,
            "pending_reason": self.pending_reason,
            "hit_count": self.hit_count,
            "last_hit_at": self.last_hit_at,
            "hit_count_source": if logpoint {
                "shadowdroid_observed_log_callbacks"
            } else {
                "shadowdroid_observed_session_pauses"
            },
            "suspend_policy": self.opts.suspend.as_studio(),
            // What the VM is asked to suspend: a condition, log expression,
            // or stack logging suspends the event thread even on a logpoint.
            "wire_suspend_policy": match self.opts.request_policy() {
                0 => "NONE",
                1 => "THREAD",
                _ => "ALL",
            },
            "invoke": self.opts.invoke,
            "variant": self.opts.variant,
            "condition": self.opts.condition,
            "log_expression": self.opts.log_expression,
            "log_message": self.opts.log_message,
            "log_stack": self.opts.log_stack,
            "pass_count_enabled": self.opts.pass_count.is_some_and(|n| n > 0),
            "pass_count": self.opts.pass_count.unwrap_or(0),
            "owner": self.opts.owner,
            "managed": self.opts.owner.is_some(),
            "created_by_bridge": self.opts.owner.is_some(),
            "max_message_chars": logpoint.then(|| self.opts.max_message_chars()),
            "max_events_per_second": self.opts.needs_eval().then(|| self.opts.max_events_per_second()),
            "last_evaluation_error": self.last_evaluation_error,
            "throttled": self.throttled_until.is_some(),
            "rearm_at": self.throttled_until,
            "dropped": self.dropped,
            "expired": self.expired,
            "expired_reason": self.expired_reason,
            "slow_until": self.slow_until,
            "note": self.note,
            "created_at": self.created_at,
        });
        let extra = match &self.kind {
            BreakpointKind::Line { target, line } => json!({
                "type": "line",
                "file": target_key(target),
                "source": target.basename,
                "package": target.package,
                "line": line,
            }),
            BreakpointKind::Exception {
                class,
                caught,
                uncaught,
            } => json!({
                "type": "exception",
                "exception": class,
                "caught": caught,
                "uncaught": uncaught,
            }),
            BreakpointKind::Method {
                class,
                method,
                entry,
                exit,
            } => json!({
                "type": "method",
                "class": class,
                "method": method,
                "entry": entry,
                "exit": exit,
                "mechanism": "line_breakpoints",
                "return_value": if *exit { "unavailable at a return instruction" } else { "" },
            }),
            BreakpointKind::Field {
                class,
                field,
                access,
                modification,
                watch,
            } => {
                let watched = self
                    .locations
                    .iter()
                    .find(|l| l.role.is_some_and(|r| r.starts_with("field_")))
                    .map(|l| l.method.clone());
                let accessors = self
                    .locations
                    .iter()
                    .any(|l| matches!(l.role, Some("setter" | "getter")));
                let sites = self.dex_sites.as_deref().unwrap_or_default();
                let writes = sites.iter().any(|s| s.site.write);
                let reads = sites.iter().any(|s| !s.site.write);
                let strategy = if *watch {
                    "field_watch"
                } else if writes && reads {
                    "access_sites"
                } else if writes {
                    "write_sites"
                } else if reads {
                    "read_sites"
                } else {
                    "accessors"
                };
                json!({
                    "type": "field",
                    "class": class,
                    "field": field,
                    "watched_field": watched,
                    "access": access,
                    "modification": modification,
                    "strategy": strategy,
                    "accessors": accessors,
                    "mechanism": if *watch {
                        "field_watch"
                    } else if sites.is_empty() {
                        "accessor_breakpoints"
                    } else {
                        "line_breakpoints_at_field_instructions"
                    },
                    "sites": sites.iter().map(super::members::SiteState::to_json).collect::<Vec<_>>(),
                    "dex_error": self.dex_error,
                    "warning": watch.then_some(SLOW_WATCH_WARNING),
                })
            }
        };
        if let (Json::Object(map), Json::Object(extra)) = (&mut value, extra) {
            map.extend(extra);
        }
        value
    }
}

/// A line breakpoint's file identity: the local path when known, else the
/// source file name.
pub(super) fn target_key(target: &SourceTarget) -> String {
    target
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| target.basename.clone())
}

#[derive(Clone, Debug)]
pub struct Suspension {
    pub reason: &'static str,
    pub thread: Option<u64>,
    pub location: Option<Location>,
    pub breakpoint_id: Option<String>,
    pub exception: Option<Value>,
    pub at: f64,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) breakpoints: BTreeMap<String, Breakpoint>,
    next_breakpoint: u32,
    pub(super) owners: HashMap<i32, Owner>,
    /// source basename → ClassPrepare request id
    line_prepares: HashMap<String, i32>,
    /// `(breakpoint id, class, method, code index)` claimed by a binder.
    /// The scan and ClassPrepare paths can race on one class; the claim is
    /// taken before the request is set so each location binds once.
    claims: BTreeSet<(String, u64, u64, u64)>,
    /// Throwables an uncaught-only breakpoint already stopped on: a
    /// framework catch-and-rethrow re-raises the same object.
    reported_throwables: VecDeque<u64>,
    pub(super) suspension: Option<Suspension>,
    pub(super) epoch: u64,
    pinned: BTreeSet<u64>,
    pub(super) closed: Option<String>,
    recent_events: VecDeque<Json>,
    events_seen: u64,
}

#[derive(Default)]
pub(super) struct Cache {
    pub signatures: HashMap<u64, String>,
    pub source_files: HashMap<u64, Option<String>>,
    pub methods: HashMap<u64, Arc<Vec<MethodInfo>>>,
    pub fields: HashMap<u64, Arc<Vec<FieldInfo>>>,
    pub superclasses: HashMap<u64, Option<u64>>,
    pub line_tables: HashMap<(u64, u64), Arc<LineTable>>,
    pub variable_tables: HashMap<(u64, u64), Arc<Vec<Variable>>>,
    pub thread_names: HashMap<u64, String>,
    /// Object → runtime type. JDWP never reuses an object id for another
    /// object unless it is disposed, so this cannot go stale.
    pub object_types: HashMap<u64, u64>,
}

pub struct Session {
    pub(super) jdwp: Jdwp,
    pub info: SessionInfo,
    state: Mutex<State>,
    pub(super) cache: Mutex<Cache>,
    changed: watch::Sender<u64>,
    last_activity: Mutex<Instant>,
    use_source_name_match: bool,
    pub(super) logpoint_log: super::logpoints::LogpointLog,
    initial_breakpoints: Mutex<Vec<Json>>,
    pub(super) invoke_state: super::invoke::SharedInvokeState,
    /// Watch specs and their cached values (by watch id).
    pub(super) watches: Mutex<(Vec<super::watches::WatchSpec>, HashMap<String, Json>)>,
    coroutine_classes: Mutex<Option<super::coroutines::CoroutineClasses>>,
    /// Classes loaded since the coroutine class scan, classified lazily.
    pub(super) coroutine_pending: Mutex<Vec<(u64, String)>>,
    /// ANR detection for attach-to-running sessions.
    pub(super) anr: Mutex<super::anr::AnrState>,
    /// Reads the app's dex files (the daemon pulls the APK); unset in tests
    /// that do not need it.
    pub(super) dex_loader: std::sync::OnceLock<super::members::DexLoader>,
    /// The dex files, read once per session.
    pub(super) dex_files: tokio::sync::Mutex<Option<super::members::DexFiles>>,
}

impl Session {
    pub fn new(jdwp: Jdwp, info: SessionInfo) -> Arc<Session> {
        let (changed, _) = watch::channel(0);
        Arc::new(Session {
            jdwp,
            info,
            state: Mutex::new(State::default()),
            cache: Mutex::new(Cache::default()),
            changed,
            last_activity: Mutex::new(Instant::now()),
            // The P0 spike confirmed ART honours SourceNameMatch on
            // ClassPrepare although CapabilitiesNew reports
            // canUseSourceNameFilters=false. Opt-out kept for other VMs.
            use_source_name_match: !crate::hostenv::env_truthy(
                "SHADOWDROID_JDWP_NO_SOURCE_NAME_MATCH",
            ),
            initial_breakpoints: Mutex::new(Vec::new()),
            invoke_state: Default::default(),
            watches: Mutex::new((Vec::new(), HashMap::new())),
            coroutine_classes: Mutex::new(None),
            coroutine_pending: Mutex::new(Vec::new()),
            anr: Mutex::new(Default::default()),
            dex_loader: std::sync::OnceLock::new(),
            dex_files: tokio::sync::Mutex::new(None),
            logpoint_log: super::logpoints::LogpointLog::new(
                format!(
                    "logpoints_jdwp_{}_{}",
                    std::process::id(),
                    (crate::events::now_ts() * 1000.0) as u64
                ),
                super::logpoints::DEFAULT_CAPACITY,
            ),
        })
    }

    pub(super) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("session state lock")
    }

    pub(super) fn coroutine_classes_cache(&self) -> Option<super::coroutines::CoroutineClasses> {
        self.coroutine_classes
            .lock()
            .expect("coroutine cache")
            .clone()
    }

    pub(super) fn set_coroutine_classes_cache(&self, classes: super::coroutines::CoroutineClasses) {
        *self.coroutine_classes.lock().expect("coroutine cache") = Some(classes);
    }

    /// Results of the launch-time breakpoints (`__debugd --init`).
    pub fn set_initial_breakpoints(&self, results: Vec<Json>) {
        *self.initial_breakpoints.lock().expect("initial lock") = results;
    }

    pub fn initial_breakpoints(&self) -> Json {
        Json::Array(
            self.initial_breakpoints
                .lock()
                .expect("initial lock")
                .clone(),
        )
    }

    pub(super) fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    pub fn touch(&self) {
        *self.last_activity.lock().expect("activity lock") = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_activity.lock().expect("activity lock").elapsed()
    }

    /// No breakpoints and nothing suspended: safe to exit on idle.
    pub fn is_quiescent(&self) -> bool {
        let state = self.state();
        state.breakpoints.is_empty() && state.suspension.is_none()
    }

    pub fn closed_reason(&self) -> Option<String> {
        self.state().closed.clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// The enabled line and exception breakpoints, as launch-time
    /// breakpoints for `debug attach --relaunch` (logpoints and conditions
    /// stay behind).
    pub fn export_initial_breakpoints(&self) -> super::daemon::InitialBreakpoints {
        let state = self.state();
        let mut out = super::daemon::InitialBreakpoints::default();
        for breakpoint in state.breakpoints.values() {
            if !breakpoint.opts.enabled || breakpoint.opts.is_logpoint() {
                continue;
            }
            match &breakpoint.kind {
                BreakpointKind::Line { target, line } => {
                    out.lines.push(super::daemon::InitialLine {
                        target: target.clone(),
                        line: *line,
                    });
                }
                BreakpointKind::Exception {
                    class,
                    caught,
                    uncaught,
                } => out.exceptions.push(super::daemon::InitialException {
                    class: class.clone(),
                    caught: *caught,
                    uncaught: *uncaught,
                }),
                _ => {}
            }
        }
        out
    }

    /// Attached while running and suspended long enough to risk an ANR.
    pub fn risks_anr(&self) -> bool {
        !self.info.launched_under_debugger
            && self
                .suspension()
                .is_some_and(|s| crate::events::now_ts() - s.at > ANR_WARNING_SECS)
    }

    pub fn suspension(&self) -> Option<Suspension> {
        self.state().suspension.clone()
    }

    pub(super) fn epoch(&self) -> u64 {
        self.state().epoch
    }

    pub(super) fn ensure_open(&self) -> RpcResult<()> {
        match self.closed_reason() {
            Some(reason) => Err(RpcError::new(
                "debuggee_exited",
                format!("the debugged process is gone: {reason}"),
            )
            .next(&["shadowdroid debug attach --backend jdwp --package <pkg>"])),
            None => Ok(()),
        }
    }

    // ── cached type metadata ──────────────────────────────────────────

    pub(super) async fn signature(&self, type_id: u64) -> RpcResult<String> {
        if let Some(found) = self.cache.lock().expect("cache").signatures.get(&type_id) {
            return Ok(found.clone());
        }
        let signature = self.jdwp.signature(type_id).await?;
        self.cache
            .lock()
            .expect("cache")
            .signatures
            .insert(type_id, signature.clone());
        Ok(signature)
    }

    pub(super) async fn source_file(&self, type_id: u64) -> RpcResult<Option<String>> {
        if let Some(found) = self.cache.lock().expect("cache").source_files.get(&type_id) {
            return Ok(found.clone());
        }
        let source = self.jdwp.source_file(type_id).await?;
        self.cache
            .lock()
            .expect("cache")
            .source_files
            .insert(type_id, source.clone());
        Ok(source)
    }

    pub(super) async fn methods(&self, type_id: u64) -> RpcResult<Arc<Vec<MethodInfo>>> {
        if let Some(found) = self.cache.lock().expect("cache").methods.get(&type_id) {
            return Ok(found.clone());
        }
        let methods = Arc::new(self.jdwp.methods(type_id).await?);
        self.cache
            .lock()
            .expect("cache")
            .methods
            .insert(type_id, methods.clone());
        Ok(methods)
    }

    pub(super) async fn fields(&self, type_id: u64) -> RpcResult<Arc<Vec<FieldInfo>>> {
        if let Some(found) = self.cache.lock().expect("cache").fields.get(&type_id) {
            return Ok(found.clone());
        }
        let fields = Arc::new(self.jdwp.fields(type_id).await?);
        self.cache
            .lock()
            .expect("cache")
            .fields
            .insert(type_id, fields.clone());
        Ok(fields)
    }

    pub(super) async fn superclass(&self, type_id: u64) -> RpcResult<Option<u64>> {
        if let Some(found) = self.cache.lock().expect("cache").superclasses.get(&type_id) {
            return Ok(*found);
        }
        // Interfaces and arrays have no ClassType superclass; treat a VM
        // error here as "top of the hierarchy".
        let superclass = self.jdwp.superclass(type_id).await.unwrap_or(None);
        self.cache
            .lock()
            .expect("cache")
            .superclasses
            .insert(type_id, superclass);
        Ok(superclass)
    }

    pub(super) async fn line_table(
        &self,
        type_id: u64,
        method_id: u64,
    ) -> RpcResult<Arc<LineTable>> {
        let key = (type_id, method_id);
        if let Some(found) = self.cache.lock().expect("cache").line_tables.get(&key) {
            return Ok(found.clone());
        }
        let table = Arc::new(self.jdwp.line_table(type_id, method_id).await?);
        self.cache
            .lock()
            .expect("cache")
            .line_tables
            .insert(key, table.clone());
        Ok(table)
    }

    pub(super) async fn variable_table(
        &self,
        type_id: u64,
        method_id: u64,
    ) -> RpcResult<Arc<Vec<Variable>>> {
        let key = (type_id, method_id);
        if let Some(found) = self.cache.lock().expect("cache").variable_tables.get(&key) {
            return Ok(found.clone());
        }
        let table = Arc::new(self.jdwp.variable_table(type_id, method_id).await?);
        self.cache
            .lock()
            .expect("cache")
            .variable_tables
            .insert(key, table.clone());
        Ok(table)
    }

    pub(super) async fn thread_name(&self, thread: u64) -> String {
        if let Some(found) = self.cache.lock().expect("cache").thread_names.get(&thread) {
            return found.clone();
        }
        match self.jdwp.thread_name(thread).await {
            Ok(name) => {
                self.cache
                    .lock()
                    .expect("cache")
                    .thread_names
                    .insert(thread, name.clone());
                name
            }
            Err(_) => format!("<thread {thread}>"),
        }
    }

    /// `(class name, method name, line, source)` of a location.
    pub(super) async fn describe_location(&self, location: &Location) -> Json {
        let class = self
            .signature(location.class_id)
            .await
            .map(|s| resolve::type_name(&s))
            .ok();
        let method = self
            .methods(location.class_id)
            .await
            .ok()
            .and_then(|methods| {
                methods
                    .iter()
                    .find(|m| m.method_id == location.method_id)
                    .map(|m| (m.name.clone(), m.signature.clone()))
            });
        let line = self
            .line_table(location.class_id, location.method_id)
            .await
            .ok()
            .and_then(|table| table.line_at(location.index));
        let source = self.source_file(location.class_id).await.ok().flatten();
        // JDWP reports a native frame's index as -1 (u64::MAX on the wire).
        let native = location.is_native();
        let mut value = json!({
            "class": class,
            "method": method.as_ref().map(|(name, _)| name),
            "method_signature": method.as_ref().map(|(_, signature)| signature),
            "line": if native { None } else { line },
            "source": source,
            "code_index": if native { None } else { Some(location.index) },
        });
        if native {
            value["native"] = json!(true);
        }
        value
    }

    // ── status ────────────────────────────────────────────────────────

    pub async fn status(&self) -> Json {
        let suspension = self.suspension();
        let position = match suspension.as_ref().and_then(|s| s.location) {
            Some(location) => self.describe_location(&location).await,
            None => Json::Null,
        };
        let thread_name = match suspension.as_ref().and_then(|s| s.thread) {
            Some(thread) => Some(self.thread_name(thread).await),
            None => None,
        };
        // Pinned so the thrown object stays inspectable for this suspension.
        let exception_handle = match suspension
            .as_ref()
            .and_then(|s| s.exception)
            .and_then(|e| e.object_id())
        {
            Some(object) if object != 0 => self.pin(object).await,
            _ => None,
        };
        let slow_requests = self.slow_requests();
        // An attach-to-running process still has ANR timers: a stop with
        // pending input shows "Application Not Responding" after ~15 s.
        let anr = self.anr_report();
        let anr_warning = if anr.is_some() {
            Some(self.anr_warning())
        } else {
            suspension.as_ref().and_then(|s| {
                let held = crate::events::now_ts() - s.at;
                (!self.info.launched_under_debugger && held > ANR_WARNING_SECS).then(|| {
                    format!(
                        "suspended for {held:.0} s in a process attached while running: Android shows an ANR dialog for pending input; for a long inspection restart it under the debugger: {}",
                        super::anr::relaunch_action(self.info.package.as_deref())
                    )
                })
            })
        };
        let state = self.state();
        json!({
            "id": self.info.session_id,
            "epoch": state.epoch,
            "launched_under_debugger": self.info.launched_under_debugger,
            "warning": anr_warning,
            "anr": anr,
            "index": 0,
            "name": self.info.package.clone().unwrap_or_else(|| format!("pid {}", self.info.pid)),
            "backend": "jdwp",
            "device": {"serial": self.info.serial},
            "pid": self.info.pid,
            "package": self.info.package,
            "attached_at": self.info.attached_at,
            "suspended": suspension.is_some(),
            "suspend_reason": suspension.as_ref().map(|s| s.reason),
            "suspended_at": suspension.as_ref().map(|s| s.at),
            "exception_id": suspension.as_ref().and_then(|s| s.exception).and_then(|e| e.object_id()),
            "exception_handle": exception_handle,
            "exception_expression": exception_handle.as_ref().map(|_| super::inspect::EXCEPTION_ROOT),
            "breakpoint_id": suspension.as_ref().and_then(|s| s.breakpoint_id.clone()),
            "thread": thread_name,
            "position": position,
            "breakpoints": state.breakpoints.len(),
            "live_handles": state.pinned.len(),
            "events_seen": state.events_seen,
            "slow_requests": slow_requests,
            "slow_watch_warning": (slow_requests > 0).then_some(SLOW_WATCH_WARNING),
            "invoke": self.invoke_stats(),
            "closed": state.closed,
        })
    }

    pub fn details(&self) -> Json {
        let state = self.state();
        json!({
            "vm": self.info.vm,
            "capabilities": self.info.capabilities,
            "recent_events": state.recent_events,
        })
    }

    // ── breakpoints ───────────────────────────────────────────────────

    pub fn breakpoints(&self) -> Json {
        let state = self.state();
        Json::Array(
            state
                .breakpoints
                .values()
                .map(Breakpoint::to_json)
                .collect(),
        )
    }

    pub(super) fn allocate_breakpoint_id(&self) -> String {
        let mut state = self.state();
        state.next_breakpoint += 1;
        format!("bp_{}", state.next_breakpoint)
    }

    /// Set a line breakpoint. Binds every loaded location now and keeps a
    /// ClassPrepare request armed so classes loaded later (lambdas, lazily
    /// loaded activities) bind on prepare.
    pub async fn break_line(
        &self,
        target: SourceTarget,
        line: u32,
        opts: BreakpointOptions,
    ) -> RpcResult<Json> {
        self.ensure_open()?;
        if target.inline_body {
            return Err(RpcError::new(
                "unsupported_location",
                format!(
                    "{}:{line} is inside a Kotlin inline function body; inlined copies are not resolvable yet",
                    target.basename
                ),
            )
            .detail(json!({"reason": "inline_body", "file": target.basename, "line": line}))
            .next(&["set the breakpoint at the call site of the inline function instead"]));
        }
        let id = self.allocate_breakpoint_id();
        // Registered before the scan so a class prepared meanwhile binds
        // through ClassPrepare instead of falling between the two paths.
        self.state().breakpoints.insert(
            id.clone(),
            Breakpoint::new(
                id.clone(),
                BreakpointKind::Line {
                    target: target.clone(),
                    line,
                },
                opts,
            ),
        );
        let deferred = self.ensure_line_prepare(&target).await;
        let scanned = self.scan_loaded_classes(&id, &target, line).await;
        let source_matched = match scanned {
            Ok(count) => count,
            Err(error) => {
                let _ = self.remove_breakpoint(&id).await;
                return Err(error);
            }
        };
        let unbound = {
            let mut state = self.state();
            let breakpoint = state.breakpoints.get_mut(&id).expect("registered above");
            if breakpoint.locations.is_empty() {
                breakpoint.pending_reason = Some(if source_matched == 0 {
                    "class_not_loaded"
                } else if breakpoint.note == Some(VARIANT_MATCHES_NOTHING_NOTE) {
                    "no_location_for_variant"
                } else {
                    "line_not_in_loaded_classes"
                });
            }
            breakpoint.locations.is_empty()
        };
        if unbound && deferred.is_none() {
            // Nothing bound and no way to bind later: do not leave a
            // breakpoint that can never fire.
            let _ = self.remove_breakpoint(&id).await;
            return Err(RpcError::new(
                "breakpoint_unresolved",
                format!(
                    "no loaded class holds {}:{line}, and the VM rejected deferred binding",
                    target.basename
                ),
            )
            .detail(json!({"file": target.basename, "line": line, "package": target.package}))
            .next(&["pass --project-root so the file's package can be read"]));
        }
        self.touch();
        let value = self
            .state()
            .breakpoints
            .get(&id)
            .map(Breakpoint::to_json)
            .unwrap_or(Json::Null);
        Ok(value)
    }

    /// Bind `line` in every loaded class compiled from the target file;
    /// returns how many classes matched the source file name.
    async fn scan_loaded_classes(
        &self,
        id: &str,
        target: &SourceTarget,
        line: u32,
    ) -> RpcResult<usize> {
        let classes = self.jdwp.all_classes().await?;
        let mut source_matched = 0usize;
        for class in classes
            .iter()
            .filter(|c| resolve::is_candidate(&c.signature, target))
        {
            self.cache
                .lock()
                .expect("cache")
                .signatures
                .insert(class.type_id, class.signature.clone());
            // `R$*` and synthetic classes answer ABSENT_INFORMATION: skip.
            match self.source_file(class.type_id).await {
                Ok(Some(source)) if source == target.basename => {}
                Ok(_) => continue,
                Err(error) if error.code == "debug_info_absent" => continue,
                Err(error) => return Err(error),
            }
            source_matched += 1;
            if self.already_bound(id, class.type_id) {
                continue;
            }
            let locations = self.bind_class(id, class.type_id, line).await?;
            if let Some(breakpoint) = self.state().breakpoints.get_mut(id) {
                breakpoint.locations.extend(locations);
            }
        }
        Ok(source_matched)
    }

    fn already_bound(&self, id: &str, class_id: u64) -> bool {
        self.state()
            .breakpoints
            .get(id)
            .is_some_and(|b| b.locations.iter().any(|l| l.class_id == class_id))
    }

    /// Set breakpoint requests at `line` in every method of `class_id`.
    async fn bind_class(
        &self,
        id: &str,
        class_id: u64,
        line: u32,
    ) -> RpcResult<Vec<BoundLocation>> {
        let signature = self.signature(class_id).await?;
        let methods = self.methods(class_id).await?;
        let variant = self
            .state()
            .breakpoints
            .get(id)
            .map(|b| b.opts.variant)
            .unwrap_or_default();
        // Candidate methods holding the line, with their lambda depth.
        let mut candidates = Vec::new();
        for method in methods.iter() {
            if super::lambdas::is_bridge(method) {
                continue;
            }
            let table = self.line_table(class_id, method.method_id).await?;
            let Some(index) = table.first_index_of(line as i32) else {
                continue;
            };
            let depth = self.lambda_depth(class_id, &signature, method).await;
            candidates.push((method, index, depth));
        }
        let had_code = !candidates.is_empty();
        let candidates = super::lambdas::select_variant(candidates, variant);
        if had_code && candidates.is_empty() {
            // The line has code here, just none the variant asks for (an
            // `--variant outer` on a line that only holds lambdas).
            if let Some(b) = self.state().breakpoints.get_mut(id) {
                b.note = Some(VARIANT_MATCHES_NOTHING_NOTE);
            }
        }
        let mut bound = Vec::new();
        for (method, index, depth) in candidates {
            let location = Location {
                type_tag: protocol::type_tag::CLASS,
                class_id,
                method_id: method.method_id,
                index,
            };
            let claim = (id.to_string(), class_id, method.method_id, index);
            if !self.state().claims.insert(claim.clone()) {
                continue;
            }
            let Some((kind, opts)) = self
                .state()
                .breakpoints
                .get(id)
                .map(|b| (b.kind.clone(), b.opts.clone()))
            else {
                self.state().claims.remove(&claim);
                continue;
            };
            let armed = if opts.enabled {
                self.arm(id, &kind, Arm::Line(location), &opts)
                    .await
                    .map(Some)
            } else {
                Ok(None)
            };
            let request_id = match armed {
                Ok(request_id) => request_id,
                Err(error) => {
                    self.state().claims.remove(&claim);
                    return Err(error);
                }
            };
            let removed_meanwhile = {
                let mut state = self.state();
                if state.breakpoints.contains_key(id) {
                    false
                } else {
                    state.claims.remove(&claim);
                    if let Some(request) = request_id {
                        state.owners.remove(&request);
                    }
                    true
                }
            };
            if removed_meanwhile {
                // Removed while this request was in flight.
                if let Some(request) = request_id {
                    let _ = self.jdwp.clear_event(event_kind::BREAKPOINT, request).await;
                }
                continue;
            }
            bound.push(BoundLocation {
                request_id,
                class: resolve::type_name(&signature),
                method: method.name.clone(),
                code_index: index,
                role: None,
                kind: None,
                lambda: depth > 0,
                lambda_depth: depth,
                class_id,
                arm: Arm::Line(location),
            });
        }
        Ok(bound)
    }

    /// One ClassPrepare request per source file name, shared by every line
    /// breakpoint in that file. `None` when the VM rejects every variant.
    async fn ensure_line_prepare(&self, target: &SourceTarget) -> Option<i32> {
        if let Some(existing) = self.state().line_prepares.get(&target.basename) {
            return Some(*existing);
        }
        let package = resolve::package_pattern(target);
        let mut attempts: Vec<Vec<Modifier>> = Vec::new();
        if self.use_source_name_match {
            let mut mods = Vec::new();
            if let Some(pattern) = &package {
                mods.push(Modifier::ClassMatch(pattern.clone()));
            }
            mods.push(Modifier::SourceNameMatch(target.basename.clone()));
            attempts.push(mods);
        }
        if let Some(pattern) = &package {
            attempts.push(vec![Modifier::ClassMatch(pattern.clone())]);
        }
        for modifiers in attempts {
            match self
                .jdwp
                .set_event(
                    event_kind::CLASS_PREPARE,
                    suspend_policy::EVENT_THREAD,
                    &modifiers,
                )
                .await
            {
                Ok(request_id) => {
                    let mut state = self.state();
                    state
                        .owners
                        .insert(request_id, Owner::LinePrepare(target.basename.clone()));
                    state
                        .line_prepares
                        .insert(target.basename.clone(), request_id);
                    return Some(request_id);
                }
                Err(error) => {
                    tracing::warn!("ClassPrepare {modifiers:?} rejected: {error}");
                }
            }
        }
        None
    }

    pub async fn break_exception(
        &self,
        class: &str,
        caught: bool,
        uncaught: bool,
        opts: BreakpointOptions,
    ) -> RpcResult<Json> {
        self.ensure_open()?;
        opts.validate()?;
        let id = self.allocate_breakpoint_id();
        let kind = BreakpointKind::Exception {
            class: class.to_string(),
            caught,
            uncaught,
        };
        self.state()
            .breakpoints
            .insert(id.clone(), Breakpoint::new(id.clone(), kind, opts));
        let signature = format!("L{};", class.replace('.', "/"));
        let loaded = match self.jdwp.classes_by_signature(&signature).await {
            Ok(loaded) => loaded,
            Err(error) => {
                self.state().breakpoints.remove(&id);
                return Err(error.into());
            }
        };
        if let Some(class_info) = loaded.first() {
            let location = self.set_exception_request(&id, class_info.type_id).await?;
            if let Some(breakpoint) = self.state().breakpoints.get_mut(&id) {
                breakpoint.locations.push(location);
            }
        } else {
            let request_id = self
                .jdwp
                .set_event(
                    event_kind::CLASS_PREPARE,
                    suspend_policy::EVENT_THREAD,
                    &[Modifier::ClassMatch(class.to_string())],
                )
                .await?;
            let mut state = self.state();
            state
                .owners
                .insert(request_id, Owner::ExceptionPrepare(id.clone()));
            if let Some(breakpoint) = state.breakpoints.get_mut(&id) {
                breakpoint.pending_reason = Some("class_not_loaded");
            }
        }
        self.touch();
        Ok(self
            .state()
            .breakpoints
            .get(&id)
            .map(Breakpoint::to_json)
            .unwrap_or(Json::Null))
    }

    async fn set_exception_request(&self, id: &str, type_id: u64) -> RpcResult<BoundLocation> {
        let (kind, opts) = self
            .state()
            .breakpoints
            .get(id)
            .map(|b| (b.kind.clone(), b.opts.clone()))
            .ok_or_else(|| RpcError::new("breakpoint_not_found", format!("no breakpoint {id}")))?;
        let request_id = if opts.enabled {
            Some(self.arm(id, &kind, Arm::Exception(type_id), &opts).await?)
        } else {
            None
        };
        let class = self
            .signature(type_id)
            .await
            .map(|s| resolve::type_name(&s))
            .unwrap_or_default();
        Ok(BoundLocation {
            request_id,
            class,
            method: String::new(),
            code_index: 0,
            role: None,
            kind: None,
            lambda: false,
            lambda_depth: 0,
            class_id: type_id,
            arm: Arm::Exception(type_id),
        })
    }

    pub async fn remove_breakpoint(&self, id: &str) -> RpcResult<Json> {
        let removed = {
            let mut state = self.state();
            state.claims.retain(|(owner, ..)| owner != id);
            state.breakpoints.remove(id)
        };
        let Some(breakpoint) = removed else {
            return Err(RpcError::new(
                "breakpoint_not_found",
                format!("no JDWP breakpoint with id {id}"),
            )
            .next(&["shadowdroid debug breakpoints --backend jdwp"]));
        };
        for location in &breakpoint.locations {
            if let Some(request) = location.request_id {
                let _ = self
                    .jdwp
                    .clear_event(location.arm.event_kind(), request)
                    .await;
                self.state().owners.remove(&request);
            }
        }
        // Exception breakpoints still waiting for their class.
        let pending: Vec<i32> = self
            .state()
            .owners
            .iter()
            .filter(|(_, owner)| {
                matches!(owner, Owner::ExceptionPrepare(owner_id) | Owner::MemberPrepare(owner_id) if owner_id == id)
            })
            .map(|(request, _)| *request)
            .collect();
        for request in pending {
            let _ = self
                .jdwp
                .clear_event(event_kind::CLASS_PREPARE, request)
                .await;
            self.state().owners.remove(&request);
        }
        if let BreakpointKind::Line { target, .. } = &breakpoint.kind {
            let still_used = self.state().breakpoints.values().any(|b| {
                matches!(&b.kind, BreakpointKind::Line { target: t, .. } if t.basename == target.basename)
            });
            if !still_used {
                let request = self.state().line_prepares.remove(&target.basename);
                if let Some(request) = request {
                    let _ = self
                        .jdwp
                        .clear_event(event_kind::CLASS_PREPARE, request)
                        .await;
                    self.state().owners.remove(&request);
                }
            }
        }
        self.touch();
        Ok(json!({"id": id, "removed": true}))
    }

    // ── execution control ─────────────────────────────────────────────

    pub async fn pause(&self) -> RpcResult<Json> {
        self.ensure_open()?;
        if self.suspension().is_none() {
            self.jdwp.suspend().await?;
            let mut state = self.state();
            state.epoch += 1;
            state.suspension = Some(Suspension {
                reason: "pause",
                thread: None,
                location: None,
                breakpoint_id: None,
                exception: None,
                at: crate::events::now_ts(),
            });
            drop(state);
            self.bump();
        }
        self.touch();
        Ok(self.status().await)
    }

    /// Clear the suspension, release pinned handles, and resume the VM. The
    /// state is cleared first so a fast next event is never lost.
    async fn resume_vm(&self) -> RpcResult<()> {
        let pinned: Vec<u64> = {
            let mut state = self.state();
            state.suspension = None;
            state.epoch += 1;
            std::mem::take(&mut state.pinned).into_iter().collect()
        };
        self.bump();
        for object in pinned {
            let _ = self.jdwp.enable_collection(object).await;
        }
        self.jdwp.resume().await?;
        Ok(())
    }

    pub async fn resume(&self) -> RpcResult<Json> {
        self.ensure_open()?;
        let was_suspended = self.suspension().is_some();
        if was_suspended {
            self.resume_vm().await?;
        }
        self.touch();
        let mut status = self.status().await;
        if !was_suspended {
            status["warning"] = json!("session was not suspended");
        }
        Ok(status)
    }

    /// Step over/into/out on the suspended thread and wait for the next stop.
    pub async fn step(
        &self,
        depth: &str,
        thread: Option<u64>,
        timeout: Duration,
    ) -> RpcResult<Json> {
        self.ensure_open()?;
        let suspension = self.suspension().ok_or_else(RpcError::not_suspended)?;
        let thread = match thread.or(suspension.thread) {
            Some(thread) => thread,
            None => self.default_thread().await?,
        };
        let depth_code = match depth {
            "into" => step::DEPTH_INTO,
            "out" => step::DEPTH_OUT,
            _ => step::DEPTH_OVER,
        };
        let mut modifiers = vec![Modifier::Step {
            thread,
            size: step::SIZE_LINE,
            depth: depth_code,
        }];
        if depth_code == step::DEPTH_INTO {
            modifiers.extend(
                DEFAULT_STEP_FILTERS
                    .iter()
                    .map(|pattern| Modifier::ClassExclude((*pattern).to_string())),
            );
        }
        // ART ignores a Count that precedes ClassExclude (the request never
        // reports): Count must be the last modifier.
        modifiers.push(Modifier::Count(1));
        let request_id = self
            .jdwp
            .set_event(event_kind::SINGLE_STEP, suspend_policy::ALL, &modifiers)
            .await?;
        self.state().owners.insert(request_id, Owner::Step);
        let mut changes = self.subscribe();
        let epoch = self.epoch();
        let resumed = self.resume_vm().await;
        let outcome = match resumed {
            Ok(()) => self.wait_for_stop(&mut changes, epoch + 1, timeout).await,
            Err(error) => Err(error),
        };
        let _ = self
            .jdwp
            .clear_event(event_kind::SINGLE_STEP, request_id)
            .await;
        self.state().owners.remove(&request_id);
        self.touch();
        match outcome {
            Ok(()) => {
                let mut status = self.status().await;
                status["action"] = json!(format!("step_{depth}"));
                Ok(status)
            }
            Err(error) => Err(error),
        }
    }

    /// Wait until a suspension newer than `after_epoch` exists.
    pub(super) async fn wait_for_stop(
        &self,
        changes: &mut watch::Receiver<u64>,
        after_epoch: u64,
        timeout: Duration,
    ) -> RpcResult<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let state = self.state();
                if state.suspension.is_some() && state.epoch > after_epoch {
                    return Ok(());
                }
                if let Some(reason) = &state.closed {
                    return Err(RpcError::new(
                        "debuggee_exited",
                        format!("the process ended while stepping: {reason}"),
                    ));
                }
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return Err(RpcError::new("debugger_disconnected", "session ended")),
                Err(_) => {
                    return Err(RpcError::new(
                        "debugger_timeout",
                        format!(
                            "no step event within {} ms; the thread is running (blocked in a call, or the method returned into filtered code)",
                            timeout.as_millis()
                        ),
                    )
                    .retryable()
                    .detail(json!({"timeout_ms": timeout.as_millis() as u64, "command": "step"}))
                    .next(&[
                        "shadowdroid debug pause --backend jdwp",
                        "shadowdroid debug status --backend jdwp",
                    ]));
                }
            }
        }
    }

    /// The thread to read when a suspension has none (after `pause`): the
    /// app's `main` thread, else the first thread.
    pub(super) async fn default_thread(&self) -> RpcResult<u64> {
        let threads = self.jdwp.all_threads().await?;
        for thread in &threads {
            if self.thread_name(*thread).await == "main" {
                return Ok(*thread);
            }
        }
        threads
            .first()
            .copied()
            .ok_or_else(|| RpcError::new("debugger_no_threads", "the VM reported no threads"))
    }

    // ── handles ───────────────────────────────────────────────────────

    /// Pin `object` so its id stays valid until resume; returns the handle.
    pub(super) async fn pin(&self, object: u64) -> Option<String> {
        {
            let state = self.state();
            if state.pinned.contains(&object) {
                return Some(handle_for(object));
            }
            if state.pinned.len() >= MAX_LIVE_HANDLES || state.suspension.is_none() {
                return None;
            }
        }
        if self.jdwp.disable_collection(object).await.is_err() {
            return None;
        }
        self.state().pinned.insert(object);
        Some(handle_for(object))
    }

    pub(super) async fn resolve_handle(&self, handle: &str) -> RpcResult<u64> {
        let object = parse_handle(handle).ok_or_else(|| {
            RpcError::new(
                "invalid_object_handle",
                format!("not an object handle: {handle}"),
            )
        })?;
        if !self.state().pinned.contains(&object) {
            return Err(RpcError::new(
                "stale_object_handle",
                format!("{handle} is not valid in this suspension (handles expire on resume)"),
            )
            .next(&["shadowdroid debug variables --backend jdwp"]));
        }
        if self.jdwp.is_collected(object).await.unwrap_or(true) {
            return Err(RpcError::new(
                "stale_object_handle",
                format!("{handle} was garbage collected"),
            ));
        }
        Ok(object)
    }

    // ── event loop ────────────────────────────────────────────────────

    pub async fn run_events(self: Arc<Self>, mut incoming: mpsc::UnboundedReceiver<Incoming>) {
        // The forwarder runs beside the handler: while an invoke runs (often
        // started by the handler itself, for a condition), events the invoked
        // code raises on its thread are resumed here, never queued behind
        // the busy handler — otherwise the invoke could never return.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let forwarder = {
            let session = self.clone();
            tokio::spawn(async move {
                while let Some(message) = incoming.recv().await {
                    if let Incoming::Command(command) = &message
                        && command.command_set == protocol::set::EVENT
                        && command.command == protocol::event::COMPOSITE
                        && let Some(thread) = session.invoking_thread()
                        && let Ok(composite) = Composite::parse(&command.data, session.jdwp.sizes())
                        && !composite.events.is_empty()
                        && composite.events.iter().all(|e| e.thread() == Some(thread))
                    {
                        session.absorb_during_invoke(composite, thread).await;
                        continue;
                    }
                    if tx.send(message).is_err() {
                        break;
                    }
                }
            })
        };
        self.handle_incoming(&mut rx).await;
        forwarder.abort();
    }

    /// Resume what an invoked method's own events suspended: breakpoints in
    /// app code an evaluation calls are not stops (as in IntelliJ).
    async fn absorb_during_invoke(&self, composite: Composite, thread: u64) {
        for event in &composite.events {
            self.note_during_invoke(event);
        }
        {
            let mut state = self.state();
            for event in &composite.events {
                state.events_seen += 1;
                if state.recent_events.len() >= RECENT_EVENTS {
                    state.recent_events.pop_front();
                }
                let mut summary = event.to_json();
                summary["skipped_during_invoke"] = json!(true);
                state.recent_events.push_back(summary);
            }
        }
        let resumed = match composite.suspend_policy {
            suspend_policy::ALL => self.jdwp.resume().await,
            suspend_policy::EVENT_THREAD => self.jdwp.thread_resume(thread).await,
            _ => Ok(()),
        };
        if let Err(error) = resumed {
            tracing::warn!("resuming an event raised during an invoke: {error}");
        }
    }

    async fn handle_incoming(&self, incoming: &mut mpsc::UnboundedReceiver<Incoming>) {
        while let Some(message) = incoming.recv().await {
            match message {
                Incoming::Command(command)
                    if command.command_set == protocol::set::EVENT
                        && command.command == protocol::event::COMPOSITE =>
                {
                    match Composite::parse(&command.data, self.jdwp.sizes()) {
                        Ok(composite) => self.handle_composite(composite).await,
                        Err(error) => {
                            tracing::warn!("unparseable composite event: {error}");
                        }
                    }
                }
                Incoming::Command(command) => {
                    tracing::debug!(
                        "ignoring VM command #{} {}.{} ({} bytes)",
                        command.id,
                        command.command_set,
                        command.command,
                        command.data.len()
                    );
                }
                Incoming::Closed(reason) => {
                    tracing::info!("JDWP connection closed: {reason}");
                    {
                        let mut state = self.state();
                        state.closed = Some(reason);
                        state.suspension = None;
                        state.pinned.clear();
                    }
                    self.bump();
                    break;
                }
            }
        }
    }

    pub(super) async fn handle_composite(&self, composite: Composite) {
        self.drain_deferred().await;
        let mut stopped = false;
        let mut thread_to_resume = None;
        // One stop per (breakpoint, thread, location) per composite, even if
        // two requests of the same breakpoint sit at that location.
        let mut stops = BTreeSet::new();
        for event in &composite.events {
            if let Event::Breakpoint {
                thread, location, ..
            } = event
            {
                let owner = self.state().owners.get(&event.request_id()).cloned();
                if let Some(Owner::Breakpoint(id)) = owner
                    && !stops.insert((
                        id,
                        *thread,
                        location.class_id,
                        location.method_id,
                        location.index,
                    ))
                {
                    continue;
                }
            }
            {
                let mut state = self.state();
                state.events_seen += 1;
                if state.recent_events.len() >= RECENT_EVENTS {
                    state.recent_events.pop_front();
                }
                state.recent_events.push_back(event.to_json());
            }
            if thread_to_resume.is_none() {
                thread_to_resume = event.thread();
            }
            match self.handle_event(event).await {
                Ok(true) => stopped = true,
                Ok(false) => {}
                Err(error) => tracing::warn!("handling {}: {}", event.kind_name(), error.message),
            }
        }
        if let Some(error) = &composite.error {
            tracing::warn!("composite parsed partially: {error}");
        }
        if stopped {
            self.bump();
            // Watches are evaluated on every stop, as in Studio.
            self.refresh_watches(super::inspect::RenderOptions::new(1, 48, 24))
                .await;
        } else {
            let result = match composite.suspend_policy {
                suspend_policy::ALL => self.jdwp.resume().await,
                suspend_policy::EVENT_THREAD => match thread_to_resume {
                    Some(thread) => self.jdwp.thread_resume(thread).await,
                    None => self.jdwp.resume().await,
                },
                _ => Ok(()),
            };
            if let Err(error) = result {
                tracing::warn!("resuming after an internal event: {error}");
            }
        }
        self.touch();
    }

    /// Returns whether the event is a user-visible stop.
    pub(super) async fn handle_event(&self, event: &Event) -> RpcResult<bool> {
        let owner = self.state().owners.get(&event.request_id()).cloned();
        match (event, owner) {
            (
                Event::Breakpoint {
                    thread, location, ..
                },
                Some(Owner::Breakpoint(id)),
            ) => {
                self.on_hit(&id, event.request_id(), *thread, *location, None)
                    .await
            }
            (
                Event::Exception {
                    thread,
                    location,
                    exception,
                    catch_location,
                    ..
                },
                Some(Owner::Breakpoint(id)),
            ) => {
                let uncaught_only = matches!(
                    self.state().breakpoints.get(&id).map(|b| &b.kind),
                    Some(BreakpointKind::Exception {
                        caught: false,
                        uncaught: true,
                        ..
                    })
                );
                if uncaught_only {
                    if !self
                        .escapes_app_code(*thread, catch_location.as_ref())
                        .await
                    {
                        return Ok(false);
                    }
                    if let Some(object) = exception.object_id() {
                        let mut state = self.state();
                        if state.reported_throwables.contains(&object) {
                            return Ok(false);
                        }
                        if state.reported_throwables.len() >= RECENT_EVENTS {
                            state.reported_throwables.pop_front();
                        }
                        state.reported_throwables.push_back(object);
                    }
                }
                self.on_hit(
                    &id,
                    event.request_id(),
                    *thread,
                    *location,
                    Some(*exception),
                )
                .await
            }
            (
                Event::SingleStep {
                    thread, location, ..
                },
                Some(Owner::Step),
            ) => {
                // A breakpoint at the same location wins the reason.
                if self.suspension().is_none() {
                    self.record_stop("step", Some(*thread), Some(*location), None, None);
                }
                Ok(true)
            }
            (
                Event::ClassPrepare {
                    type_id, signature, ..
                },
                Some(Owner::LinePrepare(basename)),
            ) => {
                self.bind_prepared_class(&basename, *type_id, signature)
                    .await?;
                Ok(false)
            }
            (Event::ClassPrepare { type_id, .. }, Some(Owner::ExceptionPrepare(id))) => {
                let class = self.state().breakpoints.get(&id).map(|b| b.kind.clone());
                if let Some(BreakpointKind::Exception { .. }) = class {
                    let location = self.set_exception_request(&id, *type_id).await?;
                    {
                        let mut state = self.state();
                        if let Some(breakpoint) = state.breakpoints.get_mut(&id) {
                            breakpoint.locations.push(location);
                            breakpoint.pending_reason = None;
                        }
                        state.owners.remove(&event.request_id());
                    }
                    let _ = self
                        .jdwp
                        .clear_event(event_kind::CLASS_PREPARE, event.request_id())
                        .await;
                }
                Ok(false)
            }
            (
                Event::ClassPrepare {
                    type_id, signature, ..
                },
                Some(Owner::MemberPrepare(id)),
            ) => {
                self.bind_member(&id, *type_id, signature).await?;
                Ok(false)
            }
            (
                Event::ClassPrepare {
                    type_id, signature, ..
                },
                Some(Owner::CoroutinePrepare),
            ) => {
                self.note_loaded_class(*type_id, signature);
                Ok(false)
            }
            (
                Event::Field {
                    thread, location, ..
                },
                Some(Owner::Breakpoint(id)),
            ) => {
                self.on_hit(&id, event.request_id(), *thread, *location, None)
                    .await
            }
            (Event::VmDeath { .. }, _) => {
                let mut state = self.state();
                state.closed = Some("VM death".into());
                state.suspension = None;
                drop(state);
                self.bump();
                Ok(false)
            }
            (other, owner) => {
                tracing::debug!(
                    "event {} for request {} ({owner:?}) not handled",
                    other.kind_name(),
                    other.request_id()
                );
                Ok(false)
            }
        }
    }

    /// Whether an app frame catches the exception. A catch in framework or
    /// library code (the same packages step-into skips) still lets it crash
    /// the app on Android, so it counts as uncaught.
    /// Whether an exception escapes app code: an uncaught-only breakpoint's
    /// "uncaught". On Android a crash is always caught somewhere in framework
    /// code that rethrows it (Looper.loopOnce, Compose pointer dispatch,
    /// AccessibilityInteractionController), so "no catch location" is not
    /// enough; but framework code also throws and catches internally all the
    /// time (`ErrnoException` inside `File.exists`). The rule: no catcher, or
    /// a catcher in app code is decided directly; a framework catcher counts
    /// only when the exception unwinds through at least one app frame on the
    /// way to it.
    async fn escapes_app_code(&self, thread: u64, catch_location: Option<&Location>) -> bool {
        if self.thrown_by_class_resolution(thread).await {
            return false;
        }
        let Some(catch) = catch_location else {
            return true;
        };
        let catcher_is_app = match self.signature(catch.class_id).await {
            Ok(signature) => !is_framework_class(&resolve::type_name(&signature)),
            // Unknown catcher: report the stop rather than hide a crash.
            Err(_) => return true,
        };
        if catcher_is_app {
            return false;
        }
        let Ok(frames) = self.jdwp.frames(thread, 0, -1).await else {
            return true;
        };
        let catching = frames.iter().position(|(_, location)| {
            location.class_id == catch.class_id && location.method_id == catch.method_id
        });
        let Some(catching) = catching else {
            return true;
        };
        for (_, location) in &frames[..catching] {
            match self.signature(location.class_id).await {
                Ok(signature) if is_framework_class(&resolve::type_name(&signature)) => {}
                // An app frame (or one we cannot name) unwinds.
                _ => return true,
            }
        }
        false
    }

    /// Whether the exception was thrown inside class loading the runtime
    /// started to resolve a class for an instruction (`sget`, `new-instance`,
    /// `const-class`, …) rather than an explicit `loadClass`/`forName` call.
    /// ART catches those in native code and throws `NoClassDefFoundError`
    /// at the instruction instead, but JDWP reports the next Java handler up
    /// the stack as the catcher: a library probing for optional classes
    /// (okhttp's platform detection during startup) looks like a crash.
    async fn thrown_by_class_resolution(&self, thread: u64) -> bool {
        let Ok(frames) = self.jdwp.frames(thread, 0, 16).await else {
            return false;
        };
        let mut loader_frames = 0;
        for (_, location) in &frames {
            let Ok(signature) = self.signature(location.class_id).await else {
                return false;
            };
            if is_class_loading_class(&resolve::type_name(&signature)) {
                loader_frames += 1;
                continue;
            }
            if loader_frames == 0 || location.is_native() {
                return false;
            }
            let Ok(code) = self
                .jdwp
                .bytecodes(location.class_id, location.method_id)
                .await
            else {
                return false;
            };
            let at = usize::try_from(location.index).unwrap_or(usize::MAX);
            return code
                .get(at.saturating_mul(2))
                .is_some_and(|opcode| !super::dex::is_invoke(*opcode));
        }
        false
    }

    pub(super) fn record_stop(
        &self,
        reason: &'static str,
        thread: Option<u64>,
        location: Option<Location>,
        breakpoint_id: Option<String>,
        exception: Option<Value>,
    ) {
        let mut state = self.state();
        state.epoch += 1;
        state.suspension = Some(Suspension {
            reason,
            thread,
            location,
            breakpoint_id,
            exception,
            at: crate::events::now_ts(),
        });
    }

    async fn bind_prepared_class(
        &self,
        basename: &str,
        type_id: u64,
        signature: &str,
    ) -> RpcResult<()> {
        self.cache
            .lock()
            .expect("cache")
            .signatures
            .insert(type_id, signature.to_string());
        let pending: Vec<(String, SourceTarget, u32)> = self
            .state()
            .breakpoints
            .values()
            .filter_map(|b| match &b.kind {
                BreakpointKind::Line { target, line }
                    if target.basename == basename
                        && !b.locations.iter().any(|l| l.class_id == type_id) =>
                {
                    Some((b.id.clone(), target.clone(), *line))
                }
                _ => None,
            })
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        match self.source_file(type_id).await {
            Ok(Some(source)) if source == basename => {}
            Ok(_) => return Ok(()),
            Err(error) if error.code == "debug_info_absent" => return Ok(()),
            Err(error) => return Err(error),
        }
        for (id, target, line) in pending {
            if !resolve::is_candidate(signature, &target) {
                continue;
            }
            let locations = self.bind_class(&id, type_id, line).await?;
            if locations.is_empty() {
                continue;
            }
            let mut state = self.state();
            if let Some(breakpoint) = state.breakpoints.get_mut(&id) {
                breakpoint.locations.extend(locations);
                breakpoint.pending_reason = None;
            }
        }
        Ok(())
    }

    // ── teardown ──────────────────────────────────────────────────────

    /// Dispose the connection: the VM clears every request and resumes.
    /// EOF right after the Dispose reply is the normal close.
    pub async fn dispose(&self) -> RpcResult<()> {
        if self.closed_reason().is_some() {
            return Ok(());
        }
        match self.jdwp.dispose(Duration::from_secs(3)).await {
            Ok(()) | Err(JdwpError::Closed(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Non-blocking dispose for `Drop` paths.
    pub fn dispose_nowait(&self) {
        if self.closed_reason().is_none() {
            self.jdwp.dispose_nowait();
        }
    }

    /// Release handles before an explicit detach (Dispose would release
    /// them too; doing it first keeps the VM-side count balanced if Dispose
    /// is not honoured).
    pub async fn release_handles(&self) {
        let pinned: Vec<u64> = std::mem::take(&mut self.state().pinned)
            .into_iter()
            .collect();
        for object in pinned {
            let _ = self.jdwp.enable_collection(object).await;
        }
    }

    #[cfg(test)]
    pub(super) fn backdate_suspension(&self, seconds: f64) {
        if let Some(suspension) = self.state().suspension.as_mut() {
            suspension.at -= seconds;
        }
    }

    #[cfg(test)]
    pub(super) fn is_pinned(&self, object: u64) -> bool {
        self.state().pinned.contains(&object)
    }

    #[cfg(test)]
    pub(super) fn owner_count(&self) -> usize {
        self.state().owners.len()
    }
}

pub fn handle_for(object: u64) -> String {
    format!("obj_{object}")
}

pub fn parse_handle(handle: &str) -> Option<u64> {
    handle.strip_prefix("obj_")?.parse().ok()
}

/// Set when a line has code in a loaded class but none of it matches the
/// breakpoint's `--variant`.
pub(super) const VARIANT_MATCHES_NOTHING_NOTE: &str = "the line has code, but none of it matches --variant (outer: the enclosing method; lambda: the innermost lambda); try --variant all";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_round_trip() {
        assert_eq!(handle_for(42), "obj_42");
        assert_eq!(parse_handle("obj_42"), Some(42));
        assert_eq!(parse_handle("obj_x"), None);
        assert_eq!(parse_handle("42"), None);
    }

    #[test]
    fn jdwp_errors_map_to_structured_codes() {
        for (code, expected) in [
            (13, "debugger_not_suspended"),
            (20, "stale_object_handle"),
            (24, "breakpoint_unresolved"),
            (30, "stale_frame"),
            (35, "variable_not_in_scope"),
            (101, "debug_info_absent"),
            (112, "debuggee_exited"),
            (999, "jdwp_error"),
        ] {
            let error: RpcError = JdwpError::Vm {
                command: "X.Y".into(),
                code,
                name: protocol::error_name(code),
            }
            .into();
            assert_eq!(error.code, expected);
            assert_eq!(error.detail["jdwp_error"], code);
        }
        let timeout: RpcError = JdwpError::Timeout {
            command: "VirtualMachine.Version".into(),
            timeout_ms: 5,
        }
        .into();
        assert_eq!(timeout.code, "debugger_timeout");
        assert!(timeout.retryable);
        assert_eq!(timeout.detail["command"], "VirtualMachine.Version");
    }
}
