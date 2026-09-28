//! What every fault kind does, needs, and changes: one table that drives
//! `fault kinds`, validation before a device is touched, conflict detection,
//! the command catalog's agent metadata, and effect contracts.

use serde::Serialize;

/// A state stays until `fault clear` (or `--duration-ms`); an action happens
/// once and has nothing to undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    State,
    Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Any device or emulator reachable over adb.
    Device,
    /// Needs the emulator console.
    Emulator,
    /// Needs the ShadowDroid proxy (`net start`) routing the app's traffic.
    Proxy,
}

#[derive(Debug, Clone, Serialize)]
pub struct KindInfo {
    pub kind: &'static str,
    pub category: &'static str,
    pub effect: Effect,
    pub scope: Scope,
    pub needs_app: bool,
    /// Changes something a user would lose or notice beyond the test run; on a
    /// physical device it needs `--allow-physical`.
    pub destructive: bool,
    /// Minimum Android API level.
    pub min_sdk: u32,
    pub summary: &'static str,
    /// What `fault clear` does; empty for actions.
    pub clear: &'static str,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub side_effects: &'static [&'static str],
    /// Device state the fault owns. Two active faults never own the same
    /// resource; `{app}` stands for the target package.
    #[serde(skip)]
    pub resources: &'static [&'static str],
}

const fn state(
    kind: &'static str,
    category: &'static str,
    scope: Scope,
    summary: &'static str,
    clear: &'static str,
    resources: &'static [&'static str],
) -> KindInfo {
    KindInfo {
        kind,
        category,
        effect: Effect::State,
        scope,
        needs_app: false,
        destructive: false,
        min_sdk: 26,
        summary,
        clear,
        side_effects: &[],
        resources,
    }
}

const fn action(
    kind: &'static str,
    category: &'static str,
    scope: Scope,
    summary: &'static str,
) -> KindInfo {
    KindInfo {
        kind,
        category,
        effect: Effect::Action,
        scope,
        needs_app: false,
        destructive: false,
        min_sdk: 26,
        summary,
        clear: "",
        side_effects: &[],
        resources: &[],
    }
}

impl KindInfo {
    const fn app(mut self) -> Self {
        self.needs_app = true;
        self
    }
    const fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
    const fn sdk(mut self, min_sdk: u32) -> Self {
        self.min_sdk = min_sdk;
        self
    }
    const fn side_effects(mut self, side_effects: &'static [&'static str]) -> Self {
        self.side_effects = side_effects;
        self
    }

    /// The resources this fault owns for `app`.
    pub fn resources_for(&self, app: Option<&str>) -> Vec<String> {
        self.resources
            .iter()
            .map(|resource| resource.replace("{app}", app.unwrap_or("")))
            .collect()
    }
}

use Scope::{Device, Emulator, Proxy};

