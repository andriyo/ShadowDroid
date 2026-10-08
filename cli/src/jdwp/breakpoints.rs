//! Breakpoint lifecycle and hit handling on top of [`Session`]: options
//! (enabled, temporary, condition, suspend policy, pass count, logging),
//! arming JDWP requests from them, `break update`, logpoints with their event
//! stream, daemon-side conditions (design §5.3, §5.4), and `continue-until`.
//!
//! A hit that needs evaluation (a condition or any logging) is requested with
//! SUSPEND_EVENT_THREAD: the daemon evaluates in the event thread's top frame
//! and resumes that thread unless the hit is a user-visible stop. A pass count
//! is the native Count modifier, always the last modifier, and re-armed after
//! it fires so it means "every n-th hit".

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::codec::{Location, Value};
use super::eval::{DEFAULT_INVOKE_TIMEOUT, EvalCtx};
use super::expr::{self, Expr};
use super::inspect::{RenderOptions, SelectedFrame, ToStringOptions};
use super::logpoints::{self, Filter};
use super::protocol::{event_kind, suspend_policy};
use super::resolve::SourceTarget;
use super::session::{
    Arm, Breakpoint, BreakpointKind, Owner, RpcError, RpcResult, Session, target_key,
};
use super::vm::Modifier;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuspendKind {
    #[default]
    All,
    Thread,
    None,
}

impl SuspendKind {
    pub fn as_studio(self) -> &'static str {
        match self {
            SuspendKind::All => "ALL",
            SuspendKind::Thread => "THREAD",
            SuspendKind::None => "NONE",
        }
    }
}

/// Everything a breakpoint can be configured with. Sent over RPC by the CLI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BreakpointOptions {
    pub enabled: bool,
    pub temporary: bool,
    pub condition: Option<String>,
    /// Keep a condition or log expression that does not parse.
    pub force: bool,
    pub suspend: SuspendKind,
    pub pass_count: Option<u32>,
    pub log_expression: Option<String>,
    pub log_message: bool,
    pub log_stack: bool,
    /// Ownership label: set for logpoints created by `debug logpoint add`.
    pub owner: Option<String>,
    pub max_events_per_second: Option<u32>,
    pub max_message_chars: Option<u32>,
    /// Conditions and log expressions may call methods (`--invoke`).
    pub invoke: bool,
    /// Which locations of a line to bind: all, the outer method, or the
    /// innermost lambda.
    pub variant: super::lambdas::LineVariant,
}

impl Default for BreakpointOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            temporary: false,
            condition: None,
            force: false,
            suspend: SuspendKind::All,
            pass_count: None,
            log_expression: None,
            log_message: false,
            log_stack: false,
            owner: None,
            max_events_per_second: None,
            max_message_chars: None,
            invoke: false,
            variant: Default::default(),
        }
    }
}

impl BreakpointOptions {
    pub fn logs(&self) -> bool {
        self.log_expression.is_some() || self.log_message || self.log_stack
    }

    /// A logpoint logs and never suspends.
    pub fn is_logpoint(&self) -> bool {
        self.logs() && self.suspend == SuspendKind::None
    }

    /// Hits the daemon must look at (rate-limited).
    pub fn needs_eval(&self) -> bool {
        self.condition.is_some() || self.logs()
    }

    /// Hits that must suspend the event thread so the daemon can read its
    /// frame. A logpoint that only logs the hit position does not.
    pub fn needs_suspend(&self) -> bool {
        self.condition.is_some() || self.log_expression.is_some() || self.log_stack
    }

    pub fn max_events_per_second(&self) -> u32 {
        self.max_events_per_second
            .unwrap_or(logpoints::DEFAULT_MAX_EVENTS_PER_SECOND)
            .clamp(1, logpoints::MAX_EVENTS_PER_SECOND)
    }

    pub fn max_message_chars(&self) -> u32 {
        self.max_message_chars
            .unwrap_or(logpoints::DEFAULT_MAX_MESSAGE_CHARS)
            .clamp(256, logpoints::MAX_MESSAGE_CHARS)
    }

    /// JDWP suspend policy for the request.
    pub fn request_policy(&self) -> u8 {
        if self.needs_suspend() {
            return suspend_policy::EVENT_THREAD;
        }
        match self.suspend {
            SuspendKind::All => suspend_policy::ALL,
            SuspendKind::Thread => suspend_policy::EVENT_THREAD,
            SuspendKind::None => suspend_policy::NONE,
        }
    }

