//! Expression evaluation in a suspended frame: paths (`this`, locals,
//! `$exception`, fields, indexes), the condition operators, and — only with
//! `--invoke` — method calls through ObjectReference/ClassType.InvokeMethod.
//!
//! Invocation rules:
//! * single-threaded (`INVOKE_SINGLE_THREADED`) on the thread the event or
//!   selection suspended, bounded by the caller's timeout;
//! * an exception the method throws is a structured result
//!   ([`INVOKE_THREW`] with the thrown object's type and message), never a
//!   transport error;
//! * events the invoked code raises on that thread (a breakpoint inside it)
//!   are resumed by the session's event forwarder instead of stopping, so an
//!   invoke cannot deadlock the daemon (see `Session::run_events`);
//! * frame ids die with the invoke, so the frame is re-read afterwards.
//! * Kotlin property sugar: with `--invoke`, `x.name` that is not a field
//!   calls `getName()` / `isName()`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value as Json, json};

use super::codec::Value;
use super::expr::{self, EvalValue, Expr, Literal, Path, Segment};
use super::inspect::{EXCEPTION_ROOT, SelectedFrame};
use super::invoke::InvokeCall;
use super::protocol::tag;
use super::resolve;
use super::session::{RpcError, RpcResult, Session};
use super::vm::MethodInfo;

/// Error code of an evaluation whose invoked method threw. The detail
/// carries `thrown: {type, message, object_id, object_handle}`.
pub const INVOKE_THREW: &str = "invoke_threw";
/// Root name for `inspect --handle --path`: the handle's object.
pub const HANDLE_ROOT: &str = "__handle__";
/// Default bound for invokes made by breakpoint conditions/log expressions.
pub const DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_millis(1_000);

/// One evaluation: the frame it reads, and what it may do.
pub struct EvalCtx {
    pub thread: u64,
    pub frame_index: usize,
    /// `None` for a handle-rooted read with no frame selected.
    frame: Mutex<Option<SelectedFrame>>,
    /// Set by an invoke: the frame id must be re-read before the next use.
    stale: AtomicBool,
    pub exception: Option<Value>,
    /// `Some(timeout)` when method calls are allowed (`--invoke`).
    pub invoke: Option<Duration>,
    pub handle_root: Option<Value>,
}

impl EvalCtx {
    pub fn new(
        frame: Option<SelectedFrame>,
        exception: Option<Value>,
        invoke: Option<Duration>,
    ) -> Self {
        Self {
            thread: frame.map_or(0, |f| f.thread),
            frame_index: frame.map_or(0, |f| f.frame_index),
            frame: Mutex::new(frame),
            stale: AtomicBool::new(false),
            exception,
            invoke,
            handle_root: None,
        }
    }

    pub fn with_handle(mut self, root: Value) -> Self {
        self.handle_root = Some(root);
        self
    }

    /// An invoke ran: frame ids read before it are no longer valid.
    pub fn is_stale(&self) -> bool {
        self.stale.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn selected(&self) -> Option<SelectedFrame> {
        *self.frame.lock().expect("frame lock")
    }
}

/// Argument and return types of a JNI method signature.
pub fn method_types(signature: &str) -> (Vec<String>, String) {
    let Some(inner) = signature.strip_prefix('(') else {
        return (Vec::new(), String::new());
    };
    let (params, ret) = inner.split_once(')').unwrap_or((inner, ""));
    let mut out = Vec::new();
    let bytes = params.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        out.push(params[start..i.min(params.len())].to_string());
    }
    (out, ret.to_string())
}

