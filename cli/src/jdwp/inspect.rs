//! Frame and value reads for a suspended session: `stack`, `threads`,
//! `variables`, `eval`, and `inspect`, with the deterministic path grammar
//! (`this`, locals, `.field`, `[index]`) and the phase-1 renderers.
//!
//! The JSON mirrors the Studio bridge (`DebuggerValues.kt`): each value is
//! `{name, declared_type, type, value, object_id?, object_handle?, …}` with
//! `string`, `length`/`items`, or `fields` when expanded.

use std::collections::HashSet;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value as Json, json};

use super::codec::Value;
use super::eval::{EvalCtx, HANDLE_ROOT, INVOKE_THREW};
use super::expr::Expr;
use super::protocol::{ACC_STATIC, tag, thread_status, thread_status_name};
use super::resolve;
use super::session::{RpcError, RpcResult, Session};
use super::vm::FieldInfo;

/// Render limits shared by every read verb.
#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    pub depth: u32,
    pub max_fields: u32,
    pub max_array_items: u32,
    /// Bound on rendered `toString()` text (`--max-message-chars`).
    pub max_message_chars: u32,
    /// With `--invoke`: objects without a built-in renderer show
    /// `toString()`, run on this thread.
    pub to_string: Option<ToStringOptions>,
}

