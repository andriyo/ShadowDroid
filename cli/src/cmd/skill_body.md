`shadowdroid` is a single static binary that drives Android apps and emits one
JSON line per action. A tiny on-device UI Automator service makes screen dumps
~25 ms, so the observe→act→observe loop stays responsive. It talks to the device
over `adb`; no Appium, no Python.

## When to use it

Reach for ShadowDroid whenever a task touches the **live UI of an Android app**:
navigate to a screen, tap/type, take a screenshot, reproduce a crash, exercise a
flow, or inspect what's on screen. It is *not* for building/compiling the app
(Gradle, Kotlin source) — only for observing or acting on a running app.

## First contact

```bash
shadowdroid devices                 # attached devices as JSON: serial, state, model, android version
shadowdroid connect                 # install + start the on-device service
shadowdroid commands --json         # the full command catalog, for discovery
shadowdroid config schema --json    # config format, paths, fields, example
shadowdroid ui dump | jq            # current UI as a flat element list
```

If no device is attached, ask the user to start one — don't boot an emulator
silently. With multiple devices, read `devices` (each entry has `model` /
`android_release` / `state`) and pass `-d <serial>`.

## Low-token project config

Use config when the same device/app/project/package flags would repeat across
commands. Prefer `config init` over hand-writing JSON, then validate before
relying on it:

```bash
shadowdroid config paths --json
shadowdroid config init --project --app Example --package com.example.app --project-path /path/to/app
shadowdroid config validate --json
```

Project config lives in `.shadowdroid.json`; user config lives in
`~/.shadowdroid/config.json`. Project config wins over user config. Minimal
project config:

```json
{
  "app": "Example",
  "apps": {
    "Example": {
      "package": "com.example.app",
      "project": "/path/to/app",
      "run_configuration": "app"
    }
  }
}
```

## The driving loop

