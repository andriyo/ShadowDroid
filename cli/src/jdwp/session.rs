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
}

#[derive(Clone, Debug)]
enum Owner {
    Breakpoint(String),
    Step,
    /// Deferred line binding for one source file name.
    LinePrepare(String),
    /// Deferred exception-class binding for one breakpoint.
    ExceptionPrepare(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct BoundLocation {
    pub request_id: i32,
    pub class: String,
    pub method: String,
    pub code_index: u64,
    #[serde(skip)]
    pub class_id: u64,
}

#[derive(Clone, Debug)]
enum BreakpointKind {
    Line {
        target: SourceTarget,
        line: u32,
    },
    Exception {
        class: String,
        caught: bool,
        uncaught: bool,
    },
}

#[derive(Clone, Debug)]
struct Breakpoint {
    id: String,
    kind: BreakpointKind,
    locations: Vec<BoundLocation>,
    pending_reason: Option<&'static str>,
    hit_count: u64,
    created_at: f64,
}

impl Breakpoint {
    fn to_json(&self) -> Json {
        let mut value = json!({
            "id": self.id,
            "enabled": true,
            "bound": !self.locations.is_empty(),
            "locations": self.locations,
            "pending_reason": self.pending_reason,
            "hit_count": self.hit_count,
            "suspend_policy": "all",
            "created_at": self.created_at,
        });
        let extra = match &self.kind {
            BreakpointKind::Line { target, line } => json!({
                "type": "line",
                "file": target.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| target.basename.clone()),
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
        };
        if let (Json::Object(map), Json::Object(extra)) = (&mut value, extra) {
            map.extend(extra);
        }
        value
    }
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
struct State {
    breakpoints: BTreeMap<String, Breakpoint>,
    next_breakpoint: u32,
    owners: HashMap<i32, Owner>,
    /// source basename → ClassPrepare request id
    line_prepares: HashMap<String, i32>,
    /// `(breakpoint id, class, method, code index)` claimed by a binder.
    /// The scan and ClassPrepare paths can race on one class; the claim is
    /// taken before the request is set so each location binds once.
    claims: BTreeSet<(String, u64, u64, u64)>,
    suspension: Option<Suspension>,
    epoch: u64,
    pinned: BTreeSet<u64>,
    closed: Option<String>,
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
}

pub struct Session {
    pub(super) jdwp: Jdwp,
    pub info: SessionInfo,
    state: Mutex<State>,
    pub(super) cache: Mutex<Cache>,
    changed: watch::Sender<u64>,
    last_activity: Mutex<Instant>,
    use_source_name_match: bool,
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
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("session state lock")
    }

    fn bump(&self) {
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

    pub fn suspension(&self) -> Option<Suspension> {
        self.state().suspension.clone()
    }

    pub(super) fn epoch(&self) -> u64 {
        self.state().epoch
    }

    fn ensure_open(&self) -> RpcResult<()> {
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
        json!({
            "class": class,
            "method": method.as_ref().map(|(name, _)| name),
            "method_signature": method.as_ref().map(|(_, signature)| signature),
            "line": line,
            "source": source,
            "code_index": location.index,
        })
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
        let state = self.state();
        json!({
            "id": self.info.session_id,
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
            "breakpoint_id": suspension.as_ref().and_then(|s| s.breakpoint_id.clone()),
            "thread": thread_name,
            "position": position,
            "breakpoints": state.breakpoints.len(),
            "live_handles": state.pinned.len(),
            "events_seen": state.events_seen,
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

    fn allocate_breakpoint_id(&self) -> String {
        let mut state = self.state();
        state.next_breakpoint += 1;
        format!("bp_{}", state.next_breakpoint)
    }

    /// Set a line breakpoint. Binds every loaded location now and keeps a
    /// ClassPrepare request armed so classes loaded later (lambdas, lazily
    /// loaded activities) bind on prepare.
    pub async fn break_line(&self, target: SourceTarget, line: u32) -> RpcResult<Json> {
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
            Breakpoint {
                id: id.clone(),
                kind: BreakpointKind::Line {
                    target: target.clone(),
                    line,
                },
                locations: Vec::new(),
                pending_reason: None,
                hit_count: 0,
                created_at: crate::events::now_ts(),
            },
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
        let mut bound = Vec::new();
        for method in methods.iter() {
            let table = self.line_table(class_id, method.method_id).await?;
            let Some(index) = table.first_index_of(line as i32) else {
                continue;
            };
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
            let request_id = match self
                .jdwp
                .set_event(
                    event_kind::BREAKPOINT,
                    suspend_policy::ALL,
                    &[Modifier::LocationOnly(location)],
                )
                .await
            {
                Ok(request_id) => request_id,
                Err(error) => {
                    self.state().claims.remove(&claim);
                    return Err(error.into());
                }
            };
            let removed_meanwhile = {
                let mut state = self.state();
                if state.breakpoints.contains_key(id) {
                    state
                        .owners
                        .insert(request_id, Owner::Breakpoint(id.to_string()));
                    false
                } else {
                    state.claims.remove(&claim);
                    true
                }
            };
            if removed_meanwhile {
                // Removed while this request was in flight.
                let _ = self
                    .jdwp
                    .clear_event(event_kind::BREAKPOINT, request_id)
                    .await;
                continue;
            }
            bound.push(BoundLocation {
                request_id,
                class: resolve::type_name(&signature),
                method: method.name.clone(),
                code_index: index,
                class_id,
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
    ) -> RpcResult<Json> {
        self.ensure_open()?;
        let id = self.allocate_breakpoint_id();
        let mut breakpoint = Breakpoint {
            id: id.clone(),
            kind: BreakpointKind::Exception {
                class: class.to_string(),
                caught,
                uncaught,
            },
            locations: Vec::new(),
            pending_reason: None,
            hit_count: 0,
            created_at: crate::events::now_ts(),
        };
        let signature = format!("L{};", class.replace('.', "/"));
        let loaded = self.jdwp.classes_by_signature(&signature).await?;
        if let Some(class_info) = loaded.first() {
            let location = self
                .set_exception_request(&id, class_info.type_id, caught, uncaught)
                .await?;
            breakpoint.locations.push(location);
        } else {
            let request_id = self
                .jdwp
                .set_event(
                    event_kind::CLASS_PREPARE,
                    suspend_policy::EVENT_THREAD,
                    &[Modifier::ClassMatch(class.to_string())],
                )
                .await?;
            self.state()
                .owners
                .insert(request_id, Owner::ExceptionPrepare(id.clone()));
            breakpoint.pending_reason = Some("class_not_loaded");
        }
        let value = breakpoint.to_json();
        self.state().breakpoints.insert(id, breakpoint);
        self.touch();
        Ok(value)
    }

    async fn set_exception_request(
        &self,
        id: &str,
        type_id: u64,
        caught: bool,
        uncaught: bool,
    ) -> RpcResult<BoundLocation> {
        let request_id = self
            .jdwp
            .set_event(
                event_kind::EXCEPTION,
                suspend_policy::ALL,
                &[Modifier::ExceptionOnly {
                    exception: type_id,
                    caught,
                    uncaught,
                }],
            )
            .await?;
        self.state()
            .owners
            .insert(request_id, Owner::Breakpoint(id.to_string()));
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
            class_id: type_id,
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
        let kind = match breakpoint.kind {
            BreakpointKind::Line { .. } => event_kind::BREAKPOINT,
            BreakpointKind::Exception { .. } => event_kind::EXCEPTION,
        };
        for location in &breakpoint.locations {
            let _ = self.jdwp.clear_event(kind, location.request_id).await;
            self.state().owners.remove(&location.request_id);
        }
        // Exception breakpoints still waiting for their class.
        let pending: Vec<i32> = self
            .state()
            .owners
            .iter()
            .filter(
                |(_, owner)| matches!(owner, Owner::ExceptionPrepare(owner_id) if owner_id == id),
            )
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
    async fn wait_for_stop(
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
    async fn handle_event(&self, event: &Event) -> RpcResult<bool> {
        let owner = self.state().owners.get(&event.request_id()).cloned();
        match (event, owner) {
            (
                Event::Breakpoint {
                    thread, location, ..
                },
                Some(Owner::Breakpoint(id)),
            ) => {
                self.record_stop("breakpoint", Some(*thread), Some(*location), Some(id), None);
                Ok(true)
            }
            (
                Event::Exception {
                    thread,
                    location,
                    exception,
                    ..
                },
                Some(Owner::Breakpoint(id)),
            ) => {
                self.record_stop(
                    "exception",
                    Some(*thread),
                    Some(*location),
                    Some(id),
                    Some(*exception),
                );
                Ok(true)
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
                if let Some(BreakpointKind::Exception {
                    caught, uncaught, ..
                }) = class
                {
                    let location = self
                        .set_exception_request(&id, *type_id, caught, uncaught)
                        .await?;
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

    fn record_stop(
        &self,
        reason: &'static str,
        thread: Option<u64>,
        location: Option<Location>,
        breakpoint_id: Option<String>,
        exception: Option<Value>,
    ) {
        let mut state = self.state();
        if let Some(id) = &breakpoint_id
            && let Some(breakpoint) = state.breakpoints.get_mut(id)
        {
            breakpoint.hit_count += 1;
        }
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
