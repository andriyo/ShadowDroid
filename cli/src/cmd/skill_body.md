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

Project config lives in `.shadowdroid/config.json`; user config lives in
`~/.shadowdroid/config.json`. Project config wins over user config. The project
`.shadowdroid/` folder also holds companion files — most importantly a
git-ignored per-project proxy CA (`ca.{crt,key}`). Minimal project config:

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

### Collapse the loop: `--observe` and `--if-screen`

The loop-fusion verbs (`tap`, `text`, `key`, `back`, `home`, swipes, drags,
pinch) take two flags that cut round-trips:

```bash
shadowdroid ui tap --text "Sign in" --observe       # response includes the post-action compact screen — no follow-up dump
shadowdroid ui tap --text "Buy" --if-screen a1b2c3d4e5f67890   # only act if the screen still matches this hash
```

`--observe` waits for the action to settle (`--observe-delay-ms`, default 150)
and attaches the resulting screen under `screen` in the same response.
`--if-screen <hash>` is optimistic concurrency for the UI: pass the
`screen_hash` from your last read; if a dialog or navigation changed the screen
in between, the command **refuses to act** and fails with
`code:"screen_changed"` — with the fresh compact screen in `detail.screen`, so
the failure is your re-observe. Combine both for a check-act-observe cycle in
one call.

### Crashes surface on your next command — no watcher needed

Every `ui` and `app` response (success or error) carries an `events` array when
the app **crashed or ANRed since your previous command**:

```json
{"type":"action","cmd":"wait","matched":false,"timeout":true,
 "events":[{"type":"crash","kind":"java","package":"com.example.app",
            "exception":"java.lang.IllegalStateException","stack":["…top 5 frames…"]}]}
```

If you tap and the app dies, your next `ui dump` says so — you don't have to
guess why the launcher is suddenly on screen. No `events` key means nothing
crashed. (`SHADOWDROID_NO_EVENTS=1` disables the probe.)

## When something looks wrong: `why` and `log`

`why` is one bounded read answering "what just went wrong?" — reach for it
after any surprise instead of a forensic command sequence:

```bash
shadowdroid why | jq '{verdict, explanation, hints}'
shadowdroid why | jq '.evidence.crash.project_frames'   # your-code stack frames, mapped to files
```

It fuses the last crash/ANR (stack frames mapped into your project's source
files when a project root is configured), recent error-level logs, the current
screen, and network failures (when the `net` proxy runs) into an explicit
`verdict` (`app_crashed`, `app_not_responding`, `tls_rejected`,
`backend_errors`, `app_not_foreground`, …) with `evidence` and next-step
`hints`. Read-only; works even when the on-device server is down.

`log` is structured, bounded logcat — never raw-tail `adb logcat`:

```bash
shadowdroid log --last 2m                  # configured app's lines + parsed crashes, deduped
shadowdroid log --level e --grep "auth"    # error-level lines matching a regex
shadowdroid log --all --last 30s --max 50  # every process, tightly capped
```

One JSON object per line (`{"type":"log",…}` entries, `{"type":"crash",…}`
parsed blocks with `project_frames`), then an action summary. Scoped to the
configured app by default; repeated lines collapse with a `repeat` count.

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

The signing CA is resolved per invocation: `proxy.ca_cert`/`proxy.ca_key` in
config, else a per-project `<project>/.shadowdroid/ca.{crt,key}`, else the global
`~/.shadowdroid/net/ca.{crt,key}`. Scope the `net ca` verbs with
`--project`/`--global` (default auto-picks the project CA when a `.shadowdroid/`
dir exists). To reuse a CA the device already trusts (mitmproxy/Charles/corporate)
instead of the generated one, import it before `net trust` — then the whole chain
(`trust`, `check`, leaf signing) uses your CA:

```bash
shadowdroid net ca reset --project                    # mint a per-project CA (git-ignored)
shadowdroid net ca import --cert mitmproxy-ca.pem      # combined cert+key PEM (auto scope)
shadowdroid net ca import --project --cert corp.crt --key corp.key
shadowdroid net ca info | jq        # source (generated|imported), scope, validity, hash
shadowdroid net ca reset            # go back to a generated CA
```

If the CA is already trusted on the device (e.g. baked into a custom emulator
image), set `proxy.ca_trusted: true` so `net trust`/`net check` skip the install
and readback (reported as basis `asserted`). Otherwise a successful check is
cached per device (by CA fingerprint) and reused; `net check --fresh` /
`net trust --fresh` force a real probe.

The proxy serves HTTP/2 and HTTP/1.1, decodes gzip/deflate/br/zstd, and streams
SSE / large responses (and large request uploads) through instead of buffering.
Streamed flows carry `"streamed":true` (response) or `"req_streamed":true`
(request) and have no captured body — re-request with a narrower scope or inspect
on-device if you need the payload. WebSocket upgrades are tunnelled (handshake
captured as a `matched:"websocket"` flow, frames not decoded). Add `--redact` to `net start` to
mask auth/cookie headers in captured flows, or `--verify-upstream` to validate the
real server's TLS cert (off by default for self-signed dev backends).

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
shadowdroid why               # what just went wrong? verdict + evidence + hints
shadowdroid log --last 5m     # what did the app log? (structured, bounded)
shadowdroid doctor            # device state / APK / forward / server / owners / clock
shadowdroid doctor --fix      # repair (reinstall, re-forward, restart)
shadowdroid collect --app com.example.app   # bundle logs+screen+screenshot+diagnostics
```

Failures explain themselves: selector misses come back with `top_texts` (what
IS on screen) and `closest` (ranked near-matches for what you searched);
`ui wait` timeouts carry `top_texts` and `current_app`; `screen_changed`
carries the fresh screen; crashes ride the `events` key of whatever you run
next. Prefer reading the error's `detail` over immediately re-dumping.

## Output contract

Most one-shot commands print exactly one JSON object on **stdout**: successes as
`{"type":"action","cmd":…}` (or a read's payload), failures as
`{"type":"error","code":…,"msg":…}`. JSONL commands are explicit: `watch`
streams one event per line, and `log` prints filtered log/crash lines followed
by a summary. Parse stdout as JSON, line by line for JSONL commands, and branch
on `type`; never scrape human text. Even unknown-flag / usage errors are JSON
and name the offending flag with a spelling suggestion.

ShadowDroid's own operational logs go to **stderr**, so `… | jq` already sees
clean JSON. If you merge streams with `2>&1`, or just want the tidiest output,
add `--quiet`/`-q` (or set `SHADOWDROID_QUIET=1`) to silence those logs — then
stdout is JSON and nothing else.

Tap by selector and re-read the screen rather than trusting fixed coordinates
across layouts.
