//! End-to-end `debug --backend jdwp` through the real binary and its
//! `__debugd` daemon, against the fake VM (`tests/support/fake_jdwp.rs`).
//! `SHADOWDROID_JDWP_TCP` points the daemon at the fake instead of adb.
#![cfg(unix)]

mod support;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use support::fake_jdwp::FakeVm;

struct Env {
    _temp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    vm: FakeVm,
}

impl Env {
    fn new() -> Env {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let source = project.join("app/src/main/kotlin/io/example/app");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("MainActivity.kt"),
            // Code on every line the fake VM's line tables name (20..32).
            format!(
                "package io.example.app\n\nclass MainActivity {{\n{}}}\n",
                "    val x = 1\n".repeat(40)
            ),
        )
        .unwrap();
        Env {
            _temp: temp,
            home,
            project,
            vm: FakeVm::start(),
        }
    }

    fn run(&self, args: &[&str]) -> (Value, i32) {
        let output = Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
            .args(["-d", "fake-serial", "--project-root"])
            .arg(&self.project)
            .args(args)
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env_remove("USERPROFILE")
            .env("SHADOWDROID_QUIET", "1")
            .env("SHADOWDROID_JDWP_TCP", self.vm.address())
            .output()
            .expect("spawn shadowdroid");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .unwrap_or_else(|| {
                panic!(
                    "no stdout for {args:?}; stderr={}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        let value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {line}"));
        (value, output.status.code().unwrap_or(-1))
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (value, code) = self.run(args);
        assert_eq!(code, 0, "{args:?} failed: {value}");
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["backend"], "jdwp", "{value}");
        value
    }

    /// `~/.shadowdroid/debug` (serial subdirectories are hashed names).
    fn registry_dir(&self) -> PathBuf {
        self.home.join(".shadowdroid/debug")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Never leave a daemon behind, even when an assertion failed.
        let _ = Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
            .args(["-d", "fake-serial", "debug", "detach", "--backend", "jdwp"])
            .env("HOME", &self.home)
            .env("SHADOWDROID_QUIET", "1")
            .env("SHADOWDROID_JDWP_TCP", self.vm.address())
            .output();
    }
}

fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Registry entries under every serial directory.
fn registry_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|serial| std::fs::read_dir(serial.path()).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json") && !name.ends_with(".startup.json"))
        .collect()
}