/// Whether an argument can be passed for a parameter of JNI type `param`
/// (used to pick between overloads).
fn arg_fits(arg: &Arg, param: &str) -> bool {
    let reference = param.starts_with('L') || param.starts_with('[');
    match arg {
        Arg::Literal(Literal::Null) => reference,
        Arg::Literal(Literal::Bool(_)) => param == "Z",
        Arg::Literal(Literal::Int(_)) => matches!(param, "I" | "J" | "S" | "B" | "F" | "D"),
        Arg::Literal(Literal::Float(_)) => matches!(param, "F" | "D"),
        Arg::Literal(Literal::Str(text)) => {
            matches!(
                param,
                "Ljava/lang/String;" | "Ljava/lang/CharSequence;" | "Ljava/lang/Object;"
            ) || (param == "C" && text.chars().count() == 1)
        }
        Arg::Value(value) => match value {
            Value::Object { .. } => reference,
            Value::Boolean(_) => param == "Z",
            Value::Char(_) => matches!(param, "C" | "I" | "J"),
            Value::Byte(_) | Value::Short(_) | Value::Int(_) => {
                matches!(param, "I" | "J" | "F" | "D" | "S" | "B")
            }
            Value::Long(_) => matches!(param, "J" | "F" | "D"),
            Value::Float(_) | Value::Double(_) => matches!(param, "F" | "D"),
            Value::Void => false,
        },
    }
}

/// An evaluated call argument before coercion to the parameter type.
enum Arg {
    Literal(Literal),
    Value(Value),
}

impl Session {
    /// The context's frame, re-read if an invoke invalidated its id.
    async fn ctx_frame(&self, ctx: &EvalCtx) -> RpcResult<SelectedFrame> {
        if ctx.thread != 0 {
            self.ensure_thread_not_busy(ctx.thread)?;
        }
        if ctx.stale.swap(false, Ordering::SeqCst) {
            let frames = self
                .jdwp
                .frames(ctx.thread, ctx.frame_index as i32, 1)
                .await?;
            if let Some((frame_id, location)) = frames.first().copied()
                && let Some(frame) = ctx.frame.lock().expect("frame lock").as_mut()
            {
                frame.frame_id = frame_id;
                frame.location = location;
            }
        }
        ctx.selected()
            .ok_or_else(|| RpcError::new("invalid_expression", "no frame is selected"))
    }

