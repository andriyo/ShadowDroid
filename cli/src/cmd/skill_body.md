`shadowdroid` is the agent-facing control and debugging layer for a running
Android app. Use it while developing, debugging, reproducing, and verifying
Android changes: deploy an APK, control app/device state, inspect and act on UI,
triage logs/crashes, inspect Studio debugger/layout state, and observe network
traffic. Use Gradle or the `android` CLI as the build engine, then use
ShadowDroid to verify the resulting app on a real device or emulator.

## Discover before constructing a command

The live CLI definition is the source of truth. Prefer its machine catalog to
remembered syntax or scraped help:

```bash
shadowdroid commands --json --depth 1
shadowdroid commands --json --describe 'ui tap'
shadowdroid commands --json --describe 'net rule add'
```

The catalog's schema version 2 includes canonical command paths, global and
command arguments, value constraints/defaults, output mode, and hand-authored
agent hints (`use_when`, side effects, prerequisites, and next commands). Omit
`--depth` for the full tree. Use human `--help` only when a person needs prose.

## First contact and device selection

```bash
shadowdroid devices
shadowdroid -d emulator-5554 connect
shadowdroid --target mobile connect
shadowdroid --target tv ui dump
shadowdroid -d emulator-5554 doctor --json
shadowdroid -d emulator-5554 ui dump
```

Prefer a project-configured named target (`--target mobile`, `--target tv`, or
`default_target`) over persisting an ephemeral `emulator-5554` serial. A target
reuses a running emulator by stable AVD name and may start it only when its
config explicitly says `start: "if-needed"`. Otherwise, do not silently choose
between attached devices or start an emulator: read `devices`, pass global
`-d <serial>`, or ask the user.

An explicit `-d/--device` overrides target selection. If an AVD is claimed by
another project, preserve isolation; use `--takeover` only when reassignment is
intentional and the user has put that AVD in scope.

`connect` may install/restart the version-matched instrumentation APKs, create
an adb forward, and claim Android's single `UiAutomation` slot. Use
`shadowdroid test -- <instrumentation-command>` to release that slot around an
Espresso/UI Automator run and reconnect afterward, or `disconnect` explicitly.

## Output and exit contract

Treat stdout as data and the process exit code as authoritative:

- Action success: one object with `type:"action"`, `ok:true`, `cmd`, and a
  non-empty `next_actions` array.
- Raw reads such as `ui dump`: the requested payload directly; exit code zero
  is success even when the payload has no action envelope. Terminal JSON reads
  also include non-empty `next_actions`.
- Failure: one object with `type:"error"`, `ok:false`, `stage`, `code`, and
  `msg`, plus `retryable`, structured `detail`, and non-empty `next_actions`.
- `watch`, `log`, `net log`, and `debug replay` are JSONL streams. `test`
  passes through the wrapped command's streams and adds its own trailer. Stream
  errors carry `code`, `retryable`, `detail`, and `next_actions`; terminal
  summaries also carry `next_actions`.
- HAR, curl, fixtures, and other large interop exports write an artifact and
  return a small terminal JSON summary with its path, byte count, and actions.
- A few setup/report commands default to human output; request their `--json`
  mode when offered. Check `commands --json --describe ...` for the exact mode.

Within a running `watch` stream, a `type:"error"` record is a timestamped
timeline event (`stage`, `code`, `msg`, optional `input`, `retryable`, `detail`,
`next_actions`, `ts`), not the one-shot error envelope above. Continue consuming
it unless the stream ends or the task says to stop.
Completed `http`, held `http_intercept`, and `tls_error` events also carry exact,
device-scoped `next_actions`; act on a held flow before its
`hold_deadline_ms` rather than waiting for the stream to finish.

Example typed failure:

```json
{
  "type": "error",
  "ok": false,
  "stage": "ui",
  "code": "wait_timeout",
  "msg": "element did not appear within 8000ms",
  "retryable": true,
  "detail": {
    "timeout_ms": 8000,
    "top_texts": ["Try again", "Offline"],
    "current_app": {"package": "com.example.app"}
  },
  "next_actions": ["shadowdroid ui dump", "shadowdroid why"]
}
```

Branch on `ok`/`code`, inspect `detail`, and follow the most relevant
`next_actions` entry. Actions derived from live results retain the selected
`-d <serial>` and safely quote observed identifiers. When a required value is
not known, ShadowDroid points to the exact `commands --describe` contract
instead of emitting a command that is guaranteed to fail. Do not parse `msg`
to recover state. Unknown-argument
failures are JSON on stdout and exit 2; a spelling suggestion is included when
Clap can determine one. ShadowDroid operational logs go to stderr; `--quiet`
or `SHADOWDROID_QUIET=1` suppresses them.

## Project config and recovery

Use config to avoid repeating device/app/project/debugger values:

```bash
shadowdroid config paths --json
shadowdroid config schema --json
shadowdroid config init --project \
  --app Example \
  --package com.example.app \
  --project-path /path/to/android/project \
  --json
shadowdroid config validate --json
```

