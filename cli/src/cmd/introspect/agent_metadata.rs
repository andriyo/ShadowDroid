//! The hand-curated agent hints (`use_when`, `output`, `side_effects`,
//! `next_commands`, …) attached to each command in the `commands --json`
//! catalog. Pure data keyed by the space-joined command path; the
//! `catalog_advertises_agent_metadata_for_every_public_command` test in [super] keeps it in lockstep
//! with the real clap tree.

pub(super) fn agent_metadata(path: &[String]) -> Option<serde_json::Value> {
    let key = path.join(" ");
    match key.as_str() {
        "commands" => Some(serde_json::json!({
            "use_when": ["Discover ShadowDroid's command tree, flags, and agent decision hints without scraping human help text."],
            "output": "json catalog when --json is passed; human tree otherwise",
            "side_effects": ["none"],
            "next_commands": ["config schema --json", "ui dump", "watch"]
        })),
        "devices" => Some(serde_json::json!({
            "use_when": ["Need to choose an adb serial or verify whether any emulator/device is attached before running device-backed commands."],
            "output": "device list JSON/action output",
            "side_effects": ["none"],
            "next_commands": ["connect", "doctor", "app current"]
        })),
        "log" => Some(serde_json::json!({
            "use_when": [
                "Need to know what the app just logged — after a failed action, an unexpected screen, or a crash event on a previous response.",
                "Need crash/ANR details: blocks are parsed into structured events with project_frames mapping the stack into your source tree."
            ],
            "output": "line-delimited JSON: {type:log} entries and {type:crash} events, then one {type:action,cmd:log} summary",
            "side_effects": ["none"],
            "prerequisites": ["works without the on-device server; scopes to the configured app by default (--all for everything)"],
            "next_commands": ["why", "ui dump", "watch"]
        })),
        "why" => Some(serde_json::json!({
            "use_when": [
                "Something unexpected happened and you want one bounded read instead of a forensic sequence: was it a crash, an ANR, a network failure, or just a different screen?",
                "An action failed or a wait timed out and the screen alone doesn't explain it."
            ],
            "output": "one action JSON: verdict + explanation, evidence (crash/anr/log_errors/screen/net), checked coverage list, and next-step hints",
            "side_effects": ["none — read-only; uses the server only if it is already up"],
            "next_commands": ["log", "ui dump", "net log", "collect"]
        })),
        "usage" => Some(serde_json::json!({
            "use_when": ["Manage the opt-in local usage log (verb + duration + error code per invocation; no argument values, never uploaded)."],
            "output": "action JSON per subcommand",
            "side_effects": ["enable/disable write the user config; clear deletes the local log"],
            "next_commands": ["usage status", "usage report"]
        })),
        "usage status" => Some(serde_json::json!({
            "use_when": ["Check whether usage logging is enabled and where the log lives."],
            "output": "usage_status action JSON",
            "side_effects": ["none"],
            "next_commands": ["usage enable", "usage report"]
        })),
        "usage enable" => Some(serde_json::json!({
            "use_when": ["Opt in to the local usage log."],
            "output": "usage_set action JSON",
            "side_effects": ["writes usage_log:true to ~/.shadowdroid/config.json"],
            "next_commands": ["usage status", "usage report"]
        })),
        "usage disable" => Some(serde_json::json!({
            "use_when": ["Opt out of the local usage log."],
            "output": "usage_set action JSON",
            "side_effects": ["writes usage_log:false to ~/.shadowdroid/config.json"],
            "next_commands": ["usage status"]
        })),
        "usage report" => Some(serde_json::json!({
            "use_when": ["See which verbs run most, their error rates and durations, and the top error codes — the data-driven UX backlog."],
            "output": "usage_report action JSON (per-verb count/errors/p50/p95 + error-code ranking)",
            "side_effects": ["none"],
            "next_commands": ["usage clear"]
        })),
        "usage clear" => Some(serde_json::json!({
            "use_when": ["Delete the accumulated local usage log."],
            "output": "usage_clear action JSON",
            "side_effects": ["removes ~/.shadowdroid/usage.jsonl (and its rotation)"],
            "next_commands": ["usage status"]
        })),
        "connect" => Some(serde_json::json!({
            "use_when": ["Need to install/start the ShadowDroid server and establish the host-device control pipe."],
            "output": "connected action JSON with device/server/app state and UiAutomation advisory",
            "side_effects": ["installs/restarts the server APK", "creates adb forwards", "disables the stylus handwriting tutorial"],
            "next_commands": ["ui dump", "doctor --json", "watch"]
        })),
        "disconnect" => Some(serde_json::json!({
            "use_when": ["Need to release the device UiAutomation slot, stop ShadowDroid, or unblock instrumentation/Espresso/UIAutomator tests."],
            "output": "disconnected action JSON",
            "side_effects": ["stops ShadowDroid server process", "removes adb forwards"],
            "next_commands": ["test -- <command>", "connect"]
        })),
        "test" => Some(serde_json::json!({
            "use_when": ["Need to run Android instrumentation tests while ShadowDroid is connected without manually freeing and restoring UiAutomation."],
            "output": "inherits the wrapped command's stdio and exits with its status",
            "side_effects": ["disconnects ShadowDroid before the command", "reconnects afterward unless --no-reconnect is passed"],
            "prerequisites": ["pass the test command after --"],
            "next_commands": ["doctor", "connect", "collect"]
        })),
        "update" => Some(serde_json::json!({
            "use_when": ["Need to check whether the local CLI is older than the latest GitHub release."],
            "output": "human text or JSON with --json",
            "side_effects": ["--check/--json are read-only; update mode may invoke the detected package manager or installer"],
            "next_commands": ["commands --json", "doctor"]
        })),
        "init" => Some(serde_json::json!({
            "use_when": ["Need first-run host setup or need to install/update agent skills and the Android Studio plugin bridge."],
            "output": "setup report; --json emits machine-readable plugin/skill status",
            "side_effects": ["writes/refreshes agent skill files unless --no-skills", "installs/updates the Android Studio plugin unless --no-studio-plugin"],
            "next_commands": ["studio status --json", "doctor --json", "commands --json"]
        })),
        "config" => Some(serde_json::json!({
            "use_when": ["Repeated app, package, device, project, or debugger parameters would cost tokens across commands."],
            "output": "json for schema/paths/validate when --json is passed",
            "side_effects": ["config init writes .shadowdroid.json or ~/.shadowdroid/config.json"],
            "next_commands": ["config paths --json", "config schema --json", "config init --project", "config validate --json", "debug auto"]
        })),
        "config paths" => Some(serde_json::json!({
            "use_when": ["Need to know which user/project config files ShadowDroid will read and in what precedence order."],
            "output": "config path/precedence report; --json for machine-readable loaded files",
            "side_effects": ["none"],
            "next_commands": ["config schema --json", "config validate --json", "config init"]
        })),
        "config schema" => Some(serde_json::json!({
            "use_when": ["Need the supported .shadowdroid.json shape before generating or editing config."],
            "output": "machine-readable config schema and example when --json is passed",
            "side_effects": ["none"],
            "next_commands": ["config init --project", "config validate --json"]
        })),
        "config explain" => Some(serde_json::json!({
            "use_when": ["Need agent-facing guidance for app aliases, default package/device/project values, and config precedence."],
            "output": "config usage explanation; --json for structured guidance",
            "side_effects": ["none"],
            "next_commands": ["config paths --json", "config init", "commands --json"]
        })),
        "config init" => Some(serde_json::json!({
            "use_when": ["Need to create or update project/user defaults for app package, device, Android Studio, debugger, or run configuration."],
            "output": "config write report; --json for changed fields and target path",
            "side_effects": ["writes .shadowdroid.json by default or ~/.shadowdroid/config.json with --user"],
            "next_commands": ["config validate --json", "debug auto", "app current"]
        })),
        "config validate" => Some(serde_json::json!({
            "use_when": ["Need to verify discovered config files parse cleanly before relying on defaults."],
            "output": "validation report; --json for machine-readable file/status details",
            "side_effects": ["none"],
            "next_commands": ["config paths --json", "doctor --json", "commands --json"]
        })),
        "skill" => Some(serde_json::json!({
            "use_when": ["Need to generate or refresh ShadowDroid instructions for a supported coding agent."],
            "output": "agent integration file content or install/sync JSON",
            "side_effects": ["--install/--sync write conventional agent skill/rule files"],
            "next_commands": ["commands --json", "init"]
        })),
        "studio" => Some(serde_json::json!({
            "use_when": ["Need Android Studio plugin installation/status for debugger, Layout Inspector, source mapping, or recomposition features."],
            "output": "Studio/plugin/bridge status or install report",
            "side_effects": ["install subcommand writes plugin files into Android Studio's plugin directory"],
            "next_commands": ["studio status --json", "studio install", "debug snapshot", "layout snapshot --compose"]
        })),
        "studio status" => Some(serde_json::json!({
            "use_when": ["Need to know whether Android Studio, the ShadowDroid plugin, and the local bridge are present/running."],
            "output": "studio status report; --json for machine-readable bridge/project/plugin details",
            "side_effects": ["none"],
            "next_commands": ["init", "studio install", "debug snapshot", "layout recompositions"]
        })),
        "studio install" => Some(serde_json::json!({
            "use_when": ["Need to install or update the ShadowDroid Android Studio plugin for debugger/Layout Inspector support."],
            "output": "install report and restart guidance",
            "side_effects": ["copies plugin zip contents into the selected Android Studio plugin directory"],
            "prerequisites": ["restart Android Studio after install/update so the bridge registers"],
            "next_commands": ["studio status --json", "debug snapshot", "layout snapshot --compose"]
        })),
        "doctor" => Some(serde_json::json!({
            "use_when": ["ShadowDroid cannot connect, screen reads fail, adb/device state is unclear, or networking may be miswired."],
            "output": "diagnostic report; use --json for machine-readable status",
            "side_effects": ["--fix may reinstall the server, recreate forwards, restart components, and clear dangling device proxy state"],
            "next_commands": ["doctor --fix", "connect", "collect"]
        })),
        "collect" => Some(serde_json::json!({
            "use_when": ["Need a shareable evidence bundle after a failure or before handing off an investigation."],
            "output": "directory with doctor report, device info, logcat/crash context, and best-effort screen/screenshot/app state",
            "side_effects": ["writes files under --out or a generated collection directory"],
            "next_commands": ["doctor", "debug snapshot", "layout snapshot"]
        })),
        "watch" => Some(serde_json::json!({
            "use_when": [
                "Need one live timeline for screen changes, crashes, toasts, watcher actions, and HTTP(S) traffic when the net proxy is running.",
                "Need to correlate UI state with network responses, app crashes, or watcher automation during a flow."
            ],
            "avoid_when": ["Need one immediate actionable element list; use ui dump instead.", "Need a saved layout/source artifact; use layout snapshot instead."],
            "output": "jsonl event stream: ready, screen_compact/screen, crash, watcher_fired, http/http_intercept, tls_error, warning, error",
            "side_effects": ["polls the screen", "tails logcat", "may run watcher actions", "auto-attaches to a running net proxy unless --no-net is passed"],
            "prerequisites": ["shadowdroid connect", "shadowdroid net start for HTTP(S) events"],
            "next_commands": ["ui tap", "ui text", "ui wait", "net start", "net show <id>", "debug snapshot"],
            "prefer_over": {
                "ui dump": "for long flows or correlation across multiple event types",
                "net log": "for live UI plus network correlation"
            }
        })),
        "ui" => Some(serde_json::json!({
            "use_when": ["Need to read or manipulate the currently visible UI."],
            "output": "one JSON object per read/action",
            "side_effects": ["action subcommands can tap, type, scroll, press keys, or navigate"],
            "next_commands": ["ui dump", "ui tap --text <label>", "ui text <value>", "ui wait --text <label>"]
        })),
        "ui dump" => Some(serde_json::json!({
            "use_when": ["Need the current actionable UI state for selector choice before tapping, typing, or waiting."],
            "avoid_when": ["Need Compose/source/layout inspection or a durable artifact; use layout snapshot."],
            "output": "compact screen JSON by default, including ime.keyboard_visible/focused input context; --full adds bounds and every UIAutomator flag",
            "side_effects": ["none"],
            "next_commands": ["ui tap --id <id>", "ui tap --text <text>", "ui text --id <id> <value>", "ui hide-keyboard", "ui wait"],
            "prefer_over": {
                "layout snapshot": "when the next step is acting on the UI rather than debugging layout/source structure"
            }
        })),
        "ui audit" => Some(serde_json::json!({
            "use_when": ["Need to identify interactive elements lacking stable resource-id or Compose testTag before authoring tests."],
            "output": "selector audit JSON with stable/unstable element findings",
            "side_effects": ["none"],
            "next_commands": ["ui gen", "layout source", "ui dump --full"]
        })),
        "ui gen" => Some(serde_json::json!({
            "use_when": ["Need a starter Kotlin screen object generated from the current screen's stable selectors."],
            "output": "Kotlin screen-object scaffold plus TODOs for untagged elements",
            "side_effects": ["none"],
            "prerequisites": ["run on the target screen after app/profile setup"],
            "next_commands": ["ui audit", "ui dump", "layout source"]
        })),
        "ui screenshot" => Some(serde_json::json!({
            "use_when": ["Need a visual artifact of the current device screen for evidence, review, or comparison."],
            "output": "screenshot file path/action JSON",
            "side_effects": ["writes an image file"],
            "next_commands": ["layout snapshot --screenshot", "collect", "ui dump"]
        })),
        "ui find" => Some(serde_json::json!({
            "use_when": ["Need to resolve a selector without tapping it."],
            "output": "matching elements, compact by default",
            "side_effects": ["none"],
            "next_commands": ["ui tap --id <id>", "ui text --id <id>"]
        })),
        "ui tap" => Some(serde_json::json!({
            "use_when": ["Need to activate a visible element by selector, fresh ui dump id, or coordinates."],
            "output": "action JSON with the chosen target/action",
            "side_effects": ["taps the device UI"],
            "prerequisites": ["prefer selectors or ids from a fresh ui dump over hard-coded coordinates"],
            "next_commands": ["ui wait", "ui dump", "watch"]
        })),
        "ui double-tap" => Some(serde_json::json!({
            "use_when": ["Need to double-tap fixed coordinates for gestures that have no stable selector."],
            "output": "double-tap action JSON",
            "side_effects": ["taps the device UI twice"],
            "prerequisites": ["prefer selector-based ui tap unless the target is genuinely gesture-only"],
            "next_commands": ["ui wait", "ui dump"]
        })),
        "ui long-tap" => Some(serde_json::json!({
            "use_when": ["Need to open a context menu, reorder mode, or other long-press-only interaction."],
            "output": "long-tap action JSON",
            "side_effects": ["long-presses device coordinates"],
            "next_commands": ["ui wait", "ui dump", "ui tap"]
        })),
        "ui swipe" => Some(serde_json::json!({
            "use_when": ["Need a precise coordinate swipe for carousels, maps, or custom gesture surfaces."],
            "output": "swipe action JSON",
            "side_effects": ["swipes the device UI"],
            "next_commands": ["ui wait", "ui dump", "ui scroll-to"]
        })),
        "ui drag" => Some(serde_json::json!({
            "use_when": ["Need a slower drag gesture for drag-and-drop, reorder, sliders, or map movement."],
            "output": "drag action JSON",
            "side_effects": ["drags across the device UI"],
            "next_commands": ["ui wait", "ui dump"]
        })),
        "ui swipe-ext" => Some(serde_json::json!({
            "use_when": ["Need a direction-based screen-relative swipe without hard-coding exact coordinates."],
            "output": "extended swipe action JSON",
            "side_effects": ["swipes the device UI"],
            "next_commands": ["ui wait", "ui dump", "ui scroll-to"]
        })),
        "ui pinch" => Some(serde_json::json!({
            "use_when": ["Need pinch zoom on a map/image/custom surface matched by selector."],
            "output": "pinch action JSON",
            "side_effects": ["performs a multi-touch pinch gesture"],
            "next_commands": ["ui wait", "ui dump", "ui screenshot"]
        })),
        "ui scroll-to" => Some(serde_json::json!({
            "use_when": ["Need to scroll a list until a selector becomes visible, optionally tapping it."],
            "output": "scroll/search action JSON with final match when found",
            "side_effects": ["scrolls the visible UI and may tap when requested"],
            "prerequisites": ["prefer this over blind repeated swipes for list search"],
            "next_commands": ["ui tap", "ui text", "ui dump"]
        })),
        "ui focus" => Some(serde_json::json!({
            "use_when": ["Need TV/leanback D-pad focus movement to a selector, optionally pressing center."],
            "output": "focus movement JSON",
            "side_effects": ["sends D-pad key events and may activate the focused element"],
            "next_commands": ["ui dump", "ui key", "ui wait"]
        })),
        "ui text" => Some(serde_json::json!({
            "use_when": ["Need to type into the focused field or a field selected by id/text/rid/desc/xpath."],
            "output": "action JSON",
            "side_effects": ["changes text in the app UI"],
            "next_commands": ["ui key enter", "ui hide-keyboard", "ui wait", "ui dump"]
        })),
        "ui key" => Some(serde_json::json!({
            "use_when": ["Need to press a named Android key/keycode such as enter, back, home, dpad, or media keys."],
            "output": "key action JSON",
            "side_effects": ["sends a key event to the device"],
            "next_commands": ["ui dump", "ui wait"]
        })),
        "ui hide-keyboard" => Some(serde_json::json!({
            "use_when": ["Need to dismiss the soft keyboard without risking Back navigation when the keyboard is already hidden."],
            "output": "action JSON with keyboard_visible, injected, and compact ime context",
            "side_effects": ["presses Back only when ui dump reports ime.keyboard_visible=true"],
            "prerequisites": ["use ui dump ime.keyboard_visible or call directly; hidden keyboard is a no-op"],
            "next_commands": ["ui dump", "ui tap", "layout snapshot"]
        })),
        "ui back" => Some(serde_json::json!({
            "use_when": ["Need explicit Android Back navigation, not just keyboard dismissal."],
            "output": "back action JSON",
            "side_effects": ["presses Back and may navigate away or close dialogs"],
            "next_commands": ["ui wait", "ui dump", "app current"]
        })),
        "ui home" => Some(serde_json::json!({
            "use_when": ["Need to leave the app and return to the launcher."],
            "output": "home action JSON",
            "side_effects": ["presses Home and backgrounds the app"],
            "next_commands": ["app current", "app start <pkg>"]
        })),
        "ui wait" => Some(serde_json::json!({
            "use_when": ["Need to block until an element, activity, or package appears or disappears."],
            "output": "JSON match result",
            "side_effects": ["polls current UI/app state"],
            "next_commands": ["ui dump", "ui tap", "watch"]
        })),
        "ui toast" => Some(serde_json::json!({
            "use_when": ["Need to capture transient Android toast text that may not appear in the normal UI tree."],
            "output": "toast capture JSON",
            "side_effects": ["listens for toast events until wait budget expires"],
            "next_commands": ["ui wait", "watch", "debug snapshot"]
        })),
        "layout" => Some(serde_json::json!({
            "use_when": ["Need visual/layout/source structure artifacts rather than immediate UI actions."],
            "output": "layout JSON artifacts and diffs",
            "side_effects": ["snapshot can write files and screenshots"],
            "next_commands": ["layout snapshot", "layout diff", "layout source", "layout recompositions"]
        })),
        "layout snapshot" => Some(serde_json::json!({
            "use_when": ["Need a saved UI structure artifact, layout diff input, screenshot pairing, Compose semantics, or source mapping."],
            "avoid_when": ["Need to tap/type based on the current UI; use ui dump."],
            "output": "layout_snapshot JSON with sample_valid/sample diagnostics; --out writes it, --screenshot writes a sibling screenshot artifact",
            "side_effects": ["optional file writes with --out/--screenshot"],
            "prerequisites": ["Android Studio Layout Inspector bridge is needed for Compose/source enrichment; UIAutomator tree is still returned without it"],
            "next_commands": ["layout diff <before> <after>", "layout source --id <id>", "layout source --draw-id <id>"],
            "prefer_over": {
                "ui dump": "when preserving or debugging layout/source structure matters more than immediate action"
            }
        })),
        "layout diff" => Some(serde_json::json!({
            "use_when": ["Need to compare two saved layout snapshots after an interaction, rotation, data load, or regression repro."],
            "output": "layout diff JSON summarizing structural/element changes",
            "side_effects": ["none"],
            "prerequisites": ["capture before/after files with layout snapshot --out"],
            "next_commands": ["layout source --id <id>", "layout snapshot --compose --source-map"]
        })),
        "layout source" => Some(serde_json::json!({
            "use_when": ["Need to map a current UIAutomator element or Studio Layout Inspector draw id back to source when available."],
            "output": "layout_source JSON with matched node, source availability, and sample_valid/sample diagnostics",
            "side_effects": ["none"],
            "next_commands": ["debug break line", "debug auto", "layout snapshot --source-map"]
        })),
        "layout recompositions" => Some(serde_json::json!({
            "use_when": ["Need Compose recomposition/skip counters for the current screen, or want to isolate recompositions caused by one interaction."],
            "output": "layout_recompositions JSON with sample_valid/sample diagnostics, summary totals, and source-mapped Compose nodes when Android Studio Layout Inspector is available",
            "side_effects": ["--reset clears Android Studio Layout Inspector recomposition counters for the selected app/process"],
            "prerequisites": ["Android Studio must be running with the ShadowDroid plugin and Layout Inspector model available", "Use --reset before the interaction, then run again after the interaction to rank changed nodes"],
            "next_commands": ["layout recompositions --reset", "layout source --draw-id <id>", "layout snapshot --compose --source-map", "debug snapshot"],
            "prefer_over": {
                "layout snapshot": "when the question is runtime Compose churn rather than static layout/source structure",
                "ui dump": "when visible UI selectors are not enough and recomposition counters are needed"
            }
        })),
        "debug" => Some(serde_json::json!({
            "use_when": ["Need runtime causality, stack/variable state, breakpoints, replay, or Android Studio debugger control."],
            "output": "bounded JSON debug state or JSONL timelines depending on subcommand",
            "side_effects": ["attach/break/resume/step commands affect debugger/app execution"],
            "next_commands": ["debug auto", "debug snapshot", "debug record", "debug run-until-crash"]
        })),
        "debug auto" => Some(serde_json::json!({
            "use_when": ["Need the fastest agent entrypoint for launching/configuring the app, attaching the debugger when available, and returning a useful snapshot."],
            "output": "debug snapshot JSON with sample_valid/sample diagnostics",
            "side_effects": ["may launch the app and attach Android Studio debugger"],
            "next_commands": ["debug variables", "debug eval", "debug break line", "ui dump"]
        })),
        "debug snapshot" => Some(serde_json::json!({
            "use_when": ["Need current app/runtime/debugger/logcat/screen state for causality, not just visible UI."],
            "avoid_when": ["Need layout/source structure; use layout snapshot/source."],
            "output": "bounded debug state JSON with sample_valid/sample diagnostics",
            "side_effects": ["reads app/debugger/logcat/screen state"],
            "next_commands": ["debug variables", "debug eval", "layout source", "collect"]
        })),
        "debug record" => Some(serde_json::json!({
            "use_when": ["Need a durable JSONL timeline of a flow for later triage, replay, or handoff."],
            "output": "JSONL file with screen/app/logcat/debugger/screenshot events",
            "side_effects": ["writes --out file and optional screenshot artifacts", "polls screen/logcat/debugger until stopped or duration ends"],
            "next_commands": ["debug replay", "collect", "layout source"]
        })),
        "debug replay" => Some(serde_json::json!({
            "use_when": ["Need to replay action events from a prior debug record timeline and compare resulting screen hashes."],
            "output": "replay result JSON with per-action status and optional screen hashes",
            "side_effects": ["replays recorded UI actions against the connected device"],
            "prerequisites": ["use a trusted JSONL timeline created by debug record"],
            "next_commands": ["debug snapshot", "debug record", "collect"]
        })),
        "debug status" => Some(serde_json::json!({
            "use_when": ["Need raw Android Studio bridge/debugger session status before attaching, stepping, or reading variables."],
            "output": "bridge status JSON",
            "side_effects": ["none"],
            "next_commands": ["debug clients", "debug attach", "studio status --json"]
        })),
        "debug sessions" => Some(serde_json::json!({
            "use_when": ["Need to list active Android Studio debugger sessions before selecting one for stack, variables, stepping, or resume."],
            "output": "debug session list JSON",
            "side_effects": ["none"],
            "next_commands": ["debug stack --session <n>", "debug variables --session <n>", "debug resume --session <n>"]
        })),
        "debug clients" => Some(serde_json::json!({
            "use_when": ["Need to discover attachable Android processes visible to Android Studio before debug attach."],
            "output": "debug clients JSON",
            "side_effects": ["none"],
            "next_commands": ["debug attach --package <pkg>", "debug auto --app <app>"]
        })),
        "debug attach" => Some(serde_json::json!({
            "use_when": ["Need Android Studio to attach its debugger to an already-running app/process."],
            "output": "attach result JSON",
            "side_effects": ["starts/attaches an Android Studio debugger session"],
            "prerequisites": ["Android Studio bridge must be running", "target app must be debuggable and visible in debug clients"],
            "next_commands": ["debug snapshot", "debug break line", "debug variables"]
        })),
        "debug break" => Some(serde_json::json!({
            "use_when": ["Need to create, update, or remove debugger breakpoints/watchpoints from the CLI."],
            "output": "breakpoint command JSON with stable breakpoint ids",
            "side_effects": ["mutates Android Studio breakpoint state"],
            "next_commands": ["debug break line", "debug breakpoints", "debug resume"]
        })),
        "debug break line" => Some(serde_json::json!({
            "use_when": ["Need to stop execution at a known source file and line, often after layout source identifies suspicious code."],
            "output": "breakpoint creation JSON with stable breakpoint id",
            "side_effects": ["adds or updates an Android Studio line breakpoint"],
            "next_commands": ["debug resume", "debug snapshot", "debug variables"]
        })),
        "debug break exception" => Some(serde_json::json!({
            "use_when": ["Need the debugger to suspend when a Java/Kotlin exception type is thrown."],
            "output": "exception breakpoint JSON with stable breakpoint id",
            "side_effects": ["adds or updates an Android Studio exception breakpoint"],
            "next_commands": ["debug resume", "debug run-until-crash", "debug variables"]
        })),
        "debug break method" => Some(serde_json::json!({
            "use_when": ["Need to suspend on method entry/exit when line source is not enough or bytecode/source mapping is ambiguous."],
            "output": "method breakpoint JSON with stable breakpoint id",
            "side_effects": ["adds or updates an Android Studio method breakpoint"],
            "next_commands": ["debug resume", "debug variables", "debug breakpoints"]
        })),
        "debug break field" => Some(serde_json::json!({
            "use_when": ["Need to suspend when a field is read or modified."],
            "output": "field watchpoint JSON with stable breakpoint id",
            "side_effects": ["adds or updates an Android Studio field watchpoint"],
            "next_commands": ["debug resume", "debug variables", "debug breakpoints"]
        })),
        "debug break update" => Some(serde_json::json!({
            "use_when": ["Need to enable, disable, condition, log, or change suspend behavior for an existing breakpoint id."],
            "output": "breakpoint update JSON",
            "side_effects": ["mutates an Android Studio breakpoint"],
            "prerequisites": ["obtain the breakpoint id from debug breakpoints or a break command result"],
            "next_commands": ["debug breakpoints", "debug resume"]
        })),
        "debug break remove" => Some(serde_json::json!({
            "use_when": ["Need to remove a breakpoint/watchpoint by stable id."],
            "output": "breakpoint remove JSON",
            "side_effects": ["removes an Android Studio breakpoint"],
            "prerequisites": ["obtain the breakpoint id from debug breakpoints or a break command result"],
            "next_commands": ["debug breakpoints", "debug snapshot"]
        })),
        "debug breakpoints" => Some(serde_json::json!({
            "use_when": ["Need to inspect currently configured Android Studio breakpoints before adding/removing/updating them."],
            "output": "breakpoint list JSON",
            "side_effects": ["none"],
            "next_commands": ["debug break line", "debug break remove", "debug resume"]
        })),
        "debug pause" => Some(serde_json::json!({
            "use_when": ["Need to interrupt a running debug session so stack/variables/eval become available."],
            "output": "pause result JSON",
            "side_effects": ["pauses the selected debugger session"],
            "next_commands": ["debug stack", "debug variables", "debug resume"]
        })),
        "debug resume" => Some(serde_json::json!({
            "use_when": ["Need to continue a suspended debugger session after inspecting state or setting breakpoints."],
            "output": "resume result JSON",
            "side_effects": ["resumes the selected debugger session"],
            "next_commands": ["debug snapshot", "debug run-until-crash", "ui dump"]
        })),
        "debug step-in" => Some(serde_json::json!({
            "use_when": ["Need to step into the next callable frame from a suspended debugger session."],
            "output": "step result JSON",
            "side_effects": ["steps the selected debugger session"],
            "prerequisites": ["debugger session must be suspended"],
            "next_commands": ["debug stack", "debug variables", "debug step-over"]
        })),
        "debug step-over" => Some(serde_json::json!({
            "use_when": ["Need to advance one source line without entering called methods."],
            "output": "step result JSON",
            "side_effects": ["steps the selected debugger session"],
            "prerequisites": ["debugger session must be suspended"],
            "next_commands": ["debug variables", "debug step-until-screen-change", "debug resume"]
        })),
        "debug step-out" => Some(serde_json::json!({
            "use_when": ["Need to run until the current method returns."],
            "output": "step result JSON",
            "side_effects": ["steps the selected debugger session"],
            "prerequisites": ["debugger session must be suspended"],
            "next_commands": ["debug stack", "debug variables", "debug resume"]
        })),
        "debug stop" => Some(serde_json::json!({
            "use_when": ["Need to terminate the selected debugger session without necessarily stopping the app process."],
            "output": "stop result JSON",
            "side_effects": ["stops the selected Android Studio debugger session"],
            "next_commands": ["debug sessions", "debug attach", "debug snapshot"]
        })),
        "debug stack" => Some(serde_json::json!({
            "use_when": ["Need call stack frames for the selected suspended debug session."],
            "output": "stack/frame JSON",
            "side_effects": ["none"],
            "prerequisites": ["debugger session should be suspended for the most useful frames"],
            "next_commands": ["debug variables --frame <n>", "debug eval", "debug break line"]
        })),
        "debug threads" => Some(serde_json::json!({
            "use_when": ["Need all debugger threads and stack frames before choosing a thread/frame for variable inspection."],
            "output": "thread/frame JSON",
            "side_effects": ["none"],
            "next_commands": ["debug variables --thread <id>", "debug coroutines threads", "debug stack"]
        })),
        "debug variables" => Some(serde_json::json!({
            "use_when": ["Need visible local variables/fields from the selected suspended debugger frame."],
            "output": "bounded variable tree JSON",
            "side_effects": ["none"],
            "prerequisites": ["debugger session must be suspended on a frame with debug information"],
            "next_commands": ["debug eval", "debug inspect", "debug resume"]
        })),
        "debug eval" => Some(serde_json::json!({
            "use_when": ["Need to evaluate a deterministic JDI path expression in the selected suspended frame."],
            "output": "bounded evaluation JSON",
            "side_effects": ["none; expressions are restricted to deterministic field/path inspection"],
            "prerequisites": ["debugger session must be suspended"],
            "next_commands": ["debug variables", "debug inspect", "debug resume"]
        })),
        "debug inspect" => Some(serde_json::json!({
            "use_when": ["Need deeper bounded inspection of an expression or object handle returned by variables/eval."],
            "output": "bounded object/value inspection JSON",
            "side_effects": ["none"],
            "prerequisites": ["debugger session must be suspended"],
            "next_commands": ["debug variables", "debug eval", "debug resume"]
        })),
        "debug coroutines" => Some(serde_json::json!({
            "use_when": ["Need coroutine, dispatcher, continuation, or Flow-like state from suspended Kotlin/JVM frames."],
            "output": "bounded coroutine/debugger JSON",
            "side_effects": ["none"],
            "prerequisites": ["debugger session should be suspended"],
            "next_commands": ["debug coroutines snapshot", "debug coroutines threads", "debug coroutines continuation", "debug coroutines flow"]
        })),
        "debug coroutines snapshot" => Some(serde_json::json!({
            "use_when": ["Need a broad coroutine-like state snapshot reachable from suspended frames."],
            "output": "coroutine snapshot JSON",
            "side_effects": ["none"],
            "prerequisites": ["debugger session should be suspended"],
            "next_commands": ["debug threads", "debug coroutines continuation", "debug variables"]
        })),
        "debug coroutines threads" => Some(serde_json::json!({
            "use_when": ["Need debugger threads annotated with coroutine/dispatcher hints."],
            "output": "coroutine thread JSON",
            "side_effects": ["none"],
            "next_commands": ["debug threads", "debug coroutines continuation"]
        })),
        "debug coroutines continuation" => Some(serde_json::json!({
            "use_when": ["Need spilled locals or continuation fields from the selected Kotlin suspended frame."],
            "output": "continuation inspection JSON",
            "side_effects": ["none"],
            "prerequisites": ["debugger session must be suspended on a Kotlin coroutine frame"],
            "next_commands": ["debug variables", "debug coroutines flow", "debug resume"]
        })),
        "debug coroutines flow" => Some(serde_json::json!({
            "use_when": ["Need bounded structural inspection of a Flow/StateFlow-like expression without invoking collection."],
            "output": "Flow-like object inspection JSON",
            "side_effects": ["none"],
            "prerequisites": ["debugger session must be suspended and --expr must be a deterministic path"],
            "next_commands": ["debug variables", "debug eval", "debug watch add"]
        })),
        "debug continue-until" => Some(serde_json::json!({
            "use_when": ["Need to resume execution until a source location or deterministic condition becomes true."],
            "output": "continue-until result JSON",
            "side_effects": ["resumes and polls the selected debugger session"],
            "prerequisites": ["provide --file/--line or --condition"],
            "next_commands": ["debug variables", "debug stack", "debug resume"]
        })),
        "debug watch" => Some(serde_json::json!({
            "use_when": ["Need persistent debugger watch expressions managed from the CLI."],
            "output": "watch expression management JSON",
            "side_effects": ["add/remove/clear mutate Android Studio watch state"],
            "next_commands": ["debug watch add", "debug watch list", "debug variables"]
        })),
        "debug watch add" => Some(serde_json::json!({
            "use_when": ["Need to save a deterministic expression for repeated debugger evaluation."],
            "output": "watch add JSON with stable id",
            "side_effects": ["adds or replaces an Android Studio watch expression"],
            "next_commands": ["debug watch list", "debug resume"]
        })),
        "debug watch list" => Some(serde_json::json!({
            "use_when": ["Need to list saved watches and evaluate them when a debugger session is suspended."],
            "output": "watch list/evaluation JSON",
            "side_effects": ["none"],
            "next_commands": ["debug watch remove", "debug variables", "debug eval"]
        })),
        "debug watch remove" => Some(serde_json::json!({
            "use_when": ["Need to delete one saved debugger watch expression by id."],
            "output": "watch remove JSON",
            "side_effects": ["removes one Android Studio watch expression"],
            "next_commands": ["debug watch list", "debug watch add"]
        })),
        "debug watch clear" => Some(serde_json::json!({
            "use_when": ["Need to remove all saved debugger watch expressions before another debugging session."],
            "output": "watch clear JSON",
            "side_effects": ["removes all Android Studio watch expressions"],
            "next_commands": ["debug watch list", "debug watch add"]
        })),
        "debug run-until-crash" => Some(serde_json::json!({
            "use_when": ["Need to resume the app and capture the next Java/native crash or ANR with debugger/logcat context."],
            "output": "crash/ANR result JSON plus final debug snapshot",
            "side_effects": ["resumes the selected debug session", "waits for crash/ANR/logcat signals"],
            "next_commands": ["debug snapshot", "collect", "debug tombstones list"]
        })),
        "debug step-until-screen-change" => Some(serde_json::json!({
            "use_when": ["Need to step over repeatedly until a UI state transition is observed by screen_hash."],
            "output": "step result JSON with initial/final screen hashes and final snapshot",
            "side_effects": ["steps the suspended debugger session"],
            "prerequisites": ["debugger session must be suspended"],
            "next_commands": ["debug variables", "layout source", "ui dump"]
        })),
        "debug step-until-log" => Some(serde_json::json!({
            "use_when": ["Need to step over until logcat emits a target line/pattern."],
            "output": "step result JSON with matched log context and final snapshot",
            "side_effects": ["steps the suspended debugger session"],
            "prerequisites": ["debugger session must be suspended"],
            "next_commands": ["debug variables", "debug snapshot", "collect"]
        })),
        "debug native" => Some(serde_json::json!({
            "use_when": ["Need native or mixed-mode readiness checks before investigating JNI/NDK crashes."],
            "output": "native debugger/artifact command JSON",
            "side_effects": ["none for status"],
            "next_commands": ["debug native status", "debug tombstones list", "collect"]
        })),
        "debug native status" => Some(serde_json::json!({
            "use_when": ["Need to check native/mixed-mode debugger readiness, symbols, ABI, process, and tombstone context."],
            "output": "native readiness JSON",
            "side_effects": ["none"],
            "next_commands": ["debug tombstones list", "debug snapshot", "collect"]
        })),
        "debug tombstones" => Some(serde_json::json!({
            "use_when": ["Need native tombstone discovery or local copies after a native crash."],
            "output": "tombstone command JSON",
            "side_effects": ["pull writes files; list is read-only"],
            "next_commands": ["debug tombstones list", "debug tombstones pull", "debug native status"]
        })),
        "debug tombstones list" => Some(serde_json::json!({
            "use_when": ["Need to see recent native tombstones visible through adb after a native crash."],
            "output": "tombstone list JSON",
            "side_effects": ["none"],
            "next_commands": ["debug tombstones pull", "collect"]
        })),
        "debug tombstones pull" => Some(serde_json::json!({
            "use_when": ["Need local copies of native tombstones for symbolication or handoff."],
            "output": "pull report JSON",
            "side_effects": ["writes tombstone files under --out"],
            "next_commands": ["debug native status", "collect"]
        })),
        "app" => Some(serde_json::json!({
            "use_when": ["Need app lifecycle, foreground, install, or metadata control for the target Android app."],
            "output": "one JSON action/result per command",
            "side_effects": ["start/stop/install/clear/reinstall mutate app/device state"],
            "next_commands": ["app current", "app start <pkg>", "app wait <pkg> --front", "ui dump"]
        })),
        "app current" => Some(serde_json::json!({
            "use_when": ["Need to confirm the foreground package/activity/pid before trusting UI, layout, debug, or recomposition samples."],
            "output": "current foreground app JSON",
            "side_effects": ["none"],
            "next_commands": ["app start <pkg>", "ui wait --pkg <pkg>", "ui dump"]
        })),
        "app start" => Some(serde_json::json!({
            "use_when": ["Need to launch a package's default activity, or a specific launcher/activity with --activity when Android exposes several choices."],
            "output": "app start action JSON including launched activity, launcher candidates, and ambiguity warnings",
            "side_effects": ["launches the app"],
            "next_commands": ["app wait <pkg> --front", "ui dump", "debug snapshot --app <pkg>"],
            "examples": ["app start com.example.app --activity .MainActivity"]
        })),
        "app stop" => Some(serde_json::json!({
            "use_when": ["Need to force-stop an app before a clean launch, reinstall, or state reset."],
            "output": "app stop action JSON",
            "side_effects": ["force-stops the package"],
            "next_commands": ["app start <pkg>", "app clear <pkg>", "app install"]
        })),
        "app wait" => Some(serde_json::json!({
            "use_when": ["Need to block until a package is running or foregrounded before sampling UI/debug/layout state."],
            "output": "wait result JSON",
            "side_effects": ["polls app state"],
            "next_commands": ["ui dump", "debug snapshot", "layout snapshot"]
        })),
        "app install" => Some(serde_json::json!({
            "use_when": ["Need to install an APK and perform the usual test setup ritual in one command."],
            "output": "install/setup result JSON",
            "side_effects": ["installs APK", "may clear app data", "may grant permissions", "may launch/wait for foreground depending on flags"],
            "next_commands": ["app current", "ui dump", "doctor --app <pkg>"]
        })),
        "app reinstall" => Some(serde_json::json!({
            "use_when": ["Need a clean reinstall path when stale app state or signatures may affect testing."],
            "output": "reinstall/setup result JSON",
            "side_effects": ["uninstalls existing package", "installs APK", "may clear/grant/launch/wait depending on flags"],
            "next_commands": ["app current", "ui dump", "doctor --app <pkg>"]
        })),
        "app clear" => Some(serde_json::json!({
            "use_when": ["Need to reset app data without reinstalling."],
            "output": "clear action JSON",
            "side_effects": ["clears app data and stops the app"],
            "next_commands": ["app start <pkg>", "perm grant <pkg> <permission>"]
        })),
        "app info" => Some(serde_json::json!({
            "use_when": ["Need installed app label/version metadata for evidence or package verification."],
            "output": "app info JSON",
            "side_effects": ["none"],
            "next_commands": ["app current", "doctor --app <pkg>"]
        })),
        "perm" => Some(serde_json::json!({
            "use_when": ["Need runtime permission state setup or verification without opening Android settings."],
            "output": "permission action/list JSON",
            "side_effects": ["grant/revoke/reset mutate runtime permission state"],
            "next_commands": ["perm list <pkg>", "perm grant <pkg> <permission>", "app start <pkg>"]
        })),
        "perm grant" => Some(serde_json::json!({
            "use_when": ["Need to pre-grant one or more runtime permissions for a deterministic test flow."],
            "output": "grant result JSON with readback verification",
            "side_effects": ["grants runtime permissions"],
            "next_commands": ["perm list <pkg>", "app start <pkg>", "ui dump"]
        })),
        "perm revoke" => Some(serde_json::json!({
            "use_when": ["Need to force a permission prompt or denied-permission path."],
            "output": "revoke result JSON",
            "side_effects": ["revokes runtime permissions"],
            "next_commands": ["perm list <pkg>", "app start <pkg>", "ui wait"]
        })),
        "perm list" => Some(serde_json::json!({
            "use_when": ["Need to inspect a package's runtime permission grant state."],
            "output": "permission state JSON",
            "side_effects": ["none"],
            "next_commands": ["perm grant <pkg> <permission>", "perm revoke <pkg> <permission>", "doctor --app <pkg>"]
        })),
        "perm reset" => Some(serde_json::json!({
            "use_when": ["Need to return a package to fresh-install runtime permission prompt state."],
            "output": "permission reset JSON",
            "side_effects": ["revokes all granted runtime permissions for the package"],
            "next_commands": ["perm list <pkg>", "app start <pkg>", "ui dump"]
        })),
        "appops" => Some(serde_json::json!({
            "use_when": ["Need to inspect or change Android app-op modes such as location, notification, or background behavior."],
            "output": "app-op get/set JSON",
            "side_effects": ["set mutates app-op mode"],
            "next_commands": ["appops get <pkg>", "appops set <pkg> <op> <mode>", "app start <pkg>"]
        })),
        "appops get" => Some(serde_json::json!({
            "use_when": ["Need current app-op modes for a package before changing them or for diagnostics."],
            "output": "app-op state JSON",
            "side_effects": ["none"],
            "next_commands": ["appops set <pkg> <op> <mode>", "collect"]
        })),
        "appops set" => Some(serde_json::json!({
            "use_when": ["Need to force an app-op mode for a specific test path."],
            "output": "app-op set JSON",
            "side_effects": ["mutates one app-op mode"],
            "next_commands": ["appops get <pkg> <op>", "app start <pkg>", "ui dump"]
        })),
        "profile" => Some(serde_json::json!({
            "use_when": ["Need deterministic emulator/device display state: animations, font scale, density, size, or rotation."],
            "output": "profile snapshot/apply/reset JSON",
            "side_effects": ["apply/reset mutate device display/settings"],
            "next_commands": ["profile snapshot", "profile apply --preset automation", "ui dump"]
        })),
        "profile snapshot" => Some(serde_json::json!({
            "use_when": ["Need to capture current display/profile settings before changing them or for evidence."],
            "output": "display profile JSON, optionally written to --out",
            "side_effects": ["writes file with --out; otherwise none"],
            "next_commands": ["profile apply", "profile reset"]
        })),
        "profile apply" => Some(serde_json::json!({
            "use_when": ["Need to make UI automation deterministic by disabling animations or applying known display/font/density settings."],
            "output": "profile apply result JSON",
            "side_effects": ["changes device settings such as animation scales, font scale, density, size, rotation"],
            "next_commands": ["ui dump", "profile snapshot", "profile reset"]
        })),
        "profile reset" => Some(serde_json::json!({
            "use_when": ["Need to restore stock display/profile defaults after automation."],
            "output": "profile reset result JSON",
            "side_effects": ["changes device display/settings back to defaults"],
            "next_commands": ["profile snapshot", "ui dump"]
        })),
        "device" => Some(serde_json::json!({
            "use_when": ["Need device/system controls outside app UI: shell, power, orientation, clipboard, notifications, URLs."],
            "output": "one JSON action/result per command",
            "side_effects": ["subcommands may mutate device/system state"],
            "next_commands": ["device info", "device shell <cmd>", "ui dump"]
        })),
        "device info" => Some(serde_json::json!({
            "use_when": ["Need model/build/locale/density facts for bug reports or environment checks."],
            "output": "device info JSON",
            "side_effects": ["none"],
            "next_commands": ["doctor", "profile snapshot"]
        })),
        "device shell" => Some(serde_json::json!({
            "use_when": ["Need a device shell command that should run through ShadowDroid's JSON envelope rather than raw adb."],
            "output": "shell action JSON with stdout and exit_code when available",
            "side_effects": ["whatever the shell command does"],
            "next_commands": ["device info", "ui dump", "collect"]
        })),
        "device wake" => Some(serde_json::json!({
            "use_when": ["Need to turn the display on before UI automation."],
            "output": "wake action JSON",
            "side_effects": ["wakes the display"],
            "next_commands": ["device unlock", "ui dump"]
        })),
        "device sleep" => Some(serde_json::json!({
            "use_when": ["Need to put the display to sleep for lifecycle, lock-screen, or notification testing."],
            "output": "sleep action JSON",
            "side_effects": ["turns the display off"],
            "next_commands": ["device wake", "device unlock"]
        })),
        "device unlock" => Some(serde_json::json!({
            "use_when": ["Need to dismiss the keyguard before UI automation or app launch."],
            "output": "unlock action JSON",
            "side_effects": ["wakes the device and attempts to dismiss the keyguard"],
            "next_commands": ["app start <pkg>", "ui dump"]
        })),
        "device orientation" => Some(serde_json::json!({
            "use_when": ["Need to read or set screen orientation for layout/responsive testing."],
            "output": "orientation JSON",
            "side_effects": ["sets orientation when a value is provided"],
            "next_commands": ["ui dump", "layout snapshot"]
        })),
        "device clipboard" => Some(serde_json::json!({
            "use_when": ["Need to read or seed clipboard contents during input/share flows."],
            "output": "clipboard JSON",
            "side_effects": ["sets clipboard when a value is provided"],
            "next_commands": ["ui text", "ui dump"]
        })),
        "device notifications" => Some(serde_json::json!({
            "use_when": ["Need to open the notification shade for push/notification flow testing."],
            "output": "notification shade action JSON",
            "side_effects": ["opens the notification shade"],
            "next_commands": ["ui dump", "ui tap", "ui back"]
        })),
        "device quick-settings" => Some(serde_json::json!({
            "use_when": ["Need to open quick settings for system state setup or verification."],
            "output": "quick settings action JSON",
            "side_effects": ["opens quick settings"],
            "next_commands": ["ui dump", "ui tap", "ui back"]
        })),
        "device open-url" => Some(serde_json::json!({
            "use_when": ["Need to launch a deep link, web URL, or intent-resolved flow through ACTION_VIEW."],
            "output": "open_url action JSON",
            "side_effects": ["opens an external or app-handled activity"],
            "next_commands": ["app current", "ui wait", "ui dump"]
        })),
        "files" => Some(serde_json::json!({
            "use_when": ["Need structured push/pull/list operations for files on the device."],
            "output": "file operation JSON",
            "side_effects": ["push/pull write files; ls is read-only"],
            "next_commands": ["files ls <remote>", "files pull <remote> <local>", "files push <local> <remote>"]
        })),
        "files ls" => Some(serde_json::json!({
            "use_when": ["Need to inspect a remote device directory before pulling or debugging artifacts."],
            "output": "remote directory listing JSON",
            "side_effects": ["none"],
            "next_commands": ["files pull <remote> <local>", "device shell"]
        })),
        "files push" => Some(serde_json::json!({
            "use_when": ["Need to copy a host file to the device with optional Unix permissions."],
            "output": "push action JSON",
            "side_effects": ["writes a remote device file"],
            "next_commands": ["files ls <remote-dir>", "device shell"]
        })),
        "files pull" => Some(serde_json::json!({
            "use_when": ["Need to copy a device artifact to the host for inspection or handoff."],
            "output": "pull action JSON",
            "side_effects": ["writes a local host file"],
            "next_commands": ["collect", "files ls <remote-dir>"]
        })),
        "net" => Some(serde_json::json!({
            "use_when": ["Need to enable, inspect, intercept, mutate, export, or replay HTTP(S) traffic."],
            "output": "one JSON object per command; live HTTP events appear on watch after net start",
            "side_effects": ["start/stop/trust/rule/intercept/resume/drop/respond change device proxy, trust, or flow behavior"],
            "next_commands": ["net check <pkg>", "net start", "watch", "net log", "net show <id>", "net intercept"]
        })),
        "net check" => Some(serde_json::json!({
            "use_when": ["Need to know whether a package is likely interceptable before relying on HTTP(S) events."],
            "output": "interceptability verdict JSON with device image, CA store evidence, and recommended trust command",
            "side_effects": ["none"],
            "next_commands": ["net trust", "net start", "watch"]
        })),
        "net trust" => Some(serde_json::json!({
            "use_when": ["Need the device/app to trust ShadowDroid's CA before expecting decrypted HTTPS traffic."],
            "output": "certificate trust/install JSON",
            "side_effects": ["pushes or installs a CA certificate; --auto chooses the best available path; --system may require emulator/root; --ui drives Settings UI"],
            "next_commands": ["net check <pkg>", "net start", "watch"]
        })),
        "net ca" => Some(serde_json::json!({
            "use_when": ["Need to use your own CA (e.g. an existing mitmproxy/Charles/corporate CA the device already trusts) instead of ShadowDroid's generated one, or to inspect/regenerate the signing CA."],
            "output": "CA management JSON (import/info/reset)",
            "side_effects": ["import/reset replace ~/.shadowdroid/net/ca.{crt,key}"],
            "next_commands": ["net ca info", "net trust", "net start"]
        })),
        "net ca import" => Some(serde_json::json!({
            "use_when": ["Have a CA cert+key to reuse as the proxy's signing CA — e.g. a CA already installed on the device/emulator image, so you can skip re-installing trust."],
            "output": "net_ca_import action JSON: resulting CA info, warnings, and next steps",
            "side_effects": ["replaces the signing CA (previous one saved as ca.crt.bak/ca.key.bak); re-run net trust and restart a running proxy so the new CA takes effect"],
            "prerequisites": ["a PEM CA certificate; its private key (in --cert as a combined PEM, or via --key). PKCS#1/SEC1 keys are converted to PKCS#8 via openssl"],
            "next_commands": ["net ca info", "net trust", "net start"],
            "examples": [
                "net ca import --cert mitmproxy-ca.pem",
                "net ca import --cert corp-ca.crt --key corp-ca.key"
            ]
        })),
        "net ca info" => Some(serde_json::json!({
            "use_when": ["Need to confirm which CA the proxy will sign with — its source (generated vs imported), subject, validity, key type, and Android trust-store hash."],
            "output": "net_ca_info action JSON describing the current CA",
            "side_effects": ["none"],
            "next_commands": ["net ca import", "net trust", "net start"]
        })),
        "net ca reset" => Some(serde_json::json!({
            "use_when": ["Need to go back to a freshly generated ShadowDroid CA after importing your own."],
            "output": "net_ca_reset action JSON with the regenerated CA info",
            "side_effects": ["replaces the signing CA with a new generated one (previous one saved as ca.crt.bak/ca.key.bak); re-run net trust afterwards"],
            "next_commands": ["net trust", "net start"]
        })),
        "net start" => Some(serde_json::json!({
            "use_when": ["Need watch to include HTTP(S) events or need to intercept/modify traffic."],
            "output": "action JSON with proxy/device wiring details",
            "side_effects": ["starts host proxy daemon", "sets adb reverse", "sets device global http_proxy"],
            "next_commands": ["watch", "net status", "net check <pkg>", "net intercept"]
        })),
        "net stop" => Some(serde_json::json!({
            "use_when": ["Need to tear down proxy wiring after network testing or restore normal device connectivity."],
            "output": "proxy stop JSON with exact proxy-state restoration plus raw-IP and DNS checks",
            "side_effects": ["stops proxy daemon", "restores the device global http_proxy value captured by net start", "removes adb reverse; --revoke-ca removes trust when possible"],
            "next_commands": ["net status", "doctor --fix"]
        })),
        "net status" => Some(serde_json::json!({
            "use_when": ["Need to verify whether the proxy daemon is running, the device points at it, or flows are held."],
            "output": "net_status action JSON",
            "side_effects": ["none"],
            "next_commands": ["net start", "net stop", "watch"]
        })),
        "net log" => Some(serde_json::json!({
            "use_when": ["Need recent HTTP flows from the session log without watching live UI."],
            "output": "line-delimited JSON: http (and tls_error, when an app rejected the CA) events in ts order, followed by a net_log summary object",
            "side_effects": ["none"],
            "next_commands": ["net show <id>", "net export har <id>", "watch"]
        })),
        "net show" => Some(serde_json::json!({
            "use_when": ["Need headers, bodies, or full detail for a flow id seen in watch or net log."],
            "output": "flow detail JSON; --body-file writes the captured response body to a file for large responses",
            "side_effects": ["none"],
            "next_commands": ["net resume <id>", "net respond <id>", "net export har <id>"]
        })),
        "net intercept" => Some(serde_json::json!({
            "use_when": ["Need the agent to pause matching HTTP flows and decide how to mutate, drop, or respond."],
            "output": "held flows appear as http_intercept events on watch",
            "side_effects": ["matching app HTTP calls block until released or timed out"],
            "next_commands": ["watch", "net show <id>", "net resume <id>", "net drop <id>", "net respond <id>"]
        })),
        "net resume" => Some(serde_json::json!({
            "use_when": ["Need to release a held flow, optionally with status/header/body/url mutations."],
            "output": "release result JSON",
            "side_effects": ["unblocks a held HTTP flow"],
            "next_commands": ["watch", "net log", "ui dump"]
        })),
        "net drop" => Some(serde_json::json!({
            "use_when": ["Need the app to experience a held request as a connection failure or explicit status."],
            "output": "release result JSON",
            "side_effects": ["unblocks a held HTTP flow with failure behavior"],
            "next_commands": ["watch", "ui dump"]
        })),
        "net respond" => Some(serde_json::json!({
            "use_when": ["Need to short-circuit a held request with a canned response without contacting upstream."],
            "output": "release result JSON",
            "side_effects": ["unblocks a held HTTP flow with a synthetic response"],
            "next_commands": ["watch", "ui dump"]
        })),
        "net export" => Some(serde_json::json!({
            "use_when": ["Need to turn captured flows into HAR, curl, or deterministic response fixtures for replay/testing."],
            "output": "export report and files in the requested format",
            "side_effects": ["writes export files"],
            "next_commands": ["net replay", "net rules", "collect"]
        })),
        "net replay" => Some(serde_json::json!({
            "use_when": ["Need to serve saved responses without the real backend for deterministic app testing."],
            "output": "replay setup/action JSON",
            "side_effects": ["starts or configures replay behavior for matching traffic"],
            "next_commands": ["net start", "watch", "ui dump"]
        })),
        "net rule" => Some(serde_json::json!({
            "use_when": ["Need declarative request/response mutation rules for repeated network scenarios."],
            "output": "rule management JSON",
            "side_effects": ["add/rm/clear mutate active proxy rules"],
            "next_commands": ["net rule add", "net rule list", "watch"]
        })),
        "net rule add" => Some(serde_json::json!({
            "use_when": ["Need to add one response/request mutation rule such as map-local, set-status, replace, block, or delay."],
            "output": "rule add JSON with rule id",
            "side_effects": ["mutates active proxy rules"],
            "next_commands": ["net rule list", "watch", "net rule rm <id>"],
            "examples": [
                "net rule add map-local response.json --host api.example.com --path /v1/dict",
                "net rule add set-status 503 --host api.example.com"
            ]
        })),
        "net override" => Some(serde_json::json!({
            "use_when": ["Need the shortest path to serve one local file for a matching URL without remembering the map-local positional form."],
            "output": "net_override action JSON with created rule id",
            "side_effects": ["adds an active map-local proxy rule"],
            "prerequisites": ["net start must be running"],
            "next_commands": ["watch", "net rule list", "net rule rm <id>"],
            "examples": ["net override --url 'https://api.example.com/v1/dict*' --file response.json"]
        })),
        "net rule list" => Some(serde_json::json!({
            "use_when": ["Need to inspect currently active proxy mutation rules."],
            "output": "active rules JSON",
            "side_effects": ["none"],
            "next_commands": ["net rule add", "net rule rm <id>", "net rule clear"]
        })),
        "net rule rm" => Some(serde_json::json!({
            "use_when": ["Need to remove one active network mutation rule by id without clearing the full scenario."],
            "output": "rule remove JSON",
            "side_effects": ["removes one active proxy rule"],
            "prerequisites": ["obtain the rule id from net rule list or net rule add"],
            "next_commands": ["net rule list", "watch"]
        })),
        "net rule clear" => Some(serde_json::json!({
            "use_when": ["Need to remove all active network mutation rules before another test."],
            "output": "rule clear JSON",
            "side_effects": ["removes all active proxy rules"],
            "next_commands": ["net rule list", "watch"]
        })),
        "net rules" => Some(serde_json::json!({
            "use_when": ["Need to apply a bulk JSON rule file for a repeatable network scenario."],
            "output": "bulk rule apply JSON",
            "side_effects": ["replaces or mutates active proxy rules from a file"],
            "next_commands": ["net rule list", "watch", "ui dump"]
        })),
        "aar" => Some(serde_json::json!({
            "use_when": ["Need the in-app debug agent path for apps you can build: above-TLS capture/intercept (pinned/Cronet traffic) or in-process coroutine dumps."],
            "output": "AAR install/status/capture/intercept/coroutines JSON or human setup reports",
            "side_effects": ["install/remove mutate project files; intercept/resume/drop affect in-app flows"],
            "next_commands": ["aar status", "aar install", "aar agent", "aar capture", "aar coroutines"]
        })),
        "aar install" => Some(serde_json::json!({
            "use_when": ["Need to wire the debug-only ShadowDroid in-app agent AAR into a Gradle app project."],
            "output": "install report with dependency/AAR path/build status",
            "side_effects": ["copies the AAR into the project", "edits the app module Gradle build file", "--build runs assembleDebug"],
            "prerequisites": ["run in or pass --project-root for an app you can build"],
            "next_commands": ["aar status", "app install", "aar agent"]
        })),
        "aar status" => Some(serde_json::json!({
            "use_when": ["Need to verify whether the in-app agent AAR is wired into the project before relying on AAR capture/intercept commands."],
            "output": "AAR wiring status JSON/human report",
            "side_effects": ["none"],
            "next_commands": ["aar install", "doctor --app <pkg>", "aar agent"]
        })),
        "aar remove" => Some(serde_json::json!({
            "use_when": ["Need to remove ShadowDroid-managed AAR wiring from a project."],
            "output": "remove report",
            "side_effects": ["edits the app module Gradle build file", "removes copied AAR file"],
            "next_commands": ["aar status"]
        })),
        "aar agent" => Some(serde_json::json!({
            "use_when": ["Need running in-app agent status: package, armed matcher, held flows, and capture count."],
            "output": "running agent status JSON/human report",
            "side_effects": ["none"],
            "prerequisites": ["debug build with AAR installed must be running"],
            "next_commands": ["aar capture", "aar intercept", "doctor --app <pkg>"]
        })),
        "aar capture" => Some(serde_json::json!({
            "use_when": ["Need to drain above-TLS HTTP(S) flows captured by the in-app agent, including pinned/Cronet cases."],
            "output": "captured flow JSON or exported artifacts",
            "side_effects": ["--drain clears the in-app capture buffer", "export/write options create files"],
            "prerequisites": ["debug build with AAR installed must be running"],
            "next_commands": ["net export", "aar intercept", "watch"]
        })),
        "aar intercept" => Some(serde_json::json!({
            "use_when": ["Need to arm or clear in-app above-TLS interception for matching flows."],
            "output": "intercept arm/clear JSON",
            "side_effects": ["matching in-app flows may be held until resume/drop"],
            "prerequisites": ["debug build with AAR installed must be running"],
            "next_commands": ["aar agent", "aar resume <id>", "aar drop <id>"]
        })),
        "aar resume" => Some(serde_json::json!({
            "use_when": ["Need to release a held in-app agent flow, optionally mutating status/body/content type."],
            "output": "resume result JSON",
            "side_effects": ["unblocks a held in-app flow"],
            "next_commands": ["aar agent", "aar capture", "ui dump"]
        })),
        "aar drop" => Some(serde_json::json!({
            "use_when": ["Need the app to experience a held in-app agent flow as a connection failure."],
            "output": "drop result JSON",
            "side_effects": ["unblocks a held in-app flow with failure behavior"],
            "next_commands": ["aar agent", "ui dump"]
        })),
        "aar coroutines" => Some(serde_json::json!({
            "use_when": ["Need to find a leaked coroutine, a stuck job, or a clogged SharedFlow: dump every live coroutine (state, context, stacks) from the running app without attaching a debugger."],
            "output": "state counts + per-coroutine context/stacks JSON or human summary; --dump/-o adds the full DebugProbes text dump",
            "side_effects": ["none"],
            "prerequisites": ["debug build with AAR installed must be running", "probes activation wired via `aar install --coroutine-probes` (otherwise the dump reports installed-but-inert)"],
            "next_commands": ["aar agent", "debug attach", "watch"]
        })),
        _ => None,
    }
}
