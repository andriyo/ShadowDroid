//! Session-level tests against the fake VM in `tests/support/fake_jdwp.rs`:
//! breakpoint resolution and deferred binding, event delivery, stepping,
//! reads and handles, timeouts, and EOF mid-session.

use std::sync::Arc;
use std::time::Duration;

use super::fake_jdwp::{ACTIVITY_OBJECT, FakeVm};
use super::inspect::RenderOptions;
use super::resolve::SourceTarget;
use super::session::{Session, SessionInfo};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(5);

async fn attach(vm: &FakeVm, timeout: Duration) -> Arc<Session> {
    let stream = super::transport::connect_tcp(&vm.address(), WAIT)
        .await
        .unwrap();
    let (conn, incoming) = super::conn::Connection::start(stream, WAIT).await.unwrap();
    let jdwp = super::vm::Jdwp::new(conn, timeout);
    jdwp.id_sizes(WAIT).await.unwrap();
    let session = Session::new(
        jdwp,
        SessionInfo {
            session_id: "jdwp:fake:1".into(),
            serial: "fake".into(),
            pid: 1,
            package: Some("io.example.app".into()),
            attached_at: 0.0,
            vm: json!({}),
            capabilities: json!({}),
            launched_under_debugger: false,
        },
    );
    tokio::spawn(session.clone().run_events(incoming));
    session
}

fn target(basename: &str) -> SourceTarget {
    SourceTarget {
        basename: basename.into(),
        package: Some("io.example.app".into()),
        path: None,
        package_known: true,
        inline_body: false,
    }
}

async fn wait_suspended(session: &Session) {
    let mut changes = session.subscribe();
    let deadline = tokio::time::Instant::now() + WAIT;
    while session.suspension().is_none() {
        tokio::time::timeout_at(deadline, changes.changed())
            .await
            .expect("suspension within the deadline")
            .unwrap();
    }
}