Read the screen, act by **selector** (don't hard-code coordinates), confirm.

```bash
shadowdroid ui dump | jq '.elements[] | {id, text, rid, tap}'
shadowdroid ui dump | jq '.ime'     # keyboard_visible + focused input context
shadowdroid ui tap --text "Sign in"        # or --rid / --desc / --xpath, or `ui tap <id>`
shadowdroid ui text "alice@example.com"    # focused field; add --rid/--text/--id to target one
shadowdroid ui hide-keyboard        # safe no-op when the keyboard is already hidden
shadowdroid ui key enter
shadowdroid ui scroll-to --text "Privacy" --tap   # scroll a list until found, then tap
shadowdroid ui wait --text "Welcome" --timeout-ms 8000   # block until it appears; result echoes the matched element + current_app
shadowdroid ui wait --pkg com.android.chrome      # wait for the foreground app to BE this (e.g. a Custom Tab / share sheet opened)
shadowdroid ui wait --pkg-not com.example.app     # wait for the foreground to LEAVE this app (returned, or an external app took over)
shadowdroid ui screenshot /tmp/after.png          # writes the PNG; result includes width/height + screen_hash (compare to ui wait's)
```

`--text`/`--desc` match a **normalized, case-insensitive substring** by default:
before comparing, surrounding whitespace is collapsed, curly quotes/ellipsis are
folded to ASCII, and zero-width characters are stripped — so `--text "sign in"`
matches a `SIGN IN` button and `--text "Don't"` matches a curly apostrophe. Add
`--exact` (on `ui find`/`tap`/`text`/`wait`/`focus`) for a full-string match.

Matching is **literal**: `*`, `.`, `?`, `[`, `$`, … match those characters — there
are no wildcards or regex, so `--text "Bask*t"` will not match "Basket". (A
selector value that *starts* with `-`, like `-50%`, must use the equals form
`--text=-50%` so it isn't parsed as a flag.)

Selector **actions** are **strict**: if `ui tap`/`text`/`focus` matches several
elements and none is an exact match, they return `{"type":"error",
"code":"ambiguous_match", ...}` listing the candidates — narrow with `--exact`,
`--rid`, or `--clickable` (or use `ui find` to inspect all matches first). On a
hit, `ui tap`/`find`/`wait`/`scroll-to`/`focus` all use the same shape — a
`matched` boolean plus the echoed node under `element` (`rid`/`tap`/`text`) — so
you can confirm the right node the same way for every command.

For a long flow, stream every change and watch for crashes:

```bash
shadowdroid watch --app com.example.app | jq -c .
# emits ready → screen_compact → http/http_intercept when `net start` is running
# emits a structured warning with `suggested_command` when network capture is unavailable
```

`watch` is the unified live timeline. It tries to attach HTTP(S) events by
default, warns when the net proxy is unavailable, and accepts `--no-net` only
when you intentionally want UI/crash-only events.

## Network capture notes

Use `net check <pkg>` before relying on HTTPS capture. It reports the device
image kind (Play Store vs Google APIs/AOSP), CA store evidence, and the
recommended trust command (`net trust --auto` for rootable images, `net trust
--ui` for locked/Play images). For targetSdk 24+ apps, user-store CA trust can
still be conditional on the app's Network Security Config; prove the final loop
by running `net start`, exercising the app, and observing a decrypted `http`
event.

If `net log`/`watch` show no flows after exercising the app, look for a
`tls_error` event: it means the app rejected the proxy CA (untrusted or pinned),
and its `reason` names the fix (`net check`/`net trust`, NSC user-CA opt-in, or
cert pinning). No `tls_error` and no flows usually means the app bypassed the
proxy (Cronet/QUIC) — see `net check`.

```bash
shadowdroid net check com.example.app | jq
shadowdroid net trust --auto       # or use the command recommended by net check
shadowdroid net start
shadowdroid net log | jq -c 'select(.type=="http")'
shadowdroid net show f1 --body --body-file /tmp/response.json
shadowdroid net override --url 'https://api.example.com/v1/dict*' --file fixtures/dict.json
```

To reuse a CA the device already trusts (an existing mitmproxy/Charles/corporate
CA) instead of ShadowDroid's generated one, import it before `net trust` — then
the whole chain (`trust`, `check`, leaf signing) uses your CA:

```bash
shadowdroid net ca import --cert mitmproxy-ca.pem      # combined cert+key PEM
shadowdroid net ca import --cert corp.crt --key corp.key
shadowdroid net ca info | jq        # source (generated|imported), validity, hash
shadowdroid net ca reset            # go back to a generated CA
```

`net log` is line-delimited JSON: one `http` object per line, followed by one
`{"cmd":"net_log","count":...}` summary object. Do not use `jq '.flows[]'`.
Filter events with `jq -c 'select(.type=="http")'`.

Large response bodies are capped inline. When `resp_truncated` is true, or when
the body is just too large for the conversation, use `net show <id> --body-file
<path>` so the body is written to disk instead of pasted into stdout.

## Debugging for agents

Use a bounded snapshot when you need causality, not just the screen:

```bash
shadowdroid debug auto Example | jq
shadowdroid debug snapshot --depth 1 | jq
```

`debug auto` resolves the configured/default app, launches it, attaches the
Android Studio debugger when the bridge is available, and returns a full
snapshot. With the Android Studio plugin installed and Studio restarted, use
	lower-level `debug` commands for attach, breakpoints, stack, threads, variables,
	deterministic eval/inspect, coroutine hints, native status, and watches:

```bash
shadowdroid debug attach
shadowdroid debug break line --file app/src/main/java/Foo.kt --line 42
shadowdroid debug variables --thread 0 --frame 0 --depth 2 --timeout-ms 2500
shadowdroid debug eval 'this.state' --thread 0 --frame 0 --depth 2 --timeout-ms 5000
shadowdroid debug inspect 'this.state' --depth 2
shadowdroid debug coroutines snapshot --depth 1
shadowdroid debug native status
shadowdroid debug watch add 'this.state'
```

Debugging several devices in one Studio: `debug sessions` reports each session's
`device`, and `-d <serial>` selects that device's session for `pause`/`resume`/
`step`/`stack`/`variables`/`eval` (otherwise the focused session is used; an
explicit `--session <index>` still wins). Unknown device → `no debugger session`
rather than acting on the wrong one.

Prefer `debug record` for longer investigations; it writes a JSONL timeline of
screen changes, logcat, debugger snapshots, screenshots, and app lifecycle.
Use `layout snapshot --compose --semantics --source-map` when the question is
visual structure or Compose source; use `layout source` to map a node back to
code (`--id` for UIAutomator nodes, `--draw-id` for Android Studio Layout
Inspector nodes) and `layout recompositions --reset` to isolate Compose
recomposition counts during one interaction:

```bash
shadowdroid layout recompositions --reset
# perform one tap/type/scroll or wait through one state change
shadowdroid layout recompositions | jq '{valid: .sample_valid, summary, top: (.nodes // [] | sort_by(-(.recomposition.count // 0), -(.recomposition.skips // 0))[:10])}'
```

Debugger read commands are bounded and return structured JSON warnings/errors
instead of waiting indefinitely when the app is running or stopped on a frame
without debug information.

## Make a device deterministic before driving

```bash
shadowdroid app install ./app-debug.apk --grant-all --wait-front  # install + grant + launch
shadowdroid profile apply --preset automation                     # animations off (+ stylus tutorial)
shadowdroid perm grant com.example.app android.permission.CAMERA
```

`connect` already disables the Android 14+ stylus-handwriting tutorial that
otherwise hijacks the first text-field focus.

## When something breaks

```bash
shadowdroid doctor            # device state / APK / forward / server / owners / clock
shadowdroid doctor --fix      # repair (reinstall, re-forward, restart)
shadowdroid collect --app com.example.app   # bundle logs+screen+screenshot+diagnostics
```

## Output contract

Every command prints exactly one JSON object on **stdout**: successes as
`{"type":"action","cmd":…}` (or a read's payload), failures as
`{"type":"error","code":…,"msg":…}`. Even unknown-flag / usage errors are JSON
and name the offending flag (with a spelling suggestion). Parse stdout and
branch on `type`; never scrape human text.

ShadowDroid's own operational logs go to **stderr**, so `… | jq` already sees
clean JSON. If you merge streams with `2>&1`, or just want the tidiest output,
add `--quiet`/`-q` (or set `SHADOWDROID_QUIET=1`) to silence those logs — then
stdout is JSON and nothing else.

Tap by selector and re-read the screen rather than trusting fixed coordinates
across layouts.
