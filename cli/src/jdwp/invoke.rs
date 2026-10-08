//! Running app methods for `--invoke` without wedging the debugger.
//!
//! Device-measured rules this follows:
//! * an invoke needs a thread suspended by an event; after `debug pause`
//!   (VirtualMachine.Suspend only) ART answers INVALID_THREAD, so a paused
//!   session is refused up front with `invoke_requires_event_stop`;
//! * if the invoked code reaches one of our breakpoints, the reply waits
//!   until that thread is resumed — so every daemon-owned breakpoint request
//!   is cleared for the invoke and re-armed after it, and any event that
//!   still arrives on the invoking thread is resumed by the event forwarder
//!   (`Session::run_events`) without recording a stop;
//! * a call that outlives the caller's deadline keeps running; the VM stays
//!   healthy and the thread returns to its stop when the late reply comes.
//!   Until then the thread is "busy": frame reads on it are refused, and the
//!   late reply is consumed by a watcher task.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::codec::Value;
use super::conn::JdwpError;
use super::events::Event;
use super::session::{RpcError, RpcResult, Session};
use super::vm::{InvokeResult, Jdwp};

/// Deadline the connection gives an invoke that already missed the
/// caller's deadline: long enough for any sane late reply.
const LATE_REPLY_WINDOW: Duration = Duration::from_secs(600);

/// Shared with watcher tasks, so it lives outside the session state.
#[derive(Default)]
pub struct InvokeState {
    /// The thread an invoke is running on.
    pub invoking: Option<u64>,
    /// A thread whose invoke missed its deadline and still runs.
    pub busy: Option<u64>,
    /// ClassPrepare events absorbed during an invoke, bound afterwards.
    pub deferred: Vec<Event>,
    pub skipped: u64,
    pub invokes: u64,
    pub timeouts: u64,
}

pub type SharedInvokeState = Arc<Mutex<InvokeState>>;

/// What to run.
pub enum InvokeCall {
    Instance {
        object: u64,
        class_id: u64,
        method_id: u64,
        args: Vec<Value>,
    },
    Static {
        class_id: u64,
        method_id: u64,
        args: Vec<Value>,
    },
}

async fn run_call(
    jdwp: &Jdwp,
    thread: u64,
    call: &InvokeCall,
    timeout: Duration,
) -> Result<InvokeResult, JdwpError> {
    match call {
        InvokeCall::Instance {
            object,
            class_id,
            method_id,
            args,
        } => {
            jdwp.invoke_instance(*object, thread, *class_id, *method_id, args, timeout)
                .await
        }
        InvokeCall::Static {
            class_id,
            method_id,
            args,
        } => {
            jdwp.invoke_static(*class_id, thread, *method_id, args, timeout)
                .await
        }
    }
}