    /// Reject a condition or log expression that does not parse, unless
    /// `force` is set (it then fails at each hit, which is recorded).
    pub fn validate(&self) -> RpcResult<()> {
        for (kind, source) in [
            ("condition", &self.condition),
            ("log_expression", &self.log_expression),
        ] {
            if let Some(source) = source
                && !self.invoke
                && expr::parse(source).is_ok_and(|parsed| parsed.has_calls())
            {
                return Err(RpcError::new(
                    "invoke_not_allowed",
                    format!("the {kind} `{source}` calls a method; add --invoke to run app code"),
                )
                .detail(json!({"kind": kind, "expression": source}))
                .next(&["re-run with --invoke"]));
            }
        }
        if self.force {
            return Ok(());
        }
        for (kind, source) in [
            ("condition", &self.condition),
            ("log_expression", &self.log_expression),
        ] {
            if let Some(source) = source
                && let Err(error) = expr::parse(source)
            {
                return Err(RpcError::new(
                    "debug_expression_invalid",
                    format!("the {kind} `{source}` is not a valid path expression: {error}"),
                )
                .detail(json!({"kind": kind, "expression": source, "error": error}))
                .next(&[
                    "use paths (this.x, local, a[0]), literals, == != < <= > >=, && || !, parentheses",
                    "re-run with --force to set it anyway; it then fails at each hit",
                ]));
            }
        }
        Ok(())
    }
}

/// `break update`: only the fields that are `Some` change.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BreakpointUpdate {
    pub enabled: Option<bool>,
    pub temporary: Option<bool>,
    pub condition: Option<String>,
    pub clear_condition: bool,
    pub log_expression: Option<String>,
    pub clear_log_expression: bool,
    pub log_message: Option<bool>,
    pub log_stack: Option<bool>,
    pub suspend: Option<SuspendKind>,
    pub pass_count: Option<u32>,
    pub force: bool,
    pub invoke: Option<bool>,
}

impl BreakpointUpdate {
    fn apply(&self, opts: &mut BreakpointOptions) {
        if let Some(enabled) = self.enabled {
            opts.enabled = enabled;
        }
        if let Some(temporary) = self.temporary {
            opts.temporary = temporary;
        }
        if self.clear_condition {
            opts.condition = None;
        }
        if let Some(condition) = &self.condition {
            opts.condition = Some(condition.clone());
        }
        if self.clear_log_expression {
            opts.log_expression = None;
        }
        if let Some(expression) = &self.log_expression {
            opts.log_expression = Some(expression.clone());
        }
        if let Some(log_message) = self.log_message {
            opts.log_message = log_message;
        }
        if let Some(log_stack) = self.log_stack {
            opts.log_stack = log_stack;
        }
        if let Some(suspend) = self.suspend {
            opts.suspend = suspend;
        }
        if let Some(pass_count) = self.pass_count {
            opts.pass_count = (pass_count > 0).then_some(pass_count);
        }
        opts.force = self.force;
        if let Some(invoke) = self.invoke {
            opts.invoke = invoke;
        }
    }
}

/// Max chars of a single rendered stack in a log message.
const MAX_LOG_STACK_FRAMES: i32 = 32;

impl Session {
    // ── arming ────────────────────────────────────────────────────────

    /// Set the JDWP request for one location of breakpoint `id` per `opts`.
    pub(super) async fn arm(
        &self,
        id: &str,
        kind: &BreakpointKind,
        arm: Arm,
        opts: &BreakpointOptions,
    ) -> RpcResult<i32> {
        let (event, mut modifiers) = match (arm, kind) {
            (Arm::Line(location), _) => (
                event_kind::BREAKPOINT,
                vec![Modifier::LocationOnly(location)],
            ),
            (
                Arm::Exception(type_id),
                BreakpointKind::Exception {
                    caught, uncaught, ..
                },
            ) => (
                event_kind::EXCEPTION,
                // On Android a crash is never "uncaught" to JDWP: Looper and
                // Compose catch and rethrow (spike Q11). Ask for caught events
                // too and keep, daemon-side, those no app frame catches.
                vec![Modifier::ExceptionOnly {
                    exception: type_id,
                    caught: *caught || *uncaught,
                    uncaught: *uncaught,
                }],
            ),
            (
                Arm::Field {
                    type_id,
                    field_id,
                    modification,
                },
                _,
            ) => (
                if modification {
                    event_kind::FIELD_MODIFICATION
                } else {
                    event_kind::FIELD_ACCESS
                },
                vec![Modifier::FieldOnly {
                    declaring: type_id,
                    field: field_id,
                }],
            ),
            (Arm::Exception(_), _) => {
                return Err(RpcError::new(
                    "invalid_request",
                    "exception arm on a non-exception breakpoint",
                ));
            }
        };
        if let Some(count) = opts.pass_count.filter(|n| *n > 0) {
            // ART drops a request whose Count precedes another filter:
            // Count is always the last modifier.
            modifiers.push(Modifier::Count(count as i32));
        }
        let request_id = self
            .jdwp
            .set_event(event, opts.request_policy(), &modifiers)
            .await?;
        self.state()
            .owners
            .insert(request_id, Owner::Breakpoint(id.to_string()));
        Ok(request_id)
    }

