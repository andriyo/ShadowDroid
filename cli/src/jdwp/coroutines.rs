//! `debug coroutines snapshot|threads|continuation|flow --backend jdwp`,
//! in the Studio bridge's shapes, read with field access only (no invoke).
//!
//! Beyond what the Studio bridge reads from suspended frames, `snapshot`
//! discovers coroutines process-wide with ReferenceType.Instances (device:
//! ~2.3 s for 75 coroutines and 53 continuations, capped at 100 instances
//! per class): concrete `AbstractCoroutine` classes give the coroutines
//! (CoroutineName, dispatcher, job state from fields), app
//! `BaseContinuationImpl` subclasses give the suspended continuations, and
//! each continuation's `completion` chain leads to its coroutine — through
//! `DebugProbesImpl$CoroutineOwner.delegate` when DebugProbes is installed.
//! Source lines need `getStackTraceElement()` (an invoke); they are not
//! read here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use serde_json::{Value as Json, json};

use super::codec::Value;
use super::inspect::{RenderOptions, SelectedFrame};
use super::resolve;
use super::session::{RpcError, RpcResult, Session, is_framework_class};

/// Instances read per class (device default cap).
const MAX_INSTANCES_PER_CLASS: i32 = 100;
/// Base of every single-handler job state (`Incomplete`).
const JOB_NODE: &str = "Lkotlinx/coroutines/JobNode;";
const ABSTRACT_COROUTINE: &str = "Lkotlinx/coroutines/AbstractCoroutine;";
const BASE_CONTINUATION: &str = "Lkotlin/coroutines/jvm/internal/BaseContinuationImpl;";
const COROUTINE_OWNER: &str = "Lkotlinx/coroutines/debug/internal/DebugProbesImpl$CoroutineOwner;";

/// Thread-name dispatcher hint, as the Studio bridge derives it.
pub fn dispatcher_hint(thread: &str) -> Json {
    let lower = thread.to_lowercase();
    if lower.contains("main") {
        json!({"name": "Dispatchers.Main", "confidence": "medium"})
    } else if lower.contains("defaultdispatcher") || lower.contains("default") {
        json!({"name": "Dispatchers.Default", "confidence": "low"})
    } else if lower.contains("io") {
        json!({"name": "Dispatchers.IO", "confidence": "low"})
    } else {
        json!({"name": null, "confidence": "none"})
    }
}

/// Kotlin's spilled suspend locals: `L$0`, `I$1`, ...
pub fn is_spilled(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 3
        && matches!(bytes[0], b'L' | b'I' | b'J' | b'F' | b'D' | b'Z')
        && bytes[1] == b'$'
        && bytes[2..].iter().all(u8::is_ascii_digit)
}

pub fn flow_kind(type_name: &str) -> Option<&'static str> {
    if type_name.contains("StateFlow") {
        Some("StateFlow")
    } else if type_name.contains("SharedFlow") {
        Some("SharedFlow")
    } else if type_name.ends_with("Flow") || type_name.contains(".Flow") {
        Some("Flow")
    } else {
        None
    }
}

/// Classes that hold coroutines and continuations, discovered once per
/// session (class hierarchy walks cost ~1 s on a real app).
#[derive(Default, Clone)]
pub struct CoroutineClasses {
    pub coroutines: Vec<(u64, String)>,
    pub continuations: Vec<(u64, String)>,
}

impl Session {
    pub async fn coroutine_threads(&self, limit: u32) -> RpcResult<Json> {
        self.require_stop()?;
        Ok(json!({
            "type": "coroutine_threads",
            "schema_version": 1,
            "session": self.status().await,
            "threads": self.coroutine_threads_payload(limit).await?,
        }))
    }