const OPTIONS: RenderOptions = RenderOptions::new(1, 64, 32);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn line_breakpoint_binds_hits_and_reads_the_frame() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;

    let breakpoint = session
        .break_line(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap();
    assert_eq!(breakpoint["bound"], true, "{breakpoint}");
    let locations = breakpoint["locations"].as_array().unwrap();
    assert_eq!(locations.len(), 1, "{breakpoint}");
    assert_eq!(locations[0]["method"], "onNewIntent");
    assert_eq!(locations[0]["code_index"], 5);
    // Deferred binding: package ClassMatch + SourceNameMatch on ClassPrepare.
    let prepare = vm.with_state(|s| s.requests.iter().find(|r| r.kind == 8).cloned().unwrap());
    assert_eq!(prepare.class_match.as_deref(), Some("io.example.app.*"));
    assert_eq!(prepare.source_name.as_deref(), Some("MainActivity.kt"));
    assert_eq!(prepare.policy, 1, "EVENT_THREAD");

    assert_eq!(vm.hit_breakpoint(5), 1);
    wait_suspended(&session).await;
    let status = session.status().await;
    assert_eq!(status["suspended"], true);
    assert_eq!(status["suspend_reason"], "breakpoint");
    assert_eq!(status["position"]["line"], 31);
    assert_eq!(status["position"]["method"], "onNewIntent");
    assert_eq!(status["thread"], "main");
    assert_eq!(session.breakpoints()[0]["hit_count"], 1);

    let stack = session.stack(None, 64).await.unwrap();
    let frames = stack["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["class"], "io.example.app.MainActivity");
    assert_eq!(frames[0]["source"], "MainActivity.kt");
    assert_eq!(frames[1]["method"], "onCreate");
    assert_eq!(frames[1]["line"], 21);

    let variables = session.variables(None, None, OPTIONS).await.unwrap();
    let names: Vec<_> = variables["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["tag", "count", "it", "it"],
        "markers and `this` are hidden; inlined `\\N` suffixes stripped"
    );
    assert_eq!(variables["variables"][2]["slot_name"], "it\\1");
    let it = session.eval("it", None, None, OPTIONS, None).await.unwrap();
    assert_eq!(it["result"]["value"], "42", "the innermost `it` wins");
    assert_eq!(variables["variables"][0]["value"], "hello");
    assert_eq!(variables["variables"][1]["value"], "42");
    let this = &variables["this"];
    assert_eq!(this["type"], "io.example.app.MainActivity");
    assert_eq!(this["object_handle"], format!("obj_{ACTIVITY_OBJECT}"));
    let field_names: Vec<_> = this["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        field_names,
        ["counter", "label", "numbers", "status$delegate"],
        "shadow$ hidden"
    );
    assert!(
        vm.with_state(|s| s.pinned.contains(&ACTIVITY_OBJECT)),
        "handles are pinned with DisableCollection at creation"
    );
    assert!(session.is_pinned(ACTIVITY_OBJECT));

    let eval = session
        .eval("this.label", None, None, OPTIONS, None)
        .await
        .unwrap();
    assert_eq!(eval["result"]["value"], "hello");
    assert_eq!(eval["result"]["declared_type"], "java.lang.String");
    let eval = session
        .eval("this.numbers[1]", None, None, OPTIONS, None)
        .await
        .unwrap();
    assert_eq!(eval["result"]["value"], "2");
    let array = session
        .eval("this.numbers", None, None, OPTIONS, None)
        .await
        .unwrap();
    assert_eq!(array["result"]["length"], 3);
    assert_eq!(array["result"]["items"][2]["value"], "3");
    let error = session
        .eval("this.numbers[9]", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_expression");
    let error = session
        .eval("nope", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert!(error.message.contains("unknown local"), "{error:?}");

    let inspect = session
        .inspect(
            None,
            Some("obj_500"),
            Some(".counter"),
            None,
            None,
            OPTIONS,
            None,
        )
        .await
        .unwrap();
    assert_eq!(inspect["result"]["value"], "7");
    assert_eq!(inspect["mode"], "object_handle");
    let stale = session
        .inspect(None, Some("obj_777"), None, None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(stale.code, "stale_object_handle");

    let threads = session.threads(4).await.unwrap();
    assert_eq!(threads["threads"][0]["name"], "main");
    assert_eq!(threads["threads"][0]["current"], true);
    assert_eq!(threads["threads"][1]["name"], "worker");

    // Step over: Count must be the last modifier; the step stops on line 32.
    let stepped = session.step("over", None, WAIT).await.unwrap();
    assert_eq!(stepped["suspend_reason"], "step");
    assert_eq!(stepped["position"]["line"], 32);
    let step = vm.with_state(|s| {
        s.requests
            .iter()
            .rev()
            .find(|r| r.kind == 1)
            .cloned()
            .unwrap()
    });
    assert_eq!(step.modifier_kinds, [10, 1]);
    assert_eq!(step.step_thread, Some(super::fake_jdwp::MAIN_THREAD));
    assert!(
        vm.with_state(|s| s.cleared.contains(&step.id)),
        "step cleared"
    );
    assert!(
        vm.with_state(|s| s.pinned.is_empty()),
        "resuming for the step released handles"
    );

    let stepped = session.step("into", None, WAIT).await.unwrap();
    assert_eq!(stepped["suspended"], true);
    let step = vm.with_state(|s| {
        s.requests
            .iter()
            .rev()
            .find(|r| r.kind == 1)
            .cloned()
            .unwrap()
    });
    assert_eq!(step.modifier_kinds.first(), Some(&10));
    assert_eq!(step.modifier_kinds.last(), Some(&1), "Count last");
    assert!(step.modifier_kinds.contains(&6), "step-into filters");

    let resumed = session.resume().await.unwrap();
    assert_eq!(resumed["suspended"], false);
    let error = session
        .eval("tag", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "debugger_not_suspended");
    let stack = session.stack(None, 8).await.unwrap();
    assert_eq!(stack["warning"], "session is not suspended");

    let removed = session.remove_breakpoint("bp_1").await.unwrap();
    assert_eq!(removed["removed"], true);
    assert_eq!(session.owner_count(), 0, "every request cleared");
    assert!(session.is_quiescent());

    session.dispose().await.unwrap();
    assert!(vm.with_state(|s| s.disposed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_binding_handles_every_event_in_one_composite() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let line = session
        .break_line(target("Late.kt"), 7, Default::default())
        .await
        .unwrap();
    assert_eq!(line["bound"], false);
    assert_eq!(line["pending_reason"], "class_not_loaded");
    let exception = session
        .break_exception("io.example.app.Late", true, true, Default::default())
        .await
        .unwrap();
    assert_eq!(exception["pending_reason"], "class_not_loaded");

    // One class load, two matching ClassPrepare requests: one composite.
    assert_eq!(vm.load_late_class(), 2);
    vm.wait_for(WAIT, "the prepare thread to resume", |s| {
        s.thread_resumes == 1
    });
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let breakpoints = session.breakpoints();
        if breakpoints[0]["bound"] == true && breakpoints[1]["bound"] == true {
            assert_eq!(breakpoints[0]["locations"][0]["method"], "run");
            assert_eq!(breakpoints[1]["type"], "exception");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{breakpoints}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        vm.with_state(|s| s.thread_resumes),
        1,
        "EVENT_THREAD composite resumed exactly once"
    );
    assert!(session.suspension().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unowned_events_resume_and_lines_without_code_stay_pending() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    // Line 99 is in a loaded file but has no code: pending, still armed.
    let breakpoint = session
        .break_line(target("MainActivity.kt"), 99, Default::default())
        .await
        .unwrap();
    assert_eq!(breakpoint["pending_reason"], "line_not_in_loaded_classes");
    // A breakpoint event nobody owns (e.g. cleared in flight) is resumed.
    vm.with_state(|s| {
        s.requests.push(super::fake_jdwp::Request {
            id: 4242,
            kind: 2,
            policy: 2,
            location: Some((100, 1001, 5)),
            source_name: None,
            class_match: None,
            step_thread: None,
            exception_flags: None,
            field: None,
            modifier_kinds: vec![7],
        })
    });
    assert_eq!(vm.hit_breakpoint(5), 1);
    vm.wait_for(WAIT, "a VM resume", |s| s.resumes == 1);
    assert!(session.suspension().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_bodies_are_reported_as_unsupported_locations() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let mut inline = target("MainActivity.kt");
    inline.inline_body = true;
    let error = session
        .break_line(inline, 31, Default::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, "unsupported_location");
    assert_eq!(error.detail["reason"], "inline_body");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_commands_time_out_with_the_command_named() {
    let vm = FakeVm::start();
    let session = attach(&vm, Duration::from_millis(200)).await;
    session.pause().await.unwrap();
    vm.with_state(|s| s.silent.push((11, 7)));
    let error = session.stack(None, 8).await.unwrap_err();
    assert_eq!(error.code, "debugger_timeout");
    assert_eq!(error.detail["command"], "ThreadReference.FrameCount");
    assert!(error.retryable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eof_mid_session_marks_the_debuggee_gone() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session.pause().await.unwrap();
    vm.kill_connection();
    let mut changes = session.subscribe();
    let deadline = tokio::time::Instant::now() + WAIT;
    while session.closed_reason().is_none() {
        tokio::time::timeout_at(deadline, changes.changed())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(session.suspension().is_none(), "no stale suspension");
    let error = session.resume().await.unwrap_err();
    assert_eq!(error.code, "debuggee_exited");
    // Dispose after EOF is a no-op, not an error.
    session.dispose().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_reads_the_main_thread_and_resume_balances_it() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let paused = session.pause().await.unwrap();
    assert_eq!(paused["suspend_reason"], "pause");
    assert_eq!(vm.with_state(|s| s.suspend_count), 1);
    let stack = session.stack(None, 8).await.unwrap();
    assert_eq!(stack["frames"][0]["thread"], "main");
    let again = session.pause().await.unwrap();
    assert_eq!(again["suspended"], true);
    assert_eq!(vm.with_state(|s| s.suspend_count), 1, "pause is idempotent");
    session.resume().await.unwrap();
    assert_eq!(vm.with_state(|s| s.suspend_count), 0);
    let warning = session.resume().await.unwrap();
    assert_eq!(warning["warning"], "session was not suspended");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_class_prepared_during_the_scan_is_bound_once() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    vm.with_state(|s| s.prepare_late_during_scan = true);
    // The ClassPrepare for Late arrives before the AllClasses reply that
    // already lists it: both binding paths see the same class.
    let line = session
        .break_line(target("Late.kt"), 7, Default::default())
        .await
        .unwrap();
    vm.wait_for(WAIT, "the prepare thread to resume", |s| {
        s.thread_resumes == 1
    });
    // Let the event-loop bind (if any) settle.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let at_late_run = vm.with_state(|s| {
        s.requests
            .iter()
            .filter(|r| r.kind == 2 && r.location.is_some_and(|(_, m, i)| m == 1050 && i == 0))
            .count()
    });
    assert_eq!(
        at_late_run, 1,
        "one Breakpoint request per location: {line}"
    );
    let breakpoints = session.breakpoints();
    assert_eq!(
        breakpoints[0]["locations"].as_array().unwrap().len(),
        1,
        "{breakpoints}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncaught_means_not_caught_by_app_code() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    // java.lang.String (id 103) stands in for any loaded throwable class.
    let breakpoint = session
        .break_exception("java.lang.String", false, true, Default::default())
        .await
        .unwrap();
    assert_eq!(breakpoint["bound"], true, "{breakpoint}");
    // ART reports a catch location for Android crashes (Looper rethrows):
    // the request must ask for caught events too.
    let flags = vm.with_state(|s| {
        s.requests
            .iter()
            .find(|r| r.kind == 4)
            .and_then(|r| r.exception_flags)
    });
    assert_eq!(flags, Some((true, true)));

    // Caught by app code (MainActivity, id 100): not a stop, VM resumed.
    assert_eq!(vm.throw_exception(Some(100)), 1);
    vm.wait_for(WAIT, "the filtered exception to resume", |s| s.resumes == 1);
    assert!(session.suspension().is_none());

    // Caught by framework code (java.lang.Object, id 104): a crash path.
    assert_eq!(vm.throw_exception(Some(104)), 1);
    wait_suspended(&session).await;
    let status = session.status().await;
    assert_eq!(status["suspend_reason"], "exception", "{status}");
    assert_eq!(session.breakpoints()[0]["hit_count"], 1);
    // The thrown object is pinned and reachable as `$exception`.
    assert_eq!(
        status["exception_handle"],
        format!("obj_{ACTIVITY_OBJECT}"),
        "{status}"
    );
    assert_eq!(status["exception_expression"], "$exception", "{status}");
    let thrown = session
        .eval("$exception", None, None, OPTIONS, None)
        .await
        .unwrap();
    assert_eq!(
        thrown["result"]["object_handle"],
        format!("obj_{ACTIVITY_OBJECT}"),
        "{thrown}"
    );
    session.resume().await.unwrap();

    // The framework rethrows the same object: reported once, then resumed.
    let resumes = vm.with_state(|s| s.resumes);
    assert_eq!(vm.throw_exception(None), 1);
    vm.wait_for(WAIT, "the rethrow to resume", |s| s.resumes == resumes + 1);
    assert!(session.suspension().is_none());
    assert_eq!(session.breakpoints()[0]["hit_count"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_kotlin_property_points_at_its_delegate() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session
        .break_line(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap();
    vm.hit_breakpoint(5);
    wait_suspended(&session).await;
    let error = session
        .eval("this.status", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_expression");
    assert!(
        error.message.contains("status$delegate"),
        "{}",
        error.message
    );
    assert_eq!(error.detail["suggestion"], "this.status$delegate");
    let plain = session
        .eval("this.nothing", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(plain.message, "field not found: nothing");
}

// ── P1b: conditions, logpoints, lifecycle ───────────────────────────────

use super::breakpoints::{BreakpointOptions, BreakpointUpdate, SuspendKind};
use super::logpoints::Filter;

fn logpoint_options(expression: Option<&str>) -> BreakpointOptions {
    BreakpointOptions {
        suspend: SuspendKind::None,
        log_expression: expression.map(str::to_string),
        log_message: expression.is_none(),
        owner: Some("agent".into()),
        ..Default::default()
    }
}

async fn events(session: &Session, filter: &Filter) -> serde_json::Value {
    session
        .logpoint_events(Some(0), 50, filter, Duration::from_secs(5))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditions_evaluate_in_the_daemon_and_resume_when_false() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let created = session
        .break_line_with(
            target("MainActivity.kt"),
            31,
            BreakpointOptions {
                condition: Some("count > 100".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(created["created"], true);
    let request = vm.with_state(|s| {
        s.requests
            .iter()
            .rev()
            .find(|r| r.kind == 2)
            .cloned()
            .unwrap()
    });
    assert_eq!(
        request.policy, 1,
        "conditions suspend only the event thread"
    );

    // False: the thread is resumed, no stop.
    assert_eq!(vm.hit_breakpoint(5), 1);
    vm.wait_for(WAIT, "the false condition to resume", |s| {
        s.thread_resumes == 1
    });
    assert!(session.suspension().is_none());
    assert_eq!(session.breakpoints()[0]["hit_count"], 0);

    // True after an update: a stop, widened to the whole VM.
    let updated = session
        .update_breakpoint(
            "bp_1",
            BreakpointUpdate {
                condition: Some("count == 42 && tag == \"hello\"".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated["condition"], "count == 42 && tag == \"hello\"");
    assert_eq!(vm.hit_breakpoint(5), 1);
    wait_suspended(&session).await;
    assert_eq!(session.status().await["suspend_reason"], "breakpoint");
    assert_eq!(session.breakpoints()[0]["hit_count"], 1);
    session.resume().await.unwrap();

    // A condition that cannot evaluate leaves the thread suspended and
    // records the error (design §5.3).
    session
        .update_breakpoint(
            "bp_1",
            BreakpointUpdate {
                condition: Some("missing.field > 1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(vm.hit_breakpoint(5), 1);
    wait_suspended(&session).await;
    assert_eq!(session.status().await["suspend_reason"], "condition_error");
    let error = &session.breakpoints()[0]["last_evaluation_error"];
    assert_eq!(error["kind"], "condition", "{error}");
    assert!(error["message"].as_str().unwrap().contains("unknown local"));

    // An unparseable condition is rejected unless forced; calls need --invoke.
    let rejected = session
        .update_breakpoint(
            "bp_1",
            BreakpointUpdate {
                condition: Some("count >".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(rejected.code, "debug_expression_invalid");
    let call = session
        .update_breakpoint(
            "bp_1",
            BreakpointUpdate {
                condition: Some("this.toString() == \"x\"".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(call.code, "invoke_not_allowed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logpoints_log_without_stopping_and_page_by_cursor() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let added = session
        .logpoint_add(target("MainActivity.kt"), 31, logpoint_options(Some("tag")))
        .await
        .unwrap();
    assert_eq!(added["created"], true);
    assert_eq!(added["breakpoint"]["kind"], "logpoint");
    assert_eq!(added["breakpoint"]["owner"], "agent");
    assert_eq!(added["breakpoint"]["suspend_policy"], "NONE");
    let id = added["breakpoint"]["id"].as_str().unwrap().to_string();

    assert_eq!(vm.hit_breakpoint(5), 1);
    vm.wait_for(WAIT, "the logpoint thread to resume", |s| {
        s.thread_resumes == 1
    });
    let page = events(&session, &Filter::default()).await;
    let event = &page["events"][0];
    assert_eq!(event["type"], "logpoint");
    assert_eq!(event["event_kind"], "message");
    assert_eq!(event["message"], "hello");
    assert_eq!(event["breakpoint_id"], id.as_str());
    assert_eq!(event["owner"], "agent");
    assert_eq!(event["line"], 31);
    assert_eq!(event["seq"], 1);
    assert!(
        page["stream_id"]
            .as_str()
            .unwrap()
            .starts_with("logpoints_jdwp_")
    );
    assert!(session.suspension().is_none(), "logpoints never stop");

    // Same owner, same line: reconfigured, not duplicated.
    let again = session
        .logpoint_add(
            target("MainActivity.kt"),
            31,
            logpoint_options(Some("count")),
        )
        .await
        .unwrap();
    assert_eq!(again["created"], false);
    assert_eq!(again["breakpoint"]["id"], id.as_str());
    // Another owner or a plain breakpoint there conflicts.
    let other = BreakpointOptions {
        owner: Some("someone-else".into()),
        ..logpoint_options(Some("tag"))
    };
    let conflict = session
        .logpoint_add(target("MainActivity.kt"), 31, other)
        .await
        .unwrap_err();
    assert_eq!(conflict.code, "logpoint_conflict");
    assert_eq!(conflict.detail["existing_owner"], "agent");
    let breakpoint_there = session
        .break_line_with(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap_err();
    assert_eq!(breakpoint_there.code, "logpoint_conflict");

    let listed = session.logpoints(None, Some("agent"));
    assert_eq!(listed["logpoints"].as_array().unwrap().len(), 1);
    assert_eq!(listed["defaults"]["max_events_per_second"], 20);
    let wrong = session
        .logpoint_remove(&id, "someone-else")
        .await
        .unwrap_err();
    assert_eq!(wrong.code, "logpoint_owner_mismatch");
    let cleared = session.logpoint_clear("agent").await.unwrap();
    assert_eq!(cleared["removed"], 1);
    assert!(session.is_quiescent());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hit_position_logpoint_never_suspends() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session
        .logpoint_add(target("MainActivity.kt"), 31, logpoint_options(None))
        .await
        .unwrap();
    let request = vm.with_state(|s| {
        s.requests
            .iter()
            .rev()
            .find(|r| r.kind == 2)
            .cloned()
            .unwrap()
    });
    assert_eq!(request.policy, 0, "pure hit logging uses SUSPEND_NONE");
    assert_eq!(vm.hit_breakpoint(5), 1);
    let page = events(&session, &Filter::default()).await;
    assert_eq!(
        page["events"][0]["message"],
        "Breakpoint reached at io.example.app.MainActivity.onNewIntent(MainActivity.kt:31)"
    );
    assert_eq!(vm.with_state(|s| (s.resumes, s.thread_resumes)), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_the_rate_limit_a_suspending_logpoint_is_disarmed_then_rearmed() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let options = BreakpointOptions {
        max_events_per_second: Some(1),
        ..logpoint_options(Some("tag"))
    };
    session
        .logpoint_add(target("MainActivity.kt"), 31, options)
        .await
        .unwrap();
    // Two hits in one second window (retry across a boundary).
    let mut throttled = serde_json::Value::Null;
    for _ in 0..3 {
        let resumed = vm.with_state(|s| s.thread_resumes);
        vm.hit_breakpoint(5);
        vm.wait_for(WAIT, "a resume", |s| s.thread_resumes > resumed);
        vm.hit_breakpoint(5);
        vm.wait_for(WAIT, "a resume", |s| s.thread_resumes > resumed + 1);
        throttled = session.breakpoints()[0].clone();
        if throttled["throttled"] == true {
            break;
        }
    }
    assert_eq!(throttled["throttled"], true, "{throttled}");
    assert!(throttled["dropped"].as_u64().unwrap() >= 1);
    assert!(throttled["rearm_at"].is_number());
    assert!(
        throttled["locations"][0]["request_id"].is_null(),
        "disarmed, not deleted"
    );
    assert_eq!(vm.hit_breakpoint(5), 0, "no request left to fire");

    let rearm_at = throttled["rearm_at"].as_f64().unwrap();
    while crate::events::now_ts() < rearm_at {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    session.rearm_due().await;
    let rearmed = &session.breakpoints()[0];
    assert_eq!(rearmed["throttled"], false);
    assert!(rearmed["locations"][0]["request_id"].is_number());
    let page = events(&session, &Filter::default()).await;
    assert!(page["rate_limited_total"].as_u64().unwrap() >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pass_counts_are_native_count_modifiers_that_expire() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session
        .break_line_with(
            target("MainActivity.kt"),
            31,
            BreakpointOptions {
                pass_count: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = vm.with_state(|s| {
        s.requests
            .iter()
            .rev()
            .find(|r| r.kind == 2)
            .cloned()
            .unwrap()
    });
    assert_eq!(
        request.modifier_kinds,
        [7, 1],
        "LocationOnly, then Count last"
    );
    assert_eq!(request.policy, 2);
    assert_eq!(vm.hit_breakpoint(5), 1);
    wait_suspended(&session).await;
    let breakpoint = &session.breakpoints()[0];
    assert_eq!(breakpoint["expired"], true, "{breakpoint}");
    assert!(breakpoint["locations"][0]["request_id"].is_null());
    assert_eq!(breakpoint["pass_count"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn break_line_is_idempotent_and_update_disables_and_reenables() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let first = session
        .break_line_with(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap();
    let again = session
        .break_line_with(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap();
    assert_eq!(again["created"], false);
    assert_eq!(again["breakpoint"]["id"], first["breakpoint"]["id"]);
    assert_eq!(session.breakpoints().as_array().unwrap().len(), 1);

    let disabled = session
        .update_breakpoint(
            "bp_1",
            BreakpointUpdate {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(disabled["enabled"], false);
    assert!(disabled["locations"][0]["request_id"].is_null());
    assert_eq!(vm.hit_breakpoint(5), 0, "nothing armed while disabled");

    let enabled = session
        .update_breakpoint(
            "bp_1",
            BreakpointUpdate {
                enabled: Some(true),
                suspend: Some(SuspendKind::Thread),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(enabled["suspend_policy"], "THREAD");
    let request = vm.with_state(|s| {
        s.requests
            .iter()
            .rev()
            .find(|r| r.kind == 2)
            .cloned()
            .unwrap()
    });
    assert_eq!(request.policy, 1);

    // --disabled at creation sets nothing on the VM.
    let created = session
        .break_line_with(
            target("MainActivity.kt"),
            21,
            BreakpointOptions {
                enabled: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(created["breakpoint"]["locations"][0]["request_id"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_breakpoints_and_continue_until_clean_up() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session
        .break_line_with(
            target("MainActivity.kt"),
            31,
            BreakpointOptions {
                temporary: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(vm.hit_breakpoint(5), 1);
    wait_suspended(&session).await;
    assert!(
        session.breakpoints().as_array().unwrap().is_empty(),
        "removed after its hit"
    );
    session.resume().await.unwrap();

    // continue-until arms a temporary breakpoint, waits, and removes it.
    let hitter = {
        let vm = vm.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(30)).await;
                if vm.hit_breakpoint(5) > 0 {
                    return;
                }
            }
        })
    };
    let reached = session
        .continue_until(target("MainActivity.kt"), 31, None, WAIT)
        .await
        .unwrap();
    hitter.await.unwrap();
    assert_eq!(reached["matched"], true);
    assert_eq!(reached["temporary_breakpoint"], true);
    assert_eq!(reached["session"]["position"]["line"], 31);
    assert!(session.breakpoints().as_array().unwrap().is_empty());

    // Nothing hits: a typed timeout, and the temporary one is gone too.
    session.resume().await.unwrap();
    let timeout = session
        .continue_until(
            target("MainActivity.kt"),
            31,
            None,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
    assert_eq!(timeout.code, "debug_wait_timeout");
    assert!(session.breakpoints().as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_stop_reports_stops_and_process_exit() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let idle = session.wait_stop(None, Duration::from_millis(50)).await;
    assert_eq!(idle["timed_out"], true);
    session
        .break_line(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap();
    let epoch = session.status().await["epoch"].as_u64().unwrap();
    let hitter = {
        let vm = vm.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            vm.hit_breakpoint(5);
        })
    };
    let stopped = session.wait_stop(Some(epoch), WAIT).await;
    hitter.await.unwrap();
    assert_eq!(stopped["stopped"], true);
    assert_eq!(stopped["session"]["breakpoint_id"], "bp_1");
    // EOF without VMDeath (an Android crash): reported as closed.
    vm.kill_connection();
    let closed = session.wait_stop(Some(u64::MAX), WAIT).await;
    assert!(closed["closed"].is_string(), "{closed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_warns_about_anr_after_a_long_stop_in_an_attached_process() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session.pause().await.unwrap();
    assert!(session.status().await["warning"].is_null());
    session.backdate_suspension(5.0);
    let status = session.status().await;
    assert!(
        status["warning"]
            .as_str()
            .unwrap()
            .contains("wait-for-launch"),
        "{status}"
    );
    let stack = session.stack(None, 4).await.unwrap();
    assert!(stack["warning"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn framework_internal_exceptions_are_not_crashes() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session
        .break_exception("java.lang.String", false, true, Default::default())
        .await
        .unwrap();
    // `ErrnoException` inside `File.exists`: thrown in framework code (String
    // 103 stands in) and caught by framework code (Object 104) above the app
    // frame that called it. Never reaches app code: resumed, not a stop.
    vm.with_state(|s| s.main_frames = Some(vec![(103, 5000, 0), (104, 6000, 3), (100, 1001, 5)]));
    assert_eq!(vm.throw_exception_at(Some((104, 6000))), 1);
    vm.wait_for(WAIT, "the internal exception to resume", |s| s.resumes == 1);
    assert!(session.suspension().is_none());

    // A crash: thrown in app code, caught by a framework rethrower below it.
    vm.with_state(|s| s.main_frames = Some(vec![(100, 1001, 5), (104, 6000, 3)]));
    assert_eq!(vm.throw_exception_at(Some((104, 6000))), 1);
    wait_suspended(&session).await;
    assert_eq!(session.status().await["suspend_reason"], "exception");
}

// ── P1c: --invoke ───────────────────────────────────────────────────────

const INVOKE: Option<Duration> = Some(Duration::from_secs(2));

async fn stopped_at_line_31(vm: &FakeVm) -> Arc<Session> {
    let session = attach(vm, WAIT).await;
    session
        .break_line(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap();
    assert_eq!(vm.hit_breakpoint(5), 1);
    wait_suspended(&session).await;
    session
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoke_calls_methods_only_when_asked() {
    let vm = FakeVm::start();
    let session = stopped_at_line_31(&vm).await;

    let refused = session
        .eval("this.getLabel()", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "invoke_not_allowed");
    assert!(vm.with_state(|s| s.invokes.is_empty()), "nothing ran");

    let label = session
        .eval("this.getLabel()", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(label["mode"], "jdi_invoke");
    assert_eq!(label["result"]["value"], "hello");
    let sum = session
        .eval("this.add(2, count - 0 == 42)", None, None, OPTIONS, INVOKE)
        .await
        .unwrap_err();
    assert_eq!(sum.code, "invalid_expression", "boolean does not fit int");
    let sum = session
        .eval("this.add(2, 40)", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(sum["result"]["value"], "42");
    let with_local = session
        .eval("add(count, 1)", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(
        with_local["result"]["value"], "43",
        "a bare call is on `this`"
    );
    let statik = session
        .eval("this.staticHelper()", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(statik["result"]["value"], "7");
    let echoed = session
        .eval("this.greet(\"yo\")", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(
        echoed["result"]["value"], "yo",
        "string literal via CreateString"
    );

    // Kotlin property sugar: no `status` field, a getStatus() getter.
    let sugar = session
        .eval("this.status", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(sugar["result"]["value"], "hello");
    let no_sugar = session
        .eval("this.status", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(no_sugar.detail["getter"], "getStatus");

    // A comparison over calls renders its value.
    let compared = session
        .eval(
            "this.add(1, 1) == 2 && tag == \"hello\"",
            None,
            None,
            OPTIONS,
            INVOKE,
        )
        .await
        .unwrap();
    assert_eq!(compared["result"]["value"], "true");

    // toString() for objects without a built-in renderer.
    let this = session
        .eval("this", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(this["result"]["value"], "MainActivity{counter=7}", "{this}");
    assert_eq!(this["result"]["to_string"]["truncated"], false);
    let plain = session
        .eval("this", None, None, OPTIONS, None)
        .await
        .unwrap();
    assert!(plain["result"].get("to_string").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thrown_exception_is_a_structured_result() {
    let vm = FakeVm::start();
    let session = stopped_at_line_31(&vm).await;
    let thrown = session
        .eval("this.boom()", None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(thrown["result"]["thrown"], true, "{thrown}");
    assert_eq!(
        thrown["result"]["exception"]["type"],
        "java.lang.IllegalStateException"
    );
    assert_eq!(thrown["result"]["exception"]["message"], "kaboom");
    // In a condition, a throw is an evaluation error (the thread stays put).
    let error = session
        .inspect(Some("this.boom()"), None, None, None, None, OPTIONS, INVOKE)
        .await
        .unwrap();
    assert_eq!(error["result"]["thrown"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breakpoints_are_disarmed_during_an_invoke_and_its_events_resumed() {
    let vm = FakeVm::start();
    let session = stopped_at_line_31(&vm).await;
    let armed = session.breakpoints()[0]["locations"][0]["request_id"]
        .as_i64()
        .unwrap() as i32;
    // trap() raises a breakpoint on the invoking thread and replies only
    // after it is resumed: without the forwarder this would deadlock.
    let trapped = tokio::time::timeout(
        WAIT,
        session.eval("this.trap()", None, None, OPTIONS, INVOKE),
    )
    .await
    .expect("no deadlock")
    .unwrap();
    assert_eq!(trapped["result"]["type"], "void");
    assert_eq!(
        session.status().await["invoke"]["events_skipped_during_invoke"],
        1
    );
    assert_eq!(session.status().await["suspend_reason"], "breakpoint");
    // Our breakpoint request was cleared for the invoke and re-armed.
    assert!(vm.with_state(|s| s.cleared.contains(&armed)));
    let rearmed = session.breakpoints()[0]["locations"][0]["request_id"].clone();
    assert!(rearmed.is_number() && rearmed != armed, "{rearmed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_invoke_marks_the_thread_busy_until_it_returns() {
    let vm = FakeVm::start();
    let session = stopped_at_line_31(&vm).await;
    let late = session
        .eval(
            "this.hang()",
            None,
            None,
            OPTIONS,
            Some(Duration::from_millis(100)),
        )
        .await
        .unwrap_err();
    assert_eq!(late.code, "invoke_timeout");
    let busy = session
        .eval("tag", None, None, OPTIONS, None)
        .await
        .unwrap_err();
    assert_eq!(busy.code, "thread_busy_invoking");
    vm.release_hang();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match session.eval("tag", None, None, OPTIONS, None).await {
            Ok(value) => {
                assert_eq!(value["result"]["value"], "hello");
                break;
            }
            Err(error) => {
                assert_eq!(error.code, "thread_busy_invoking");
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_session_refuses_invokes() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    session.pause().await.unwrap();
    let refused = session
        .eval("this.getLabel()", None, None, OPTIONS, INVOKE)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "invoke_requires_event_stop");
    assert!(refused.next_actions[0].contains("step-over"));
    assert!(vm.with_state(|s| s.invokes.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditions_and_log_expressions_can_invoke_when_allowed() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let refused = session
        .break_line_with(
            target("MainActivity.kt"),
            31,
            BreakpointOptions {
                condition: Some("this.add(1, 1) == 2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, "invoke_not_allowed");
    session
        .break_line_with(
            target("MainActivity.kt"),
            31,
            BreakpointOptions {
                condition: Some("this.add(1, 1) == 2".into()),
                invoke: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(session.breakpoints()[0]["invoke"], true);
    assert_eq!(vm.hit_breakpoint(5), 1);
    wait_suspended(&session).await;
    assert_eq!(session.status().await["suspend_reason"], "breakpoint");
    assert!(vm.with_state(|s| s.invokes.iter().any(|(method, _)| *method == 1004)));
}

// ── P1c: method breakpoints and field watches ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_breakpoints_are_line_breakpoints_at_entry_and_returns() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let breakpoint = session
        .break_method(
            "io.example.app.Main*",
            "onNew*",
            true,
            true,
            Default::default(),
        )
        .await
        .unwrap();
    let locations = breakpoint["locations"].as_array().unwrap();
    let roles: Vec<_> = locations
        .iter()
        .map(|l| {
            (
                l["method"].as_str().unwrap(),
                l["role"].as_str().unwrap(),
                l["code_index"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [("onNewIntent", "entry", 0), ("onNewIntent", "exit", 10)],
        "the synthetic bridge is skipped; exit is the DEX return-void"
    );
    assert_eq!(breakpoint["mechanism"], "line_breakpoints");
    // Never MethodEntry/MethodExit (event kinds 40/41).
    assert!(vm.with_state(|s| s.requests.iter().all(|r| r.kind != 40 && r.kind != 41)));
    assert_eq!(vm.hit_at(1001, 0), 1);
    wait_suspended(&session).await;
    assert_eq!(
        session.status().await["suspend_reason"],
        "method_breakpoint"
    );

    // A class loaded later binds through ClassPrepare.
    session.resume().await.unwrap();
    let late = session
        .break_method(
            "io.example.app.Late",
            "run",
            true,
            false,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(late["pending_reason"], "class_not_loaded");
    vm.load_late_class();
    let deadline = std::time::Instant::now() + WAIT;
    while session.breakpoints()[1]["bound"] != true {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(session.breakpoints()[1]["locations"][0]["method"], "run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn property_watches_use_accessors_unless_slowdown_is_accepted() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let accessor = session
        .break_field(
            "io.example.app.MainActivity",
            "counter",
            true,
            true,
            false,
            Duration::from_secs(60),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(accessor["mechanism"], "accessor_breakpoints");
    let roles: Vec<_> = accessor["locations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["method"].as_str().unwrap(), l["role"].as_str().unwrap()))
        .collect();
    assert_eq!(roles, [("setCounter", "setter"), ("getCounter", "getter")]);
    assert!(vm.with_state(|s| s.requests.iter().all(|r| r.kind != 20 && r.kind != 21)));
    assert_eq!(session.slow_requests(), 0);
    assert!(session.status().await["slow_watch_warning"].is_null());

    // No accessor: a real watch needs the opt-in.
    let refused = session
        .break_field(
            "io.example.app.MainActivity",
            "label",
            false,
            true,
            false,
            Duration::from_secs(60),
            Default::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, "unsupported_location");
    assert_eq!(refused.detail["reason"], "no_accessor");

    let watch = session
        .break_field(
            "io.example.app.MainActivity",
            "counter",
            false,
            true,
            true,
            Duration::from_secs(60),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(watch["mechanism"], "field_watch");
    assert_eq!(watch["watched_field"], "counter");
    assert!(
        watch["warning"].as_str().unwrap().contains("slows")
            || watch["warning"].as_str().unwrap().contains("interpret")
    );
    let request = vm.with_state(|s| {
        s.requests
            .iter()
            .rev()
            .find(|r| r.kind == 21)
            .cloned()
            .unwrap()
    });
    assert_eq!(request.field, Some(2000));
    assert_eq!(request.modifier_kinds, [9], "FieldOnly");
    assert_eq!(session.slow_requests(), 1);
    assert!(session.status().await["slow_watch_warning"].is_string());

    assert_eq!(vm.modify_counter(), 1);
    wait_suspended(&session).await;
    assert_eq!(session.status().await["suspend_reason"], "field_watch");
    session.resume().await.unwrap();

    // Auto-clear after the duration: disarmed, kept, marked expired.
    let id = watch["id"].as_str().unwrap().to_string();
    session
        .update_breakpoint(&id, Default::default())
        .await
        .unwrap();
    {
        let short = session
            .break_field(
                "io.example.app.MainActivity",
                "counter",
                true,
                false,
                true,
                Duration::ZERO,
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(short["mechanism"], "field_watch");
    }
    session.expire_slow_watches().await;
    let breakpoints = session.breakpoints();
    let expired = breakpoints
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["expired_reason"] == "watch_duration_elapsed")
        .expect("an expired watch");
    assert!(expired["locations"][0]["request_id"].is_null());
    assert_eq!(
        session.slow_requests(),
        1,
        "only the 60 s watch remains armed"
    );
}

// ── P1c: line vs lambda ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn line_variants_choose_outer_or_the_innermost_lambda() {
    use super::lambdas::LineVariant;
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let shape = |value: &serde_json::Value| {
        value["breakpoint"]["locations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["method"].as_str().unwrap().to_string(),
                    l["lambda"].as_bool().unwrap(),
                    l["lambda_depth"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    for (variant, expected) in [
        (
            LineVariant::All,
            vec![
                ("onNewIntent", false, 0),
                ("onNewIntent$lambda$0", true, 1),
                ("onNewIntent$lambda$0$lambda$1", true, 2),
            ],
        ),
        (LineVariant::Outer, vec![("onNewIntent", false, 0)]),
        (
            LineVariant::Lambda,
            vec![("onNewIntent$lambda$0$lambda$1", true, 2)],
        ),
    ] {
        let created = session
            .break_line_with(
                target("MainActivity.kt"),
                33,
                BreakpointOptions {
                    variant,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(m, l, d)| (m.to_string(), l, d))
            .collect();
        assert_eq!(shape(&created), expected, "{variant:?}");
        assert_eq!(
            created["breakpoint"]["variant"],
            serde_json::to_value(variant).unwrap()
        );
        let id = created["breakpoint"]["id"].as_str().unwrap().to_string();
        session.remove_breakpoint(&id).await.unwrap();
    }
    // The synthetic bridge on line 30 is never bound.
    let bridge = session
        .break_line(target("MainActivity.kt"), 30, Default::default())
        .await
        .unwrap();
    let methods: Vec<_> = bridge["locations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["method"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(methods, ["onNewIntent"]);
}

// ── P1c: watches and coroutines ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watches_are_evaluated_on_every_stop_and_on_list() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let added = session.watch_add("tag", None).unwrap();
    assert_eq!(added["watch"]["id"], super::watches::watch_id("tag", "tag"));
    session.watch_add("this.counter", Some("counter")).unwrap();
    session.watch_add("nope", None).unwrap();
    let refused = session.watch_add("this.getLabel()", None).unwrap_err();
    assert_eq!(refused.code, "invoke_not_allowed");

    // Running: cached (empty) values and a warning.
    let running = session.watch_list(OPTIONS).await.unwrap();
    assert_eq!(
        running["warning"],
        "session is not suspended; returning cached watch values"
    );
    assert!(running["watches"][0]["value"].is_null());

    session
        .break_line(target("MainActivity.kt"), 31, Default::default())
        .await
        .unwrap();
    vm.hit_breakpoint(5);
    wait_suspended(&session).await;
    // Refreshed by the stop itself (before any list).
    let deadline = tokio::time::Instant::now() + WAIT;
    while session.watches.lock().unwrap().1.len() < 3 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listed = session.watch_list(OPTIONS).await.unwrap();
    let watches = listed["watches"].as_array().unwrap();
    assert_eq!(watches[0]["value"]["value"], "hello");
    assert_eq!(watches[1]["name"], "counter");
    assert_eq!(watches[1]["value"]["value"], "7");
    assert!(
        watches[2]["error"]
            .as_str()
            .unwrap()
            .contains("unknown local")
    );
    assert!(watches[0]["updated_at"].is_number());
    assert_eq!(watches[0]["selected_frame"]["thread_name"], "main");
    assert!(listed["warning"].is_null());

    let id = watches[2]["id"].as_str().unwrap().to_string();
    assert_eq!(session.watch_remove(&id)["removed"], true);
    assert_eq!(session.watch_clear()["removed"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coroutines_are_discovered_process_wide_through_instances() {
    let vm = FakeVm::start();
    let session = stopped_at_line_31(&vm).await;
    let snapshot = session.coroutine_snapshot(64, OPTIONS).await.unwrap();
    assert_eq!(snapshot["available"], true);
    assert_eq!(snapshot["type"], "coroutine_snapshot");
    let coroutines = snapshot["coroutines"].as_array().unwrap();
    assert_eq!(coroutines.len(), 2, "{snapshot}");
    // The coroutine running app code sorts first.
    let worker = &coroutines[0];
    // A job whose state is one JobNode (ChildContinuation) is still active.
    assert_eq!(
        coroutines[1]["state_class"],
        "kotlinx.coroutines.ChildContinuation"
    );
    assert_eq!(coroutines[1]["state"], "active", "{snapshot}");
    assert_eq!(worker["class"], "kotlinx.coroutines.StandaloneCoroutine");
    assert_eq!(worker["name"], "worker");
    assert_eq!(worker["dispatcher"], "Dispatchers.Default");
    assert_eq!(worker["state"], "active");
    // Through DebugProbesImpl$CoroutineOwner.delegate to the coroutine.
    assert_eq!(
        worker["continuations"][0]["class"],
        "io.example.app.Work$run$1"
    );
    assert_eq!(worker["continuations"][0]["label"], 2);
    assert_eq!(snapshot["discovery"]["coroutine_classes"], 1);
    assert_eq!(snapshot["discovery"]["continuation_classes"], 1);
    assert!(snapshot["threads"][0]["dispatcher"]["name"] == "Dispatchers.Main");

    let threads = session.coroutine_threads(8).await.unwrap();
    assert_eq!(threads["type"], "coroutine_threads");
    let flow = session
        .coroutine_flow("this.label", None, None, OPTIONS)
        .await
        .unwrap();
    assert_eq!(flow["type"], "coroutine_flow");
    assert!(flow["kind"].is_null());
    let continuation = session
        .coroutine_continuation(None, None, OPTIONS)
        .await
        .unwrap();
    assert_eq!(continuation["type"], "coroutine_continuation");

    // A tight limit keeps the app's coroutine, not discovery order.
    let capped = session.coroutine_snapshot(1, OPTIONS).await.unwrap();
    assert_eq!(capped["coroutines"][0]["name"], "worker", "{capped}");
    assert_eq!(capped["discovery"]["truncated"], true);

    session.resume().await.unwrap();
    let running = session.coroutine_snapshot(8, OPTIONS).await.unwrap();
    assert_eq!(running["available"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_watch_on_a_delegated_property_breaks_on_its_setter_instead() {
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    // `status` is `by mutableStateOf(...)`: the class has `status$delegate`
    // (assigned once) and `setStatus`; a field watch would never fire.
    let value = session
        .break_field(
            "io.example.app.MainActivity",
            "status",
            false,
            true,
            true,
            Duration::from_secs(60),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(value["mechanism"], "accessor_breakpoints", "{value}");
    assert!(
        value["note"]
            .as_str()
            .unwrap()
            .contains("delegated property"),
        "{value}"
    );
    assert_eq!(value["locations"][0]["method"], "setStatus", "{value}");
    assert!(value["slow_until"].is_null(), "{value}");
    assert_eq!(session.slow_requests(), 0);
    assert!(vm.with_state(|s| s.requests.iter().all(|r| r.kind != 20 && r.kind != 21)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_variant_of_a_line_is_its_own_breakpoint() {
    use super::lambdas::LineVariant;
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    let add = |variant| {
        let session = session.clone();
        async move {
            session
                .break_line_with(
                    target("MainActivity.kt"),
                    33,
                    BreakpointOptions {
                        variant,
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
        }
    };
    let lambda = add(LineVariant::Lambda).await;
    let outer = add(LineVariant::Outer).await;
    assert_eq!(outer["created"], true, "{outer}");
    assert_ne!(lambda["breakpoint"]["id"], outer["breakpoint"]["id"]);
    assert_eq!(outer["breakpoint"]["variant"], "outer");
    assert_eq!(outer["breakpoint"]["locations"][0]["method"], "onNewIntent");
    // The same variant again is idempotent.
    let again = add(LineVariant::Lambda).await;
    assert_eq!(again["created"], false);
    assert_eq!(again["breakpoint"]["id"], lambda["breakpoint"]["id"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_variant_with_no_location_on_the_line_says_so() {
    use super::lambdas::LineVariant;
    let vm = FakeVm::start();
    let session = attach(&vm, WAIT).await;
    // Line 27 holds only the lambda body `onCreate$lambda$5`.
    let outer = session
        .break_line_with(
            target("MainActivity.kt"),
            27,
            BreakpointOptions {
                variant: LineVariant::Outer,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        outer["breakpoint"]["pending_reason"], "no_location_for_variant",
        "{outer}"
    );
    assert!(
        outer["breakpoint"]["note"]
            .as_str()
            .unwrap()
            .contains("--variant")
    );
}
