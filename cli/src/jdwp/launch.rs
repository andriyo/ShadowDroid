//! Launch-time debugging (design §5.1): start the app under
//! `am set-debug-app -w`, find the new process on the `jdwp` service, attach
//! while it waits in `Debug.waitForDebugger`, and clear the debug-app
//! setting on every exit path.
//!
//! The spike measured the window: the main thread sleeps in
//! `waitForDebugger` (not JDWP-suspended, so no VirtualMachine.Resume) and
//! returns about 1.5 s after attach once debugger traffic goes idle. The
//! attach callback therefore installs breakpoints before it returns.
//!
//! Device operations go through [`LaunchHost`] so the orchestration is
//! tested without a device.

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value as Json, json};

use super::transport;

/// Device operations a launch-time attach needs.
pub trait LaunchHost {
    fn shell(&self, command: &str) -> impl Future<Output = Result<String>> + Send;
    fn debuggable_pids(&self) -> impl Future<Output = Result<Vec<u32>>> + Send;
    fn process_names(
        &self,
        pids: &[u32],
    ) -> impl Future<Output = Result<Vec<(u32, String)>>> + Send;
}

/// The real device, through the in-tree ADB client.
pub struct AdbHost {
    pub serial: String,
}

impl LaunchHost for AdbHost {
    async fn shell(&self, command: &str) -> Result<String> {
        transport::shell_line(&self.serial, command).await
    }

    async fn debuggable_pids(&self) -> Result<Vec<u32>> {
        transport::debuggable_pids(&self.serial, Duration::from_secs(3)).await
    }

    async fn process_names(&self, pids: &[u32]) -> Result<Vec<(u32, String)>> {
        transport::process_names(&self.serial, pids).await
    }
}

/// Pids of `package`'s main process (exact process name).
async fn package_pids<H: LaunchHost>(host: &H, package: &str) -> Result<Vec<u32>> {
    let pids = host.debuggable_pids().await?;
    let names = host.process_names(&pids).await?;
    Ok(names
        .into_iter()
        .filter(|(_, name)| name == package)
        .map(|(pid, _)| pid)
        .collect())
}

/// `monkey` launch of the app's launcher activity. It returns at once, which
/// matters here: `am start -W` would block while the app waits for us.
pub async fn monkey_launch<H: LaunchHost>(host: &H, package: &str) -> Result<Json> {
    let quoted = crate::config::quote_device_shell_arg(package);
    let output = host
        .shell(&format!(
            "monkey -p {quoted} -c android.intent.category.LAUNCHER 1"
        ))
        .await?;
    if output.contains("aborted") || output.contains("No activities found") {
        bail!("could not launch {package}: {}", output.trim());
    }
    Ok(json!({"launcher": "monkey", "output": output.trim()}))
}

/// Start `package` for a launch-time attach without waiting (`am start` with
/// no `-W` returns while the app sits in `waitForDebugger`).
///
/// The activity is `activity` when given (`.Main`, `pkg/.Main`, or a class
/// name). Otherwise the app's only launcher activity; with several, the root
/// of the app's most recent task if that is one of them, else the first in
/// manifest order, with a warning. `monkey -c LAUNCHER` is not used: with
/// several launcher activities it picks one at random, so a `--break` in the
/// main activity would hit on one run and never on the next.
pub async fn launcher_launch<H: LaunchHost>(
    host: &H,
    package: &str,
    activity: Option<&str>,
) -> Result<Json> {
    let (component, chosen_by, candidates) = match activity {
        Some(activity) => (component_name(package, activity), "explicit", Vec::new()),
        None => {
            let quoted = crate::config::quote_device_shell_arg(package);
            let listing = host
                .shell(&format!(
                    "cmd package query-activities --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER {quoted}"
                ))
                .await?;
            let candidates = launcher_components(&listing, package);
            match candidates.as_slice() {
                [] => return monkey_launch(host, package).await,
                [only] => (only.clone(), "only_launcher", candidates.clone()),
                [first, ..] => {
                    let recents = host
                        .shell("dumpsys activity recents")
                        .await
                        .unwrap_or_default();
                    match recent_root(&recents, package)
                        .filter(|root| candidates.iter().any(|c| same_component(c, root)))
                    {
                        Some(root) => (root, "recent_task_root", candidates.clone()),
                        None => (first.clone(), "first_launcher", candidates.clone()),
                    }
                }
            }
        }
    };
    let output = host
        .shell(&format!(
            "am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n {}",
            crate::config::quote_device_shell_arg(&component)
        ))
        .await?;
    if output.contains("Error") || output.contains("Exception") {
        bail!("could not launch {component}: {}", output.trim());
    }
    let mut value = json!({
        "launcher": "am_start",
        "component": component,
        "chosen_by": chosen_by,
        "output": output.trim(),
    });
    if candidates.len() > 1 {
        value["candidates"] = json!(candidates);
        value["warning"] = json!(format!(
            "{package} has {} launcher activities; started {component} ({chosen_by}); pass --launch-activity to choose",
            candidates.len()
        ));
    }
    Ok(value)
}