    /// Evaluate a path to a JDWP value and its declared type.
    pub(super) fn eval_path<'a>(
        &'a self,
        ctx: &'a EvalCtx,
        path: &'a Path,
    ) -> BoxFuture<'a, RpcResult<(Value, Option<String>)>> {
        Box::pin(async move {
            let (mut value, mut declared) = self.eval_root(ctx, &path.root).await?;
            for (index, segment) in path.segments.iter().enumerate() {
                (value, declared) = match segment {
                    Segment::Index(position) => (self.index(value, *position).await?, None),
                    Segment::Field(name) => {
                        self.field_or_property(ctx, value, name, path, index)
                            .await?
                    }
                    Segment::Call(name, args) => {
                        self.call(ctx, value, name, args, &path.text).await?
                    }
                };
            }
            Ok((value, declared))
        })
    }

    async fn eval_root(&self, ctx: &EvalCtx, root: &str) -> RpcResult<(Value, Option<String>)> {
        if root == EXCEPTION_ROOT {
            let exception = ctx.exception.ok_or_else(|| {
                RpcError::new(
                    "invalid_expression",
                    "`$exception` is only set while stopped at an exception breakpoint",
                )
            })?;
            return Ok((exception, Some("java.lang.Throwable".to_string())));
        }
        if root == HANDLE_ROOT
            && let Some(handle) = ctx.handle_root
        {
            return Ok((handle, None));
        }
        let selected = self.ctx_frame(ctx).await?;
        if root == "this" {
            let this = self.this_value(&selected).await?.ok_or_else(|| {
                RpcError::new(
                    "invalid_expression",
                    "`this` is unavailable in a static frame",
                )
            })?;
            return Ok((this, None));
        }
        let locals = self.visible_locals(&selected).await?;
        // The bare name matches inlined slots too (`it` for `it\1`); the
        // innermost visible one (latest start, then narrowest) wins.
        let (local, value) = locals
            .into_iter()
            .filter(|(local, _)| {
                local.name == root || resolve::display_local_name(&local.name) == root
            })
            .max_by_key(|(local, _)| {
                (
                    local.name == root,
                    local.code_index,
                    std::cmp::Reverse(local.length),
                )
            })
            .ok_or_else(|| {
                RpcError::new("invalid_expression", format!("unknown local: {root}"))
                    .next(&["shadowdroid debug variables --backend jdwp"])
            })?;
        Ok((value, Some(resolve::type_name(&local.signature))))
    }

    async fn index(&self, value: Value, position: i64) -> RpcResult<Value> {
        let array = match value {
            Value::Object {
                tag: tag::ARRAY,
                id,
            } if id != 0 => id,
            _ => {
                return Err(RpcError::new(
                    "invalid_expression",
                    "cannot index a non-array value",
                ));
            }
        };
        let length = self.jdwp.array_length(array).await?;
        if position < 0 || position >= i64::from(length) {
            return Err(RpcError::new(
                "invalid_expression",
                format!("array index out of bounds: {position} (length {length})"),
            ));
        }
        Ok(self
            .jdwp
            .array_values(array, position as i32, 1)
            .await?
            .into_iter()
            .next()
            .unwrap_or(Value::Object {
                tag: tag::OBJECT,
                id: 0,
            }))
    }

    async fn field_or_property(
        &self,
        ctx: &EvalCtx,
        value: Value,
        name: &str,
        path: &Path,
        index: usize,
    ) -> RpcResult<(Value, Option<String>)> {
        let object = value.object_id().ok_or_else(|| {
            RpcError::new(
                "invalid_expression",
                format!("cannot read field {name} from a null or primitive value"),
            )
        })?;
        if let Some((owner, field)) = self.find_field(object, name).await? {
            let value = self.read_field(object, owner, &field).await?;
            return Ok((value, Some(resolve::type_name(&field.signature))));
        }
        let getter = self.property_getter(object, name).await?;
        if let (Some(_), Some(getter)) = (ctx.invoke, &getter) {
            // Kotlin property sugar: `x.name` → `x.getName()`.
            return self.call(ctx, value, getter, &[], &path.text).await;
        }
        // A Kotlin delegated property (`by mutableStateOf`, `by lazy`) is
        // stored as `<name>$delegate`.
        let delegate = format!("{name}$delegate");
        if self.find_field(object, &delegate).await?.is_some() {
            let mut suggestion = Path {
                text: String::new(),
                root: path.root.clone(),
                segments: path.segments.clone(),
            };
            suggestion.segments[index] = Segment::Field(delegate.clone());
            return Err(RpcError::new(
                "invalid_expression",
                format!(
                    "field not found: {name} (a Kotlin delegated property; its state is in `{delegate}`)"
                ),
            )
            .detail(json!({
                "suggestion": path_text(&suggestion),
                "getter": getter,
            })));
        }
        let mut error = RpcError::new("invalid_expression", format!("field not found: {name}"));
        if let Some(getter) = getter {
            error = RpcError::new(
                "invalid_expression",
                format!("field not found: {name}; `--invoke` would call {getter}()"),
            )
            .detail(json!({"getter": getter}));
        }
        Err(error)
    }

    /// `getName` / `isName` with no parameters, if the object has one.
    async fn property_getter(&self, object: u64, name: &str) -> RpcResult<Option<String>> {
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return Ok(None);
        };
        let capitalized: String = first.to_uppercase().chain(chars).collect();
        let candidates = [format!("get{capitalized}"), format!("is{capitalized}")];
        let (type_id, _) = self.runtime_type(object).await?;
        for class in self.hierarchy(type_id).await? {
            for method in self.methods(class).await?.iter() {
                if candidates.contains(&method.name) && method.signature.starts_with("()") {
                    return Ok(Some(method.name.clone()));
                }
            }
        }
        Ok(None)
    }

    /// `receiver.name(args)`: pick the overload, coerce arguments, invoke.
    async fn call(
        &self,
        ctx: &EvalCtx,
        receiver: Value,
        name: &str,
        args: &[Expr],
        text: &str,
    ) -> RpcResult<(Value, Option<String>)> {
        let Some(timeout) = ctx.invoke else {
            return Err(RpcError::new(
                "invoke_not_allowed",
                format!("`{text}` calls a method; add --invoke to run app code"),
            )
            .next(&["re-run with --invoke"]));
        };
        let object = receiver.object_id().ok_or_else(|| {
            RpcError::new(
                "invalid_expression",
                format!("cannot call {name}() on a null or primitive value"),
            )
        })?;
        let mut evaluated = Vec::with_capacity(args.len());
        for arg in args {
            evaluated.push(match arg {
                Expr::Literal(literal) => Arg::Literal(literal.clone()),
                Expr::Path(path) => Arg::Value(self.eval_path(ctx, path).await?.0),
                other => match self.eval_expr(ctx, other).await {
                    Ok(EvalValue::Bool(value)) => Arg::Value(Value::Boolean(value)),
                    Ok(other) => Arg::Literal(match other {
                        EvalValue::Int(v) => Literal::Int(v),
                        EvalValue::Float(v) => Literal::Float(v),
                        EvalValue::Str(v) => Literal::Str(v),
                        _ => Literal::Null,
                    }),
                    Err(message) => return Err(RpcError::new("invalid_expression", message)),
                },
            });
        }
        let (type_id, _) = self.runtime_type(object).await?;
        let (declaring, method) = self.pick_method(type_id, name, &evaluated).await?;
        let (params, ret) = method_types(&method.signature);
        let mut values = Vec::with_capacity(evaluated.len());
        for (arg, param) in evaluated.iter().zip(&params) {
            values.push(self.coerce(arg, param).await?);
        }
        let call = if method.is_static() {
            InvokeCall::Static {
                class_id: declaring,
                method_id: method.method_id,
                args: values,
            }
        } else {
            InvokeCall::Instance {
                object,
                class_id: declaring,
                method_id: method.method_id,
                args: values,
            }
        };
        let result = self.invoke_on(ctx.thread, call, timeout).await;
        // Frame ids die with an invoke: re-read before the next local.
        ctx.stale.store(true, Ordering::SeqCst);
        let result = result?;
        if let Some(thrown) = result.exception.object_id() {
            return Err(self.thrown_error(thrown, name).await);
        }
        Ok((result.value, Some(resolve::type_name(&ret))))
    }

    async fn thrown_error(&self, thrown: u64, method: &str) -> RpcError {
        let info = self.thrown_info(thrown).await;
        RpcError::new(
            INVOKE_THREW,
            format!(
                "{method}() threw {}{}",
                info["type"].as_str().unwrap_or("an exception"),
                info["message"]
                    .as_str()
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            ),
        )
        .detail(json!({"thrown": info}))
    }

    /// `{type, message, object_id, object_handle}` of a thrown object.
    pub(super) async fn thrown_info(&self, thrown: u64) -> Json {
        let type_name = self
            .runtime_type(thrown)
            .await
            .map(|(_, signature)| resolve::type_name(&signature))
            .ok();
        let message = match self.field_by_name(thrown, "detailMessage").await {
            Some(Value::Object { id, .. }) if id != 0 => self.jdwp.string_value(id).await.ok(),
            _ => None,
        };
        json!({
            "type": type_name,
            "message": message,
            "object_id": thrown,
            "object_handle": self.pin(thrown).await,
        })
    }

    async fn pick_method(
        &self,
        type_id: u64,
        name: &str,
        args: &[Arg],
    ) -> RpcResult<(u64, MethodInfo)> {
        let mut seen = Vec::new();
        for class in self.hierarchy(type_id).await? {
            for method in self.methods(class).await?.iter() {
                if method.name != name {
                    continue;
                }
                let (params, _) = method_types(&method.signature);
                seen.push(method.signature.clone());
                if params.len() == args.len()
                    && args
                        .iter()
                        .zip(&params)
                        .all(|(arg, param)| arg_fits(arg, param))
                {
                    return Ok((class, method.clone()));
                }
            }
        }
        Err(RpcError::new(
            "invalid_expression",
            if seen.is_empty() {
                format!("no method {name}")
            } else {
                format!(
                    "no overload of {name} takes these {} argument(s)",
                    args.len()
                )
            },
        )
        .detail(json!({"method": name, "signatures": seen})))
    }

    /// Convert an argument to the parameter's JDWP value.
    async fn coerce(&self, arg: &Arg, param: &str) -> RpcResult<Value> {
        let bad = || {
            RpcError::new(
                "invalid_expression",
                format!(
                    "argument does not fit parameter type {}",
                    resolve::type_name(param)
                ),
            )
        };
        Ok(match arg {
            Arg::Literal(Literal::Null) => Value::Object {
                tag: tag::OBJECT,
                id: 0,
            },
            Arg::Literal(Literal::Bool(v)) => Value::Boolean(*v),
            Arg::Literal(Literal::Int(v)) => match param {
                "I" => Value::Int(i32::try_from(*v).map_err(|_| bad())?),
                "J" => Value::Long(*v),
                "S" => Value::Short(i16::try_from(*v).map_err(|_| bad())?),
                "B" => Value::Byte(i8::try_from(*v).map_err(|_| bad())?),
                "F" => Value::Float(*v as f32),
                "D" => Value::Double(*v as f64),
                _ => return Err(bad()),
            },
            Arg::Literal(Literal::Float(v)) => match param {
                "F" => Value::Float(*v as f32),
                "D" => Value::Double(*v),
                _ => return Err(bad()),
            },
            Arg::Literal(Literal::Str(text)) if param == "C" => {
                Value::Char(text.chars().next().map(|c| c as u32 as u16).unwrap_or(0))
            }
            Arg::Literal(Literal::Str(text)) => Value::Object {
                tag: tag::STRING,
                id: self.jdwp.create_string(text).await?,
            },
            Arg::Value(value) => match (value, param) {
                (Value::Int(v), "J") => Value::Long(i64::from(*v)),
                (Value::Int(v), "D") => Value::Double(f64::from(*v)),
                (Value::Int(v), "F") => Value::Float(*v as f32),
                (Value::Long(v), "D") => Value::Double(*v as f64),
                (Value::Float(v), "D") => Value::Double(f64::from(*v)),
                (Value::Char(v), "I") => Value::Int(i32::from(*v)),
                (other, _) => *other,
            },
        })
    }

    /// Evaluate a condition or log expression.
    pub(super) fn eval_expr<'a>(
        &'a self,
        ctx: &'a EvalCtx,
        expr: &'a Expr,
    ) -> BoxFuture<'a, Result<EvalValue, String>> {
        Box::pin(async move {
            match expr {
                Expr::Literal(literal) => Ok(EvalValue::from(literal)),
                Expr::Path(path) => {
                    let (value, _) = self
                        .eval_path(ctx, path)
                        .await
                        .map_err(|error| error.message)?;
                    self.to_eval_value(value).await
                }
                Expr::Not(inner) => {
                    Ok(EvalValue::Bool(!self.eval_expr(ctx, inner).await?.truthy()))
                }
                Expr::And(left, right) => {
                    if !self.eval_expr(ctx, left).await?.truthy() {
                        return Ok(EvalValue::Bool(false));
                    }
                    Ok(EvalValue::Bool(self.eval_expr(ctx, right).await?.truthy()))
                }
                Expr::Or(left, right) => {
                    if self.eval_expr(ctx, left).await?.truthy() {
                        return Ok(EvalValue::Bool(true));
                    }
                    Ok(EvalValue::Bool(self.eval_expr(ctx, right).await?.truthy()))
                }
                Expr::Compare(left, op, right) => {
                    let left = self.eval_expr(ctx, left).await?;
                    let right = self.eval_expr(ctx, right).await?;
                    expr::compare(&left, *op, &right).map(EvalValue::Bool)
                }
            }
        })
    }

    /// JDWP value → comparable value: strings are read, boxed primitives
    /// unboxed, other objects stay opaque.
    pub(super) fn to_eval_value(&self, value: Value) -> BoxFuture<'_, Result<EvalValue, String>> {
        Box::pin(async move {
            Ok(match value {
                Value::Void => EvalValue::Null,
                Value::Boolean(v) => EvalValue::Bool(v),
                Value::Byte(v) => EvalValue::Int(i64::from(v)),
                Value::Short(v) => EvalValue::Int(i64::from(v)),
                Value::Int(v) => EvalValue::Int(i64::from(v)),
                Value::Long(v) => EvalValue::Int(v),
                Value::Char(v) => {
                    EvalValue::Char(char::from_u32(u32::from(v)).unwrap_or('\u{fffd}'))
                }
                Value::Float(v) => EvalValue::Float(f64::from(v)),
                Value::Double(v) => EvalValue::Float(v),
                Value::Object { id: 0, .. } => EvalValue::Null,
                Value::Object { id, .. } => {
                    let (_, signature) =
                        self.runtime_type(id).await.map_err(|error| error.message)?;
                    if signature == "Ljava/lang/String;" {
                        return self
                            .jdwp
                            .string_value(id)
                            .await
                            .map(EvalValue::Str)
                            .map_err(|error| error.to_string());
                    }
                    if super::inspect::is_boxed(&signature)
                        && let Some(inner) = self.unboxed(id).await
                    {
                        return self.to_eval_value(inner).await;
                    }
                    EvalValue::Object {
                        id,
                        text: format!("instance of {}(id={id})", resolve::type_name(&signature)),
                    }
                }
            })
        })
    }

    /// `toString()` of `object` on `thread`, bounded, for renderers.
    pub(super) async fn to_string_of(
        &self,
        thread: u64,
        object: u64,
        timeout: Duration,
        max_chars: u32,
    ) -> Option<Json> {
        let (type_id, _) = self.runtime_type(object).await.ok()?;
        let mut target = None;
        for class in self.hierarchy(type_id).await.ok()? {
            if let Some(method) = self
                .methods(class)
                .await
                .ok()?
                .iter()
                .find(|m| m.name == "toString" && m.signature == "()Ljava/lang/String;")
            {
                target = Some((class, method.method_id));
                break;
            }
        }
        let (class, method) = target?;
        let outcome = self
            .invoke_on(
                thread,
                InvokeCall::Instance {
                    object,
                    class_id: class,
                    method_id: method,
                    args: Vec::new(),
                },
                timeout,
            )
            .await;
        match outcome {
            Ok(result) if result.exception.object_id().is_some() => {
                let thrown = self.thrown_info(result.exception.object_id()?).await;
                Some(json!({"thrown": thrown}))
            }
            Ok(result) => {
                let text = match result.value.object_id() {
                    Some(id) => self.jdwp.string_value(id).await.ok()?,
                    None => "null".to_string(),
                };
                let (text, truncated, _) = super::logpoints::truncate_message(&text, max_chars);
                Some(json!({"text": text, "truncated": truncated}))
            }
            Err(error) => Some(json!({"error": error.message, "code": error.code})),
        }
    }
}