    /// Clear every request of `id` and set them again from its current
    /// options (none while disabled).
    pub(super) async fn rearm(&self, id: &str) -> RpcResult<()> {
        let Some(snapshot) = self.state().breakpoints.get(id).cloned() else {
            return Ok(());
        };
        for location in &snapshot.locations {
            if let Some(request) = location.request_id {
                let _ = self
                    .jdwp
                    .clear_event(location.arm.event_kind(), request)
                    .await;
                self.state().owners.remove(&request);
            }
        }
        let mut armed = Vec::with_capacity(snapshot.locations.len());
        for location in &snapshot.locations {
            let request =
                if snapshot.opts.enabled && snapshot.throttled_until.is_none() && !snapshot.expired
                {
                    Some(
                        self.arm(id, &snapshot.kind, location.arm, &snapshot.opts)
                            .await?,
                    )
                } else {
                    None
                };
            armed.push(request);
        }
        let mut state = self.state();
        if let Some(breakpoint) = state.breakpoints.get_mut(id) {
            for (location, request) in breakpoint.locations.iter_mut().zip(armed) {
                location.request_id = request;
            }
        }
        Ok(())
    }

    // ── creation and lookup ───────────────────────────────────────────

    pub(super) fn breakpoint_at(&self, target: &SourceTarget, line: u32) -> Option<Breakpoint> {
        let key = (target_key(target), line);
        self.state()
            .breakpoints
            .values()
            .find(|b| b.line_key().as_ref() == Some(&key))
            .cloned()
    }

    /// `break line`: idempotent per file:line. An existing plain breakpoint
    /// is returned (with `created: false`) after applying the new options; a
    /// logpoint there is a conflict.
    pub async fn break_line_with(
        &self,
        target: SourceTarget,
        line: u32,
        opts: BreakpointOptions,
    ) -> RpcResult<Json> {
        opts.validate()?;
        if let Some(existing) = self.breakpoint_at(&target, line) {
            if existing.opts.owner.is_some() || existing.opts.is_logpoint() {
                return Err(conflict(&existing, &target, line));
            }
            let update = BreakpointUpdate {
                enabled: Some(opts.enabled),
                temporary: Some(opts.temporary),
                condition: opts.condition.clone(),
                clear_condition: false,
                suspend: Some(opts.suspend),
                pass_count: opts.pass_count,
                force: opts.force,
                ..Default::default()
            };
            let breakpoint = self.update_breakpoint(&existing.id, update).await?;
            return Ok(json!({"breakpoint": breakpoint, "created": false}));
        }
        let breakpoint = self.break_line(target, line, opts).await?;
        Ok(json!({"breakpoint": breakpoint, "created": true}))
    }

    pub async fn update_breakpoint(&self, id: &str, update: BreakpointUpdate) -> RpcResult<Json> {
        self.ensure_open()?;
        let Some(mut opts) = self.state().breakpoints.get(id).map(|b| b.opts.clone()) else {
            return Err(not_found(id));
        };
        update.apply(&mut opts);
        opts.validate()?;
        if let Some(breakpoint) = self.state().breakpoints.get_mut(id) {
            breakpoint.set_opts(opts);
            breakpoint.last_evaluation_error = None;
            breakpoint.expired = false;
            breakpoint.throttled_until = None;
        }
        self.rearm(id).await?;
        self.touch();
        self.state()
            .breakpoints
            .get(id)
            .map(Breakpoint::to_json)
            .ok_or_else(|| not_found(id))
    }

    // ── logpoints ─────────────────────────────────────────────────────

    /// `logpoint add`: create, or reconfigure the same owner's logpoint at
    /// file:line. A breakpoint or another owner's logpoint there conflicts.
    pub async fn logpoint_add(
        &self,
        target: SourceTarget,
        line: u32,
        mut opts: BreakpointOptions,
    ) -> RpcResult<Json> {
        opts.suspend = SuspendKind::None;
        if !opts.logs() {
            return Err(RpcError::new(
                "invalid_request",
                "a logpoint needs --expression, --log-message, or --log-stack",
            ));
        }
        opts.validate()?;
        let owner = opts.owner.clone().unwrap_or_else(|| "shadowdroid".into());
        opts.owner = Some(owner.clone());
        if let Some(existing) = self.breakpoint_at(&target, line) {
            if existing.opts.owner.as_deref() != Some(owner.as_str()) {
                return Err(conflict(&existing, &target, line));
            }
            let id = existing.id.clone();
            if let Some(breakpoint) = self.state().breakpoints.get_mut(&id) {
                breakpoint.set_opts(opts);
                breakpoint.last_evaluation_error = None;
            }
            self.rearm(&id).await?;
            let breakpoint = self.state().breakpoints.get(&id).map(Breakpoint::to_json);
            return Ok(json!({
                "created": false,
                "applied_to_sessions": 1,
                "breakpoint": breakpoint,
            }));
        }
        let breakpoint = self.break_line(target, line, opts).await?;
        let warning = (breakpoint["bound"] != true).then_some(
            "no loaded class holds this line yet; the logpoint binds when its class loads",
        );
        Ok(json!({
            "created": true,
            "applied_to_sessions": 1,
            "breakpoint": breakpoint,
            "warning": warning,
        }))
    }