pub static KINDS: &[KindInfo] = &[
    // ── app lifecycle ──────────────────────────────────────────────────
    action(
        "process-death",
        "lifecycle",
        Device,
        "Send the app to the background and kill its process the way Android reclaims memory; the task stays in recents so reopening restores saved state",
    )
    .app()
    .side_effects(&["presses HOME", "--relaunch reopens the app like tapping its icon"]),
    state(
        "dont-keep-activities",
        "lifecycle",
        Device,
        "Destroy every activity as soon as the user leaves it (Developer options > Don't keep activities)",
        "restores the previous Don't keep activities setting",
        &["always_finish_activities"],
    ),
    action(
        "low-memory",
        "lifecycle",
        Device,
        "Deliver an onTrimMemory level to the app (running-* levels need it in the foreground, the others in the background)",
    )
    .app(),
    state(
        "revoke-permission",
        "lifecycle",
        Device,
        "Revoke a granted runtime permission; Android kills the app when it does",
        "grants the permission again",
        &["permission:{app}"],
    )
    .app()
    .side_effects(&["Android kills the app's process on revoke and on re-grant"]),
    state(
        "orientation",
        "configuration",
        Device,
        "Lock the screen to a rotation (a configuration change for the foreground activity)",
        "restores auto-rotate and the previous rotation",
        &["rotation"],
    ),
    state(
        "night-mode",
        "configuration",
        Device,
        "Switch the system dark theme on or off",
        "restores the previous night mode",
        &["night_mode"],
    ),
    state(
        "font-scale",
        "configuration",
        Device,
        "Change the system font size",
        "restores the previous font scale",
        &["font_scale"],
    ),
    state(
        "display-size",
        "configuration",
        Device,
        "Override the display size and/or density, like a foldable unfolding or a different phone",
        "restores the previous size and density overrides",
        &["display_size"],
    ),
    state(
        "app-locale",
        "configuration",
        Device,
        "Set the app's own locale (per-app language), e.g. a right-to-left language",
        "restores the app's previous locales",
        &["app_locale:{app}"],
    )
    .app()
    .sdk(33),
    state(
        "split-screen",
        "configuration",
        Device,
        "Put the app's task in multi-window mode and shrink it to part of the screen, as split screen does",
        "resizes the task to the full screen and returns it to fullscreen mode",
        &["windowing:{app}"],
    )
    .app()
    .side_effects(&["the top activity receives a new intent (onNewIntent) on inject and on clear"]),
    action(
        "pip",
        "configuration",
        Device,
        "Leave the app the way a user does (HOME) so an app that supports it enters picture-in-picture; reports whether it did",
    )
    .app()
    .side_effects(&["presses HOME"]),
    // ── power ─────────────────────────────────────────────────────────
    state(
        "doze",
        "power",
        Device,
        "Force the device into deep Doze: jobs, alarms and network access are deferred",
        "leaves forced idle",
        &["deviceidle"],
    ),
    state(
        "standby-bucket",
        "power",
        Device,
        "Move the app to an App Standby bucket (rare or restricted limit background work)",
        "restores the app's previous bucket",
        &["standby_bucket:{app}"],
    )
    .app()
    .sdk(28),
    state(
        "battery",
        "power",
        Device,
        "Report the battery as unplugged, optionally at a low level and with Battery Saver on",
        "resets the battery reading and restores Battery Saver",
        &["battery"],
    ),
    state(
        "thermal",
        "power",
        Device,
        "Report a thermal status (overheating), which throttling-aware apps react to",
        "unlocks the thermal status",
        &["thermal"],
    )
    .sdk(29),
    // ── resources ─────────────────────────────────────────────────────
    state(
        "storage-full",
        "resources",
        Device,
        "Fill the data partition until only --free-mb remains, so writes fail",
        "deletes the filler file",
        &["storage"],
    )
    .destructive()
    .side_effects(&["other apps on the device also run out of space until clear"]),
    state(
        "clock",
        "resources",
        Device,
        "Move the system clock by --offset-ms (token expiry, schedules, TLS certificate dates)",
        "sets the clock back to the true time and restores automatic time",
        &["clock"],
    )
    .destructive()
    .side_effects(&["turns automatic date & time off until clear"]),
    state(
        "timezone",
        "resources",
        Device,
        "Change the system time zone",
        "restores the previous time zone and automatic time zone setting",
        &["timezone"],
    ),
    state(
        "cpu-load",
        "resources",
        Device,
        "Keep CPU cores busy with background loops so the app competes for CPU (jank, timeouts)",
        "stops the loops",
        &["cpu"],
    ),
    // ── network ───────────────────────────────────────────────────────
    state(
        "airplane-mode",
        "network",
        Device,
        "Turn airplane mode on: no network at all",
        "turns airplane mode back off",
        &["connectivity"],
    )
    .sdk(30),
    state(
        "wifi-off",
        "network",
        Device,
        "Turn Wi-Fi off (mobile data, if any, stays up)",
        "turns Wi-Fi back on",
        &["wifi"],
    ),
    state(
        "mobile-data-off",
        "network",
        Device,
        "Turn mobile data off (Wi-Fi, if any, stays up)",
        "turns mobile data back on",
        &["mobile_data"],
    ),
    state(
        "network-flap",
        "network",
        Device,
        "Toggle airplane mode every --period-ms: the network keeps dropping and coming back",
        "stops toggling and leaves airplane mode off",
        &["connectivity"],
    )
    .sdk(30),
    state(
        "dns-failure",
        "network",
        Device,
        "Make every DNS lookup fail (Private DNS pointed at a host that doesn't exist)",
        "restores the previous Private DNS setting",
        &["private_dns"],
    )
    .sdk(28),
    state(
        "network-speed",
        "network",
        Emulator,
        "Throttle the emulator's link to a cellular speed profile",
        "restores the previous link speed",
        &["link_speed"],
    ),
    state(
        "network-latency",
        "network",
        Emulator,
        "Add cellular link latency to every packet",
        "restores the previous link latency",
        &["link_latency"],
    ),
    state(
        "http-errors",
        "proxy",
        Proxy,
        "Answer matching requests with an HTTP error status instead of reaching the server",
        "removes the fault from the proxy",
        &[],
    ),
    state(
        "http-latency",
        "proxy",
        Proxy,
        "Delay matching requests before they reach the server",
        "removes the fault from the proxy",
        &[],
    ),
    state(
        "bandwidth",
        "proxy",
        Proxy,
        "Stream matching responses to the app at a limited rate",
        "removes the fault from the proxy",
        &[],
    ),
    state(
        "connection-reset",
        "proxy",
        Proxy,
        "Break the connection partway through matching responses (the app sees an I/O error)",
        "removes the fault from the proxy",
        &[],
    ),
    state(
        "truncated-response",
        "proxy",
        Proxy,
        "Deliver only the start of matching response bodies as if they were complete",
        "removes the fault from the proxy",
        &[],
    ),
    state(
        "tls-failure",
        "proxy",
        Proxy,
        "Fail the TLS handshake for matching HTTPS hosts",
        "removes the fault from the proxy",
        &[],
    ),
    // ── interruptions ─────────────────────────────────────────────────
    state(
        "incoming-call",
        "interruption",
        Emulator,
        "Ring an incoming phone call",
        "hangs the call up",
        &["call"],
    ),
    action(
        "sms",
        "interruption",
        Emulator,
        "Deliver an incoming SMS",
    ),
    state(
        "screen-off",
        "interruption",
        Device,
        "Turn the screen off (the app is paused and stopped)",
        "wakes the screen and dismisses a non-secure keyguard",
        &["screen"],
    ),
    state(
        "notification-shade",
        "interruption",
        Device,
        "Pull the notification shade or quick settings down over the app",
        "collapses the shade",
        &["statusbar"],
    ),
    action(
        "emulator-crash",
        "interruption",
        Emulator,
        "Crash the emulator the way a host failure would, mid-whatever-the-app-is-doing; --relaunch boots it again",
    )
    .destructive()
    .side_effects(&[
        "the device disconnects; run `connect` after it boots again",
        "unsaved app data is lost, as in a real crash",
    ]),
];