/// `pkg/.Main` from `.Main`, `Main`, `pkg/.Main`, or `pkg.Main`.
fn component_name(package: &str, activity: &str) -> String {
    if activity.contains('/') {
        activity.to_string()
    } else if activity.starts_with('.') || activity.contains('.') {
        format!("{package}/{activity}")
    } else {
        format!("{package}/.{activity}")
    }
}

/// Launcher components of `package` from `cmd package query-activities --brief`.
fn launcher_components(listing: &str, package: &str) -> Vec<String> {
    let prefix = format!("{package}/");
    listing
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(&prefix))
        .map(str::to_string)
        .collect()
}

/// `realActivity` of the most recent task rooted in `package`.
fn recent_root(recents: &str, package: &str) -> Option<String> {
    let needle = format!("realActivity={{{package}/");
    recents.split("* Recent #").find_map(|task| {
        let start = task.find(&needle)? + "realActivity={".len();
        let end = task[start..].find('}')?;
        Some(task[start..start + end].to_string())
    })
}

/// `pkg/.Main` and `pkg/pkg.Main` name the same activity.
fn same_component(a: &str, b: &str) -> bool {
    let full = |c: &str| match c.split_once('/') {
        Some((pkg, class)) if class.starts_with('.') => format!("{pkg}/{pkg}{class}"),
        _ => c.to_string(),
    };
    full(a) == full(b)
}

/// The persistent debug-app setting found before the launch (Developer
/// options "Select debug app" / "Wait for debugger", or `am set-debug-app
/// --persistent`).
///
/// Our `am set-debug-app -w` is one-off (not persistent): ActivityManager
/// keeps the previous setting aside and puts it back by itself when the
/// launched app attaches, so a successful launch restores nothing. Only a
/// launch that never started the app leaves our one-off setting behind;
/// `am clear-debug-app` removes it but also clears a persistent setting, so
/// that one is written back to `Settings.Global` (`debug_app`,
/// `wait_for_debugger`) directly, which ActivityManager reads the next time
/// it loads its settings. Never `am set-debug-app --persistent <prev>`:
/// setting a debug app force-stops that app.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreviousDebugApp {
    pub package: Option<String>,
    pub wait_for_debugger: bool,
}

async fn read_debug_app<H: LaunchHost>(host: &H) -> PreviousDebugApp {
    let get = |key: &'static str| async move {
        host.shell(&format!("settings get global {key}"))
            .await
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value != "null")
    };
    PreviousDebugApp {
        package: get("debug_app").await,
        wait_for_debugger: get("wait_for_debugger").await.as_deref() == Some("1"),
    }
}

/// After a launch that did not start the app: clear our one-off setting,
/// then write a previous persistent setting back to the settings store.
async fn clear_one_off_debug_app<H: LaunchHost>(
    host: &H,
    previous: &PreviousDebugApp,
    steps: &mut Vec<Json>,
) {
    let cleared = host.shell("am clear-debug-app").await;
    steps.push(json!({
        "step": "clear_debug_app",
        "ok": cleared.is_ok(),
        "command": "am clear-debug-app",
        "error": cleared.as_ref().err().map(|e| e.to_string()),
    }));
    let Some(package) = &previous.package else {
        return;
    };
    let commands = [
        format!(
            "settings put global debug_app {}",
            crate::config::quote_device_shell_arg(package)
        ),
        format!(
            "settings put global wait_for_debugger {}",
            u8::from(previous.wait_for_debugger)
        ),
    ];
    let mut errors = Vec::new();
    for command in &commands {
        if let Err(error) = host.shell(command).await {
            errors.push(error.to_string());
        }
    }
    steps.push(json!({
        "step": "restore_debug_app",
        "ok": errors.is_empty(),
        "commands": commands,
        "note": "written to Settings.Global; ActivityManager picks it up the next time it reads its settings (not through am set-debug-app, which force-stops the app)",
        "errors": errors,
    }));
}

