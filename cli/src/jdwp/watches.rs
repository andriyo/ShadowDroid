//! `debug watch add|list|remove|clear --backend jdwp`, in the Studio
//! bridge's shape: watches are path expressions evaluated in the stopped
//! frame on every stop (cached with the time and frame) and again on
//! `list`; a running session returns the cached values with a warning.
//! A watch added with `--invoke` may call methods: it runs only on a
//! breakpoint or step stop (with our requests disarmed, as every invoke) and
//! reports `needs_event_stop` on a `debug pause` stop.

use std::collections::HashSet;

use base64::Engine;
use serde_json::{Value as Json, json};

use super::eval::EvalCtx;
use super::expr;
use super::inspect::RenderOptions;
use super::session::{RpcError, RpcResult, Session};

#[derive(Clone, Debug)]
pub struct WatchSpec {
    pub id: String,
    pub name: String,
    pub expression: String,
    /// Method calls allowed (`watch add --invoke`).
    pub invoke: bool,
}

/// The Studio bridge's watch id: `watch_` + base64url(`project|name|expr`).
pub fn watch_id(name: &str, expression: &str) -> String {
    let raw = format!("|{name}|{expression}");
    format!(
        "watch_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
    )
}

impl Session {
    pub fn watch_add(&self, expression: &str, name: Option<&str>, invoke: bool) -> RpcResult<Json> {
        let parsed =
            expr::parse(expression).map_err(|error| RpcError::new("invalid_expression", error))?;
        if parsed.has_calls() && !invoke {
            return Err(RpcError::new(
                "invoke_not_allowed",
                "watches are evaluated on every stop and run no app code unless added with --invoke",
            )
            .next(&["shadowdroid debug watch add '<expr>' --invoke --backend jdwp"]));
        }
        let name = name.filter(|n| !n.trim().is_empty()).unwrap_or(expression);
        let watch = WatchSpec {
            id: watch_id(name, expression),
            name: name.to_string(),
            expression: expression.to_string(),
            invoke: invoke && parsed.has_calls(),
        };
        let info = self.watch_info(&watch, None);
        let mut watches = self.watches.lock().expect("watch lock");
        watches.0.retain(|w| w.id != watch.id);
        watches.0.push(watch);
        Ok(json!({"watch": info}))
    }

    pub fn watch_remove(&self, id: &str) -> Json {
        let mut watches = self.watches.lock().expect("watch lock");
        let before = watches.0.len();
        watches.0.retain(|w| w.id != id);
        watches.1.remove(id);
        json!({"id": id, "removed": watches.0.len() != before})
    }

    pub fn watch_clear(&self) -> Json {
        let mut watches = self.watches.lock().expect("watch lock");
        let removed = watches.0.len();
        watches.0.clear();
        watches.1.clear();
        json!({"removed": removed})
    }

    pub async fn watch_list(&self, options: RenderOptions) -> RpcResult<Json> {
        let suspended = self.suspension().is_some();
        if suspended {
            self.refresh_watches(options).await;
        }
        let specs = self.watches.lock().expect("watch lock").0.clone();
        let payload: Vec<Json> = specs.iter().map(|w| self.watch_info(w, None)).collect();
        Ok(json!({
            "session": self.status().await,
            "warning": (!suspended).then_some("session is not suspended; returning cached watch values"),
            "watches": payload,
        }))
    }

    fn watch_info(&self, watch: &WatchSpec, value: Option<Json>) -> Json {
        let cached = self
            .watches
            .lock()
            .expect("watch lock")
            .1
            .get(&watch.id)
            .cloned();
        let cached_value = cached.as_ref().and_then(|c| c.get("value").cloned());
        json!({
            "id": watch.id,
            "project": null,
            "name": watch.name,
            "expression": watch.expression,
            "invoke": watch.invoke,
            "enabled": true,
            "value": value.or(cached_value),
            "updated_at": cached.as_ref().and_then(|c| c.get("updated_at").cloned()),
            "session": cached.as_ref().and_then(|c| c.get("session").cloned()),
            "selected_frame": cached.as_ref().and_then(|c| c.get("selected_frame").cloned()),
            "error": cached.as_ref().and_then(|c| c.get("error").cloned()),
        })
    }

    /// Evaluate every watch in the stopped frame and cache the results.
    pub(super) async fn refresh_watches(&self, options: RenderOptions) {
        let specs = self.watches.lock().expect("watch lock").0.clone();
        if specs.is_empty() {
            return;
        }
        let Ok(mut selected) = self.select_frame(None, None).await else {
            return;
        };
        let suspension = self.suspension();
        let event_stop = suspension.as_ref().is_some_and(|s| s.reason != "pause");
        let exception = suspension.and_then(|s| s.exception);
        let session = self.status().await;
        let thread_name = self.thread_name(selected.thread).await;
        let frame = json!({
            "thread": selected.thread_index,
            "thread_name": thread_name,
            "frame": selected.frame_index,
        });
        for watch in specs {
            if watch.invoke && !event_stop {
                let message = "needs_event_stop: this watch calls methods, which need a breakpoint or step stop, not `debug pause`";
                self.watches.lock().expect("watch lock").1.insert(
                    watch.id.clone(),
                    json!({
                        "value": {"ok": false, "code": "needs_event_stop", "error": message},
                        "updated_at": crate::events::now_ts(),
                        "session": session,
                        "selected_frame": frame,
                        "error": message,
                    }),
                );
                continue;
            }
            let ctx = EvalCtx::new(
                Some(selected),
                exception,
                watch.invoke.then_some(super::eval::DEFAULT_INVOKE_TIMEOUT),
            );
            let outcome = match expr::parse(&watch.expression) {
                Ok(expr::Expr::Path(path)) => match self.eval_path(&ctx, &path).await {
                    Ok((value, declared)) => {
                        let mut visiting = HashSet::new();
                        Ok(self
                            .render(
                                watch.expression.clone(),
                                value,
                                declared,
                                options,
                                &mut visiting,
                            )
                            .await)
                    }
                    Err(error) => Err(error.message),
                },
                Ok(other) => self
                    .eval_expr(&ctx, &other)
                    .await
                    .map(|value| json!({"name": watch.expression, "value": value.display()})),
                Err(error) => Err(error),
            };
            let entry = match outcome {
                Ok(value) => json!({
                    "value": value,
                    "updated_at": crate::events::now_ts(),
                    "session": session,
                    "selected_frame": frame,
                    "error": null,
                }),
                Err(message) => json!({
                    "value": {"ok": false, "error": message},
                    "updated_at": crate::events::now_ts(),
                    "session": session,
                    "selected_frame": frame,
                    "error": message,
                }),
            };
            // An invoke invalidates frame ids: re-read before the next watch.
            if ctx.is_stale()
                && let Ok(fresh) = self.select_frame(None, None).await
            {
                selected = fresh;
            }
            self.watches
                .lock()
                .expect("watch lock")
                .1
                .insert(watch.id.clone(), entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_ids_match_the_studio_bridge() {
        // Kotlin: "watch_" + Base64.getUrlEncoder().withoutPadding()("|name|expr").
        assert_eq!(watch_id("tag", "tag"), "watch_fHRhZ3x0YWc");
    }
}
