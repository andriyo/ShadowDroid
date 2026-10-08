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
        .break_line(target("MainActivity.kt"), 31)
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
    assert_eq!(names, ["tag", "count"], "markers and `this` are hidden");
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
        ["counter", "label", "numbers"],
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
    let line = session.break_line(target("Late.kt"), 7).await.unwrap();
    assert_eq!(line["bound"], false);
    assert_eq!(line["pending_reason"], "class_not_loaded");
    let exception = session
        .break_exception("io.example.app.Late", true, true)
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
        .break_line(target("MainActivity.kt"), 99)
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
    let error = session.break_line(inline, 31).await.unwrap_err();
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
    let line = session.break_line(target("Late.kt"), 7).await.unwrap();
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
        .break_exception("java.lang.String", false, true)
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

    // Truly uncaught: a stop as well.
    assert_eq!(vm.throw_exception(None), 1);
    wait_suspended(&session).await;
    assert_eq!(session.breakpoints()[0]["hit_count"], 2);
}
