//! Method breakpoints and field/property watches without the requests that
//! make ART slow (design §5.2, device measurements in round 3):
//!
//! * MethodEntry/MethodExit and FieldAccess/FieldModification each make ART
//!   interpret the whole app (idle CPU ~3 → 30+ ticks/s, UI frames ~4x
//!   fewer) for as long as the request exists, even with ClassOnly or
//!   FieldOnly. So a method breakpoint is a line breakpoint at the method's
//!   first line-table entry (entry) and at each DEX return instruction
//!   (exit, decoded from Method.Bytecodes).
//! * A property watch defaults to line breakpoints on the Kotlin accessor
//!   (`set<Name>` for modification, `get<Name>`/`is<Name>` for access). A
//!   real field watch needs `--accept-slowdown`, warns on every response,
//!   and auto-clears after `--duration-ms`.
//! * A field with no accessor for what is asked (a private Kotlin property
//!   with no setter, a Java field) is watched at its write (`iput*`/`sput*`)
//!   or read (`iget*`/`sget*`) instructions, found in the APK's dex files
//!   ([`super::dex`]): line breakpoints at each such code index, bound in
//!   every class as it loads. No slowdown.

use std::time::Duration;

use serde_json::{Value as Json, json};

use super::breakpoints::BreakpointOptions;
use super::codec::Location;
use super::protocol::{event_kind, suspend_policy, type_tag};
use super::resolve;
use super::session::{
    Arm, BoundLocation, Breakpoint, BreakpointKind, Owner, RpcError, RpcResult, Session,
};
use super::vm::{MethodInfo, Modifier};

const ACC_NATIVE: i32 = 0x0100;
const ACC_ABSTRACT: i32 = 0x0400;
const ACC_BRIDGE: i32 = 0x0040;
const ACC_SYNTHETIC: i32 = 0x1000;

/// Reads the app's dex files (`classes*.dex` of every APK).
pub type DexLoader = std::sync::Arc<
    dyn Fn() -> futures_util::future::BoxFuture<'static, Result<Vec<Vec<u8>>, String>>
        + Send
        + Sync,
>;

/// The app's dex files once read, or why they could not be.
pub type DexFiles = Result<std::sync::Arc<Vec<Vec<u8>>>, String>;

/// A field read/write site and what binding found for it.
#[derive(Clone, Debug)]
pub struct SiteState {
    pub site: super::dex::FieldSite,
    /// Java class name of `site.class`.
    pub class_name: String,
    pub line: Option<i32>,
    pub bound: bool,
}

impl SiteState {
    pub fn to_json(&self) -> Json {
        json!({
            "kind": if self.site.write { "write" } else { "read" },
            "class": self.class_name,
            "method": self.site.method,
            "signature": self.site.signature,
            "code_index": self.site.index,
            "line": self.line,
            "bound": self.bound,
        })
    }
}

/// Default lifetime of a slow field watch.
pub const DEFAULT_WATCH_DURATION: Duration = Duration::from_secs(60);

/// `*`-glob match (`*` anywhere, any run of characters).
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            let Some(stripped) = rest.strip_prefix(part) else {
                return false;
            };
            rest = stripped;
        } else if index == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(at) = rest.find(part) {
            rest = &rest[at + part.len()..];
        } else {
            return false;
        }
    }
    true
}

/// A JDWP `ClassMatch` pattern (single leading or trailing `*`) that
/// covers `pattern`; the daemon re-checks matches with [`glob`].
pub fn class_match_pattern(pattern: &str) -> String {
    match pattern.find('*') {
        None => pattern.to_string(),
        Some(0) if pattern.matches('*').count() == 1 => pattern.to_string(),
        Some(at) => format!("{}*", &pattern[..at]),
    }
}

