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

const OPTIONS: RenderOptions = RenderOptions {
    depth: 1,
    max_fields: 64,
    max_array_items: 32,
};

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
    let it = session.eval("it", None, None, OPTIONS).await.unwrap();
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
        .eval("this.label", None, None, OPTIONS)
        .await
        .unwrap();
    assert_eq!(eval["result"]["value"], "hello");
    assert_eq!(eval["result"]["declared_type"], "java.lang.String");
    let eval = session
        .eval("this.numbers[1]", None, None, OPTIONS)
        .await
        .unwrap();
    assert_eq!(eval["result"]["value"], "2");
    let array = session
        .eval("this.numbers", None, None, OPTIONS)
        .await
        .unwrap();
    assert_eq!(array["result"]["length"], 3);
    assert_eq!(array["result"]["items"][2]["value"], "3");
    let error = session
        .eval("this.numbers[9]", None, None, OPTIONS)
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_expression");
    let error = session.eval("nope", None, None, OPTIONS).await.unwrap_err();
    assert!(error.message.contains("unknown local"), "{error:?}");

    let inspect = session
        .inspect(None, Some("obj_500"), Some(".counter"), None, None, OPTIONS)
        .await
        .unwrap();
    assert_eq!(inspect["result"]["value"], "7");
    assert_eq!(inspect["mode"], "object_handle");
    let stale = session
        .inspect(None, Some("obj_777"), None, None, None, OPTIONS)
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
    let error = session.eval("tag", None, None, OPTIONS).await.unwrap_err();
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
        .eval("$exception", None, None, OPTIONS)
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
        .eval("this.status", None, None, OPTIONS)
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
        .eval("this.nothing", None, None, OPTIONS)
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

    // An unparseable condition is rejected unless forced.
    let rejected = session
        .update_breakpoint(
            "bp_1",
            BreakpointUpdate {
                condition: Some("this.toString()".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(rejected.code, "debug_expression_invalid");
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