    pub fn logpoints(&self, id: Option<&str>, owner: Option<&str>) -> Json {
        let state = self.state();
        let logpoints: Vec<Json> = state
            .breakpoints
            .values()
            .filter(|b| b.opts.is_logpoint())
            .filter(|b| id.is_none_or(|id| b.id == id))
            .filter(|b| owner.is_none_or(|owner| b.opts.owner.as_deref() == Some(owner)))
            .map(Breakpoint::to_json)
            .collect();
        json!({
            "logpoints": logpoints,
            "defaults": {
                "event_capacity": logpoints::DEFAULT_CAPACITY,
                "max_message_chars": logpoints::DEFAULT_MAX_MESSAGE_CHARS,
                "max_configurable_message_chars": logpoints::MAX_MESSAGE_CHARS,
                "max_events_per_second": logpoints::DEFAULT_MAX_EVENTS_PER_SECOND,
            },
            "stream_id": self.logpoint_log.stream_id(),
        })
    }

    pub async fn logpoint_remove(&self, id: &str, owner: &str) -> RpcResult<Json> {
        let found = self.state().breakpoints.get(id).cloned();
        let Some(breakpoint) = found.filter(|b| b.opts.is_logpoint()) else {
            return Err(RpcError::new(
                "logpoint_not_found",
                format!("logpoint not found: {id}"),
            ));
        };
        match breakpoint.opts.owner.as_deref() {
            None => {
                return Err(RpcError::new(
                    "logpoint_not_owned",
                    "logpoint is manual or no longer owned; refusing to remove it",
                ));
            }
            Some(actual) if actual != owner => {
                return Err(RpcError::new(
                    "logpoint_owner_mismatch",
                    format!("logpoint is owned by '{actual}', not '{owner}'"),
                )
                .detail(json!({"owner": actual})));
            }
            Some(_) => {}
        }
        self.remove_breakpoint(id).await?;
        Ok(json!({"removed": true, "id": id, "owner": owner}))
    }

    pub async fn logpoint_clear(&self, owner: &str) -> RpcResult<Json> {
        let ids: Vec<String> = self
            .state()
            .breakpoints
            .values()
            .filter(|b| b.opts.is_logpoint() && b.opts.owner.as_deref() == Some(owner))
            .map(|b| b.id.clone())
            .collect();
        for id in &ids {
            self.remove_breakpoint(id).await?;
        }
        Ok(json!({"owner": owner, "removed": ids.len(), "ids": ids}))
    }

    pub async fn logpoint_events(
        &self,
        after: Option<u64>,
        limit: usize,
        filter: &Filter,
        timeout: Duration,
    ) -> Json {
        self.logpoint_log
            .read(
                after,
                limit.clamp(1, 200),
                filter,
                timeout.min(Duration::from_secs(30)),
            )
            .await
    }

    // ── hits ──────────────────────────────────────────────────────────

    /// Handle a Breakpoint or Exception event of breakpoint `id`. Returns
    /// whether it is a user-visible stop (the thread stays suspended).
    pub(super) async fn on_hit(
        &self,
        id: &str,
        request: i32,
        thread: u64,
        location: Location,
        exception: Option<Value>,
    ) -> RpcResult<bool> {
        let Some(breakpoint) = self.state().breakpoints.get(id).cloned() else {
            return Ok(false);
        };
        if !breakpoint.opts.enabled {
            return Ok(false);
        }
        if breakpoint.opts.pass_count.is_some_and(|n| n > 0) {
            // JDWP Count fires once on the n-th hit, then the request is
            // spent (IntelliJ's pass count): mark it, do not re-arm.
            self.expire_request(id, request);
        }
        let opts = &breakpoint.opts;
        if opts.needs_eval() && !self.admit_hit(id, opts).await {
            return Ok(false);
        }
        let frame = if opts.needs_suspend() {
            match self.top_frame(thread).await {
                Ok(frame) => Some(frame),
                Err(error) => {
                    self.record_evaluation_error(id, "frame", &error.message);
                    return Ok(!opts.is_logpoint());
                }
            }
        } else {
            None
        };

        let ctx = frame.map(|frame| {
            EvalCtx::new(
                Some(frame),
                exception,
                opts.invoke.then_some(DEFAULT_INVOKE_TIMEOUT),
            )
        });
        if let (Some(condition), Some(ctx)) = (&breakpoint.condition, &ctx) {
            let outcome = match condition {
                Ok(expr) => self.eval_expr(ctx, expr).await.map(|v| v.truthy()),
                Err(parse_error) => Err(format!("condition does not parse: {parse_error}")),
            };
            match outcome {
                Ok(true) => {}
                Ok(false) => return Ok(false),
                Err(message) => {
                    self.record_evaluation_error(id, "condition", &message);
                    if opts.logs() {
                        self.append_log_event(
                            &breakpoint,
                            thread,
                            Some(("condition", message)),
                            None,
                        )
                        .await;
                    }
                    if opts.is_logpoint() {
                        return Ok(false);
                    }
                    // Design §5.3: a condition that fails to evaluate leaves
                    // the thread suspended rather than skipping silently.
                    self.stop_on_hit(&breakpoint, "condition_error", thread, location, exception)
                        .await;
                    return Ok(true);
                }
            }
        }

        {
            let mut state = self.state();
            if let Some(b) = state.breakpoints.get_mut(id) {
                b.hit_count += 1;
                b.last_hit_at = Some(crate::events::now_ts());
            }
        }
        if opts.logs() {
            match self.log_message(&breakpoint, location, ctx.as_ref()).await {
                Ok(message) => {
                    self.append_log_event(&breakpoint, thread, None, Some(message))
                        .await
                }
                Err(message) => {
                    self.record_evaluation_error(id, "log_expression", &message);
                    self.append_log_event(
                        &breakpoint,
                        thread,
                        Some(("log_expression", message)),
                        None,
                    )
                    .await;
                }
            }
        }
        if opts.is_logpoint() || opts.suspend == SuspendKind::None {
            if opts.temporary {
                let _ = self.remove_breakpoint(id).await;
            }
            return Ok(false);
        }
        let reason = match breakpoint.kind {
            BreakpointKind::Exception { .. } => "exception",
            BreakpointKind::Field { watch: true, .. } => "field_watch",
            BreakpointKind::Method { .. } => "method_breakpoint",
            _ => "breakpoint",
        };
        self.stop_on_hit(&breakpoint, reason, thread, location, exception)
            .await;
        if opts.temporary {
            let _ = self.remove_breakpoint(id).await;
        }
        Ok(true)
    }