/// After a launch that started the app: ActivityManager already reverted
/// our one-off setting to whatever was there before.
fn note_debug_app_reverted(previous: &PreviousDebugApp, steps: &mut Vec<Json>) {
    steps.push(json!({
        "step": "debug_app_reverted",
        "ok": true,
        "note": "the one-off set-debug-app reverts when the launched app starts; nothing to restore",
        "previous_debug_app": previous.package,
    }));
}

/// Resolves when the CLI is asked to stop (Ctrl-C, or SIGTERM on unix).
/// Never resolves if the handlers cannot be installed.
pub async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Run `attach` against a fresh process of `package` started by `launch`
/// under `am set-debug-app -w`. Steps are appended to `steps` (also on
/// failure) for the reply.
///
/// Every exit path, including `interrupt` resolving (Ctrl-C/SIGTERM),
/// restores the debug-app setting; an interrupted launch also force-stops
/// the new process, which would otherwise wait for a debugger forever.
#[allow(clippy::too_many_arguments)]
pub async fn launch_for_debug<H, L, LFut, A, AFut, T, I>(
    host: &H,
    package: &str,
    timeout: Duration,
    poll: Duration,
    steps: &mut Vec<Json>,
    launch: L,
    attach: A,
    interrupt: I,
) -> Result<T>
where
    H: LaunchHost,
    L: FnOnce() -> LFut,
    LFut: Future<Output = Result<Json>>,
    A: FnOnce(u32) -> AFut,
    AFut: Future<Output = Result<T>>,
    I: Future<Output = ()>,
{
    let quoted = crate::config::quote_device_shell_arg(package);
    let previous = read_debug_app(host).await;
    let before = package_pids(host, package).await.unwrap_or_default();
    if !before.is_empty() {
        // The debug-app setting only applies to a new process.
        host.shell(&format!("am force-stop {quoted}")).await?;
        steps.push(json!({"step": "force_stop", "ok": true, "previous_pids": before}));
    }
    let set = host.shell(&format!("am set-debug-app -w {quoted}")).await?;
    if set.contains("Error") || set.contains("Exception") {
        bail!("am set-debug-app failed: {}", set.trim());
    }
    steps.push(json!({
        "step": "set_debug_app",
        "ok": true,
        "wait": true,
        "persistent": false,
        "previous_debug_app": previous.package,
        "previous_wait_for_debugger": previous.wait_for_debugger,
    }));

    let mut work_steps = Vec::new();
    let outcome = {
        let work = async {
            let launched = launch().await?;
            work_steps.push(json!({"step": "launch", "ok": true, "result": launched}));
            let pid = wait_for_new_pid(host, package, &before, timeout, poll).await?;
            work_steps.push(json!({"step": "process_started", "ok": true, "pid": pid}));
            attach(pid).await
        };
        tokio::select! {
            result = work => Some(result),
            () = interrupt => None,
        }
    };
    steps.append(&mut work_steps);

    // Every exit path leaves the setting as it was: a leftover `-w` would
    // freeze the next launch. Success: the app started, so ActivityManager
    // already reverted our one-off setting. Failure or interrupt: clear it
    // (harmless when it already reverted) and write back a persistent one.
    let outcome = match outcome {
        Some(Ok(attached)) => {
            note_debug_app_reverted(&previous, steps);
            return Ok(attached);
        }
        failed => {
            clear_one_off_debug_app(host, &previous, steps).await;
            failed
        }
    };
    // Failed or interrupted before a debugger took the process: a process
    // already in waitForDebugger keeps waiting after clear-debug-app (the
    // setting is read once, at start), so stop it.
    let started: Vec<u32> = package_pids(host, package)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect();
    let interrupted = outcome.is_none();
    if interrupted {
        steps.push(json!({"step": "interrupted", "ok": false}));
    }
    if !started.is_empty() {
        let stopped = host.shell(&format!("am force-stop {quoted}")).await;
        steps.push(json!({
            "step": "force_stop_waiting_process",
            "ok": stopped.is_ok(),
            "pids": started,
        }));
    }
    match outcome {
        Some(result) => result,
        None => Err(crate::diagnostic::DiagnosticError::new(
            "debug_launch_interrupted",
            "debugger",
            format!("the launch of {package} under the debugger was interrupted"),
        )
        .detail(json!({"backend": "jdwp", "package": package, "launch_steps": steps}))
        .next_actions(["shadowdroid debug attach --backend jdwp --wait-for-launch --package <pkg>"])
        .into()),
    }
}