/// Code indexes (16-bit units) of every return instruction in DEX code.
/// Walks instruction widths per the Dalvik format table and skips the
/// switch/array payload pseudo-instructions.
pub fn dex_return_indices(code: &[u8]) -> Vec<u64> {
    let mut returns = Vec::new();
    super::dex::walk(&super::dex::code_units(code), |pc, rest| {
        // return-void, return, return-wide, return-object, and ART's
        // quickened return-void-no-barrier.
        if matches!((rest[0] & 0xff) as u8, 0x0e..=0x11 | 0x73) {
            returns.push(pc as u64);
        }
    });
    returns
}

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn has_code(method: &MethodInfo) -> bool {
    method.mod_bits & (ACC_NATIVE | ACC_ABSTRACT) == 0
}

impl Session {
    /// `break method --class <pattern> --method <pattern>`.
    pub async fn break_method(
        &self,
        class: &str,
        method: &str,
        entry: bool,
        exit: bool,
        opts: BreakpointOptions,
    ) -> RpcResult<Json> {
        self.ensure_open()?;
        opts.validate()?;
        if !entry && !exit {
            return Err(RpcError::new(
                "invalid_request",
                "a method breakpoint needs --entry or --exit",
            ));
        }
        let kind = BreakpointKind::Method {
            class: class.to_string(),
            method: method.to_string(),
            entry,
            exit,
        };
        self.add_member_breakpoint(kind, class, opts, None).await
    }

    /// `break field --class --field [--access] [--modification]`.
    #[allow(clippy::too_many_arguments)]
    pub async fn break_field(
        &self,
        class: &str,
        field: &str,
        access: bool,
        modification: bool,
        accept_slowdown: bool,
        duration: Duration,
        opts: BreakpointOptions,
    ) -> RpcResult<Json> {
        self.ensure_open()?;
        opts.validate()?;
        if !access && !modification {
            return Err(RpcError::new(
                "invalid_request",
                "a field watch needs --access or --modification",
            ));
        }
        let kind = BreakpointKind::Field {
            class: class.to_string(),
            field: field.to_string(),
            access,
            modification,
            watch: accept_slowdown,
        };
        let slow_until = accept_slowdown.then(|| crate::events::now_ts() + duration.as_secs_f64());
        let value = self
            .add_member_breakpoint(kind, class, opts, slow_until)
            .await?;
        let id = value["id"].as_str().unwrap_or_default().to_string();
        // Accessor mode found no accessor in a loaded class: say so, and how
        // to opt into the slow watch, instead of leaving a dead breakpoint.
        let unbound = self
            .state()
            .breakpoints
            .get(&id)
            .is_some_and(|b| b.locations.is_empty() && b.pending_reason == Some("no_accessor"));
        if unbound && !accept_slowdown {
            let dex_error = self
                .state()
                .breakpoints
                .get(&id)
                .and_then(|b| b.dex_error.clone());
            let _ = self.remove_breakpoint(&id).await;
            return Err(RpcError::new(
                "unsupported_location",
                format!(
                    "{class} has no set{0}/get{0} accessor to break on and no instruction in the app's dex files {1} `{field}`; a real field watch slows the whole app",
                    capitalized(field),
                    if modification { "writes" } else { "reads" },
                ),
            )
            .detail(json!({"reason": "no_accessor", "class": class, "field": field, "dex_error": dex_error}))
            .next(&["re-run with --accept-slowdown (auto-clears after --duration-ms)"]));
        }
        Ok(value)
    }