fn path_text(path: &Path) -> String {
    let mut out = path.root.clone();
    for segment in &path.segments {
        match segment {
            Segment::Field(name) => {
                out.push('.');
                out.push_str(name);
            }
            Segment::Index(index) => out.push_str(&format!("[{index}]")),
            Segment::Call(name, args) => {
                out.push('.');
                out.push_str(name);
                out.push_str(if args.is_empty() { "()" } else { "(…)" });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_signatures_split_into_parameter_types() {
        assert_eq!(
            method_types("(I[Ljava/lang/String;JLa/B;[[Z)Ljava/lang/Object;"),
            (
                vec![
                    "I".to_string(),
                    "[Ljava/lang/String;".to_string(),
                    "J".to_string(),
                    "La/B;".to_string(),
                    "[[Z".to_string()
                ],
                "Ljava/lang/Object;".to_string()
            )
        );
        assert_eq!(method_types("()V"), (vec![], "V".to_string()));
    }

    #[test]
    fn overloads_are_chosen_by_argument_kind() {
        assert!(arg_fits(&Arg::Literal(Literal::Int(1)), "J"));
        assert!(!arg_fits(
            &Arg::Literal(Literal::Int(1)),
            "Ljava/lang/String;"
        ));
        assert!(arg_fits(
            &Arg::Literal(Literal::Str("x".into())),
            "Ljava/lang/String;"
        ));
        assert!(arg_fits(&Arg::Literal(Literal::Str("x".into())), "C"));
        assert!(arg_fits(&Arg::Literal(Literal::Null), "[I"));
        assert!(!arg_fits(&Arg::Literal(Literal::Null), "I"));
        assert!(arg_fits(
            &Arg::Value(Value::Object {
                tag: tag::OBJECT,
                id: 3
            }),
            "La/B;"
        ));
    }

    #[test]
    fn suggestions_render_paths() {
        let path = expr::parse_path("this.counter.x[2].f()").unwrap();
        assert_eq!(path_text(&path), "this.counter.x[2].f()");
    }
}