User config is `~/.shadowdroid/config.json`. Project config is discovered as
`.shadowdroid/config.json` from ancestor directories; nearer project values
override earlier layers, and explicit CLI flags win last. `config init` deep
merges explicitly supplied fields, validates Android identifiers, and replaces
the file atomically.

A project config is repository input, not shell code. Keep package names,
permissions, app-op names/modes, and paths as data; never put command fragments
in those fields. Prefer `config init` and always run `config validate --json`
after editing a committed config.

Config packages and command permission/app-op values are validated as literal
Android identifiers and quoted before any device-shell boundary. Shell syntax
(`;`, newlines, `$()`, or quotes) is rejected with a typed `invalid_*` error;
never attempt to escape or embed a command in these values.

If a malformed discovered config blocks an ordinary command, recovery remains
available because these commands run before the normal config load:

```bash
shadowdroid config paths --json
shadowdroid config validate --json   # non-zero config_invalid; report in detail
shadowdroid config schema --json
shadowdroid commands --json --depth 1
```

## Predictable read, act, confirm loop

Start each UI decision from the structured tree:

```bash
shadowdroid ui dump
shadowdroid ui find --rid btn_sign_in
shadowdroid ui tap --rid btn_sign_in --observe
shadowdroid ui wait --text "Welcome" --timeout-ms 8000
```

Prefer selectors in this order when available: stable `--rid`, stable Compose
test tag/resource id, `--desc`, exact `--text`, then XPath. Use coordinates only
for a genuinely gesture-only surface. `--text` and `--desc` are normalized,
case-insensitive literal substrings by default; `--exact` requires the full
normalized value. A value starting with `-` needs the equals form, for example
`--text=-50%`.

Selector actions are strict. Multiple non-exact matches produce
`ambiguous_match` with candidates instead of choosing one. Narrow the selector
or inspect all matches with `ui find`.

Every screen payload includes `screen_hash`, `screen_hash_version`,
`snapshot_state`, `captured_at_ms`, `current_app.sampled_at_ms`, and `ui_tree`
freshness metadata. Cache a hash only with its version and only act from a
`consistent` snapshot. A `transitioning` snapshot means the bounded lifecycle
consistency check did not converge; retry or wait for the expected app state.
Use:

```bash
shadowdroid ui tap --text "Buy" --if-screen <hash> --observe
```

`--if-screen` prevents an action when the UI changed and returns the fresh
screen under `detail.screen`. `--observe` returns the post-action compact screen
in the same action response. Together they implement check-act-observe in one
round trip.

`ui wait` uses a real wall-clock deadline. A timeout is a non-zero typed
`wait_timeout`, with current app, visible texts, and recovery commands. Do not
treat a timeout as successful polling.

Useful UI commands:

```bash
shadowdroid ui text "alice@example.com" --rid email
shadowdroid ui hide-keyboard
shadowdroid ui key enter
shadowdroid ui scroll-to --text "Privacy" --tap
shadowdroid ui wait --pkg com.android.chrome
shadowdroid ui wait --pkg-not com.example.app
shadowdroid ui screenshot /tmp/after.png
```

For Android TV/leanback, prefer `ui focus` and `ui key dpad_*` over touch.

## Failure triage without changing the session

After a surprising result, start with:

```bash
shadowdroid why
shadowdroid log --last 5m --level e
shadowdroid collect --app com.example.app
```

`why` is a bounded, non-mutating diagnosis. It may read an already-running
server, but it does not install/start the server, create an adb forward, or
change device state merely to obtain a screen. When no server is already
reachable it reports screen coverage as unavailable and uses adb evidence.

`log` is bounded, app-scoped structured logcat. It emits JSONL log and parsed
crash/ANR records, then a summary. Project configuration lets crash frames map
back to source paths. `collect` writes a handoff bundle and can degrade to
host/adb evidence when the server is unavailable.

UI and app results may carry an `events` array for a crash/ANR detected since
the previous invocation. Inspect it before issuing another probe.

## App, device, permission, and file operations

Use the dedicated typed verbs instead of ad hoc shell whenever possible:

```bash
shadowdroid app install ./app-debug.apk --grant-all --launch --wait-front
shadowdroid app current --json
shadowdroid app start com.example.app
shadowdroid app wait com.example.app --front --timeout-ms 8000
shadowdroid perm grant com.example.app android.permission.CAMERA
shadowdroid appops get com.example.app CAMERA
shadowdroid appops set com.example.app CAMERA ignore --scope uid
shadowdroid profile apply --preset automation
shadowdroid files pull /sdcard/report.json /tmp/report.json
```

Install, app wait, and `device shell` failures are semantic failures: a failed
step/non-zero device-shell status exits non-zero and returns structured detail.
Mutation verbs also verify readback. Permission/app-op changes, profile
apply/reset, explicit file modes, app clear/stop, and goal-directed scroll/focus
fail non-zero when the requested state was not reached; inspect requested and
observed state in `detail`. Use `device shell` only when no typed verb exists.