async fn wait_for_new_pid<H: LaunchHost>(
    host: &H,
    package: &str,
    before: &[u32],
    timeout: Duration,
    poll: Duration,
) -> Result<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // A process shows `<pre-initialized>` until bindApplication names it;
        // the name is set before `waitForDebugger`, so keep polling.
        if let Some(pid) = package_pids(host, package)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|pid| !before.contains(pid))
        {
            return Ok(pid);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(crate::diagnostic::DiagnosticError::new(
                "debugger_timeout",
                "debugger",
                format!(
                    "{package} did not start a debuggable process within {} ms",
                    timeout.as_millis()
                ),
            )
            .retryable(true)
            .detail(json!({"backend": "jdwp", "command": "wait_for_launch", "package": package}))
            .next_actions(["check that the app is installed and debuggable, then retry"])
            .into());
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A device whose app process appears after `launch` and N polls.
    struct FakeHost {
        log: Mutex<Vec<String>>,
        running: Mutex<Vec<(u32, String)>>,
        polls_until_up: Mutex<u32>,
        launched: Mutex<bool>,
        /// `(command prefix, reply)` for shell commands that print something.
        replies: Mutex<Vec<(String, String)>>,
    }

    impl FakeHost {
        fn new(running: Vec<(u32, &str)>) -> Self {
            Self {
                log: Mutex::new(Vec::new()),
                running: Mutex::new(running.into_iter().map(|(p, n)| (p, n.into())).collect()),
                polls_until_up: Mutex::new(2),
                launched: Mutex::new(false),
                replies: Mutex::new(Vec::new()),
            }
        }

        fn reply(self, prefix: &str, output: &str) -> Self {
            self.replies
                .lock()
                .unwrap()
                .push((prefix.to_string(), output.to_string()));
            self
        }

        fn commands(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl LaunchHost for FakeHost {
        async fn shell(&self, command: &str) -> Result<String> {
            self.log.lock().unwrap().push(command.to_string());
            if command.starts_with("am force-stop") {
                self.running.lock().unwrap().clear();
            }
            Ok(self
                .replies
                .lock()
                .unwrap()
                .iter()
                .find(|(prefix, _)| command.starts_with(prefix.as_str()))
                .map(|(_, reply)| reply.clone())
                .unwrap_or_default())
        }

        async fn debuggable_pids(&self) -> Result<Vec<u32>> {
            if *self.launched.lock().unwrap() {
                let mut left = self.polls_until_up.lock().unwrap();
                if *left == 0 {
                    let mut running = self.running.lock().unwrap();
                    if !running.iter().any(|(pid, _)| *pid == 900) {
                        running.push((900, "io.example.app".into()));
                    }
                } else {
                    *left -= 1;
                }
            }
            Ok(self
                .running
                .lock()
                .unwrap()
                .iter()
                .map(|(p, _)| *p)
                .collect())
        }

        async fn process_names(&self, pids: &[u32]) -> Result<Vec<(u32, String)>> {
            Ok(self
                .running
                .lock()
                .unwrap()
                .iter()
                .filter(|(pid, _)| pids.contains(pid))
                .cloned()
                .collect())
        }
    }

    const FAST: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn restarts_launches_attaches_and_always_clears() {
        let host = FakeHost::new(vec![
            (100, "io.example.app"),
            (101, "io.example.app:remote"),
        ]);
        let mut steps = Vec::new();
        let attached = launch_for_debug(
            &host,
            "io.example.app",
            Duration::from_secs(5),
            FAST,
            &mut steps,
            || async {
                *host.launched.lock().unwrap() = true;
                Ok(json!({"launcher": "test"}))
            },
            |pid| async move { Ok(pid) },
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(attached, 900);
        assert_eq!(
            host.commands(),
            [
                "settings get global debug_app",
                "settings get global wait_for_debugger",
                "am force-stop 'io.example.app'",
                "am set-debug-app -w 'io.example.app'",
            ],
            "the one-off setting reverts by itself once the app starts"
        );
        let names: Vec<_> = steps.iter().map(|s| s["step"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "force_stop",
                "set_debug_app",
                "launch",
                "process_started",
                "debug_app_reverted"
            ]
        );
    }

    #[tokio::test]
    async fn clears_the_debug_app_when_attach_or_the_wait_fails() {
        let host = FakeHost::new(vec![]);
        let mut steps = Vec::new();
        let error = launch_for_debug(
            &host,
            "io.example.app",
            Duration::from_secs(5),
            FAST,
            &mut steps,
            || async {
                *host.launched.lock().unwrap() = true;
                Ok(json!({}))
            },
            |_pid| async { Err::<(), _>(anyhow::anyhow!("attach failed")) },
            std::future::pending(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("attach failed"));
        // The launched process is still waiting for a debugger: stopped
        // after the setting is cleared.
        let commands = host.commands();
        let n = commands.len();
        assert_eq!(commands[n - 2], "am clear-debug-app", "{commands:?}");
        assert_eq!(
            commands[n - 1],
            "am force-stop 'io.example.app'",
            "{commands:?}"
        );
        assert_eq!(steps.last().unwrap()["step"], "force_stop_waiting_process");

        // The process never appears: a typed timeout, still cleared.
        let host = FakeHost::new(vec![]);
        let mut steps = Vec::new();
        let error = launch_for_debug(
            &host,
            "io.example.app",
            Duration::from_millis(20),
            FAST,
            &mut steps,
            || async { Ok(json!({})) },
            |pid| async move { Ok(pid) },
            std::future::pending(),
        )
        .await
        .unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.code, "debugger_timeout");
        assert_eq!(host.commands().last().unwrap(), "am clear-debug-app");
        assert_eq!(steps.last().unwrap()["step"], "clear_debug_app");
    }

    #[tokio::test]
    async fn a_failed_launch_is_reported_and_still_cleared() {
        struct NoActivity(FakeHost);
        impl LaunchHost for NoActivity {
            async fn shell(&self, command: &str) -> Result<String> {
                self.0.shell(command).await?;
                Ok(if command.starts_with("monkey") {
                    "** No activities found to run, monkey aborted.".into()
                } else {
                    String::new()
                })
            }
            async fn debuggable_pids(&self) -> Result<Vec<u32>> {
                self.0.debuggable_pids().await
            }
            async fn process_names(&self, pids: &[u32]) -> Result<Vec<(u32, String)>> {
                self.0.process_names(pids).await
            }
        }
        let host = NoActivity(FakeHost::new(vec![]));
        let mut steps = Vec::new();
        let error = launch_for_debug(
            &host,
            "io.example.app",
            Duration::from_secs(1),
            FAST,
            &mut steps,
            || monkey_launch(&host, "io.example.app"),
            |pid| async move { Ok(pid) },
            std::future::pending(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("could not launch"), "{error}");
        assert_eq!(host.0.commands().last().unwrap(), "am clear-debug-app");
    }

    #[tokio::test]
    async fn a_previous_debug_app_setting_is_left_alone_on_success() {
        let host = FakeHost::new(vec![])
            .reply("settings get global debug_app", "com.other.app\n")
            .reply("settings get global wait_for_debugger", "1\n");
        let mut steps = Vec::new();
        launch_for_debug(
            &host,
            "io.example.app",
            Duration::from_secs(5),
            FAST,
            &mut steps,
            || async {
                *host.launched.lock().unwrap() = true;
                Ok(json!({}))
            },
            |pid| async move { Ok(pid) },
            std::future::pending(),
        )
        .await
        .unwrap();
        let commands = host.commands();
        // `am set-debug-app --persistent <prev>` would force-stop com.other.app.
        assert!(
            !commands.iter().any(|c| c.contains("com.other.app")),
            "{commands:?}"
        );
        assert!(!commands.iter().any(|c| c == "am clear-debug-app"));
        assert_eq!(steps[0]["previous_debug_app"], "com.other.app");
        assert_eq!(steps[0]["previous_wait_for_debugger"], true);
        assert_eq!(steps.last().unwrap()["step"], "debug_app_reverted");
    }

    #[tokio::test]
    async fn a_failed_launch_writes_a_persistent_setting_back_without_am() {
        let host = FakeHost::new(vec![])
            .reply("settings get global debug_app", "com.other.app\n")
            .reply("settings get global wait_for_debugger", "1\n");
        let mut steps = Vec::new();
        launch_for_debug(
            &host,
            "io.example.app",
            Duration::from_millis(20),
            FAST,
            &mut steps,
            || async { Ok(json!({})) },
            |pid| async move { Ok(pid) },
            std::future::pending(),
        )
        .await
        .unwrap_err();
        let commands = host.commands();
        let n = commands.len();
        assert_eq!(
            &commands[n - 3..],
            [
                "am clear-debug-app",
                "settings put global debug_app 'com.other.app'",
                "settings put global wait_for_debugger 1",
            ],
            "{commands:?}"
        );
        assert!(!commands.iter().any(|c| c.contains("--persistent")));
        let restore = steps.last().unwrap();
        assert_eq!(restore["step"], "restore_debug_app");
        assert!(
            restore["note"]
                .as_str()
                .unwrap()
                .contains("Settings.Global")
        );
    }

    #[tokio::test]
    async fn an_interrupted_launch_clears_and_stops_the_waiting_process() {
        let host = FakeHost::new(vec![]);
        *host.polls_until_up.lock().unwrap() = 0;
        let mut steps = Vec::new();
        let error = launch_for_debug(
            &host,
            "io.example.app",
            Duration::from_secs(5),
            FAST,
            &mut steps,
            || async {
                *host.launched.lock().unwrap() = true;
                Ok(json!({}))
            },
            // The attach never completes; Ctrl-C arrives meanwhile.
            |_pid| std::future::pending::<Result<u32>>(),
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await
        .unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.code, "debug_launch_interrupted");
        let commands = host.commands();
        let clear = commands
            .iter()
            .position(|c| c == "am clear-debug-app")
            .unwrap();
        let stop = commands
            .iter()
            .rposition(|c| c == "am force-stop 'io.example.app'")
            .unwrap();
        assert!(clear < stop, "{commands:?}");
        let names: Vec<_> = steps.iter().map(|s| s["step"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "set_debug_app",
                "launch",
                "process_started",
                "clear_debug_app",
                "interrupted",
                "force_stop_waiting_process"
            ]
        );
    }

    #[tokio::test]
    async fn the_launch_activity_is_chosen_deterministically() {
        const LISTING: &str = "2 activities found:\n  Activity #0:\n    priority=0\n    io.example.app/.AltLauncher\n  Activity #1:\n    priority=0\n    io.example.app/.MainActivity\n";
        // Several launchers, the last task was rooted in MainActivity.
        let host = FakeHost::new(vec![])
            .reply("cmd package query-activities", LISTING)
            .reply(
                "dumpsys activity recents",
                "* Recent #0: Task{1 #9 type=standard A=10341:io.example.app}\n    realActivity={io.example.app/io.example.app.MainActivity}\n",
            );
        let value = launcher_launch(&host, "io.example.app", None)
            .await
            .unwrap();
        assert_eq!(
            value["component"],
            "io.example.app/io.example.app.MainActivity"
        );
        assert_eq!(value["chosen_by"], "recent_task_root");
        assert!(
            value["warning"]
                .as_str()
                .unwrap()
                .contains("--launch-activity")
        );
        assert!(
            host.commands()
                .last()
                .unwrap()
                .starts_with("am start -a android.intent.action.MAIN")
        );
        assert!(!host.commands().iter().any(|c| c.starts_with("monkey")));

        // No recent task: the first launcher in manifest order, never random.
        let host = FakeHost::new(vec![]).reply("cmd package query-activities", LISTING);
        let value = launcher_launch(&host, "io.example.app", None)
            .await
            .unwrap();
        assert_eq!(value["component"], "io.example.app/.AltLauncher");
        assert_eq!(value["chosen_by"], "first_launcher");

        // Explicit wins, in any spelling.
        let host = FakeHost::new(vec![]);
        let value = launcher_launch(&host, "io.example.app", Some(".MainActivity"))
            .await
            .unwrap();
        assert_eq!(value["component"], "io.example.app/.MainActivity");
        assert_eq!(value["chosen_by"], "explicit");
        assert_eq!(
            component_name("io.example.app", "MainActivity"),
            "io.example.app/.MainActivity"
        );
        assert_eq!(
            component_name("io.example.app", "io.example.app.Main"),
            "io.example.app/io.example.app.Main"
        );
    }
}
