//! "Application Not Responding" for sessions attached to a running app.
//!
//! Device findings: a stopped app that was attached while running still has
//! ANR timers, and pending input raises the ANR dialog ~15 s later. Nothing
//! on the device suppresses that safely (`hide_error_dialogs` makes the
//! system kill the app; "Wait" only lasts until the next input; `am monitor`
//! can hang system_server; `isDebugging` cannot be set on a running
//! process). So the daemon only *detects* it, cheaply and without touching
//! the app's UI thread, and points at `debug attach --relaunch`, which
//! restarts the app under the debugger (no ANR timers at all). Nothing is
//! ever tapped.

use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use super::session::Session;

/// How often a suspended attach-to-running session is checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(2);

/// Read-only system_server dumps, filtered on the device: ANR dialog
/// windows, and the process records' `notResponding` state.
pub fn probe_command(package: Option<&str>) -> String {
    let processes = match package {
        Some(package) => format!(
            "dumpsys activity processes {}",
            crate::config::quote_device_shell_arg(package)
        ),
        None => "dumpsys activity processes".to_string(),
    };
    format!(
        "dumpsys window windows | grep -i 'not responding'; echo ---; {processes} | grep -E 'ProcessRecord[{{]|notResponding'"
    )
}

/// What one probe saw for the debugged process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnrObservation {
    /// An "Application Not Responding: <app>" window is showing.
    pub dialog: bool,
    /// ActivityManager marks the process `notResponding=true`.
    pub not_responding: bool,
}

impl AnrObservation {
    pub fn any(self) -> bool {
        self.dialog || self.not_responding
    }
}

/// Parse [`probe_command`]'s output for `pid` / `package`.
pub fn parse_probe(text: &str, pid: u32, package: Option<&str>) -> AnrObservation {
    let mut observation = AnrObservation::default();
    let mut current_pid: Option<u32> = None;
    for line in text.lines() {
        if let Some(rest) = line
            .find("Not Responding: ")
            .map(|at| &line[at + "Not Responding: ".len()..])
        {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == '}')
                .next()
                .unwrap_or_default();
            if package.is_some_and(|package| {
                name == package
                    || name
                        .strip_prefix(package)
                        .is_some_and(|r| r.starts_with(':'))
            }) {
                observation.dialog = true;
            }
        }
        if let Some(at) = line.find("ProcessRecord{") {
            // `ProcessRecord{9f3a1b2 4242:io.example.app/u0a123}`
            current_pid = line[at + "ProcessRecord{".len()..]
                .split_whitespace()
                .nth(1)
                .and_then(|record| record.split(':').next())
                .and_then(|value| value.parse().ok());
        }
        if line.contains("notResponding=true") && current_pid == Some(pid) {
            observation.not_responding = true;
        }
    }
    observation
}

/// Per-session detection state.
#[derive(Debug, Default)]
pub struct AnrState {
    /// The suspension (`at`) the observations belong to.
    suspension_at: Option<f64>,
    checking: bool,
    last_check: Option<Instant>,
    checks: u64,
    observation: AnrObservation,
    since: Option<f64>,
    checked_at: Option<f64>,
}

/// The command to point at for a long, ANR-free inspection.
pub fn relaunch_action(package: Option<&str>) -> String {
    format!(
        "shadowdroid debug attach --relaunch --backend jdwp --package {}",
        package.map_or_else(|| "<pkg>".to_string(), crate::events::shell_token)
    )
}

impl Session {
    /// Whether a probe is due: attached while running, suspended, and the
    /// last check is [`CHECK_EVERY`] old. Marks the check as running.
    pub fn begin_anr_check(&self) -> bool {
        if self.info.launched_under_debugger {
            return false;
        }
        let Some(suspension) = self.suspension() else {
            self.reset_anr(None);
            return false;
        };
        let mut state = self.anr.lock().expect("anr lock");
        if state.suspension_at != Some(suspension.at) {
            *state = AnrState {
                suspension_at: Some(suspension.at),
                ..AnrState::default()
            };
        }
        if state.checking
            || state
                .last_check
                .is_some_and(|at| at.elapsed() < CHECK_EVERY)
        {
            return false;
        }
        state.checking = true;
        true
    }

    /// Record a probe result (for the suspension it was started under).
    pub fn record_anr(&self, observation: AnrObservation) {
        let at = self.suspension().map(|s| s.at);
        let mut state = self.anr.lock().expect("anr lock");
        state.checking = false;
        state.last_check = Some(Instant::now());
        if at.is_none() || state.suspension_at != at {
            return;
        }
        state.checks += 1;
        state.checked_at = Some(crate::events::now_ts());
        if observation.any() && !state.observation.any() {
            state.since = Some(crate::events::now_ts());
        }
        state.observation = observation;
    }