    fn expire_request(&self, id: &str, request: i32) {
        let mut state = self.state();
        state.owners.remove(&request);
        if let Some(b) = state.breakpoints.get_mut(id) {
            b.expired = true;
            for location in &mut b.locations {
                if location.request_id == Some(request) {
                    location.request_id = None;
                }
            }
        }
    }

    /// Rate limit (design §5.3). A suspending hit above the limit disarms
    /// the breakpoint for the rest of the window instead of suspending
    /// again (each suspending hit costs ~5–8 ms of the app's main loop); a
    /// non-suspending one only drops the event.
    async fn admit_hit(&self, id: &str, opts: &BreakpointOptions) -> bool {
        let now = now_ms();
        let throttle = {
            let mut state = self.state();
            let Some(b) = state.breakpoints.get_mut(id) else {
                return false;
            };
            if b.rate.admit(now, opts.max_events_per_second()) {
                return true;
            }
            b.dropped += 1;
            if opts.needs_suspend() && b.throttled_until.is_none() {
                // Re-arm at the start of the next one-second window.
                b.throttled_until = Some(((now / 1000) + 1) as f64);
                true
            } else {
                false
            }
        };
        if opts.logs() {
            self.logpoint_log.note_rate_limited();
        }
        if throttle {
            self.disarm(id).await;
        }
        false
    }

    /// Clear every request of `id`, keeping its locations for a later
    /// [`Session::rearm`].
    pub(super) async fn disarm(&self, id: &str) {
        let Some(snapshot) = self.state().breakpoints.get(id).cloned() else {
            return;
        };
        for location in &snapshot.locations {
            if let Some(request) = location.request_id {
                let _ = self
                    .jdwp
                    .clear_event(location.arm.event_kind(), request)
                    .await;
                self.state().owners.remove(&request);
            }
        }
        if let Some(b) = self.state().breakpoints.get_mut(id) {
            for location in &mut b.locations {
                location.request_id = None;
            }
        }
    }

    /// Re-arm breakpoints whose throttle window has passed. Called on the
    /// daemon's tick.
    pub async fn rearm_due(&self) {
        let now = crate::events::now_ts();
        let due: Vec<String> = {
            let mut state = self.state();
            state
                .breakpoints
                .values_mut()
                .filter(|b| b.throttled_until.is_some_and(|at| at <= now))
                .map(|b| {
                    b.throttled_until = None;
                    b.id.clone()
                })
                .collect()
        };
        for id in due {
            if let Err(error) = self.rearm(&id).await {
                tracing::warn!("re-arming throttled {id}: {}", error.message);
            }
        }
    }

    /// Record the stop and widen it to the whole VM when the user asked for
    /// SUSPEND_ALL but the request only suspended the event thread.
    async fn stop_on_hit(
        &self,
        breakpoint: &Breakpoint,
        reason: &'static str,
        thread: u64,
        location: Location,
        exception: Option<Value>,
    ) {
        if breakpoint.opts.request_policy() == suspend_policy::EVENT_THREAD
            && breakpoint.opts.suspend == SuspendKind::All
        {
            // Suspend everyone, then drop the extra count on the event thread.
            if self.jdwp.suspend().await.is_ok() {
                let _ = self.jdwp.thread_resume(thread).await;
            }
        }
        self.record_stop(
            reason,
            Some(thread),
            Some(location),
            Some(breakpoint.id.clone()),
            exception,
        );
    }