#[test]
fn attach_break_hit_inspect_step_resume_detach() {
    let env = Env::new();
    let attached = env.ok(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    assert_eq!(attached["already_attached"], false, "{attached}");
    assert_eq!(attached["session"]["id"], "jdwp:fake-serial:4242");
    assert_eq!(attached["vm"]["vm_name"], "Dalvik");
    assert_eq!(attached["capabilities"]["can_watch_field_access"], true);
    assert_eq!(registry_files(&env.registry_dir()), ["4242.json"]);

    // Idempotent: a second attach reuses the live daemon.
    let again = env.ok(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    assert_eq!(again["already_attached"], true);

    let sessions = env.ok(&["debug", "sessions", "--backend", "jdwp"]);
    assert_eq!(sessions["sessions"][0]["backend"], "jdwp");
    assert_eq!(sessions["sessions"][0]["suspended"], false);

    // The file resolves through the project index (name only, no path).
    let breakpoint = env.ok(&[
        "debug",
        "break",
        "line",
        "--backend",
        "jdwp",
        "--file",
        "MainActivity.kt",
        "--line",
        "31",
    ]);
    assert_eq!(breakpoint["breakpoint"]["bound"], true, "{breakpoint}");
    assert_eq!(breakpoint["breakpoint"]["package"], "io.example.app");
    assert_eq!(
        breakpoint["breakpoint"]["locations"][0]["method"],
        "onNewIntent"
    );

    // Reads before a stop are bounded and structured, never a hang.
    let (early, code) = env.run(&["debug", "eval", "--backend", "jdwp", "tag"]);
    assert_ne!(code, 0);
    assert_eq!(early["code"], "debugger_not_suspended", "{early}");

    assert_eq!(env.vm.hit_breakpoint(5), 1);
    wait_until("the breakpoint stop", || {
        env.ok(&["debug", "status", "--backend", "jdwp"])["sessions"][0]["suspended"] == true
    });

    let stack = env.ok(&["debug", "stack", "--backend", "jdwp"]);
    assert_eq!(stack["frames"][0]["line"], 31, "{stack}");
    assert_eq!(stack["frames"][0]["source"], "MainActivity.kt");

    let variables = env.ok(&["debug", "variables", "--backend", "jdwp", "--depth", "1"]);
    assert_eq!(variables["variables"][0]["name"], "tag", "{variables}");
    assert_eq!(variables["variables"][0]["value"], "hello");
    assert_eq!(variables["this"]["object_handle"], "obj_500");

    let eval = env.ok(&["debug", "eval", "--backend", "jdwp", "this.counter"]);
    assert_eq!(eval["result"]["value"], "7", "{eval}");
    let inspect = env.ok(&[
        "debug",
        "inspect",
        "--backend",
        "jdwp",
        "--handle",
        "obj_500",
        "--path",
        ".numbers[0]",
    ]);
    assert_eq!(inspect["result"]["value"], "1", "{inspect}");

    let stepped = env.ok(&["debug", "step-over", "--backend", "jdwp"]);
    assert_eq!(stepped["session"]["position"]["line"], 32, "{stepped}");

    let resumed = env.ok(&["debug", "resume", "--backend", "jdwp"]);
    assert_eq!(resumed["session"]["suspended"], false);

    let breakpoints = env.ok(&["debug", "breakpoints", "--backend", "jdwp"]);
    assert_eq!(breakpoints["breakpoints"][0]["hit_count"], 1);

    let detached = env.ok(&["debug", "detach", "--backend", "jdwp"]);
    assert_eq!(detached["result"]["detached"], true);
    env.vm
        .wait_for(Duration::from_secs(5), "Dispose", |state| state.disposed);
    wait_until("the registry entry to go away", || {
        registry_files(&env.registry_dir()).is_empty()
    });
    let (gone, code) = env.run(&["debug", "stack", "--backend", "jdwp"]);
    assert_ne!(code, 0);
    assert_eq!(gone["code"], "debugger_session_not_found", "{gone}");
}

#[test]
fn a_held_process_is_reported_as_already_attached() {
    let env = Env::new();
    env.vm.with_state(|state| state.reject_handshake = true);
    let (error, code) = env.run(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    assert_ne!(code, 0);
    assert_eq!(error["code"], "debugger_already_attached", "{error}");
    assert_eq!(error["detail"]["handshake_bytes_read"], 0, "{error}");
    assert!(registry_files(&env.registry_dir()).is_empty());
}

#[test]
fn a_studio_attach_to_a_held_pid_is_refused_up_front() {
    let env = Env::new();
    env.ok(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    // The bridge URL is dead on purpose: the refusal must not need Studio.
    let (error, code) = env.run(&[
        "debug",
        "attach",
        "--backend",
        "studio",
        "--pid",
        "4242",
        "--studio-url",
        "http://127.0.0.1:9",
    ]);
    assert_ne!(code, 0);
    assert_eq!(error["code"], "debugger_already_attached", "{error}");
    assert_eq!(error["detail"]["holder"], "jdwp", "{error}");
    env.ok(&["debug", "detach", "--backend", "jdwp"]);
}

#[test]
fn a_silent_handshake_is_a_timeout_not_another_debugger() {
    let env = Env::new();
    env.vm.with_state(|state| state.silent_handshake = true);
    let (error, code) = env.run(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    assert_ne!(code, 0);
    assert_eq!(error["code"], "debugger_timeout", "{error}");
    assert_eq!(error["detail"]["command"], "JDWP-Handshake", "{error}");
    assert_eq!(error["detail"]["pid"], 4242, "{error}");
    assert!(error["detail"]["elapsed_ms"].as_u64().is_some(), "{error}");
    assert!(registry_files(&env.registry_dir()).is_empty());
}

#[test]
fn the_daemon_exits_when_the_process_dies() {
    let env = Env::new();
    env.ok(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    env.vm.kill_connection();
    wait_until("the daemon to deregister", || {
        registry_files(&env.registry_dir()).is_empty()
    });
    let (error, code) = env.run(&["debug", "pause", "--backend", "jdwp"]);
    assert_ne!(code, 0);
    assert_eq!(error["code"], "debugger_session_not_found", "{error}");
}

#[test]
fn unsupported_verbs_fail_typed_and_studio_stays_the_default() {
    let env = Env::new();
    let (error, code) = env.run(&["debug", "watch", "list", "--backend", "jdwp"]);
    assert_ne!(code, 0);
    assert_eq!(error["code"], "unsupported_by_backend", "{error}");
    // Rejected before any device or server bring-up.
    let (error, _) = env.run(&["debug", "record", "-o", "t.jsonl", "--backend", "jdwp"]);
    assert_eq!(error["code"], "unsupported_by_backend", "{error}");

    // Without --backend nothing reaches the JDWP registry or daemon: the
    // Studio bridge answers (here: is unreachable) exactly as before.
    let (studio, _) = env.run(&["debug", "sessions", "--studio-url", "http://127.0.0.1:9"]);
    assert_ne!(studio["backend"], "jdwp", "{studio}");
    assert!(!env.registry_dir().exists());
}

// ── P1b ──────────────────────────────────────────────────────────────────

impl Env {
    /// Every JSON line a command printed.
    fn run_lines(&self, args: &[&str]) -> Vec<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_shadowdroid"))
            .args(["-d", "fake-serial", "--project-root"])
            .arg(&self.project)
            .args(args)
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env_remove("USERPROFILE")
            .env("SHADOWDROID_QUIET", "1")
            .env("SHADOWDROID_JDWP_TCP", self.vm.address())
            .output()
            .expect("spawn shadowdroid");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("JSON line"))
            .collect()
    }
}

#[test]
fn wait_for_launch_installs_breakpoints_before_reporting_ready() {
    let env = Env::new();
    // Studio does not take launch-time options; its default path is unchanged.
    let (studio, code) = env.run(&[
        "debug",
        "attach",
        "--wait-for-launch",
        "--package",
        "io.example.app",
        "--studio-url",
        "http://127.0.0.1:9",
    ]);
    assert_ne!(code, 0);
    assert_eq!(studio["code"], "unsupported_by_backend", "{studio}");

    let attached = env.ok(&[
        "debug",
        "attach",
        "--backend",
        "jdwp",
        "--package",
        "io.example.app",
        "--wait-for-launch",
        "--break",
        "MainActivity.kt:31",
        "--break",
        "Late.kt:7",
        "--break-exception",
        "java.lang.IllegalStateException",
    ]);
    assert_eq!(attached["launch"]["wait_for_launch"], true, "{attached}");
    assert_eq!(attached["session"]["launched_under_debugger"], true);
    let breakpoints = attached["breakpoints"].as_array().unwrap();
    assert_eq!(breakpoints.len(), 3, "{attached}");
    assert_eq!(breakpoints[0]["bound"], true);
    assert_eq!(breakpoints[0]["locations"][0]["method"], "onNewIntent");
    // Not loaded yet: armed through ClassPrepare + SourceNameMatch.
    assert_eq!(breakpoints[1]["pending_reason"], "class_not_loaded");
    assert_eq!(breakpoints[2]["type"], "exception");
    let prepares = env.vm.with_state(|s| {
        s.requests
            .iter()
            .filter(|r| r.kind == 8)
            .filter_map(|r| r.source_name.clone())
            .collect::<Vec<_>>()
    });
    assert!(prepares.contains(&"Late.kt".to_string()), "{prepares:?}");

    // A malformed spec is a typed usage error.
    let (bad, code) = env.run(&[
        "debug",
        "attach",
        "--backend",
        "jdwp",
        "--package",
        "io.example.app",
        "--break",
        "MainActivity.kt",
    ]);
    assert_ne!(code, 0);
    assert_eq!(bad["code"], "invalid_arguments", "{bad}");
}

#[test]
fn logpoints_round_trip_through_the_cli() {
    let env = Env::new();
    env.ok(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    let added = env.ok(&[
        "debug",
        "logpoint",
        "add",
        "--backend",
        "jdwp",
        "--file",
        "MainActivity.kt",
        "--line",
        "31",
        "--expression",
        "tag",
        "--owner",
        "agent",
    ]);
    assert_eq!(added["created"], true, "{added}");
    let id = added["breakpoint"]["id"].as_str().unwrap().to_string();

    env.vm.hit_breakpoint(5);
    let mut events = Value::Null;
    wait_until("the logpoint event", || {
        events = env.ok(&["debug", "logpoint", "events", "--backend", "jdwp"]);
        events["events"].as_array().is_some_and(|e| !e.is_empty())
    });
    assert_eq!(events["events"][0]["message"], "hello", "{events}");
    let stream = events["stream_id"].as_str().unwrap().to_string();
    let cursor = events["next_cursor"].as_u64().unwrap();

    // Paging by cursor + stream id, as on Studio.
    let page = env.ok(&[
        "debug",
        "logpoint",
        "events",
        "--backend",
        "jdwp",
        "--after",
        &cursor.to_string(),
        "--stream-id",
        &stream,
    ]);
    assert_eq!(page["events"].as_array().unwrap().len(), 0);
    let (stale, _) = env.run(&[
        "debug",
        "logpoint",
        "events",
        "--backend",
        "jdwp",
        "--after",
        "0",
        "--stream-id",
        "logpoints_other",
    ]);
    assert_eq!(stale["code"], "logpoint_stream_changed", "{stale}");

    let follow = env.run_lines(&[
        "debug",
        "logpoint",
        "follow",
        "--backend",
        "jdwp",
        "--replay-existing",
        "--max-events",
        "1",
        "--duration-ms",
        "3000",
    ]);
    assert_eq!(follow[0]["type"], "logpoint", "{follow:?}");
    assert_eq!(follow.last().unwrap()["reason"], "max_events", "{follow:?}");

    let listed = env.ok(&[
        "debug",
        "logpoint",
        "list",
        "--backend",
        "jdwp",
        "--owner",
        "agent",
    ]);
    assert_eq!(listed["logpoints"][0]["id"], id.as_str());
    let (mismatch, _) = env.run(&[
        "debug",
        "logpoint",
        "remove",
        "--backend",
        "jdwp",
        "--id",
        &id,
        "--owner",
        "other",
    ]);
    assert_eq!(mismatch["code"], "logpoint_owner_mismatch", "{mismatch}");
    let cleared = env.ok(&[
        "debug",
        "logpoint",
        "clear",
        "--backend",
        "jdwp",
        "--owner",
        "agent",
    ]);
    assert_eq!(cleared["removed"], 1);
}

#[test]
fn breakpoint_lifecycle_through_the_cli() {
    let env = Env::new();
    env.ok(&["debug", "attach", "--backend", "jdwp", "--pid", "4242"]);
    let created = env.ok(&[
        "debug",
        "break",
        "line",
        "--backend",
        "jdwp",
        "--file",
        "MainActivity.kt",
        "--line",
        "31",
        "--condition",
        "count > 100",
    ]);
    assert_eq!(created["created"], true, "{created}");
    assert_eq!(created["breakpoint"]["condition"], "count > 100");
    let id = created["breakpoint"]["id"].as_str().unwrap().to_string();
    // Idempotent per file:line; --clear-condition on the repeat clears it.
    let again = env.ok(&[
        "debug",
        "break",
        "line",
        "--backend",
        "jdwp",
        "--file",
        "MainActivity.kt",
        "--line",
        "31",
        "--clear-condition",
    ]);
    assert_eq!(again["created"], false, "{again}");
    assert_eq!(again["breakpoint"]["id"], id.as_str());
    assert!(again["breakpoint"]["condition"].is_null(), "{again}");

    let (invalid, _) = env.run(&[
        "debug",
        "break",
        "update",
        "--backend",
        "jdwp",
        "--id",
        &id,
        "--condition",
        "f()",
    ]);
    assert_eq!(invalid["code"], "debug_expression_invalid", "{invalid}");
    let disabled = env.ok(&[
        "debug",
        "break",
        "update",
        "--backend",
        "jdwp",
        "--id",
        &id,
        "--enabled",
        "false",
        "--pass-count",
        "2",
        "--suspend",
        "thread",
    ]);
    assert_eq!(disabled["breakpoint"]["enabled"], false, "{disabled}");
    assert_eq!(disabled["breakpoint"]["pass_count"], 2);
    assert_eq!(disabled["breakpoint"]["suspend_policy"], "THREAD");
    env.ok(&["debug", "break", "remove", "--backend", "jdwp", "--id", &id]);

    // continue-until arms a temporary breakpoint and removes it again.
    let vm = env.vm.clone();
    let hitter = std::thread::spawn(move || {
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(50));
            if vm.hit_breakpoint(5) > 0 {
                return true;
            }
        }
        false
    });
    let reached = env.ok(&[
        "debug",
        "continue-until",
        "--backend",
        "jdwp",
        "--file",
        "MainActivity.kt",
        "--line",
        "31",
        "--timeout-ms",
        "8000",
    ]);
    assert!(hitter.join().unwrap());
    assert_eq!(reached["matched"], true, "{reached}");
    assert_eq!(reached["temporary_breakpoint"], true);
    let listed = env.ok(&["debug", "breakpoints", "--backend", "jdwp"]);
    assert!(
        listed["breakpoints"].as_array().unwrap().is_empty(),
        "{listed}"
    );
    let (condition_only, _) = env.run(&[
        "debug",
        "continue-until",
        "--backend",
        "jdwp",
        "--condition",
        "count > 1",
    ]);
    assert_eq!(
        condition_only["code"], "unsupported_by_backend",
        "{condition_only}"
    );
}