    async fn coroutine_threads_payload(&self, limit: u32) -> RpcResult<Vec<Json>> {
        let threads = self.threads(limit).await?;
        Ok(threads["threads"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(limit as usize)
            .map(|thread| {
                let name = thread["name"].as_str().unwrap_or("").to_string();
                json!({
                    "index": thread["index"],
                    "name": name,
                    "dispatcher": dispatcher_hint(&name),
                    "top_frame": thread["top_frame"],
                    "frames": thread["frames"],
                })
            })
            .collect())
    }

    fn require_stop(&self) -> RpcResult<()> {
        self.suspension()
            .map(|_| ())
            .ok_or_else(RpcError::not_suspended)
    }

    pub async fn coroutine_continuation(
        &self,
        thread: Option<&str>,
        frame: Option<usize>,
        options: RenderOptions,
    ) -> RpcResult<Json> {
        self.require_stop()?;
        let selected = self.select_frame(thread, frame).await?;
        let continuations = self.continuation_candidates(&selected, options, 64).await?;
        let thread_name = self.thread_name(selected.thread).await;
        Ok(json!({
            "type": "coroutine_continuation",
            "schema_version": 1,
            "source": "jdwp_suspended_frame",
            "session": self.status().await,
            "selected_frame": {
                "thread": selected.thread_index,
                "thread_name": thread_name,
                "frame": selected.frame_index,
            },
            "continuations": continuations,
        }))
    }

    pub async fn coroutine_flow(
        &self,
        expression: &str,
        thread: Option<&str>,
        frame: Option<usize>,
        options: RenderOptions,
    ) -> RpcResult<Json> {
        self.require_stop()?;
        let mut value = self.eval(expression, thread, frame, options, None).await?;
        let type_name = value["result"]["type"].as_str().unwrap_or("").to_string();
        let kind = flow_kind(&type_name);
        // A StateFlow's current value is its `_state` field.
        let state = if kind == Some("StateFlow") {
            self.eval(
                &format!("{expression}._state"),
                thread,
                frame,
                options,
                None,
            )
            .await
            .ok()
            .map(|v| v["result"].clone())
        } else {
            None
        };
        Ok(json!({
            "type": "coroutine_flow",
            "schema_version": 1,
            "source": "jdwp_field_only",
            "session": value["session"].take(),
            "selected_frame": value["selected_frame"].take(),
            "expression": expression,
            "kind": kind,
            "confidence": if kind.is_some() { "medium" } else { "low" },
            "observation": "field_only_no_collection_no_getters",
            "value": value["result"].take(),
            "state_value": state,
        }))
    }

    pub async fn coroutine_snapshot(&self, limit: u32, options: RenderOptions) -> RpcResult<Json> {
        if self.suspension().is_none() {
            return Ok(json!({
                "available": false,
                "type": "coroutine_snapshot",
                "reason": "session is not suspended",
                "session": self.status().await,
            }));
        }
        let threads = self.coroutine_threads_payload(limit).await?;
        // Continuation-like objects in the stopped thread's frame.
        let continuations = match self.select_frame(None, None).await {
            Ok(selected) => self
                .continuation_candidates(&selected, options, limit as usize)
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let started = Instant::now();
        let discovered = self.discover_coroutines(limit as usize).await;
        let (coroutines, discovery) = match discovered {
            Ok((coroutines, discovery)) => (coroutines, discovery),
            Err(error) => (Vec::new(), json!({"ok": false, "error": error.message})),
        };
        Ok(json!({
            "available": true,
            "type": "coroutine_snapshot",
            "schema_version": 1,
            "source": "jdwp_suspended_frame_and_instances",
            "session": self.status().await,
            "summary": {
                "threads": threads.len(),
                "continuations": continuations.len(),
                "coroutines": coroutines.len(),
            },
            "threads": threads,
            "continuations": continuations,
            "coroutines": coroutines,
            "discovery": discovery,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "source_lines": "need getStackTraceElement() (an invoke); `aar coroutines` gives in-app dumps with lines",
        }))
    }

    async fn continuation_candidates(
        &self,
        selected: &SelectedFrame,
        options: RenderOptions,
        limit: usize,
    ) -> RpcResult<Vec<Json>> {
        let mut out = Vec::new();
        if let Ok(Some(this)) = self.this_value(selected).await
            && let Some(info) = self.continuation_info("this", this, None, options).await
        {
            out.push(info);
        }
        for (local, value) in self.visible_locals(selected).await? {
            if out.len() >= limit {
                break;
            }
            let declared = resolve::type_name(&local.signature);
            if let Some(info) = self
                .continuation_info(
                    resolve::display_local_name(&local.name),
                    value,
                    Some(declared),
                    options,
                )
                .await
            {
                out.push(info);
            }
        }
        Ok(out)
    }

    async fn continuation_info(
        &self,
        name: &str,
        value: Value,
        declared: Option<String>,
        options: RenderOptions,
    ) -> Option<Json> {
        let object = value.object_id()?;
        let (type_id, signature) = self.runtime_type(object).await.ok()?;
        let type_name = resolve::type_name(&signature);
        let mut fields = Vec::new();
        for class in self.hierarchy(type_id).await.ok()? {
            fields.extend(self.fields(class).await.ok()?.iter().cloned());
        }
        let names: HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        let continuation_like = type_name.contains("Continuation")
            || (names.contains("label") && names.contains("completion"))
            || fields.iter().any(|f| is_spilled(&f.name));
        if !continuation_like {
            return None;
        }
        let label = self.read_named(object, "label").await;
        let completion = self
            .field_by_name(object, "completion")
            .await
            .and_then(|v| v.object_id());
        let mut spilled = Vec::new();
        for field in fields
            .iter()
            .filter(|f| is_spilled(&f.name))
            .take(options.max_fields as usize)
        {
            let value = self.field_by_name(object, &field.name).await?;
            let mut visiting = HashSet::new();
            spilled.push(
                self.render(
                    field.name.clone(),
                    value,
                    Some(resolve::type_name(&field.signature)),
                    RenderOptions {
                        depth: options.depth.saturating_sub(1),
                        ..options
                    },
                    &mut visiting,
                )
                .await,
            );
        }
        let completion_handle = match completion {
            Some(id) => self.pin(id).await,
            None => None,
        };
        Some(json!({
            "name": name,
            "class": type_name,
            "declared_type": declared,
            "object_id": object,
            "object_handle": self.pin(object).await,
            "label": label,
            "completion_handle": completion_handle,
            "spilled_locals": spilled,
            "confidence": if type_name.contains("Continuation") { "medium" } else { "low" },
        }))
    }

    async fn read_named(&self, object: u64, field: &str) -> Option<Json> {
        match self.field_by_name(object, field).await? {
            Value::Int(v) => Some(json!(v)),
            Value::Long(v) => Some(json!(v)),
            Value::Object { id, .. } if id != 0 => match self.jdwp.string_value(id).await {
                Ok(text) => Some(json!(text)),
                Err(_) => Some(json!(format!("object {id}"))),
            },
            _ => None,
        }
    }

    /// Classes of coroutines (concrete AbstractCoroutine subclasses) and of
    /// app continuations (BaseContinuationImpl subclasses outside framework
    /// packages), cached for the session.
    async fn coroutine_classes(&self) -> RpcResult<CoroutineClasses> {
        if let Some(cached) = self.coroutine_classes_cache() {
            return Ok(cached);
        }
        let mut found = CoroutineClasses::default();
        for class in self.jdwp.all_classes().await? {
            let signature = &class.signature;
            if !signature.starts_with('L') || signature.contains("$$") {
                continue;
            }
            let name = resolve::type_name(signature);
            let in_coroutines = signature.starts_with("Lkotlinx/coroutines/");
            let app = !is_framework_class(&name) && signature.contains('$');
            if !in_coroutines && !app {
                continue;
            }
            self.cache
                .lock()
                .expect("cache")
                .signatures
                .insert(class.type_id, signature.clone());
            let Ok(chain) = self.hierarchy(class.type_id).await else {
                continue;
            };
            let mut ancestors = Vec::new();
            for ancestor in chain.iter().skip(1) {
                if let Ok(sig) = self.signature(*ancestor).await {
                    ancestors.push(sig);
                }
            }
            if in_coroutines && ancestors.iter().any(|s| s == ABSTRACT_COROUTINE) {
                found.coroutines.push((class.type_id, name));
            } else if app && ancestors.iter().any(|s| s == BASE_CONTINUATION) {
                found.continuations.push((class.type_id, name));
            }
        }
        self.set_coroutine_classes_cache(found.clone());
        Ok(found)
    }

    async fn discover_coroutines(&self, limit: usize) -> RpcResult<(Vec<Json>, Json)> {
        let started = Instant::now();
        let classes = self.coroutine_classes().await?;
        let classes_ms = started.elapsed().as_millis() as u64;
        let mut truncated = false;
        let mut coroutines: BTreeMap<u64, Json> = BTreeMap::new();
        for (class_id, class_name) in &classes.coroutines {
            let instances = self
                .jdwp
                .instances(*class_id, MAX_INSTANCES_PER_CLASS)
                .await?;
            truncated |= instances.len() as i32 >= MAX_INSTANCES_PER_CLASS;
            for object in instances {
                coroutines.insert(object, self.coroutine_info(object, class_name).await);
            }
        }
        // Continuations, grouped under the coroutine their completion chain
        // reaches.
        let mut chains: HashMap<u64, Vec<Json>> = HashMap::new();
        let mut orphans = Vec::new();
        let mut parents = HashSet::new();
        let mut continuations = Vec::new();
        for (class_id, class_name) in &classes.continuations {
            let instances = self
                .jdwp
                .instances(*class_id, MAX_INSTANCES_PER_CLASS)
                .await?;
            truncated |= instances.len() as i32 >= MAX_INSTANCES_PER_CLASS;
            for object in instances {
                let completion = self
                    .field_by_name(object, "completion")
                    .await
                    .and_then(|v| v.object_id());
                if let Some(parent) = completion {
                    parents.insert(parent);
                }
                continuations.push((object, class_name.clone(), completion));
            }
        }
        for (object, class_name, completion) in continuations {
            let info = json!({
                "class": class_name,
                "object_id": object,
                "label": self.read_named(object, "label").await,
                "leaf": !parents.contains(&object),
            });
            match self.owning_coroutine(completion, &coroutines).await {
                Some(owner) => chains.entry(owner).or_default().push(info),
                None => orphans.push(info),
            }
        }
        let mut out = Vec::with_capacity(coroutines.len());
        for (object, mut info) in coroutines {
            info["continuations"] = json!(chains.remove(&object).unwrap_or_default());
            out.push(info);
        }
        // `limit` keeps the coroutines an agent is after: ones running app
        // code first, then named, then live ones; framework coroutines
        // (Compose, lifecycle) fill the rest. Capping in discovery order
        // dropped app coroutines behind dozens of Compose ones.
        out.sort_by_key(|info| {
            let has_app_code = info["continuations"]
                .as_array()
                .is_some_and(|c| !c.is_empty());
            (
                !has_app_code,
                info["name"].is_null(),
                info["state"] != "active",
            )
        });
        if out.len() > limit {
            out.truncate(limit);
            truncated = true;
        }
        Ok((
            out,
            json!({
                "ok": true,
                "coroutine_classes": classes.coroutines.len(),
                "continuation_classes": classes.continuations.len(),
                "classes_ms": classes_ms,
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "max_instances_per_class": MAX_INSTANCES_PER_CLASS,
                "truncated": truncated,
                "unowned_continuations": orphans,
            }),
        ))
    }

    /// Follow `completion` links to a known coroutine, through
    /// `DebugProbesImpl$CoroutineOwner.delegate` when DebugProbes is on.
    async fn owning_coroutine(
        &self,
        mut current: Option<u64>,
        coroutines: &BTreeMap<u64, Json>,
    ) -> Option<u64> {
        for _ in 0..64 {
            let object = current?;
            if coroutines.contains_key(&object) {
                return Some(object);
            }
            let (_, signature) = self.runtime_type(object).await.ok()?;
            current = if signature == COROUTINE_OWNER {
                self.field_by_name(object, "delegate").await?.object_id()
            } else {
                self.field_by_name(object, "completion").await?.object_id()
            };
        }
        None
    }

    async fn coroutine_info(&self, object: u64, class_name: &str) -> Json {
        let mut name = None;
        let mut dispatcher = None;
        if let Some(Value::Object { id, .. }) = self.field_by_name(object, "context").await {
            self.walk_context(id, &mut name, &mut dispatcher, 0).await;
        }
        let (state, incomplete) = match self.state_object(object).await {
            Some(state) => {
                let (type_id, signature) =
                    self.runtime_type(state).await.unwrap_or((0, String::new()));
                (
                    resolve::type_name(&signature),
                    self.is_job_node(type_id).await,
                )
            }
            None => ("unknown".to_string(), false),
        };
        json!({
            "object_id": object,
            "object_handle": self.pin(object).await,
            "class": class_name,
            "name": name,
            "dispatcher": dispatcher,
            "state": if incomplete { "active" } else { coarse_state(&state) },
            "state_class": state,
        })
    }

    /// A job whose state is a `JobNode` (one completion handler such as
    /// `ChildContinuation`, `ChildHandleNode`, `InvokeOnCompletion`) is
    /// still incomplete: `JobNode` implements `Incomplete`. Only the class
    /// name was checked before, so a suspended worker read as "completed".
    async fn is_job_node(&self, type_id: u64) -> bool {
        if type_id == 0 {
            return false;
        }
        let Ok(chain) = self.hierarchy(type_id).await else {
            return false;
        };
        for class in chain {
            if self.signature(class).await.ok().as_deref() == Some(JOB_NODE) {
                return true;
            }
        }
        false
    }

    /// The job state object (`_state`, or `_state$volatile` in newer
    /// kotlinx.coroutines).
    async fn state_object(&self, job: u64) -> Option<u64> {
        for field in ["_state$volatile", "_state"] {
            if let Some(value) = self.field_by_name(job, field).await {
                return value.object_id();
            }
        }
        None
    }

    /// Collect CoroutineName and the dispatcher from a context.
    fn walk_context<'a>(
        &'a self,
        context: u64,
        name: &'a mut Option<String>,
        dispatcher: &'a mut Option<String>,
        depth: u32,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            if depth > 16 {
                return;
            }
            let Ok((_, signature)) = self.runtime_type(context).await else {
                return;
            };
            let type_name = resolve::type_name(&signature);
            if signature == "Lkotlin/coroutines/CombinedContext;" {
                for part in ["left", "element"] {
                    if let Some(Value::Object { id, .. }) = self.field_by_name(context, part).await
                        && id != 0
                    {
                        self.walk_context(id, name, dispatcher, depth + 1).await;
                    }
                }
            } else if signature == "Lkotlinx/coroutines/CoroutineName;" {
                if let Some(Value::Object { id, .. }) = self.field_by_name(context, "name").await {
                    *name = self.jdwp.string_value(id).await.ok();
                }
            } else if type_name.contains("Dispatcher")
                || type_name.contains("Scheduler")
                || type_name.contains("HandlerContext")
            {
                *dispatcher = Some(dispatcher_label(&type_name));
            }
        })
    }
}