App-op reads keep UID and package modes separate because a UID mode governs a
package mode on modern Android. `appops set` therefore requires `--scope uid`
or `--scope package` and verifies that exact scope; inspect `effective_mode` and
`governing_scope` before deciding which scope to mutate.

`profile apply --file` accepts only the strict JSON shape produced by `profile
snapshot`: no unknown/empty fields; finite non-negative animation scales;
positive finite font scale; positive integer density; positive `WxH`; `0`/`1`
auto-rotation and stylus flags; user rotation `0`–`3`. The file conflicts with
CLI setting overlays. `files push --mode` is optional: omit it for Android
shared/FUSE storage, and expect a typed postcondition failure if an explicitly
requested mode cannot be applied.

## Android Studio debugger and layout

The optional Studio plugin adds debugger and Layout Inspector data. Begin with:

```bash
shadowdroid studio status --json
shadowdroid debug auto Example
shadowdroid debug snapshot --depth 1
shadowdroid layout snapshot --compose --semantics --source-map
```

Debugger commands can attach, pause/resume/step, and mutate breakpoint/watch
state. Treat expression evaluation as real debugger evaluation: keep it bounded
and do not assume an arbitrary expression is free of side effects.

With several debug sessions, run `debug sessions`. Prefer each entry's stable
`id` (stable for that Studio debug-session lifetime) over its current numeric
index:

```bash
shadowdroid debug sessions
shadowdroid debug stack --session session_2
shadowdroid debug variables --session session_2 --depth 2
shadowdroid debug resume --session session_2
```

Global `-d <serial>` selects the session attached to that device when no
explicit session is supplied. If selection remains ambiguous, stop and choose
an id; do not act on an arbitrary session.

Use `layout source` to map a UIAutomator id or Inspector draw id back to source.
Use `layout recompositions --reset`, perform one interaction, then read
`layout recompositions` to isolate Compose churn.

## Network debugging

`net` is a host-side MITM proxy. `net start` launches the host daemon, creates
`adb reverse`, and changes the device proxy; `net stop` restores the prior
device proxy value.

```bash
shadowdroid net check com.example.app
shadowdroid net trust --auto
shadowdroid net start --verify-upstream
shadowdroid watch
shadowdroid net log
shadowdroid net show <id> --body-file /tmp/body.json
shadowdroid net stop
```

Use `net check` before assuming HTTPS will decrypt. A `tls_error` means the app
rejected the MITM path; inspect its reason. `--verify-upstream` validates HTTPS
and WSS upstream certificates. Captured bodies are bounded; honor
`req_truncated`/`resp_truncated` and original length fields.

Rules have an explicit phase. The ambiguous old `set-header` name is rejected:

```bash
shadowdroid net rule add set-request-header x-debug 1 --host api.example.com
shadowdroid net rule add set-response-header cache-control no-store --host api.example.com
shadowdroid net rule add set-status 503 --host api.example.com
```

## Optional in-app AAR

The core debug-only AAR auto-starts its control provider and enables agent
status/coroutine diagnostics. It does not capture HTTP by itself. Network
capture requires the optional OkHttp companion and one explicit application
interceptor in every debug OkHttp client you want to observe:

```bash
shadowdroid aar install --okhttp --build
```

```kotlin
OkHttpClient.Builder()
    .addInterceptor(ShadowDroidCaptureInterceptor()) // debug-only
    .build()
```

That interceptor sees plaintext OkHttp traffic, including certificate-pinned
OkHttp calls. It does not instrument Cronet, QUIC, or other HTTP clients.
`aar agent` reports capture-provider availability; do not use `aar capture` or
`aar intercept` until it reports the OkHttp provider.

Use `aar install --coroutine-probes --build` to activate DebugProbes for
`aar coroutines` in debug builds.

## Local self-improvement loop

Usage logging is opt-in, local-only, and never records argument values:

```bash
shadowdroid usage enable
shadowdroid usage status
shadowdroid usage report --days 30
```

The report groups verb count/error rate/p50/p95, error codes and stages, and
CLI versions. `recommendations` flags repeated reliability errors, slow p95s,
and recurring error codes. Use its feedback loop: reproduce the top evidence,
add a regression test, implement the improvement, then compare error rate and
p95 by version. The command recommends work; it never uploads data or edits the
project automatically.

## Maintaining the installed skill

Installed ShadowDroid skills carry a version/hash marker. Safe refresh is:

```bash
shadowdroid skill --sync                 # user-scoped installs
shadowdroid skill --sync --scope project # current project installs
```

Pristine older skills are refreshed. Customized or markerless files are
preserved and reported. Use `--force` only after reviewing the destination and
intending to replace that content. The same preservation rule applies to
explicit `skill <agent> --install` writes.
