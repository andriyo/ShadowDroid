# Agent Debugging

ShadowDroid's debug commands are designed for AI agents that need a fast,
bounded view of a running Android app without scraping Android Studio UI panes.

## One-shot State

```bash
shadowdroid debug auto Livd | jq
shadowdroid debug snapshot --app com.example.app --depth 1 | jq
```

`debug auto` is the low-effort entry point. It accepts a config alias, package,
or installed app label; with no argument it uses config and then the foreground
app. It launches the app, asks Android Studio to attach its debugger when the
bridge is available, then returns a full `debug_snapshot`. If Studio or the
plugin is missing, the response includes `available:false`, `shadowdroid init`,
and `shadowdroid doctor` guidance instead of failing the device/UI snapshot.

The snapshot includes device/build info, foreground app/activity/PID, screen
hash and element tree, screenshot path/hash, recent logcat, Android Studio
debug sessions, current stack, top-frame variables, watches, and breakpoints.
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
shadowdroid init
shadowdroid doctor
shadowdroid debug clients --project /path/to/app --package com.example.app
shadowdroid debug attach --project /path/to/app --package com.example.app
shadowdroid debug break line --file app/src/main/java/Foo.kt --line 42 --condition 'state != null'
shadowdroid debug breakpoints
shadowdroid debug stack --limit 32 --timeout-ms 2500
shadowdroid debug variables --thread 0 --frame 0 --depth 2 --timeout-ms 2500
shadowdroid debug eval 'this.presenter.state' --thread 0 --frame 0 --depth 2 --timeout-ms 5000
shadowdroid debug inspect 'this.presenter.state' --depth 2
shadowdroid debug inspect --handle obj_s0_e2_123 --path '.items[0]' --depth 2
shadowdroid debug watch add 'this.presenter.state'
shadowdroid debug watch list --depth 2 --timeout-ms 2500
```

Breakpoints have stable IDs and can be updated or removed:

```bash
shadowdroid debug break update --id bp_... --enabled false
shadowdroid debug break update --id bp_... --suspend none --log-message true
shadowdroid debug break remove --id bp_...
```

Supported breakpoint creation includes line breakpoints, exception breakpoints,
wildcard method breakpoints, and source-line field watchpoints. Line and field
breakpoints support `--temporary` for remove-on-hit behavior. Pass counts,
conditions, logpoints, and suspend policy are exposed where Android Studio's
debugger APIs support them. `hit_count` is ShadowDroid-observed: it increments
when a suspended debugger session lands on a matching line breakpoint, and
logpoint callbacks are counted when Studio emits them.

Expression evaluation and inspection are deterministic and read-only: `this`,
visible locals, fields, and array indexes are supported. Arbitrary code
execution, getters, method calls, and mutation are deliberately not enabled.
Object values include per-session/per-suspend `object_handle` values; use
`debug inspect --handle ... --path ...` to expand a returned object until the
session resumes. Watches are cached and refreshed whenever a debug session
suspends, then also evaluated on demand by `debug watch list` when a suspended
frame is available.

Debugger read commands use bounded IDE/JDI requests. When a session is running,
missing, or stopped on a frame without debug information, stack/threads/variables
and eval return structured `ok: false` JSON or a warning instead of blocking.

Put repeated debugger values in `~/.shadowdroid/config.json` or a project
`.shadowdroid.json` so agents can use shorter commands:

```bash
shadowdroid config schema --json
shadowdroid config init --project --app Example --package com.example.app --project-path /path/to/app
shadowdroid config validate --json
shadowdroid debug auto
```

```json
{
  "app": "Example",
  "project": "/path/to/app",
  "apps": {
    "Example": {
      "package": "com.example.app",
      "run_configuration": "app"
    }
  }
}
```

## Continue-until Primitives

```bash
shadowdroid debug continue-until --file app/src/main/java/Foo.kt --line 42 --timeout-ms 10000
shadowdroid debug continue-until --condition 'state.ready' --timeout-ms 10000
shadowdroid debug step-until-screen-change --app com.example.app --timeout-ms 10000
shadowdroid debug step-until-log --pattern 'Loaded profile' --app com.example.app
shadowdroid debug run-until-crash --app com.example.app --timeout-ms 30000 --bundle /tmp/crash
```

The `debug ...` variants return a final `debug_snapshot` so the caller gets the
screen, logs, debugger state, and screenshot at the end of the wait.
`run-until-crash` emits a parsed Java/native/ANR crash event, works without
Studio, and can write a local bundle with logcat, snapshot, and best-effort
ANR/tombstone artifacts.

## Native / Mixed Mode

```bash
shadowdroid debug auto Example --mode mixed
shadowdroid debug attach --package com.example.app --mode native
shadowdroid debug native status --app Example
shadowdroid debug tombstones list
shadowdroid debug tombstones pull -o /tmp/tombstones
```

`--mode auto|java|native|mixed` is semantic: ShadowDroid asks Studio for a
matching Android debugger when one is available, while `--debugger` remains the
exact debugger-id escape hatch. Native live control is intentionally not exposed
yet; native status reports client ABI/native-debuggable metadata, mixed-mode
session metadata, and local artifact availability.

## Coroutines

```bash
shadowdroid debug coroutines snapshot --session 0 --limit 64 --depth 1
shadowdroid debug coroutines threads --session 0
shadowdroid debug coroutines continuation --thread 0 --frame 0 --depth 2
shadowdroid debug coroutines flow --expr 'this.viewModel.state' --depth 2
```

Coroutine insight is suspended-state, read-only JDI inference. It reports
physical debugger threads, dispatcher hints, continuation-like objects,
`label`/`completion` fields, spilled locals such as `L$0`, and field-only
Flow/StateFlow object state. It does not install DebugProbes, collect flows,
call getters, cancel jobs, or claim exact IDE coroutine-debugger state.

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