    fn record_evaluation_error(&self, id: &str, kind: &str, message: &str) {
        if let Some(b) = self.state().breakpoints.get_mut(id) {
            b.last_evaluation_error = Some(json!({
                "kind": kind,
                "message": message,
                "at": crate::events::now_ts(),
            }));
        }
    }

    async fn top_frame(&self, thread: u64) -> RpcResult<SelectedFrame> {
        let frames = self.jdwp.frames(thread, 0, 1).await?;
        let (frame_id, location) = frames
            .first()
            .copied()
            .ok_or_else(|| RpcError::new("frame_not_found", "the event thread has no frames"))?;
        Ok(SelectedFrame {
            thread,
            thread_index: None,
            frame_index: 0,
            frame_id,
            location,
        })
    }

    /// The rendered log message: default hit text, expression value, stack.
    async fn log_message(
        &self,
        breakpoint: &Breakpoint,
        location: Location,
        ctx: Option<&EvalCtx>,
    ) -> Result<String, String> {
        let mut parts = Vec::new();
        if breakpoint.opts.log_message {
            let position = self.describe_location(&location).await;
            parts.push(format!(
                "Breakpoint reached at {}",
                position_text(&position)
            ));
        }
        let needs_frame = breakpoint.log_expression.is_some() || breakpoint.opts.log_stack;
        let ctx = match ctx {
            Some(ctx) => ctx,
            None if needs_frame => return Err("no suspended frame to read".into()),
            None => return Ok(parts.join("\n")),
        };
        match &breakpoint.log_expression {
            Some(Ok(expr)) => {
                let value = match expr {
                    // A bare path renders like `debug eval` (strings, boxed
                    // values, `instance of T(id=N)`).
                    Expr::Path(path) => {
                        let (value, declared) = self
                            .eval_path(ctx, path)
                            .await
                            .map_err(|error| error.message)?;
                        let mut visiting = std::collections::HashSet::new();
                        let options = RenderOptions {
                            max_message_chars: breakpoint.opts.max_message_chars(),
                            to_string: ctx.invoke.map(|timeout| ToStringOptions {
                                thread: ctx.thread,
                                timeout,
                                max_chars: breakpoint.opts.max_message_chars(),
                            }),
                            ..RenderOptions::new(0, 8, 8)
                        };
                        let rendered = self
                            .render(path.text.clone(), value, declared, options, &mut visiting)
                            .await;
                        match rendered.get("value") {
                            Some(Json::String(text)) => text.clone(),
                            Some(Json::Null) | None => "null".into(),
                            Some(other) => other.to_string(),
                        }
                    }
                    other => self.eval_expr(ctx, other).await?.display(),
                };
                parts.push(value);
            }
            Some(Err(parse_error)) => {
                return Err(format!("log expression does not parse: {parse_error}"));
            }
            None => {}
        }
        if breakpoint.opts.log_stack {
            let count = self
                .jdwp
                .frame_count(ctx.thread)
                .await
                .unwrap_or(1)
                .clamp(1, MAX_LOG_STACK_FRAMES);
            let frames = self
                .jdwp
                .frames(ctx.thread, 0, count)
                .await
                .map_err(|error| error.to_string())?;
            let mut lines = Vec::with_capacity(frames.len());
            for (_, location) in frames {
                let position = self.describe_location(&location).await;
                lines.push(format!("\tat {}", position_text(&position)));
            }
            parts.push(lines.join("\n"));
        }
        Ok(parts.join("\n"))
    }

    async fn append_log_event(
        &self,
        breakpoint: &Breakpoint,
        thread: u64,
        error: Option<(&str, String)>,
        message: Option<String>,
    ) {
        let (file, line) = match &breakpoint.kind {
            BreakpointKind::Line { target, line } => (Some(target_key(target)), Some(*line)),
            _ => (None, None),
        };
        let raw = match (&message, &error) {
            (Some(message), _) => message.clone(),
            (None, Some((kind, text))) => format!("Unable to evaluate the {kind}: {text}"),
            (None, None) => String::new(),
        };
        let (text, truncated, original) =
            logpoints::truncate_message(&raw, breakpoint.opts.max_message_chars());
        let thread = self.thread_name(thread).await;
        let payload = json!({
            "seq": 0,
            "timestamp_ms": now_ms(),
            "type": "logpoint",
            "schema_version": 1,
            "backend": "jdwp",
            "event_kind": if error.is_some() { "evaluation_error" } else { "message" },
            "breakpoint_id": breakpoint.id,
            "owner": breakpoint.opts.owner,
            "managed": breakpoint.opts.owner.is_some(),
            "project": {"name": null, "base_path": null},
            "session": {"id": self.info.session_id, "name": self.info.package},
            "device": {"serial": self.info.serial, "avd": null},
            "package": self.info.package,
            "process_name": self.info.package,
            "pid": self.info.pid,
            "thread": thread,
            "source": {"file": file, "url": null, "line": line},
            "file": file,
            "url": null,
            "line": line,
            "breakpoint_type": "jdwp_line",
            "condition": breakpoint.opts.condition,
            "log_expression": breakpoint.opts.log_expression,
            "log_message": breakpoint.opts.log_message,
            "log_stack": breakpoint.opts.log_stack,
            "message": text,
            "evaluation_error": error.as_ref().map(|(kind, message)| json!({
                "kind": kind,
                "title": message,
                "action": "resumed_without_dialog",
            })),
            "message_truncated": truncated,
            "original_message_chars": original,
        });
        self.logpoint_log.append(
            &breakpoint.id,
            breakpoint.opts.owner.as_deref(),
            &self.info.session_id,
            payload,
        );
    }