impl Session {
    fn invoke_state(&self) -> std::sync::MutexGuard<'_, InvokeState> {
        self.invoke_state.lock().expect("invoke state lock")
    }

    pub(super) fn invoking_thread(&self) -> Option<u64> {
        self.invoke_state().invoking
    }

    /// Refuse frame reads on a thread whose invoke is still running.
    pub(super) fn ensure_thread_not_busy(&self, thread: u64) -> RpcResult<()> {
        if self.invoke_state().busy == Some(thread) {
            return Err(RpcError::new(
                "thread_busy_invoking",
                "this thread is still running a method an earlier --invoke started (it missed its deadline); it returns to its stop when the method finishes",
            )
            .retryable()
            .next(&["retry in a moment", "shadowdroid debug status --backend jdwp"]));
        }
        Ok(())
    }

    pub fn invoke_stats(&self) -> serde_json::Value {
        let state = self.invoke_state();
        json!({
            "invokes": state.invokes,
            "timeouts": state.timeouts,
            "events_skipped_during_invoke": state.skipped,
            "busy_thread": state.busy,
        })
    }

    /// Run one invoke on `thread`, bounded by `timeout`.
    /// Why an invoke on `thread` cannot run now, if it cannot.
    pub(super) fn invoke_preflight(&self, thread: u64) -> RpcResult<()> {
        if self.suspension().is_some_and(|s| s.reason == "pause") {
            return Err(RpcError::new(
                "invoke_requires_event_stop",
                "method calls need a thread stopped by a breakpoint or step; `debug pause` suspends threads in a way ART refuses to invoke on (INVALID_THREAD)",
            )
            .next(&[
                "shadowdroid debug step-over --backend jdwp",
                "shadowdroid debug break line --backend jdwp --file <File.kt> --line <n>",
            ]));
        }
        if thread == 0 {
            return Err(RpcError::new(
                "invalid_expression",
                "an invoke needs a suspended thread; select a frame",
            ));
        }
        self.ensure_thread_not_busy(thread)
    }

    /// Several calls on `thread` under one disarm/re-arm of our requests,
    /// each bounded by `per_call` and all by `budget`. A call that misses
    /// its deadline ends the batch and leaves the thread busy, as in
    /// [`Session::invoke_on`]. Returns one result per call made and why the
    /// batch stopped early (`budget`, `timeout`), if it did.
    pub(super) async fn invoke_many(
        &self,
        thread: u64,
        calls: Vec<InvokeCall>,
        per_call: Duration,
        budget: Duration,
    ) -> RpcResult<(Vec<Result<InvokeResult, String>>, Option<&'static str>)> {
        self.invoke_preflight(thread)?;
        let started = std::time::Instant::now();
        let disarmed = self.begin_invoke(thread).await;
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let Some(remaining) = budget.checked_sub(started.elapsed()) else {
                self.end_invoke(disarmed, true).await;
                return Ok((results, Some("budget")));
            };
            let deadline = per_call.min(remaining);
            let jdwp = self.jdwp.clone();
            let mut task = tokio::spawn(async move {
                run_call(&jdwp, thread, &call, deadline + LATE_REPLY_WINDOW).await
            });
            match tokio::time::timeout(deadline, &mut task).await {
                Ok(joined) => results.push(match joined {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(format!("invoke task failed: {error}")),
                }),
                Err(_) => {
                    {
                        let mut state = self.invoke_state();
                        state.busy = Some(thread);
                        state.timeouts += 1;
                    }
                    self.end_invoke(disarmed, false).await;
                    let shared = self.invoke_state.clone();
                    tokio::spawn(async move {
                        let _ = task.await;
                        let mut state = shared.lock().expect("invoke state lock");
                        state.busy = None;
                        state.invoking = None;
                    });
                    results.push(Err(format!(
                        "invoke_timeout: no return within {} ms",
                        deadline.as_millis()
                    )));
                    return Ok((results, Some("timeout")));
                }
            }
        }
        self.end_invoke(disarmed, true).await;
        Ok((results, None))
    }

    pub(super) async fn invoke_on(
        &self,
        thread: u64,
        call: InvokeCall,
        timeout: Duration,
    ) -> RpcResult<InvokeResult> {
        self.invoke_preflight(thread)?;
        let disarmed = self.begin_invoke(thread).await;

        let jdwp = self.jdwp.clone();
        let mut task = tokio::spawn(async move {
            run_call(&jdwp, thread, &call, timeout + LATE_REPLY_WINDOW).await
        });
        let outcome = tokio::time::timeout(timeout, &mut task).await;
        match outcome {
            Ok(joined) => {
                self.end_invoke(disarmed, true).await;
                let result = joined.map_err(|error| {
                    RpcError::new("jdwp_error", format!("invoke task failed: {error}"))
                })?;
                result.map_err(|error| error.into())
            }
            Err(_) => {
                {
                    let mut state = self.invoke_state();
                    state.busy = Some(thread);
                    state.timeouts += 1;
                }
                // Re-arm now; `invoking` stays set so a breakpoint the late
                // call reaches is still resumed, not recorded.
                self.end_invoke(disarmed, false).await;
                let shared = self.invoke_state.clone();
                tokio::spawn(async move {
                    let _ = task.await;
                    let mut state = shared.lock().expect("invoke state lock");
                    state.busy = None;
                    state.invoking = None;
                });
                Err(RpcError::new(
                    "invoke_timeout",
                    format!(
                        "the invoked method did not return within {} ms; it keeps running in the app and the thread returns to its stop when it does",
                        timeout.as_millis()
                    ),
                )
                .retryable()
                .detail(json!({"timeout_ms": timeout.as_millis() as u64, "thread": thread}))
                .next(&["raise --timeout-ms", "shadowdroid debug status --backend jdwp"]))
            }
        }
    }

    /// Mark the invoke and clear every armed breakpoint request so the
    /// invoked code cannot stop on one. Returns the ids to re-arm.
    async fn begin_invoke(&self, thread: u64) -> Vec<String> {
        {
            let mut state = self.invoke_state();
            state.invoking = Some(thread);
            state.invokes += 1;
        }
        let armed: Vec<String> = self
            .state()
            .breakpoints
            .values()
            .filter(|b| b.locations.iter().any(|l| l.request_id.is_some()))
            .map(|b| b.id.clone())
            .collect();
        for id in &armed {
            self.disarm(id).await;
        }
        armed
    }

    /// Re-arm what [`Session::begin_invoke`] cleared and bind classes that
    /// loaded meanwhile. `finished == false` keeps the thread marked.
    async fn end_invoke(&self, disarmed: Vec<String>, finished: bool) {
        for id in disarmed {
            if let Err(error) = self.rearm(&id).await {
                tracing::warn!("re-arming {id} after an invoke: {}", error.message);
            }
        }
        if finished {
            self.invoke_state().invoking = None;
        }
        self.drain_deferred().await;
    }

    /// Bind classes absorbed during invokes that have since finished.
    pub(super) async fn drain_deferred(&self) {
        let deferred = {
            let mut state = self.invoke_state();
            if state.invoking.is_some() {
                return;
            }
            std::mem::take(&mut state.deferred)
        };
        for event in deferred {
            if let Err(error) = Box::pin(self.handle_event(&event)).await {
                tracing::warn!("binding a class loaded during an invoke: {}", error.message);
            }
        }
    }

    /// Record an event the invoked code raised (and that the forwarder
    /// resumes instead of reporting).
    pub(super) fn note_during_invoke(&self, event: &Event) {
        let mut state = self.invoke_state();
        if matches!(event, Event::ClassPrepare { .. }) {
            state.deferred.push(event.clone());
        } else {
            state.skipped += 1;
        }
    }
}
