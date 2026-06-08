# Agent Debugging

ShadowDroid's debug commands are designed for AI agents that need a fast,
bounded view of a running Android app without scraping Android Studio UI panes.

## One-shot State

```bash
shadowdroid debug snapshot --app com.example.app --depth 1 | jq
```

The snapshot includes device/build info, foreground app/activity/PID, screen
hash and element tree, screenshot path/hash, recent logcat, Android Studio
debugger sessions, current stack, top-frame variables, watches, and breakpoints.
If the Android Studio plugin is not running, the debugger section reports
`available: false` while the device/UI portions still work.

## Timeline Record / Replay

```bash
shadowdroid debug record --app com.example.app --out /tmp/run.jsonl --duration-ms 15000
shadowdroid debug replay /tmp/run.jsonl --dry-run
```

Records are JSONL timelines with screen changes, app lifecycle changes, logcat
lines, debugger snapshots, screenshots, and replayable action events. Replay is
intentionally conservative: it replays supported action events such as taps,
text, keys, swipes, drags, and app starts.

## Android Studio Debugger Bridge

```bash
shadowdroid debugger clients --project /path/to/app --package com.example.app
shadowdroid debugger attach --project /path/to/app --package com.example.app
shadowdroid debugger break line --file app/src/main/java/Foo.kt --line 42 --condition 'state != null'
shadowdroid debugger breakpoints
shadowdroid debugger stack --limit 32 --timeout-ms 2500
shadowdroid debugger variables --thread 0 --frame 0 --depth 2 --timeout-ms 2500
shadowdroid debugger eval 'this.presenter.state' --thread 0 --frame 0 --depth 2 --timeout-ms 5000
shadowdroid debugger watch add 'this.presenter.state'
shadowdroid debugger watch list --depth 2 --timeout-ms 2500
```

Breakpoints have stable IDs and can be updated or removed:

```bash
shadowdroid debugger break update --id bp_... --enabled false
shadowdroid debugger break update --id bp_... --suspend none --log-message true
shadowdroid debugger break remove --id bp_...
```

Supported breakpoint creation includes line breakpoints, exception breakpoints,
wildcard method breakpoints, and source-line field watchpoints. Line and field
breakpoints support `--temporary` for remove-on-hit behavior. Pass counts,
conditions, logpoints, and suspend policy are exposed where Android Studio's
debugger APIs support them. `hit_count` is ShadowDroid-observed: it increments
when a suspended debugger session lands on a matching line breakpoint, and
logpoint callbacks are counted when Studio emits them.

Expression evaluation is deterministic and read-only: `this`, visible locals,
fields, and array indexes are supported. Arbitrary code execution is deliberately
not enabled for the first agent-facing surface. Object values include stable
per-session `object_handle` values. Watches are cached and refreshed whenever a
debug session suspends, then also evaluated on demand by `debugger watch list`
when a suspended frame is available.

Debugger read commands use bounded IDE/JDI requests. When a session is running,
missing, or stopped on a frame without debug information, stack/threads/variables
and eval return structured `ok: false` JSON or a warning instead of blocking.

## Continue-until Primitives

```bash
shadowdroid debugger continue-until --file app/src/main/java/Foo.kt --line 42 --timeout-ms 10000
shadowdroid debugger continue-until --condition 'state.ready' --timeout-ms 10000
shadowdroid debug step-until-screen-change --app com.example.app --timeout-ms 10000
shadowdroid debug step-until-log --pattern 'Loaded profile' --app com.example.app
shadowdroid debug run-until-crash --app com.example.app --timeout-ms 30000
```

The `debug ...` variants return a final `debug_snapshot` so the caller gets the
screen, logs, debugger state, and screenshot at the end of the wait.

## Layout

```bash
shadowdroid layout snapshot --compose --semantics --source-map --screenshot -o /tmp/layout.json
shadowdroid layout diff /tmp/before.json /tmp/after.json
shadowdroid layout source --id 12
shadowdroid layout source --draw-id=-2073001771
shadowdroid layout recompositions --reset
```

Layout snapshots always include ShadowDroid's deterministic
UIAutomator/accessibility tree. When the Android Studio plugin is running and
Layout Inspector has an active model for the app, `--compose`, `--semantics`,
and `--source-map` add Studio-backed windows, nodes, Compose source locations,
semantics flags, and recomposition counters. If Studio or Layout Inspector is
not active, the response keeps the device tree and reports the enrichment as
unavailable.

Use `layout source --id` for ShadowDroid UIAutomator elements and
`layout source --draw-id` for Android Studio Layout Inspector nodes returned by
`layout snapshot`.