    fn reset_anr(&self, at: Option<f64>) {
        let mut state = self.anr.lock().expect("anr lock");
        if state.suspension_at != at {
            *state = AnrState {
                suspension_at: at,
                ..AnrState::default()
            };
        }
    }

    /// `anr: {dialog, not_responding, since, checked_at}` while the current
    /// stop shows an ANR; `None` otherwise.
    pub fn anr_report(&self) -> Option<Json> {
        let at = self.suspension()?.at;
        let state = self.anr.lock().expect("anr lock");
        (state.suspension_at == Some(at) && state.observation.any()).then(|| {
            json!({
                "dialog": state.observation.dialog,
                "not_responding": state.observation.not_responding,
                "since": state.since,
                "checked_at": state.checked_at,
                "checks": state.checks,
            })
        })
    }

    /// The warning that goes with [`Session::anr_report`].
    pub fn anr_warning(&self) -> String {
        format!(
            "Android reports {} not responding while it is suspended; do not tap the dialog's buttons from a script (Close App kills it). Resume soon, or restart it under the debugger for a long inspection: {}",
            self.info.package.as_deref().unwrap_or("the app"),
            relaunch_action(self.info.package.as_deref())
        )
    }

    /// Add `anr`, a warning, and the relaunch follow-up to a reply for this
    /// session (status/stack/variables), when the stop shows an ANR or has
    /// lasted long enough to risk one.
    pub fn annotate_anr(&self, reply: &mut Json) {
        if !reply.is_object() {
            return;
        }
        let relaunch = relaunch_action(self.info.package.as_deref());
        if let Some(anr) = self.anr_report() {
            reply["anr"] = anr;
            let warning = self.anr_warning();
            reply["warning"] = match reply["warning"].as_str() {
                Some(existing) if !existing.is_empty() => json!(format!("{warning}; {existing}")),
                _ => json!(warning),
            };
        } else if !self.risks_anr() {
            return;
        }
        let actions = reply["next_actions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !actions
            .iter()
            .any(|a| a.as_str() == Some(relaunch.as_str()))
        {
            let mut actions = actions;
            actions.insert(0, json!(relaunch));
            actions.push(json!("shadowdroid debug resume --backend jdwp"));
            reply["next_actions"] = json!(actions);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOWS: &str = "  Window #7 Window{4e1f2a u0 Application Not Responding: io.example.app}:\n    mDisplayId=0 rootTaskId=1 mSession=Session{...}\n";
    const PROCESSES: &str = "  *APP* UID 10123 ProcessRecord{9f3a1b2 4242:io.example.app/u0a123}\n    notResponding=true\n  *APP* UID 10124 ProcessRecord{1c2d3e4 4300:io.example.other/u0a124}\n";

    #[test]
    fn a_dialog_and_a_not_responding_record_are_found_for_our_process() {
        let text = format!("{WINDOWS}---\n{PROCESSES}");
        let seen = parse_probe(&text, 4242, Some("io.example.app"));
        assert_eq!(
            seen,
            AnrObservation {
                dialog: true,
                not_responding: true
            }
        );
        assert!(seen.any());
        // Another app's ANR is not ours.
        let other = parse_probe(&text, 4300, Some("io.example.other"));
        assert!(!other.dialog);
        assert!(!other.not_responding);
    }

    #[test]
    fn subprocesses_match_their_package_and_quiet_dumps_match_nothing() {
        let text = "Window{1 u0 Application Not Responding: io.example.app:remote}\n---\n";
        assert!(parse_probe(text, 1, Some("io.example.app")).dialog);
        assert!(!parse_probe(text, 1, Some("io.example")).dialog);
        let quiet = "---\n  *APP* UID 10123 ProcessRecord{9f3a1b2 4242:io.example.app/u0a123}\n";
        assert_eq!(
            parse_probe(quiet, 4242, Some("io.example.app")),
            AnrObservation::default()
        );
        assert_eq!(parse_probe("", 4242, None), AnrObservation::default());
        // `notResponding=true` belongs to the record above it only.
        let mixed = "ProcessRecord{a 4300:io.example.other/u0a1}\n notResponding=true\nProcessRecord{b 4242:io.example.app/u0a2}\n";
        assert!(!parse_probe(mixed, 4242, Some("io.example.app")).not_responding);
    }

    #[test]
    fn the_probe_reads_only_system_server_dumps() {
        let command = probe_command(Some("io.example.app"));
        assert!(command.starts_with("dumpsys window windows"));
        assert!(command.contains("dumpsys activity processes 'io.example.app'"));
        assert!(!command.contains("input"));
        assert!(!command.contains("am "));
        assert_eq!(
            relaunch_action(Some("io.example.app")),
            "shadowdroid debug attach --relaunch --backend jdwp --package io.example.app"
        );
    }
}