pub fn find(kind: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|info| info.kind == kind)
}

/// `commands --json` agent hints for every `fault …` path.
pub fn agent_metadata(path: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    let common_next = ["fault list", "fault clear <id>", "why"];
    Some(match path {
        "fault" => json!({
            "use_when": ["Test how the app behaves under failure: process death, config changes, Doze, low battery, full storage, clock jumps, no or flaky network, failing backend calls, calls/SMS, emulator crashes."],
            "output": "fault records with an id, what was observed on the device, and the restore plan `fault clear` runs",
            "side_effects": ["state faults change the device until cleared, expired (--duration-ms), or `disconnect`"],
            "next_actions": ["fault kinds", "fault list", "commands --guide faults"]
        }),
        "fault kinds" => json!({
            "use_when": ["Choose a fault: every kind with its category, state/action effect, scope (device, emulator, proxy), app need, minimum API and what clear undoes."],
            "output": "fault_kinds JSON",
            "side_effects": ["none"],
            "next_actions": ["fault inject <kind>", "commands --guide faults"]
        }),
        "fault inject" => json!({
            "use_when": ["Inject one fault; each kind is a subcommand with its own flags."],
            "output": "fault_inject action JSON with the fault record",
            "side_effects": ["see the chosen kind"],
            "next_actions": ["fault kinds", "fault list"]
        }),
        "fault list" => json!({
            "use_when": ["See which faults are active on the device (and proxy hit counts), e.g. before judging app behavior."],
            "output": "fault_list JSON; `overdue` marks a --duration-ms fault whose timer did not clear it",
            "side_effects": ["none"],
            "next_actions": ["fault clear --all", "fault clear <id>", "why"]
        }),
        "fault clear" => json!({
            "use_when": ["Undo faults; always do this before handing the device on."],
            "output": "cleared faults with the restore steps run; fails with fault_restore_failed (faults stay listed) when a restore can't be verified",
            "side_effects": ["restores the device state each fault recorded"],
            "next_actions": ["fault list", "why"]
        }),
        "fault snapshot"
        | "fault snapshot save"
        | "fault snapshot load"
        | "fault snapshot list"
        | "fault snapshot delete" => json!({
            "use_when": ["Emulator only: save a known device state before a destructive fault run and load it back afterwards."],
            "output": "snapshot action/list JSON",
            "prerequisites": ["an emulator"],
            "side_effects": ["load replaces the whole emulator state, restarting the app and ShadowDroid server"],
            "next_actions": ["fault snapshot list", "connect"]
        }),
        "fault run" => json!({
            "use_when": ["Replay a scripted fault scenario (steps of ShadowDroid commands, faults, waits, seeded picks) deterministically."],
            "output": "fault_scenario JSON with every step's exit code and output; a failed step fails the command with fault_scenario_failed",
            "side_effects": ["whatever the steps do; faults the scenario injected are cleared at the end"],
            "next_actions": ["fault list", "why", "commands --guide faults"]
        }),
        _ => {
            let kind = path.strip_prefix("fault inject ")?;
            let info = find(kind)?;
            let mut prerequisites = Vec::new();
            if info.needs_app {
                prerequisites.push("--app <package> (or a configured default app)".to_string());
            }
            match info.scope {
                Scope::Emulator => prerequisites.push("an emulator".into()),
                Scope::Proxy => {
                    prerequisites.push("the ShadowDroid proxy routing the app (`net start`)".into())
                }
                Scope::Device => {}
            }
            if info.min_sdk > 26 {
                prerequisites.push(format!("Android API {}+", info.min_sdk));
            }
            if info.destructive {
                prerequisites.push("an emulator, or --allow-physical on a real device".into());
            }
            let mut side_effects = vec![info.summary.to_string()];
            side_effects.extend(info.side_effects.iter().map(|s| s.to_string()));
            if info.effect == Effect::State {
                side_effects.push(format!("until `fault clear` ({})", info.clear));
            }
            let next: Vec<&str> = if info.effect == Effect::State {
                common_next.to_vec()
            } else {
                vec!["fault list", "why"]
            };
            json!({
                "use_when": [info.summary],
                "output": "fault_inject action JSON: fault.id, state (active | completed), params, observed, restore_plan",
                "prerequisites": prerequisites,
                "side_effects": side_effects,
                "next_actions": next,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_unique_and_states_say_how_they_clear() {
        let mut seen = std::collections::BTreeSet::new();
        for info in KINDS {
            assert!(seen.insert(info.kind), "duplicate kind {}", info.kind);
            match info.effect {
                Effect::State => assert!(!info.clear.is_empty(), "{} has no clear text", info.kind),
                Effect::Action => assert!(info.clear.is_empty(), "{} is an action", info.kind),
            }
            if info.resources.iter().any(|r| r.contains("{app}")) {
                assert!(info.needs_app, "{} scopes a resource to an app", info.kind);
            }
        }
    }
}
