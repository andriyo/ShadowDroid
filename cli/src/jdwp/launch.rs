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

/// Run `attach` against a fresh process of `package` started by `launch`
/// under `am set-debug-app -w`. Steps are appended to `steps` (also on
/// failure) for the reply.
pub async fn launch_for_debug<H, L, LFut, A, AFut, T>(
    host: &H,
    package: &str,
    timeout: Duration,
    poll: Duration,
    steps: &mut Vec<Json>,
    launch: L,
    attach: A,
) -> Result<T>
where
    H: LaunchHost,
    L: FnOnce() -> LFut,
    LFut: Future<Output = Result<Json>>,
    A: FnOnce(u32) -> AFut,
    AFut: Future<Output = Result<T>>,
{
    let quoted = crate::config::quote_device_shell_arg(package);
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
    steps.push(json!({"step": "set_debug_app", "ok": true, "wait": true, "persistent": false}));

    let outcome = async {
        let launched = launch().await?;
        steps.push(json!({"step": "launch", "ok": true, "result": launched}));
        let pid = wait_for_new_pid(host, package, &before, timeout, poll).await?;
        steps.push(json!({"step": "process_started", "ok": true, "pid": pid}));
        attach(pid).await
    }
    .await;

    // Every exit path: a leftover `-w` would freeze the next launch.
    let cleared = host.shell("am clear-debug-app").await;
    steps.push(json!({
        "step": "clear_debug_app",
        "ok": cleared.is_ok(),
        "error": cleared.as_ref().err().map(|e| e.to_string()),
    }));
    outcome
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
    }

    impl FakeHost {
        fn new(running: Vec<(u32, &str)>) -> Self {
            Self {
                log: Mutex::new(Vec::new()),
                running: Mutex::new(running.into_iter().map(|(p, n)| (p, n.into())).collect()),
                polls_until_up: Mutex::new(2),
                launched: Mutex::new(false),
            }
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
            Ok(String::new())
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
        )
        .await
        .unwrap();
        assert_eq!(attached, 900);
        assert_eq!(
            host.commands(),
            [
                "am force-stop 'io.example.app'",
                "am set-debug-app -w 'io.example.app'",
                "am clear-debug-app",
            ]
        );
        let names: Vec<_> = steps.iter().map(|s| s["step"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "force_stop",
                "set_debug_app",
                "launch",
                "process_started",
                "clear_debug_app"
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
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("attach failed"));
        assert_eq!(host.commands().last().unwrap(), "am clear-debug-app");
        assert!(!host.commands().iter().any(|c| c.contains("force-stop")));

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
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("could not launch"), "{error}");
        assert_eq!(host.0.commands().last().unwrap(), "am clear-debug-app");
    }
}