    // ── continue-until ────────────────────────────────────────────────

    /// Resume until the session stops at `target:line` (and `condition`
    /// holds there). A temporary breakpoint is armed unless one exists, and
    /// removed afterwards.
    pub async fn continue_until(
        &self,
        target: SourceTarget,
        line: u32,
        condition: Option<String>,
        timeout: Duration,
    ) -> RpcResult<Json> {
        self.ensure_open()?;
        let existing = self.breakpoint_at(&target, line);
        let temporary_id = match &existing {
            Some(b) if b.opts.enabled && !b.opts.is_logpoint() => None,
            Some(b) => return Err(conflict(b, &target, line)),
            None => {
                let opts = BreakpointOptions {
                    condition: condition.clone(),
                    ..Default::default()
                };
                opts.validate()?;
                let created = self.break_line(target.clone(), line, opts).await?;
                created["id"].as_str().map(str::to_string)
            }
        };
        let target_id = temporary_id
            .clone()
            .or_else(|| existing.as_ref().map(|b| b.id.clone()))
            .unwrap_or_default();
        let deadline = tokio::time::Instant::now() + timeout;
        let mut resumes = 0u64;
        let outcome = loop {
            let mut changes = self.subscribe();
            let epoch = self.epoch();
            if self.suspension().is_some() {
                if let Err(error) = self.resume().await {
                    break Err(error);
                }
                resumes += 1;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break Err(continue_timeout(&target, line, timeout, resumes));
            }
            match self.wait_for_stop(&mut changes, epoch, remaining).await {
                Ok(()) => {}
                Err(error) if error.code == "debugger_timeout" => {
                    break Err(continue_timeout(&target, line, timeout, resumes));
                }
                Err(error) => break Err(error),
            }
            let suspension = self.suspension();
            let at_target = suspension.as_ref().and_then(|s| s.breakpoint_id.as_deref())
                == Some(target_id.as_str());
            if !at_target {
                // Stopped elsewhere (another breakpoint): keep going.
                continue;
            }
            // An existing breakpoint has its own condition; ours is checked
            // here too so a pre-existing unconditional one honours it.
            if let (Some(condition), Some(suspension)) = (&condition, &suspension)
                && temporary_id.is_none()
                && let Some(thread) = suspension.thread
            {
                let expr = expr::parse(condition).map_err(|e| {
                    RpcError::new("debug_expression_invalid", format!("condition: {e}"))
                })?;
                let frame = self.top_frame(thread).await?;
                let ctx = EvalCtx::new(Some(frame), suspension.exception, None);
                match self.eval_expr(&ctx, &expr).await {
                    Ok(value) if value.truthy() => {}
                    Ok(_) => continue,
                    Err(message) => {
                        break Err(RpcError::new(
                            "debug_expression_invalid",
                            format!("condition failed to evaluate: {message}"),
                        ));
                    }
                }
            }
            break Ok(());
        };
        if let Some(id) = &temporary_id {
            let _ = self.remove_breakpoint(id).await;
        }
        outcome?;
        let status = self.status().await;
        let stack = self.stack(None, 4).await.unwrap_or(Json::Null);
        Ok(json!({
            "type": "continue_until",
            "matched": true,
            "resumes": resumes,
            "temporary_breakpoint": temporary_id.is_some(),
            "session": status,
            "stack": stack,
        }))
    }

    /// Wait (bounded) for a stop newer than `after_epoch`, or for the
    /// process to end. Used by long-polls such as `run-until-crash`.
    pub async fn wait_stop(&self, after_epoch: Option<u64>, timeout: Duration) -> Json {
        let mut changes = self.subscribe();
        let after = after_epoch.unwrap_or_else(|| self.epoch());
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (stopped, epoch, closed) = {
                let state = self.state();
                (
                    state.suspension.is_some() && state.epoch > after,
                    state.epoch,
                    state.closed.clone(),
                )
            };
            if stopped || closed.is_some() {
                let session = self.status().await;
                return json!({
                    "stopped": stopped,
                    "closed": closed,
                    "epoch": epoch,
                    "session": session,
                });
            }
            if tokio::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
            {
                return json!({"stopped": false, "closed": null, "epoch": epoch, "timed_out": true});
            }
        }
    }
}