impl RenderOptions {
    pub const fn new(depth: u32, max_fields: u32, max_array_items: u32) -> Self {
        Self {
            depth,
            max_fields,
            max_array_items,
            max_message_chars: DEFAULT_TO_STRING_CHARS,
            to_string: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ToStringOptions {
    pub thread: u64,
    pub timeout: std::time::Duration,
    pub max_chars: u32,
}

/// Default bound on a rendered `toString()`.
pub const DEFAULT_TO_STRING_CHARS: u32 = 4096;

/// Parse an `eval`/`inspect` input; calls need `--invoke`.
fn parse_for_eval(source: &str, invoke: bool) -> RpcResult<Expr> {
    let parsed = super::expr::parse(source.trim())
        .map_err(|error| RpcError::new("invalid_expression", error))?;
    if parsed.has_calls() && !invoke {
        return Err(RpcError::new(
            "invoke_not_allowed",
            format!(
                "`{}` calls a method; add --invoke to run app code",
                source.trim()
            ),
        )
        .next(&["re-run with --invoke"]));
    }
    Ok(parsed)
}

fn eval_value_type(value: &super::expr::EvalValue) -> &'static str {
    use super::expr::EvalValue::*;
    match value {
        Null => "null",
        Bool(_) => "boolean",
        Int(_) => "long",
        Float(_) => "double",
        Char(_) => "char",
        Str(_) => "java.lang.String",
        Object { .. } => "object",
    }
}

impl RenderOptions {
    fn child(self) -> Self {
        Self {
            depth: self.depth.saturating_sub(1),
            ..self
        }
    }
}

/// Longest string value returned inline.
const MAX_STRING_CHARS: usize = 4096;
/// Throwable cause-chain depth rendered.
const MAX_CAUSES: usize = 8;

/// A selected frame: thread, index, and the JDWP frame id for this
/// suspension (frame ids never outlive it).
#[derive(Clone, Copy, Debug)]
pub struct SelectedFrame {
    pub thread: u64,
    pub thread_index: Option<usize>,
    pub frame_index: usize,
    pub frame_id: u64,
    pub location: super::codec::Location,
}

impl Session {
    fn require_suspended(&self) -> RpcResult<super::session::Suspension> {
        self.suspension().ok_or_else(RpcError::not_suspended)
    }

    /// Resolve `--thread` (index into `threads`, name, or raw id) or the
    /// suspension's thread.
    pub async fn select_thread(&self, selector: Option<&str>) -> RpcResult<(u64, Option<usize>)> {
        let suspension = self.require_suspended()?;
        let Some(selector) = selector.filter(|s| !s.trim().is_empty()) else {
            return Ok(match suspension.thread {
                Some(thread) => (thread, Some(0)),
                None => (self.default_thread().await?, None),
            });
        };
        let ordered = self.ordered_threads().await?;
        if let Ok(index) = selector.parse::<usize>()
            && let Some(thread) = ordered.get(index)
        {
            return Ok((*thread, Some(index)));
        }
        for (index, thread) in ordered.iter().enumerate() {
            if self.thread_name(*thread).await == selector || thread.to_string() == selector {
                return Ok((*thread, Some(index)));
            }
        }
        Err(
            RpcError::new("thread_not_found", format!("thread not found: {selector}"))
                .next(&["shadowdroid debug threads --backend jdwp"]),
        )
    }

    /// Threads with the suspension's thread first, the rest by name.
    async fn ordered_threads(&self) -> RpcResult<Vec<u64>> {
        let current = self.suspension().and_then(|s| s.thread);
        let mut named = Vec::new();
        for thread in self.jdwp.all_threads().await? {
            if Some(thread) != current {
                named.push((self.thread_name(thread).await, thread));
            }
        }
        named.sort();
        Ok(current
            .into_iter()
            .chain(named.into_iter().map(|(_, thread)| thread))
            .collect())
    }

    pub async fn select_frame(
        &self,
        thread: Option<&str>,
        frame: Option<usize>,
    ) -> RpcResult<SelectedFrame> {
        let (thread, thread_index) = self.select_thread(thread).await?;
        self.ensure_thread_not_busy(thread)?;
        let frame_index = frame.unwrap_or(0);
        let frames = self
            .jdwp
            .frames(thread, frame_index as i32, 1)
            .await
            .map_err(RpcError::from)?;
        let Some((frame_id, location)) = frames.first().copied() else {
            return Err(RpcError::new(
                "frame_not_found",
                format!("frame index out of bounds: {frame_index}"),
            ));
        };
        Ok(SelectedFrame {
            thread,
            thread_index,
            frame_index,
            frame_id,
            location,
        })
    }

    async fn frame_json(
        &self,
        index: usize,
        thread_name: &str,
        location: &super::codec::Location,
    ) -> Json {
        let mut value = self.describe_location(location).await;
        value["index"] = json!(index);
        value["thread"] = json!(thread_name);
        value
    }

    async fn thread_frames(&self, thread: u64, limit: u32) -> RpcResult<Vec<Json>> {
        let count = self.jdwp.frame_count(thread).await?;
        let length = count.clamp(0, limit as i32);
        if length == 0 {
            return Ok(Vec::new());
        }
        let frames = self.jdwp.frames(thread, 0, length).await?;
        let name = self.thread_name(thread).await;
        let mut out = Vec::with_capacity(frames.len());
        for (index, (_, location)) in frames.iter().enumerate() {
            out.push(self.frame_json(index, &name, location).await);
        }
        Ok(out)
    }

    pub async fn stack(&self, thread: Option<&str>, limit: u32) -> RpcResult<Json> {
        let session = self.status().await;
        if self.suspension().is_none() {
            return Ok(json!({
                "session": session,
                "frames": [],
                "warning": "session is not suspended",
            }));
        }
        let (thread, _) = self.select_thread(thread).await?;
        let frames = self.thread_frames(thread, limit).await?;
        let warning = session.get("warning").cloned().unwrap_or(Json::Null);
        Ok(json!({"session": session, "frames": frames, "warning": warning}))
    }

    pub async fn threads(&self, limit: u32) -> RpcResult<Json> {
        let session = self.status().await;
        if self.suspension().is_none() {
            return Ok(json!({
                "session": session,
                "threads": [],
                "warning": "session is not suspended",
            }));
        }
        let current = self.suspension().and_then(|s| s.thread);
        let mut threads = Vec::new();
        for (index, thread) in self.ordered_threads().await?.into_iter().enumerate() {
            let name = self.thread_name(thread).await;
            let (status, suspend_status) = self.jdwp.thread_status(thread).await.unwrap_or((-1, 0));
            let suspend_count = self.jdwp.suspend_count(thread).await.unwrap_or(0);
            let frames = if suspend_status & thread_status::SUSPEND_STATUS_SUSPENDED != 0 {
                match self.thread_frames(thread, limit).await {
                    Ok(frames) => frames,
                    Err(error) => vec![json!({"error": error.message})],
                }
            } else {
                Vec::new()
            };
            threads.push(json!({
                "index": index,
                "id": thread,
                "name": name,
                "status": thread_status_name(status),
                "suspended": suspend_status & thread_status::SUSPEND_STATUS_SUSPENDED != 0,
                "suspend_count": suspend_count,
                "current": Some(thread) == current,
                "top_frame": frames.first(),
                "frames": frames,
            }));
        }
        Ok(json!({"session": session, "threads": threads}))
    }

    /// Visible locals (Kotlin markers hidden, `this` reported separately).
    pub async fn variables(
        &self,
        thread: Option<&str>,
        frame: Option<usize>,
        options: RenderOptions,
    ) -> RpcResult<Json> {
        let session = self.status().await;
        if self.suspension().is_none() {
            return Ok(json!({
                "session": session,
                "variables": [],
                "warning": "session is not suspended",
            }));
        }
        let selected = self.select_frame(thread, frame).await?;
        let locals = self.visible_locals(&selected).await?;
        let mut variables = Vec::with_capacity(locals.len());
        let mut warning = None;
        if locals.is_empty() {
            let table = self
                .variable_table(selected.location.class_id, selected.location.method_id)
                .await?;
            if selected.location.is_native() {
                warning = Some("native method frame: no Java locals");
            } else if table.is_empty() {
                warning = Some("method has no local variable table (compiled without debug info?)");
            }
        }
        for (local, value) in &locals {
            let mut visiting = HashSet::new();
            let display = resolve::display_local_name(&local.name);
            let mut rendered = self
                .render(
                    display.to_string(),
                    *value,
                    Some(resolve::type_name(&local.signature)),
                    options,
                    &mut visiting,
                )
                .await;
            if display != local.name {
                // Inlined code: Kotlin suffixes the slot name (`it\1`).
                rendered["slot_name"] = json!(local.name);
            }
            variables.push(rendered);
        }
        let this = match self.this_value(&selected).await {
            Ok(Some(this)) => {
                let mut visiting = HashSet::new();
                Some(
                    self.render("this".into(), this, None, options, &mut visiting)
                        .await,
                )
            }
            _ => None,
        };
        let thread_name = self.thread_name(selected.thread).await;
        let mut out = json!({
            "session": session,
            "selected_frame": selected_json(&selected, &thread_name),
            "this": this,
            "variables": variables,
        });
        if let Some(warning) = warning {
            out["warning"] = json!(warning);
        }
        Ok(out)
    }

    /// `this` of a frame. ART keeps it in an ordinary local slot named by the
    /// variable table (not necessarily slot 0), so read that slot when the
    /// table names it and fall back to StackFrame.ThisObject.
    pub(super) async fn this_value(&self, selected: &SelectedFrame) -> RpcResult<Option<Value>> {
        let table = self
            .variable_table(selected.location.class_id, selected.location.method_id)
            .await?;
        if let Some(this) = table
            .iter()
            .find(|v| v.name == "this" && v.visible_at(selected.location.index))
        {
            let values = self
                .jdwp
                .frame_values(
                    selected.thread,
                    selected.frame_id,
                    &[(this.slot, tag::for_signature(&this.signature))],
                )
                .await;
            if let Ok(values) = values
                && let Some(value) = values.into_iter().next()
                && !value.is_null()
            {
                return Ok(Some(value));
            }
        }
        Ok(self
            .jdwp
            .this_object(selected.thread, selected.frame_id)
            .await?)
    }

    pub(super) async fn visible_locals(
        &self,
        selected: &SelectedFrame,
    ) -> RpcResult<Vec<(super::vm::Variable, Value)>> {
        let table = self
            .variable_table(selected.location.class_id, selected.location.method_id)
            .await?;
        let visible: Vec<_> = table
            .iter()
            .filter(|v| v.visible_at(selected.location.index))
            .filter(|v| v.name != "this" && !resolve::is_hidden_local(&v.name))
            .cloned()
            .collect();
        if visible.is_empty() {
            return Ok(Vec::new());
        }
        let slots: Vec<(i32, u8)> = visible
            .iter()
            .map(|v| (v.slot, tag::for_signature(&v.signature)))
            .collect();
        let values = self
            .jdwp
            .frame_values(selected.thread, selected.frame_id, &slots)
            .await?;
        Ok(visible.into_iter().zip(values).collect())
    }

    // ── path evaluation ───────────────────────────────────────────────

    pub async fn eval(
        &self,
        expression: &str,
        thread: Option<&str>,
        frame: Option<usize>,
        options: RenderOptions,
        invoke: Option<std::time::Duration>,
    ) -> RpcResult<Json> {
        let suspension = self.require_suspended()?;
        let parsed = parse_for_eval(expression, invoke.is_some())?;
        let selected = self.select_frame(thread, frame).await?;
        let ctx = EvalCtx::new(Some(selected), suspension.exception, invoke);
        let result = self
            .evaluate_for_render(&ctx, &parsed, expression, options)
            .await?;
        let thread_name = self.thread_name(selected.thread).await;
        Ok(json!({
            "session": self.status().await,
            "selected_frame": selected_json(&selected, &thread_name),
            "expression": expression,
            "mode": if invoke.is_some() { "jdi_invoke" } else { "jdi_path" },
            "result": result,
        }))
    }

    /// Evaluate and render: a path renders with its type; any other
    /// expression renders its value; a thrown invoke is a result too.
    async fn evaluate_for_render(
        &self,
        ctx: &EvalCtx,
        parsed: &Expr,
        name: &str,
        options: RenderOptions,
    ) -> RpcResult<Json> {
        let options = RenderOptions {
            to_string: ctx.invoke.map(|timeout| ToStringOptions {
                thread: ctx.thread,
                timeout,
                max_chars: options.max_message_chars,
            }),
            ..options
        };
        match parsed {
            Expr::Path(path) => match self.eval_path(ctx, path).await {
                Ok((value, declared)) => {
                    let mut visiting = HashSet::new();
                    Ok(self
                        .render(name.to_string(), value, declared, options, &mut visiting)
                        .await)
                }
                Err(error) if error.code == INVOKE_THREW => Ok(json!({
                    "name": name,
                    "thrown": true,
                    "exception": error.detail.get("thrown"),
                    "value": error.message,
                })),
                Err(error) => Err(error),
            },
            other => match self.eval_expr(ctx, other).await {
                Ok(value) => Ok(json!({
                    "name": name,
                    "type": eval_value_type(&value),
                    "value": value.display(),
                })),
                Err(message) => Err(RpcError::new("invalid_expression", message)),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn inspect(
        &self,
        expression: Option<&str>,
        handle: Option<&str>,
        path: Option<&str>,
        thread: Option<&str>,
        frame: Option<usize>,
        options: RenderOptions,
        invoke: Option<std::time::Duration>,
    ) -> RpcResult<Json> {
        let suspension = self.require_suspended()?;
        let (ctx, source, selected) = match (handle, expression) {
            (Some(handle), _) => {
                let object = self.resolve_handle(handle).await?;
                let root = Value::Object {
                    tag: tag::OBJECT,
                    id: object,
                };
                // A frame is needed only to invoke on its thread.
                let selected = match invoke {
                    Some(_) => Some(self.select_frame(thread, frame).await?),
                    None => None,
                };
                let ctx = EvalCtx::new(selected, suspension.exception, invoke).with_handle(root);
                let source = format!("{HANDLE_ROOT}{}", path.unwrap_or("").trim());
                (ctx, source, selected)
            }
            (None, Some(expression)) => {
                let selected = self.select_frame(thread, frame).await?;
                let ctx = EvalCtx::new(Some(selected), suspension.exception, invoke);
                (ctx, expression.to_string(), Some(selected))
            }
            (None, None) => {
                return Err(RpcError::new(
                    "invalid_expression",
                    "missing expression or handle",
                ));
            }
        };
        if let (Some(_), Some(path)) = (handle, path.map(str::trim))
            && !path.is_empty()
            && !path.starts_with(['.', '['])
        {
            return Err(RpcError::new(
                "invalid_expression",
                format!("relative path must start with . or [: {path}"),
            ));
        }
        let parsed = parse_for_eval(&source, invoke.is_some())?;
        if handle.is_some() && !matches!(parsed, Expr::Path(_)) {
            return Err(RpcError::new(
                "invalid_expression",
                format!(
                    "relative path must start with . or [: {}",
                    path.unwrap_or("")
                ),
            ));
        }
        let name = handle.or(expression).unwrap_or("result").to_string();
        let result = self
            .evaluate_for_render(&ctx, &parsed, &name, options)
            .await?;
        let selected_frame = match &selected {
            Some(selected) if handle.is_none() => {
                let thread_name = self.thread_name(selected.thread).await;
                selected_json(selected, &thread_name)
            }
            _ => Json::Null,
        };
        Ok(json!({
            "type": "debug_inspect",
            "schema_version": 2,
            "mode": match (handle.is_some(), invoke.is_some()) {
                (true, _) => "object_handle",
                (false, true) => "jdi_invoke",
                (false, false) => "jdi_path",
            },
            "session": self.status().await,
            "selected_frame": selected_frame,
            "expression": expression,
            "handle": handle,
            "path": path,
            "handle_scope": {"valid_until": "resume", "epoch": self.epoch()},
            "limits": {
                "depth": options.depth,
                "max_fields": options.max_fields,
                "max_array_items": options.max_array_items,
            },
            "result": result,
        }))
    }

    /// Runtime class of `object`, with its signature cached.
    pub(super) async fn runtime_type(&self, object: u64) -> RpcResult<(u64, String)> {
        let (_, type_id) = self.jdwp.object_type(object).await?;
        Ok((type_id, self.signature(type_id).await?))
    }

    /// Classes from `type_id` up to `java.lang.Object`.
    pub(super) async fn hierarchy(&self, type_id: u64) -> RpcResult<Vec<u64>> {
        let mut chain = vec![type_id];
        let mut current = type_id;
        while chain.len() < 32 {
            match self.superclass(current).await? {
                Some(parent) => {
                    chain.push(parent);
                    current = parent;
                }
                None => break,
            }
        }
        Ok(chain)
    }

    pub(super) async fn find_field(
        &self,
        object: u64,
        name: &str,
    ) -> RpcResult<Option<(u64, FieldInfo)>> {
        let (type_id, _) = self.runtime_type(object).await?;
        for class in self.hierarchy(type_id).await? {
            if let Some(field) = self.fields(class).await?.iter().find(|f| f.name == name) {
                return Ok(Some((class, field.clone())));
            }
        }
        Ok(None)
    }

    pub(super) async fn read_field(
        &self,
        object: u64,
        owner: u64,
        field: &FieldInfo,
    ) -> RpcResult<Value> {
        let values = if field.mod_bits & ACC_STATIC != 0 {
            self.jdwp.static_values(owner, &[field.field_id]).await?
        } else {
            self.jdwp.object_values(object, &[field.field_id]).await?
        };
        Ok(values.into_iter().next().unwrap_or(Value::Void))
    }

    /// The primitive inside a boxed value (`java.lang.Integer.value`, …).
    pub(super) async fn unboxed(&self, object: u64) -> Option<Value> {
        self.field_by_name(object, "value").await
    }

    pub(super) async fn field_by_name(&self, object: u64, name: &str) -> Option<Value> {
        let (owner, field) = self.find_field(object, name).await.ok()??;
        self.read_field(object, owner, &field).await.ok()
    }

    async fn is_throwable(&self, type_id: u64) -> bool {
        let Ok(chain) = self.hierarchy(type_id).await else {
            return false;
        };
        for class in chain {
            if self.signature(class).await.ok().as_deref() == Some("Ljava/lang/Throwable;") {
                return true;
            }
        }
        false
    }

    // ── rendering ─────────────────────────────────────────────────────

    pub fn render<'a>(
        &'a self,
        name: String,
        value: Value,
        declared: Option<String>,
        options: RenderOptions,
        visiting: &'a mut HashSet<u64>,
    ) -> BoxFuture<'a, Json> {
        Box::pin(async move {
            let mut payload = json!({
                "name": name,
                "declared_type": declared,
                "type": primitive_type(&value),
                "value": primitive_text(&value),
            });
            let Some(object) = value.object_id() else {
                if value.is_null() {
                    payload["value"] = Json::Null;
                    payload["type"] = Json::Null;
                }
                return payload;
            };
            let (type_id, signature) = match self.runtime_type(object).await {
                Ok(found) => found,
                Err(error) => {
                    payload["object_id"] = json!(object);
                    payload["error"] = json!(error.message);
                    return payload;
                }
            };
            let type_name = resolve::type_name(&signature);
            payload["type"] = json!(type_name);
            payload["object_id"] = json!(object);
            payload["value"] = json!(format!("instance of {type_name}(id={object})"));
            if let Some(handle) = self.pin(object).await {
                payload["object_handle"] = json!(handle);
            }

            if signature == "Ljava/lang/String;" {
                if let Ok(text) = self.jdwp.string_value(object).await {
                    let (text, truncated) = truncate(&text, MAX_STRING_CHARS);
                    payload["value"] = json!(text);
                    payload["string"] = json!(text);
                    if truncated {
                        payload["truncated"] = json!(true);
                    }
                }
                return payload;
            }
            if let Some(primitive) = boxed_primitive(&signature) {
                if let Some(inner) = self.field_by_name(object, "value").await {
                    payload["value"] = primitive_text(&inner);
                    payload["boxed"] = json!(primitive);
                }
                return payload;
            }
            // With --invoke, the root object of a read shows its own
            // toString() unless a built-in renderer describes it better.
            if let Some(to_string) = options.to_string
                && visiting.is_empty()
                && !has_builtin_renderer(&signature)
                && !self.is_throwable(type_id).await
                && let Some(rendered) = self
                    .to_string_of(
                        to_string.thread,
                        object,
                        to_string.timeout,
                        to_string.max_chars,
                    )
                    .await
            {
                if let Some(text) = rendered.get("text") {
                    payload["value"] = text.clone();
                }
                payload["to_string"] = rendered;
            }
            if options.depth == 0 {
                return payload;
            }
            if !visiting.insert(object) {
                payload["cycle"] = json!(true);
                return payload;
            }
            let child = options.child();
            if signature.starts_with('[') {
                self.render_array(
                    &mut payload,
                    object,
                    child,
                    options.max_array_items,
                    visiting,
                )
                .await;
            } else if matches!(
                signature.as_str(),
                "Ljava/util/ArrayList;" | "Ljava/util/Arrays$ArrayList;"
            ) {
                self.render_array_list(&mut payload, object, child, options, visiting)
                    .await;
            } else if matches!(
                signature.as_str(),
                "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;"
            ) {
                self.render_hash_map(&mut payload, object, child, options, visiting)
                    .await;
            } else if self.is_throwable(type_id).await {
                self.render_throwable(&mut payload, object).await;
                payload["fields"] = self
                    .render_fields(object, type_id, child, options, visiting)
                    .await;
            } else {
                payload["fields"] = self
                    .render_fields(object, type_id, child, options, visiting)
                    .await;
            }
            visiting.remove(&object);
            payload
        })
    }

    async fn render_array(
        &self,
        payload: &mut Json,
        array: u64,
        child: RenderOptions,
        max_items: u32,
        visiting: &mut HashSet<u64>,
    ) {
        let Ok(length) = self.jdwp.array_length(array).await else {
            return;
        };
        payload["length"] = json!(length);
        let count = length.clamp(0, max_items as i32);
        let values = if count > 0 {
            self.jdwp
                .array_values(array, 0, count)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut items = Vec::with_capacity(values.len());
        for (index, value) in values.into_iter().enumerate() {
            items.push(
                self.render(format!("[{index}]"), value, None, child, visiting)
                    .await,
            );
        }
        payload["items"] = Json::Array(items);
        if length > count {
            payload["truncated_items"] = json!(length - count);
        }
    }

    async fn render_array_list(
        &self,
        payload: &mut Json,
        list: u64,
        child: RenderOptions,
        options: RenderOptions,
        visiting: &mut HashSet<u64>,
    ) {
        let size = match self.field_by_name(list, "size").await {
            Some(Value::Int(size)) => size,
            _ => 0,
        };
        payload["size"] = json!(size);
        let Some(Value::Object { id: data, .. }) = self.field_by_name(list, "elementData").await
        else {
            return;
        };
        if data == 0 {
            payload["items"] = json!([]);
            return;
        }
        let count = size.clamp(0, options.max_array_items as i32);
        let values = if count > 0 {
            self.jdwp
                .array_values(data, 0, count)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut items = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            items.push(
                self.render(format!("[{index}]"), value, None, child, visiting)
                    .await,
            );
        }
        payload["items"] = Json::Array(items);
        if size > count {
            payload["truncated_items"] = json!(size - count);
        }
    }

    async fn render_hash_map(
        &self,
        payload: &mut Json,
        map: u64,
        child: RenderOptions,
        options: RenderOptions,
        visiting: &mut HashSet<u64>,
    ) {
        if let Some(Value::Int(size)) = self.field_by_name(map, "size").await {
            payload["size"] = json!(size);
        }
        let Some(Value::Object { id: table, .. }) = self.field_by_name(map, "table").await else {
            return;
        };
        let mut entries = Vec::new();
        if table != 0 {
            let length = self.jdwp.array_length(table).await.unwrap_or(0);
            let buckets = self
                .jdwp
                .array_values(table, 0, length.max(0))
                .await
                .unwrap_or_default();
            'buckets: for bucket in buckets {
                let mut node = bucket.object_id();
                let mut chain = 0;
                while let Some(current) = node {
                    if entries.len() >= options.max_array_items as usize {
                        break 'buckets;
                    }
                    chain += 1;
                    if chain > 1024 {
                        break;
                    }
                    let key = self
                        .field_by_name(current, "key")
                        .await
                        .unwrap_or(Value::Void);
                    let value = self
                        .field_by_name(current, "value")
                        .await
                        .unwrap_or(Value::Void);
                    let key = self.render("key".into(), key, None, child, visiting).await;
                    let value = self
                        .render("value".into(), value, None, child, visiting)
                        .await;
                    entries.push(json!({"key": key, "value": value}));
                    node = self
                        .field_by_name(current, "next")
                        .await
                        .and_then(|next| next.object_id());
                }
            }
        }
        payload["entries"] = Json::Array(entries);
    }

    async fn render_throwable(&self, payload: &mut Json, throwable: u64) {
        let mut causes = Vec::new();
        let mut current = throwable;
        let mut seen = HashSet::new();
        while causes.len() < MAX_CAUSES && seen.insert(current) {
            let type_name = match self.runtime_type(current).await {
                Ok((_, signature)) => resolve::type_name(&signature),
                Err(_) => break,
            };
            let message = match self.field_by_name(current, "detailMessage").await {
                Some(Value::Object { id, .. }) if id != 0 => self.jdwp.string_value(id).await.ok(),
                _ => None,
            };
            causes.push(json!({"type": type_name, "message": message}));
            match self
                .field_by_name(current, "cause")
                .await
                .and_then(|cause| cause.object_id())
            {
                // `cause == this` means "no cause" in java.lang.Throwable.
                Some(cause) if cause != current => current = cause,
                _ => break,
            }
        }
        if let Some(first) = causes.first() {
            payload["message"] = first["message"].clone();
        }
        payload["cause_chain"] = Json::Array(causes);
    }

    async fn render_fields(
        &self,
        object: u64,
        type_id: u64,
        child: RenderOptions,
        options: RenderOptions,
        visiting: &mut HashSet<u64>,
    ) -> Json {
        let mut instance_fields: Vec<FieldInfo> = Vec::new();
        if let Ok(chain) = self.hierarchy(type_id).await {
            for class in chain {
                if let Ok(fields) = self.fields(class).await {
                    instance_fields.extend(
                        fields
                            .iter()
                            .filter(|f| f.mod_bits & ACC_STATIC == 0)
                            // ART's java.lang.Object bookkeeping is never app state.
                            .filter(|f| !f.name.starts_with("shadow$"))
                            .cloned(),
                    );
                }
            }
        }
        let total = instance_fields.len();
        instance_fields.truncate(options.max_fields as usize);
        let ids: Vec<u64> = instance_fields.iter().map(|f| f.field_id).collect();
        let values = if ids.is_empty() {
            Vec::new()
        } else {
            match self.jdwp.object_values(object, &ids).await {
                Ok(values) => values,
                Err(error) => return json!([{"error": error.to_string()}]),
            }
        };
        let mut out = Vec::with_capacity(values.len() + 1);
        for (field, value) in instance_fields.iter().zip(values) {
            out.push(
                self.render(
                    field.name.clone(),
                    value,
                    Some(resolve::type_name(&field.signature)),
                    child,
                    visiting,
                )
                .await,
            );
        }
        if total > instance_fields.len() {
            out.push(
                json!({"name": "<truncated>", "truncated_fields": total - instance_fields.len()}),
            );
        }
        Json::Array(out)
    }
}

fn selected_json(selected: &SelectedFrame, thread_name: &str) -> Json {
    json!({
        "thread": selected.thread_index,
        "thread_name": thread_name,
        "frame": selected.frame_index,
    })
}

fn primitive_type(value: &Value) -> Json {
    json!(match value {
        Value::Void => "void",
        Value::Boolean(_) => "boolean",
        Value::Byte(_) => "byte",
        Value::Char(_) => "char",
        Value::Short(_) => "short",
        Value::Int(_) => "int",
        Value::Long(_) => "long",
        Value::Float(_) => "float",
        Value::Double(_) => "double",
        Value::Object { .. } => return Json::Null,
    })
}

fn primitive_text(value: &Value) -> Json {
    match *value {
        Value::Void => Json::Null,
        Value::Boolean(v) => json!(v.to_string()),
        Value::Byte(v) => json!(v.to_string()),
        Value::Char(v) => json!(
            char::from_u32(u32::from(v)).map_or_else(|| format!("\\u{v:04x}"), |c| c.to_string())
        ),
        Value::Short(v) => json!(v.to_string()),
        Value::Int(v) => json!(v.to_string()),
        Value::Long(v) => json!(v.to_string()),
        Value::Float(v) => json!(v.to_string()),
        Value::Double(v) => json!(v.to_string()),
        Value::Object { .. } => Json::Null,
    }
}

/// Types the read renderers describe without running app code.
fn has_builtin_renderer(signature: &str) -> bool {
    signature.starts_with('[')
        || matches!(
            signature,
            "Ljava/util/ArrayList;"
                | "Ljava/util/Arrays$ArrayList;"
                | "Ljava/util/HashMap;"
                | "Ljava/util/LinkedHashMap;"
        )
}

/// Whether a class signature is a boxed primitive.
pub fn is_boxed(signature: &str) -> bool {
    boxed_primitive(signature).is_some()
}

fn boxed_primitive(signature: &str) -> Option<&'static str> {
    Some(match signature {
        "Ljava/lang/Integer;" => "int",
        "Ljava/lang/Long;" => "long",
        "Ljava/lang/Boolean;" => "boolean",
        "Ljava/lang/Short;" => "short",
        "Ljava/lang/Byte;" => "byte",
        "Ljava/lang/Character;" => "char",
        "Ljava/lang/Float;" => "float",
        "Ljava/lang/Double;" => "double",
        _ => return None,
    })
}

fn truncate(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        (text.to_string(), false)
    } else {
        (text.chars().take(max).collect(), true)
    }
}

/// Request deadline for read verbs: the caller's `--timeout-ms`, bounded.
/// Expression root naming the thrown object at an exception stop.
pub const EXCEPTION_ROOT: &str = "$exception";

pub fn read_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.clamp(100, 120_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_render_like_jdi() {
        assert_eq!(primitive_text(&Value::Int(-3)), json!("-3"));
        assert_eq!(primitive_text(&Value::Boolean(true)), json!("true"));
        assert_eq!(primitive_text(&Value::Char(65)), json!("A"));
        assert_eq!(primitive_type(&Value::Long(1)), json!("long"));
        assert_eq!(boxed_primitive("Ljava/lang/Integer;"), Some("int"));
        assert_eq!(boxed_primitive("Ljava/lang/String;"), None);
        assert_eq!(truncate("abcdef", 3), ("abc".to_string(), true));
        assert_eq!(read_timeout(1), Duration::from_millis(100));
    }
}
