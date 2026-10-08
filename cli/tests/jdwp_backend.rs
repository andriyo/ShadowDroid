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
            "package io.example.app\n\nclass MainActivity\n",
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
    let (error, _) = env.run(&["debug", "snapshot", "--backend", "jdwp"]);
    assert_eq!(error["code"], "unsupported_by_backend", "{error}");

    // Without --backend nothing reaches the JDWP registry or daemon: the
    // Studio bridge answers (here: is unreachable) exactly as before.
    let (studio, _) = env.run(&["debug", "sessions", "--studio-url", "http://127.0.0.1:9"]);
    assert_ne!(studio["backend"], "jdwp", "{studio}");
    assert!(!env.registry_dir().exists());
}