    async fn add_member_breakpoint(
        &self,
        kind: BreakpointKind,
        class_pattern: &str,
        opts: BreakpointOptions,
        slow_until: Option<f64>,
    ) -> RpcResult<Json> {
        let id = self.allocate_breakpoint_id();
        let mut breakpoint = Breakpoint::new(id.clone(), kind, opts);
        breakpoint.slow_until = slow_until;
        self.state().breakpoints.insert(id.clone(), breakpoint);
        // Deferred first, so a class prepared during the scan still binds.
        let prepare = self
            .jdwp
            .set_event(
                event_kind::CLASS_PREPARE,
                suspend_policy::EVENT_THREAD,
                &[Modifier::ClassMatch(class_match_pattern(class_pattern))],
            )
            .await;
        if let Ok(request) = prepare {
            self.state()
                .owners
                .insert(request, Owner::MemberPrepare(id.clone()));
        }
        let classes = match self.jdwp.all_classes().await {
            Ok(classes) => classes,
            Err(error) => {
                let _ = self.remove_breakpoint(&id).await;
                return Err(error.into());
            }
        };
        for class in classes.iter().filter(|c| c.signature.starts_with('L')) {
            if !glob(class_pattern, &resolve::type_name(&class.signature)) {
                continue;
            }
            self.cache
                .lock()
                .expect("cache")
                .signatures
                .insert(class.type_id, class.signature.clone());
            if let Err(error) = self.bind_member(&id, class.type_id, &class.signature).await {
                let _ = self.remove_breakpoint(&id).await;
                return Err(error);
            }
        }
        {
            let mut state = self.state();
            if let Some(b) = state.breakpoints.get_mut(&id)
                && b.locations.is_empty()
                && b.pending_reason.is_none()
            {
                b.pending_reason = Some("class_not_loaded");
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

    /// Bind a method or field breakpoint in one loaded class.
    pub(super) async fn bind_member(
        &self,
        id: &str,
        class_id: u64,
        signature: &str,
    ) -> RpcResult<()> {
        let Some(breakpoint) = self.state().breakpoints.get(id).cloned() else {
            return Ok(());
        };
        if breakpoint.locations.iter().any(|l| l.class_id == class_id) {
            return Ok(());
        }
        let class_name = resolve::type_name(signature);
        let mut sites: Vec<(Arm, &'static str, String, u64)> = Vec::new();
        // A dex plan was made just now: bind it in the other classes too.
        let mut planned = false;
        match &breakpoint.kind {
            BreakpointKind::Method {
                class,
                method,
                entry,
                exit,
            } => {
                if !glob(class, &class_name) {
                    return Ok(());
                }
                for info in self.methods(class_id).await?.iter() {
                    if !glob(method, &info.name)
                        || !has_code(info)
                        || info.mod_bits & (ACC_BRIDGE | ACC_SYNTHETIC) != 0
                    {
                        continue;
                    }
                    if *entry {
                        let table = self.line_table(class_id, info.method_id).await?;
                        let index = table.lines.iter().map(|(code, _)| *code).min().unwrap_or(0);
                        sites.push((
                            self.line_arm(class_id, info, index),
                            "entry",
                            info.name.clone(),
                            index,
                        ));
                    }
                    if *exit {
                        let code = self.jdwp.bytecodes(class_id, info.method_id).await?;
                        for index in dex_return_indices(&code) {
                            sites.push((
                                self.line_arm(class_id, info, index),
                                "exit",
                                info.name.clone(),
                                index,
                            ));
                        }
                    }
                }
            }
            BreakpointKind::Field {
                class,
                field,
                access,
                modification,
                watch,
            } => {
                if *class != class_name {
                    // Another class: only the field's read/write sites in it.
                    sites.extend(self.plan_sites_in(id, class_id, &class_name).await?);
                    if sites.is_empty() {
                        return Ok(());
                    }
                } else {
                    let mut watch = *watch;
                    if watch {
                        // A delegated property (`by mutableStateOf`, `by lazy`)
                        // stores the delegate object, assigned once: a watch on
                        // `<name>$delegate` never fires on property writes, which
                        // go through set<Name>(). Break on the accessors instead,
                        // and skip the whole-app slowdown.
                        let fields = self.fields(class_id).await?;
                        let delegate = format!("{field}$delegate");
                        if !fields.iter().any(|f| f.name == *field)
                            && fields.iter().any(|f| f.name == delegate)
                        {
                            watch = false;
                            let mut state = self.state();
                            if let Some(b) = state.breakpoints.get_mut(id) {
                                b.slow_until = None;
                                b.note = Some(DELEGATED_PROPERTY_NOTE);
                                if let BreakpointKind::Field { watch, .. } = &mut b.kind {
                                    *watch = false;
                                }
                            }
                        }
                    }
                    if watch {
                        let fields = self.fields(class_id).await?;
                        let found = fields.iter().find(|f| f.name == *field);
                        let Some(found) = found else {
                            let mut state = self.state();
                            if let Some(b) = state.breakpoints.get_mut(id) {
                                b.pending_reason = Some("field_not_found");
                            }
                            return Ok(());
                        };
                        for (wanted, modifies, role) in [
                            (*modification, true, "field_modification"),
                            (*access, false, "field_access"),
                        ] {
                            if wanted {
                                sites.push((
                                    Arm::Field {
                                        type_id: class_id,
                                        field_id: found.field_id,
                                        modification: modifies,
                                    },
                                    role,
                                    found.name.clone(),
                                    0,
                                ));
                            }
                        }
                    } else {
                        let name = capitalized(field);
                        for info in self.methods(class_id).await?.iter() {
                            let role = if *modification
                                && info.name == format!("set{name}")
                                && super::eval::method_types(&info.signature).0.len() == 1
                            {
                                "setter"
                            } else if *access
                                && (info.name == format!("get{name}")
                                    || info.name == format!("is{name}"))
                                && info.signature.starts_with("()")
                            {
                                "getter"
                            } else {
                                continue;
                            };
                            if !has_code(info) {
                                continue;
                            }
                            let table = self.line_table(class_id, info.method_id).await?;
                            let index =
                                table.lines.iter().map(|(code, _)| *code).min().unwrap_or(0);
                            sites.push((
                                self.line_arm(class_id, info, index),
                                role,
                                info.name.clone(),
                                index,
                            ));
                        }
                        // No accessor for what is asked: watch the field's
                        // write/read instructions instead (from the dex files).
                        let need_write = *modification && !sites.iter().any(|s| s.1 == "setter");
                        let need_read = *access && !sites.iter().any(|s| s.1 == "getter");
                        if need_write || need_read {
                            planned = self
                                .ensure_dex_plan(id, class, field, need_write, need_read)
                                .await;
                            sites.extend(self.plan_sites_in(id, class_id, &class_name).await?);
                        }
                        if sites.is_empty() && !planned {
                            let mut state = self.state();
                            if let Some(b) = state.breakpoints.get_mut(id) {
                                b.pending_reason = Some("no_accessor");
                            }
                            return Ok(());
                        }
                    }
                }
            }
            _ => return Ok(()),
        }
        let mut bound = Vec::with_capacity(sites.len());
        for (arm, role, method, index) in sites {
            let request = if breakpoint.opts.enabled {
                Some(
                    self.arm(id, &breakpoint.kind, arm, &breakpoint.opts)
                        .await?,
                )
            } else {
                None
            };
            bound.push(BoundLocation {
                request_id: request,
                class: class_name.clone(),
                method,
                code_index: index,
                role: Some(role),
                kind: Some(role),
                lambda: false,
                lambda_depth: 0,
                class_id,
                arm,
            });
        }
        let orphaned = {
            let mut state = self.state();
            match state.breakpoints.get_mut(id) {
                Some(b) => {
                    if !bound.is_empty() {
                        b.pending_reason = None;
                    }
                    b.locations.extend(bound);
                    Vec::new()
                }
                None => bound,
            }
        };
        // Removed meanwhile: drop what was just armed.
        for location in orphaned {
            if let Some(request) = location.request_id {
                let _ = self
                    .jdwp
                    .clear_event(location.arm.event_kind(), request)
                    .await;
                self.state().owners.remove(&request);
            }
        }
        if planned {
            Box::pin(self.bind_plan_elsewhere(id, &class_name)).await;
        }
        Ok(())
    }

    /// The app's dex files, read once (via the daemon's loader).
    async fn dex_files(&self) -> Result<std::sync::Arc<Vec<Vec<u8>>>, String> {
        let mut cached = self.dex_files.lock().await;
        if let Some(result) = cached.as_ref() {
            return result.clone();
        }
        let result = match self.dex_loader.get() {
            Some(loader) => loader().await.map(std::sync::Arc::new),
            None => Err("no APK source for this session (package unknown)".to_string()),
        };
        *cached = Some(result.clone());
        result
    }

    /// Find the field's write (and/or read) sites in the dex files and keep
    /// them on the breakpoint. Returns whether any were found; a failure is
    /// recorded as `dex_error`.
    async fn ensure_dex_plan(
        &self,
        id: &str,
        class: &str,
        field: &str,
        write: bool,
        read: bool,
    ) -> bool {
        if let Some(b) = self.state().breakpoints.get(id)
            && (b.dex_sites.is_some() || b.dex_error.is_some())
        {
            return b.dex_sites.as_ref().is_some_and(|s| !s.is_empty());
        }
        let outcome = self.dex_files().await.map(|files| {
            super::dex::sites_in(&files, &super::dex::descriptor(class), field)
                .into_iter()
                .filter(|site| (site.write && write) || (!site.write && read))
                .map(|site| SiteState {
                    class_name: resolve::type_name(&site.class),
                    site,
                    line: None,
                    bound: false,
                })
                .collect::<Vec<_>>()
        });
        let mut state = self.state();
        let Some(b) = state.breakpoints.get_mut(id) else {
            return false;
        };
        match outcome {
            Ok(sites) => {
                let found = !sites.is_empty();
                b.dex_sites = Some(sites);
                found
            }
            Err(error) => {
                b.dex_error = Some(error);
                false
            }
        }
    }

    /// Line arms for the plan's sites in `class_name`, recording each
    /// site's line.
    async fn plan_sites_in(
        &self,
        id: &str,
        class_id: u64,
        class_name: &str,
    ) -> RpcResult<Vec<(Arm, &'static str, String, u64)>> {
        let wanted: Vec<(usize, SiteState)> = match self.state().breakpoints.get(id) {
            Some(b) => b
                .dex_sites
                .iter()
                .flatten()
                .enumerate()
                .filter(|(_, s)| s.class_name == class_name)
                .map(|(i, s)| (i, s.clone()))
                .collect(),
            None => return Ok(Vec::new()),
        };
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let methods = self.methods(class_id).await?;
        let mut out = Vec::new();
        for (index, site) in wanted {
            let Some(info) = methods
                .iter()
                .find(|m| m.name == site.site.method && m.signature == site.site.signature)
            else {
                continue;
            };
            let line = self
                .line_table(class_id, info.method_id)
                .await
                .ok()
                .and_then(|table| table.line_at(site.site.index));
            if let Some(b) = self.state().breakpoints.get_mut(id)
                && let Some(state) = b.dex_sites.as_mut().and_then(|s| s.get_mut(index))
            {
                state.line = line;
                state.bound = true;
            }
            out.push((
                self.line_arm(class_id, info, site.site.index),
                if site.site.write {
                    "write_site"
                } else {
                    "read_site"
                },
                info.name.clone(),
                site.site.index,
            ));
        }
        Ok(out)
    }

    /// Bind the plan in every other class it names: loaded ones now, the
    /// rest through ClassPrepare (exact class match) as they load.
    async fn bind_plan_elsewhere(&self, id: &str, bound_class: &str) {
        let classes: Vec<String> = {
            let state = self.state();
            let Some(b) = state.breakpoints.get(id) else {
                return;
            };
            let mut classes: Vec<String> = b
                .dex_sites
                .iter()
                .flatten()
                .map(|s| s.class_name.clone())
                .filter(|c| c != bound_class)
                .collect();
            classes.sort();
            classes.dedup();
            classes
        };
        for class in classes {
            if let Ok(request) = self
                .jdwp
                .set_event(
                    event_kind::CLASS_PREPARE,
                    suspend_policy::EVENT_THREAD,
                    &[Modifier::ClassMatch(class.clone())],
                )
                .await
            {
                self.state()
                    .owners
                    .insert(request, Owner::MemberPrepare(id.to_string()));
            }
            let signature = super::dex::descriptor(&class);
            if let Ok(loaded) = self.jdwp.classes_by_signature(&signature).await {
                for found in loaded {
                    self.cache
                        .lock()
                        .expect("cache")
                        .signatures
                        .insert(found.type_id, signature.clone());
                    if let Err(error) = self.bind_member(id, found.type_id, &signature).await {
                        tracing::warn!("binding field sites in {class}: {}", error.message);
                    }
                }
            }
        }
    }

    /// Set how this session reads the app's dex files.
    pub fn set_dex_loader(&self, loader: DexLoader) {
        let _ = self.dex_loader.set(loader);
    }

    fn line_arm(&self, class_id: u64, method: &MethodInfo, index: u64) -> Arm {
        Arm::Line(Location {
            type_tag: type_tag::CLASS,
            class_id,
            method_id: method.method_id,
            index,
        })
    }

    /// Armed field-watch requests (each one slows the whole app).
    pub fn slow_requests(&self) -> usize {
        self.state()
            .breakpoints
            .values()
            .flat_map(|b| b.locations.iter())
            .filter(|l| l.arm.is_slow() && l.request_id.is_some())
            .count()
    }

    /// Disarm field watches past their `--duration-ms`. Called on the
    /// daemon's tick with the throttle re-arm.
    pub async fn expire_slow_watches(&self) {
        let now = crate::events::now_ts();
        let due: Vec<String> = {
            let mut state = self.state();
            state
                .breakpoints
                .values_mut()
                .filter(|b| b.slow_until.is_some_and(|at| at <= now) && !b.expired)
                .map(|b| {
                    b.expired = true;
                    b.expired_reason = Some("watch_duration_elapsed");
                    b.id.clone()
                })
                .collect()
        };
        for id in due {
            tracing::info!("field watch {id} reached its duration; disarming");
            self.disarm(&id).await;
        }
    }
}

/// Recorded on a field breakpoint that asked for a slow watch on a Kotlin
/// delegated property and got accessor breakpoints instead.
pub(super) const DELEGATED_PROPERTY_NOTE: &str = "delegated property: its field holds the delegate object and is never reassigned, so a field watch would not fire on writes; breaking on the set/get accessors instead (no slowdown)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_and_class_match_patterns() {
        assert!(glob("io.example.*", "io.example.app.Main"));
        assert!(glob("*Activity", "io.MainActivity"));
        assert!(glob("io.*.Main*", "io.example.MainActivity"));
        assert!(!glob("io.*.Main*", "io.example.Other"));
        assert!(glob("onCreate", "onCreate"));
        assert!(!glob("onCreate", "onCreateView"));
        assert!(glob("on*", "onCreateView"));
        assert_eq!(class_match_pattern("io.example.Main"), "io.example.Main");
        assert_eq!(class_match_pattern("*Activity"), "*Activity");
        assert_eq!(class_match_pattern("io.*.Main*"), "io.*");
    }

    fn units(words: &[u16]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn dex_returns_are_found_across_instruction_widths() {
        // const/4 (1), invoke-virtual (3), if-eqz (2), return-void (1),
        // const-wide (5), return-wide (1), nop padding, packed-switch payload.
        let code = units(&[
            0x0012, // 0 const/4
            0x006e, 0x0000, 0x0000, // 1 invoke-virtual
            0x0038, 0x0003, // 4 if-eqz
            0x000e, // 6 return-void
            0x0018, 0, 0, 0, 0,      // 7 const-wide
            0x0010, // 12 return-wide
            0x0000, // 13 nop
            0x0100, 0x0001, 0, 0, 0, 0,      // 14 packed-switch payload (1 target)
            0x0011, // 20 return-object
        ]);
        assert_eq!(dex_return_indices(&code), [6, 12, 20]);
        // A payload whose words look like returns is skipped.
        let tricky = units(&[
            0x000e, 0x0200, 0x0001, 0x000e, 0x000e, 0x0011, 0x0011, 0x000f,
        ]);
        assert_eq!(dex_return_indices(&tricky), [0, 7]);
        assert!(dex_return_indices(&[]).is_empty());
    }
}