fn conflict(existing: &Breakpoint, target: &SourceTarget, line: u32) -> RpcError {
    let what = match existing.opts.owner.as_deref() {
        Some(owner) => format!("a logpoint owned by '{owner}'"),
        None if existing.opts.is_logpoint() => "an unowned logpoint".to_string(),
        None => "a breakpoint".to_string(),
    };
    RpcError::new(
        "logpoint_conflict",
        format!("{what} already exists at {}:{line}", target.basename),
    )
    .detail(json!({
        "existing_breakpoint_id": existing.id,
        "existing_owner": existing.opts.owner,
    }))
    .next(&["shadowdroid debug breakpoints --backend jdwp"])
}

fn not_found(id: &str) -> RpcError {
    RpcError::new(
        "breakpoint_not_found",
        format!("no JDWP breakpoint with id {id}"),
    )
    .next(&["shadowdroid debug breakpoints --backend jdwp"])
}

fn continue_timeout(target: &SourceTarget, line: u32, timeout: Duration, resumes: u64) -> RpcError {
    RpcError::new(
        "debug_wait_timeout",
        format!(
            "continue-until did not stop at {}:{line} within {}ms (resumed {resumes} time(s))",
            target.basename,
            timeout.as_millis()
        ),
    )
    .retryable()
    .detail(json!({
        "type": "continue_until",
        "file": target.basename,
        "line": line,
        "resumes": resumes,
        "timeout_ms": timeout.as_millis() as u64,
    }))
    .next(&["the app kept running; drive it to the target code path, then retry"])
}

/// `Class.method(Source.kt:42)` from a `describe_location` value.
fn position_text(position: &Json) -> String {
    format!(
        "{}.{}({}:{})",
        position["class"].as_str().unwrap_or("?"),
        position["method"].as_str().unwrap_or("?"),
        position["source"].as_str().unwrap_or("Unknown Source"),
        position["line"]
            .as_i64()
            .map(|line| line.to_string())
            .unwrap_or_else(|| "?".into()),
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_choose_the_request_policy() {
        let plain = BreakpointOptions::default();
        assert_eq!(plain.request_policy(), suspend_policy::ALL);
        assert!(!plain.is_logpoint());
        let thread = BreakpointOptions {
            suspend: SuspendKind::Thread,
            ..Default::default()
        };
        assert_eq!(thread.request_policy(), suspend_policy::EVENT_THREAD);
        let conditional = BreakpointOptions {
            condition: Some("x > 1".into()),
            ..Default::default()
        };
        assert_eq!(conditional.request_policy(), suspend_policy::EVENT_THREAD);
        let logpoint = BreakpointOptions {
            log_message: true,
            suspend: SuspendKind::None,
            ..Default::default()
        };
        assert!(logpoint.is_logpoint());
        // A pure hit-position logpoint never suspends.
        assert_eq!(logpoint.request_policy(), suspend_policy::NONE);
        let stack = BreakpointOptions {
            log_stack: true,
            ..logpoint.clone()
        };
        assert_eq!(stack.request_policy(), suspend_policy::EVENT_THREAD);
        assert_eq!(stack.max_events_per_second(), 20);
        let greedy = BreakpointOptions {
            max_events_per_second: Some(5_000),
            ..stack
        };
        assert_eq!(greedy.max_events_per_second(), 100, "hard ceiling");
        assert_eq!(logpoint.max_message_chars(), 16_384);
        let silent = BreakpointOptions {
            suspend: SuspendKind::None,
            ..Default::default()
        };
        assert_eq!(silent.request_policy(), suspend_policy::NONE);
    }

    #[test]
    fn invalid_expressions_are_rejected_unless_forced() {
        let bad = BreakpointOptions {
            condition: Some("this.counter >".into()),
            ..Default::default()
        };
        assert_eq!(bad.validate().unwrap_err().code, "debug_expression_invalid");
        let forced = BreakpointOptions { force: true, ..bad };
        forced.validate().unwrap();
        // Calls parse but need --invoke, even with --force.
        let call = BreakpointOptions {
            condition: Some("this.toString() == \"x\"".into()),
            force: true,
            ..Default::default()
        };
        assert_eq!(call.validate().unwrap_err().code, "invoke_not_allowed");
        BreakpointOptions {
            invoke: true,
            ..call
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn updates_change_only_named_fields() {
        let mut opts = BreakpointOptions {
            condition: Some("a".into()),
            pass_count: Some(3),
            ..Default::default()
        };
        BreakpointUpdate {
            enabled: Some(false),
            clear_condition: true,
            pass_count: Some(0),
            suspend: Some(SuspendKind::Thread),
            ..Default::default()
        }
        .apply(&mut opts);
        assert!(!opts.enabled);
        assert_eq!(opts.condition, None);
        assert_eq!(opts.pass_count, None);
        assert_eq!(opts.suspend, SuspendKind::Thread);
        assert!(!opts.temporary);
    }

    #[test]
    fn positions_render_like_stack_lines() {
        let position = json!({"class": "a.B", "method": "f", "source": "B.kt", "line": 3});
        assert_eq!(position_text(&position), "a.B.f(B.kt:3)");
        assert_eq!(position_text(&json!({})), "?.?(Unknown Source:?)");
    }
}