fn dispatcher_label(type_name: &str) -> String {
    if type_name.contains("DefaultScheduler") {
        "Dispatchers.Default".into()
    } else if type_name.contains("DefaultIoScheduler") {
        "Dispatchers.IO".into()
    } else if type_name.contains("HandlerContext") {
        "Dispatchers.Main".into()
    } else if type_name.contains("Unconfined") {
        "Dispatchers.Unconfined".into()
    } else {
        type_name.to_string()
    }
}

fn coarse_state(state_class: &str) -> &'static str {
    if state_class.contains("CompletedExceptionally") || state_class.contains("Cancelled") {
        "cancelled_or_failed"
    } else if state_class.contains("Finishing") {
        "completing"
    } else if state_class.contains("Empty")
        || state_class.contains("NodeList")
        || state_class.contains("Node")
    {
        "active"
    } else if state_class == "unknown" {
        "unknown"
    } else {
        "completed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_match_the_studio_bridge() {
        assert!(is_spilled("L$0"));
        assert!(is_spilled("I$12"));
        assert!(!is_spilled("label"));
        assert!(!is_spilled("L$x"));
        assert_eq!(
            flow_kind("kotlinx.coroutines.flow.StateFlowImpl"),
            Some("StateFlow")
        );
        assert_eq!(flow_kind("x.MyFlow"), Some("Flow"));
        assert_eq!(flow_kind("java.lang.String"), None);
        assert_eq!(dispatcher_hint("main")["name"], "Dispatchers.Main");
        assert_eq!(
            dispatcher_hint("DefaultDispatcher-worker-1")["name"],
            "Dispatchers.Default"
        );
        assert_eq!(coarse_state("kotlinx.coroutines.Empty"), "active");
        assert_eq!(
            coarse_state("kotlinx.coroutines.CompletedExceptionally"),
            "cancelled_or_failed"
        );
        assert_eq!(
            dispatcher_label("kotlinx.coroutines.scheduling.DefaultScheduler"),
            "Dispatchers.Default"
        );
    }
}
