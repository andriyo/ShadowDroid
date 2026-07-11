# ShadowDroid

**An agent-first Android control plane** for UI automation, app/device control,
log and crash triage, layout inspection, debugger access, diagnostics, and
HTTP(S) interception.

[![Latest release](https://img.shields.io/github/v/release/andriyo/ShadowDroid?sort=semver&display_name=tag&label=release&color=blue)](https://github.com/andriyo/ShadowDroid/releases/latest)
[![CI](https://github.com/andriyo/ShadowDroid/actions/workflows/ci.yml/badge.svg)](https://github.com/andriyo/ShadowDroid/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/github/license/andriyo/ShadowDroid?color=blue)](LICENSE)
[![Platform: Android](https://img.shields.io/badge/platform-Android-3DDC84?logo=android&logoColor=white)](#install)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-CE422B?logo=rust&logoColor=white)](#how-it-works)

ShadowDroid is an open-source **Android automation and debugging CLI for AI
agents**. It lets coding agents such as Claude Code, Cursor, Codex, Gemini, and
Antigravity drive, inspect, and debug real Android apps and emulators through a
fast, JSON-first command line — no test DSL, no client library, no Appium
server. If your agent can run a shell command and parse JSON, it can drive
Android.

It pairs a single Rust binary on your laptop with a Kotlin instrumentation
service on the device, then exposes UI state, app lifecycle, structured logcat,
crash/ANR events, device controls, permissions, files, display profiles,
toasts, HTTP(S) traffic, Android Studio debugger state, Layout Inspector data,
and an optional in-app debug AAR through one CLI. Warm UI reads normally complete
in tens of milliseconds, so an agent can observe after every meaningful action.

## Sixty seconds in the loop

A real session — read the screen, act by selector, and notice how the tool
answers the agent's next question *before it's asked*:

```jsonc
$ shadowdroid ui dump
{"screen_hash":"154e97ff111d4b1e","screen_hash_version":2,
 "viewport":{"w":1080,"h":2424},
 "current_app":{"package":"com.example.app","activity":".MainActivity","pid":5170},
 "element_count":39,"ime":{"keyboard_visible":false},
 "elements":[{"id":7,"text":"Sign in","rid":"btn_sign_in","tap":[540,1200],"clickable":true}, …]}

// Act + re-observe in ONE call: --observe returns the post-action screen.
$ shadowdroid ui tap --text "Sign in" --observe
{"type":"action","cmd":"tap","ok":true,"via":"selector","x":540,"y":1200,"matched":true,
 "element":{"id":7,"text":"Sign in","tap":[540,1200]},
 "screen":{"screen_hash":"9c01d2aa87b3e544","screen_hash_version":2,
           "element_count":24,"elements":[…]}}

// Only act if the screen is still the one you read (optimistic concurrency):
$ shadowdroid ui tap --text "Buy" --if-screen 154e97ff111d4b1e
{"type":"error","ok":false,"stage":"run","code":"screen_changed",
 "msg":"screen changed since your last read (expected hash 154e97ff…, now 9c01d2aa…) — not acting; re-plan from detail.screen",
 "retryable":false,
 "next_actions":["re-plan from detail.screen instead of issuing another dump"],
 "detail":{"expected":"154e97ff111d4b1e","actual":"9c01d2aa87b3e544","screen":{…fresh compact dump…}}}

// If the app crashed since your previous command, your NEXT response says so —
// success or error, no watcher required:
$ shadowdroid ui wait --text "Welcome" --timeout-ms 6000
{"type":"error","ok":false,"stage":"ui","code":"wait_timeout",
 "retryable":true,
 "detail":{"timeout_ms":6000,"gone":false,
           "screen_hash":"9c01d2aa87b3e544","screen_hash_version":2,
           "current_app":{"package":"com.example.app","activity":".MainActivity"},
           "top_texts":["Example App keeps stopping","App info","Close app"]},
 "next_actions":["inspect detail.top_texts and current_app, then correct the selector or expected screen",
                 "run `shadowdroid why` if the app reached an unexpected state"],
 "events":[{"type":"crash","kind":"java","package":"com.example.app",
            "exception":"java.lang.IllegalStateException","message":"boom",
            "stack":["com.example.CartRepo.checkout(CartRepo.kt:42)", …],
            "hint":"app process died; `shadowdroid why` or `shadowdroid log --last 2m` for detail"}]}

// One bounded read answers "what just went wrong?" — verdict, evidence, next steps:
$ shadowdroid why
{"type":"action","cmd":"why","ok":true,"verdict":"app_crashed",
 "explanation":"the app process crashed — see evidence.crash (project_frames point into your code)",
 "evidence":{"crash":{"exception":"java.lang.IllegalStateException",
   "project_frames":[{"frame":"com.example.CartRepo.checkout(CartRepo.kt:42)",
                      "path":"app/src/main/java/com/example/CartRepo.kt","line":42}], …}},
 "hints":["shadowdroid log --last 5m   # full crash context", …]}
```

> Android-only by design, and not a test framework — ShadowDroid is the fast,
> observable layer an agent drives directly against a running app.

## Key benefits

- **A fast warm path** — a persistent on-device service answers core UI reads
  in tens of milliseconds instead of starting a fresh UI dump process each time.
- **Fewer round-trips** — `--observe` fuses act + re-read into one call;
  `--if-screen` refuses to act on a stale read and hands back the fresh screen
  in the failure. Every round-trip an agent saves is an LLM inference saved.
- **Failures explain themselves** — a missed selector returns what *is* on
  screen (`top_texts`) and the closest candidates ranked; a timeout reports what
  the screen became; a crash since your last command rides the next response as
  an `events` array. The error is the diagnosis.
- **Structured logs and one-verb triage** — `log` turns logcat into bounded,
  app-scoped JSON with crash/ANR blocks parsed out and stack frames mapped to
  your source files; `why` fuses crash + logs + screen + network failures into
  a single verdict with evidence.
- **No test DSL, client SDK, or Appium server** — one binary plus `adb`.
- **Robust, selector-based actions** — tap / type / swipe / scroll by `--rid`,
  `--text`, `--desc`, or `--xpath`, so flows survive layout changes instead of
  breaking on hard-coded coordinates. Strict ambiguity handling: several
  matches and no exact hit is a structured error listing candidates, never a
  guess.
- **Full Android operator surface** — app install/start/stop/clear/info, runtime
  permissions, app-ops, device power/orientation/clipboard/notifications,
  display profiles, and on-device file push/pull live in the same CLI.
- **First-class Jetpack Compose support** — a semantics-aware element tree
  (AndroidX UI Automator 2.3.0+), enriched with Compose source locations and
  recomposition counts when Android Studio's Layout Inspector is live.
- **Sees _why_, not just _what_** — a bounded Android Studio debugger exposed
  as JSON: debugger control and breakpoints, call stack, threads, variables,
  watches, expression eval, native/tombstone readiness, and coroutine insight.
- **One live event stream** — `watch` emits screen diffs, crashes, toasts,
  popup-watcher actions, and decrypted HTTP(S) on a single timeline.
- **Built-in HTTP(S) interception** — a host-side MITM proxy built into the
  binary; an optional debug-only in-app agent adds process/coroutine diagnostics
  and explicit above-TLS OkHttp capture, including pinned OkHttp calls.
- **Self-describing and agent-ready** — `commands --json --depth 1` gives a
  compact map and `commands --json --describe '<path>'` gives precise command
  construction data; one command installs skills for Claude Code, Cursor,
  Codex, Gemini, and Antigravity.
- **Trivial to install, safe to run** — a single native binary plus a tiny,
  SHA-256-verified APK; macOS / Linux / Windows hosts; real devices, emulators,
  and Android TV / leanback.

## Contents

- [Why it exists](#why-it-exists)
- [How it works](#how-it-works)
- [Install](#install)
- [Connect](#connect)
- [The agent loop](#the-agent-loop)
- [The output contract](#the-output-contract)
- [When something goes wrong](#when-something-goes-wrong)
- [What you can drive](#what-you-can-drive)
- [Agent debugging](#agent-debugging)
- [Agent integration](#agent-integration)
- [FAQ](#faq)
- [License](#license)

## Why it exists

To drive a *running* app in a tight agent loop, the tools you'd otherwise reach
for each fall short:

| Tool                              | Gap for a live agent loop                                                          |
| --------------------------------- | ---------------------------------------------------------------------------------- |
| `adb shell uiautomator dump`      | ~500ms–1s per dump — the loop stalls between every step.                           |
| `adb shell input tap`             | Stateless: no idea what's on screen, fragile to any layout change.                 |
| `adb logcat`                      | An unscoped text firehose — no app scoping, no structure, crash blocks buried in noise. |
| `android` CLI (`layout`/`screen`) | Built for project create / build / run / SDK — and great at it. But for live UI, each `layout` call runs a fresh `ui-dump` (the slow path): no persistent service, no streaming loop, no interaction-by-selector, no crash/popup events, no agent debugger. |

ShadowDroid is the **complement, not a replacement**. Keep using the `android`
CLI to scaffold, build, deploy, and manage the SDK — then hand the *running* app
to ShadowDroid. A persistent on-device service keeps warm dumps in the
tens-of-milliseconds range, a streaming JSON event model lets the agent follow
the app live, and it ships with
**first-class Jetpack Compose support** (AndroidX UI Automator 2.3.0+),
**built-in crash detection**, **declarative popup watchers**, and — uniquely — an
**agent-facing Android Studio debugger** (see [Agent debugging](#agent-debugging)).
It even follows the `android` CLI's own conventions (`init`, `skill`, `layout`,
`studio`), so it slots in right beside it.

## How it works

```
        Laptop                         adb forward                Android device
  ┌───────────────────────┐         tcp:7912 ⇆ 7912         ┌───────────────────────────┐
  │  shadowdroid (Rust)   │  ── HTTP + JSON (loopback) ──▶  │  instrumentation APK      │
  │  • clap CLI           │                                 │  • Ktor 3 / CIO server    │
  │  • XML → element JSON │ ◀────────  adb logcat  ──────── │  • UiDevice (AndroidX     │
  │  • watch/crash/why    │                                 │    UI Automator 2.3.0+)   │
  └───────────────────────┘                                 └───────────────────────────┘
```

The on-device APK exposes low-latency UI tree reads and UI/device actions over
HTTP. The host CLI owns orchestration: the watch diff loop, logcat parsing for
the `log`/`why`/crash-event paths, watcher policy, act+observe fusion, source
mapping, and recovery. This split means host/adb evidence in `log`, `why`,
`doctor`, and `collect` remains useful when the on-device server is down.

Optional integrations extend the same command surface:

- The Android Studio plugin exposes debugger state and Layout Inspector models to
  `shadowdroid debug ...` and `shadowdroid layout ...`.
- The built-in host-side MITM proxy wires through `adb reverse` and device proxy
  settings so `shadowdroid net ...` can inspect, intercept, mutate, and replay
  HTTP(S) traffic.
- The debug-only core AAR auto-starts through a merged `ContentProvider` in apps
  you can build and supplies process/coroutine diagnostics. HTTP capture is
  separate: `aar install --okhttp` adds an optional companion, and the app must
  explicitly add `ShadowDroidCaptureInterceptor()` to each debug OkHttp client.
  That sees pinned OkHttp traffic above TLS; it does not instrument Cronet,
  QUIC, or other clients.

On the first `connect`, the CLI auto-installs a **version-matched APK pair**
(downloaded from the matching GitHub Release, SHA-256 verified, cached under
`~/.shadowdroid/`), runs `adb forward`, and starts the instrumentation. Later
calls just probe `GET /v1/state` and reuse the live server, so steady-state
latency stays low.

At the wire level the server is a loopback HTTP/JSON API, but the supported
public interface for agents is the CLI surface and `shadowdroid commands --json`.

## Install

Homebrew:

```bash
brew install andriyo/tap/shadowdroid
```

macOS / Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | sh
```

Scoop:

```powershell
scoop bucket add andriyo https://github.com/andriyo/scoop-bucket
scoop install shadowdroid
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex"
```

ShadowDroid also requires Android Platform Tools (`adb`) on PATH — the
installers print a hint if it's missing. On macOS:
`brew install --cask android-platform-tools`; on Windows: `scoop install adb`.
The direct shell/PowerShell installers also seed global agent skills; for
package-manager installs, run `shadowdroid init` once.

For manual installs, use the assets attached to the latest GitHub Release.

## Connect

Start an emulator or plug in a device with USB debugging, then:

```bash
shadowdroid devices        # list attached devices / emulators
shadowdroid connect        # install the on-device server, forward, and verify
```

On first `connect`, the CLI downloads the matching instrumentation APKs from the
GitHub Release, verifies them with SHA-256, caches them under
`~/.shadowdroid/apks/<version>/`, and installs them on the device. When working
inside this repo it auto-discovers local Gradle build outputs before falling
back to cached or release APKs.

Keep the CLI current and diagnose a flaky pipe:

```bash
shadowdroid update --check  # compare against the latest GitHub Release
shadowdroid doctor          # diagnose device state, APK version, forward, server
shadowdroid doctor --fix    # attempt repairs (reinstall, re-forward, restart)
shadowdroid collect         # bundle a self-contained diagnostic snapshot
```

> **Running instrumentation tests?** While connected, ShadowDroid holds the
> device's single `UiAutomation` slot, so Espresso / UI Automator tests
> (`AndroidJUnitRunner`) fail with `UiAutomationService ... already registered!`.
> Prefer `shadowdroid test -- ./gradlew connectedDebugAndroidTest`, which
> disconnects before the wrapped command and reconnects afterward. Or run
> `disconnect`/`connect` explicitly. `connect` reports the slot advisory and
> `doctor` shows the current owner.

Initialize host integrations (Android Studio plugin for debugger + layout, plus
agent skills):

```bash
shadowdroid init                    # install/update Studio plugin + agent skills
shadowdroid init --no-studio-plugin # only inspect Studio and install skills
```

Put repeated values in config instead of spending prompt/context on every
command. Config lives in a folder: the global `~/.shadowdroid/config.json` is
loaded first, then a project `.shadowdroid/config.json` from each of the current
directory's ancestors, with the nearest project file winning.

```bash
shadowdroid config schema --json
shadowdroid config init --project --app Livd --package com.livd --project-path /Users/you/Work/Livd --json
shadowdroid config validate --json
```

```json
{
  "device": "emulator-5554",
  "app": "Livd",
  "project": "/Users/you/Work/Livd",
  "proxy": {
    "ca_trusted": true,
    "hosts": ["*.livd.app"],
    "trust_store": "user"
  },
  "apps": {
    "Livd": {
      "package": "com.livd",
      "run_configuration": "app",
      "debugger": "Android Debugger"
    }
  }
}
```

The `project` path matters for debugging: `why` and `log` use it to map crash
stack frames back to files in your source tree (`project_frames`), so the agent
gets `app/src/main/java/.../CartRepo.kt:42` instead of a bare class name.

`config init` changes only explicitly supplied fields, deep-merges an existing
app alias, validates Android identifier fields, and atomically replaces the
target file. Treat a committed project config as repository input, never as a
place for shell fragments. If a malformed config prevents an ordinary command
from loading, recovery commands still work because they run before normal
config loading:

```bash
shadowdroid config paths --json
shadowdroid config validate --json  # non-zero config_invalid with report in detail
shadowdroid config schema --json
shadowdroid commands --json --depth 1
```

Repository config cannot supply executable device-shell fragments: app
packages are validated while the config is deserialized, permission/app-op
tokens are validated at their command boundary, and accepted values are quoted
before entering an Android shell command. Values containing `;`, newlines,
`$()`, quotes, or whitespace fail with a typed `invalid_*` error.

### Per-project proxy CA

The `net` MITM proxy signs with a CA that ShadowDroid resolves per invocation:
an explicit `proxy.ca_cert`/`proxy.ca_key` in config (absolute or `~/` paths),
else a per-project convention CA at `<project>/.shadowdroid/ca.{crt,key}`, else
the global `~/.shadowdroid/net/ca.{crt,key}`. Mint a project CA with
`shadowdroid net ca reset --project` (or import your own with `net ca import
--project --cert …`); `config init --project` and the project-scoped `net ca`
verbs write a `.shadowdroid/.gitignore` so the CA cert, key, and `.bak` backups
are never committed.

Set `proxy.ca_trusted: true` to tell ShadowDroid the CA is already trusted on
the device (e.g. baked into a custom emulator image) — `net trust`/`net check`
then skip the adb install and trust-store readback and report the basis as
`asserted`. Even without it, a successful `net trust`/`net check` is cached per
device (keyed by CA fingerprint), so repeat runs skip the probe; pass `--fresh`
to force a real check.

## The agent loop

This section is the canonical operating contract for LLMs and coding agents.
The loop is **read → act → confirm**, and ShadowDroid is built so each step
costs as few round-trips as possible.

1. **Discover the surface once.** Start with `shadowdroid commands --json
   --depth 1`; use `commands --json --describe 'ui tap'` for one command, or
   omit `--depth` for the full tree. Schema version 2 contains canonical paths,
   complete argument construction data (aliases, conflicts, requirements,
   groups, arity, and trailing/hyphen-value behavior), output contracts, and
   agent decision hints. Do not invent command names from memory or scrape
   `--help` prose.
2. **Put repeated context in config.** `shadowdroid config init ...` then
   `config validate --json`. Use an app alias instead of spending tokens on
   `--package`/`--project`/`--debugger` every call.
3. **Connect.** `shadowdroid connect`; if it fails, `shadowdroid doctor --json`,
   then `doctor --fix` only when repair side effects are acceptable.
4. **Read by dumping.** `shadowdroid ui dump` returns the actionable tree as a
   compact element list plus `screen_hash` and `screen_hash_version`. Cache the
   pair; invalidate a cached hash if its version changes.
5. **Act by selector, not coordinates.** Prefer `--rid`, then `--desc`/exact
   `--text`. Add `--observe` to get the post-action screen back in the same
   response, and `--if-screen <hash>` to refuse acting on a screen that changed
   under you (the failure returns the fresh screen — that *is* your re-read).
6. **Confirm.** `ui wait --text/--rid/--pkg` blocks until the expected state and
   echoes the matched element; a timeout returns `top_texts` so you see what the
   screen became instead of guessing.
7. **Watch when timing matters.** `shadowdroid watch` streams screen diffs,
   crashes, ANRs, toasts, watcher actions, and (with `net` running) HTTP events
   on one JSONL timeline.
8. **Triage failures with one read.** After any surprise, `shadowdroid why`
   returns a verdict + evidence; `shadowdroid log --last 5m` gives the structured
   logcat behind it. You rarely need both plus a screenshot — start with `why`.
9. **Go deeper only when needed.** `shadowdroid debug ...` (Android Studio
   debugger as JSON) and `shadowdroid layout ...` (Compose semantics/source/
   recompositions) when UI polling can't answer *why*.
10. **Free the slot for instrumentation.** `shadowdroid test -- <cmd>` (or
    `disconnect` first) before Espresso / UI Automator runs — Android allows one
    `UiAutomation` owner at a time.

## The output contract

Treat the process exit code as authoritative. Most one-shot commands print one
JSON object on stdout: action success as
`{"type":"action","ok":true,"cmd":…,…}`, a raw read such as `ui dump` as its
payload, and failure as `{"type":"error","ok":false,"stage":…,"code":…,
"msg":…}`. Every failure also includes `retryable`, structured `detail`, and
`next_actions` (an empty list when no safe recovery is known); use those fields
instead of parsing `msg`. Raw reads can omit `ok`, so exit code zero is their
success signal.

Streaming commands are explicit JSONL exceptions (`watch`, `log`, `net log`,
and `debug replay`); `test` passes through the wrapped command and adds a
ShadowDroid trailer. Some setup/report commands default to human output and
offer `--json`. Inspect `commands --json --describe '<path>'` for the exact
mode. Unknown-argument and missing-command errors are JSON and exit 2; a
spelling suggestion is included when one is available. Explicit `--help`
remains human-readable. Commands that write a large `--out` artifact still
emit one small terminal action naming the path and byte count.

Inside a running `watch` stream, `{"type":"error","stage":…,"msg":…,
"input":…,"ts":…}` is a timeline event, not the terminal one-shot error
envelope above. Keep consuming unless the stream ends or the task says to stop.

- **`events` rides any response.** When the app crashed or ANRed since your
  previous command, the next result (action *or* error) carries an `events`
  array of parsed `{"type":"crash",…}` objects. No `watch` required, no separate
  poll — the crash finds you. (`SHADOWDROID_NO_EVENTS=1` opts out.)
- **Failures are self-describing.** `element_not_found` carries `top_texts`
  (what *is* on screen) and `closest` (ranked near-matches to your selector);
  `ambiguous_match` lists the candidate nodes; `screen_changed` carries the
  fresh compact screen; `ui wait` timeouts are non-zero `wait_timeout` errors
  carrying `top_texts`, `current_app`, and recovery commands. Read `detail`
  before re-dumping.
- **Logs go to stderr.** ShadowDroid's own operational logging is on **stderr**,
  so `… | jq` already sees clean JSON. Add `--quiet`/`-q` (or
  `SHADOWDROID_QUIET=1`) to silence it entirely — handy when you merge with
  `2>&1`.
- **Selector actions are strict.** Several matches and no exact hit is an
  `ambiguous_match` error listing candidates, never a silent guess.

## When something goes wrong

Three verbs, in the order you'll usually reach for them:

```bash
shadowdroid why                       # verdict + evidence + next steps, in one read
shadowdroid log --last 5m --level e   # structured, app-scoped logcat behind it
shadowdroid collect                   # full offline bundle to hand off
```

**`why`** fuses the last crash/ANR (with stack frames mapped into your source
tree), recent error logs, the current screen, and network failures (when the
`net` proxy is up) into a single `verdict` — `app_crashed`,
`app_not_responding`, `tls_rejected`, `backend_errors`, `app_not_foreground`,
`log_errors_only`, or `no_obvious_cause` — with `evidence` and `hints`. It is
non-mutating: it reads the server only if that server is already reachable and
does not install/start it, create an adb forward, or change device state to get
a screen. Without a reachable server it marks screen evidence unavailable and
continues with adb/host evidence.

**`log`** is logcat shaped for an agent: scoped to the configured app by default
(`--all` for everything), windowed (`--last 60s`), filtered (`--level e`,
`--grep`, `--tag`), deduplicated (repeats collapse with a count), and with
crash/ANR blocks lifted out as parsed `{"type":"crash",…}` events — one JSON
object per line, then an action summary.

**`collect`** is the "I give up, here's everything" bundle: `doctor` output,
device info, logcat + crash buffer, screenshot, screen dump, and app state, all
in one directory. It degrades gracefully — the host-side diagnostics are
captured even if the on-device server can't start.

Optionally, opt in to a **local usage log**. Schema version 2 records only the
command path, duration, CLI version, outcome, and typed error code/stage/retry
posture — never argument values — and never uploads anything:

```bash
shadowdroid usage enable
shadowdroid usage report --days 30 | jq \
  '{verbs, error_codes, error_stages, versions, recommendations, feedback_loop}'
```

Recommendations require repeated evidence: high error rates, slow p95s, or
recurring error codes. The report suggests the next engineering action but does
not edit code. Reproduce the evidence, add a regression, implement the change,
then compare error rate and p95 by version.

## What you can drive

Automation commands are JSON-first, and selectors are consistent across
commands: `--text`, `--rid` (resource id), `--desc` (content description), and
`--xpath`. A typical agent reads `ui dump` once, acts by `--rid`/`--text`, and
caches `screen_hash` together with `screen_hash_version`. A hash is comparable
only within the same version; invalidate it when the version changes.

Text/desc selectors match as a **normalized, case-insensitive substring** by
default: before comparing, surrounding whitespace is collapsed, curly
quotes/apostrophes/ellipsis are folded to ASCII, and zero-width characters are
stripped — so `--text "sign in"` matches a `SIGN IN` button and `--text "Don't
allow"` matches text rendered with a typographic apostrophe. Add `--exact` (on
`ui find`/`tap`/`text`/`wait`/`focus`) to require a full match (so `--text
Allow` won't hit a label reading "Allow Disney+…"), and `--clickable` to skip
non-clickable labels. `--rid` is the most reliable target when a stable resource
id exists. Matching is **literal** — `*`, `.`, `?` and other symbols match
themselves, with no wildcards or regex (a value starting with `-` needs the
`--text=-50%` equals form so it isn't read as a flag).

Selector **actions** are **strict**: if `ui tap`/`text`/`focus` matches several
elements and none is an exact match, they fail with a structured
`ambiguous_match` error listing the candidates rather than guessing — narrow
with `--exact`, `--rid`, or `--clickable`. On a hit, `ui tap`/`wait`/`focus`
echo back the matched element so you can confirm the right node was targeted.

Loop-fusion action verbs (`ui tap`, coordinate gestures, `ui pinch`, `ui text`,
`ui key`, `ui back`, and `ui home`) accept `--observe` (return the post-action
compact screen in the same response) and `--if-screen <hash>` (optimistic
concurrency — refuse to act if the screen changed, and return the fresh one).
`ui wait` also syncs on the foreground app, not just elements: `--pkg <package>`
blocks until that app reaches the foreground (e.g. a Custom Tab or share sheet
opened), and `--pkg-not <package>` blocks until the screen leaves it.

**Android TV / leanback** is focus + D-pad driven, not touch driven:
`/v1/state` reports `is_television: true`, each element carries a `focused`
flag, and `ui focus --text/--rid/--desc [--center]` walks the D-pad to a
selector (then optionally activates it) — the TV analog of `ui tap` /
`ui scroll-to`. Prefer it (and `ui key dpad_*`) over coordinate taps there.

| Group | Commands |
| --- | --- |
| **Discovery/setup** | `commands --json --depth 1`, `commands --json --describe '<path>'`, `config paths` / `schema` / `explain` / `init` / `validate`, `skill`, `studio status` / `install`, `init`, `update`, `usage` |
| **Session/diagnostics** | `devices`, `connect`, `disconnect`, `test`, `doctor`, `collect`, `why`, `log` |
| **UI automation** | `ui dump`, `ui audit`, `ui gen`, `ui screenshot`, `ui find`, `ui tap`, `ui double-tap`, `ui long-tap`, `ui swipe`, `ui drag`, `ui swipe-ext`, `ui pinch`, `ui scroll-to`, `ui focus`, `ui text`, `ui key`, `ui hide-keyboard`, `ui back`, `ui home`, `ui wait`, `ui toast` (tap/gesture/text/key/back/home verbs take `--observe` / `--if-screen`) |
| **Triage** | `why` (one-read verdict + evidence), `log` (structured app-scoped logcat + parsed crashes) |
| **Live timeline** | `watch` (screen changes, crashes, ANRs, toasts, watcher actions, and HTTP events when network capture is active) |
| **Layout / Compose** | `layout snapshot`, `layout diff`, `layout source`, `layout recompositions` |
| **Debugger** | `debug auto`, `snapshot`, `record`, `replay`, `status`, `sessions`, `clients`, `attach`, `break`, `breakpoints`, `pause`, `resume`, `step-in`, `step-over`, `step-out`, `stop`, `stack`, `threads`, `variables`, `eval`, `inspect`, `coroutines`, `continue-until`, `watch`, `step-until-screen-change`, `step-until-log`, `run-until-crash`, `native`, `tombstones` |
| **App lifecycle** | `app start`, `stop`, `install`, `reinstall`, `clear`, `info`, `wait`, `current` |
| **Permissions/app-ops** | `perm grant`, `revoke`, `list`, `reset`; `appops get`, `set` |
| **Device/system** | `device info`, `shell`, `wake`, `sleep`, `unlock`, `orientation`, `clipboard`, `notifications`, `quick-settings`, `open-url` |
| **Display profile** | `profile snapshot`, `apply`, `reset` (animations, font, density, size, rotation) |
| **Files** | `files ls`, `push`, `pull` |
| **Network MITM** | `net check`, `trust`, `ca import/info/reset`, `start`, `stop`, `status`, `log`, `show`, `export`, `intercept`, `resume`, `drop`, `respond`, `rule`, `rules`, `replay` |
| **In-app AAR agent** | `aar install` (`--okhttp`, `--coroutine-probes`, `--build`), `status`, `remove`, `capture`, `intercept`, `resume`, `drop`, `agent`, `coroutines` |
| **Authoring/testing helpers** | `ui audit` (selector gaps), `ui gen` (Screen Object scaffold), `net export fixtures` (replayable response set + `manifest.json`, GraphQL keyed by operationName), `test` (instrumentation command with the slot freed), `debug replay --repeat --diff` (flake hunting) |

Mutation commands verify the requested postcondition instead of trusting an
empty Android shell response. Permission/app-op changes, profile apply/reset,
explicit file modes, app clear/stop, install steps, and goal-directed
scroll/focus operations exit non-zero when readback disagrees, with requested
and observed state in `detail`.

`appops get <package> [op]` reports `uid_mode` and `package_mode` separately,
plus the `governing_scope` and `effective_mode`; UID policy takes precedence
when Android returns both. `appops set` requires `--scope uid` or
`--scope package` and verifies that exact layer, preventing an apparently
successful package change from hiding an unchanged governing UID mode.

`profile apply --file` accepts the JSON shape written by `profile snapshot` and
rejects unknown, empty, or unsafe values. Values remain JSON strings:
animation scales must be finite and non-negative, `font_scale` finite and
positive, density a positive integer, size positive `WxH`, auto-rotation and
stylus flags `0`/`1`, and user rotation `0`–`3`. A file conflicts with CLI
setting overlays so no supplied value is silently ignored. For shared/FUSE
storage, omit `files push --mode`; when `--mode` is explicit, failure to apply
it is a typed error even if the bytes were transferred.

`watch` is the streaming workhorse — it emits debounced, hash-diffed `screen`
events plus `crash`, `toast`, `watcher_fired`, and `http` events when a `net`
proxy is running (plus a `tls_error` when an app rejects the proxy CA, so a
failed interception is visible instead of just missing). If network capture is
not available, `watch` emits a structured `warning` event with the suggested
recovery command, so an agent can decide whether to run `net start` or continue
UI/crash-only (`watch --no-net`).

`net` is a host-side MITM proxy built into the single binary — no Python, no
external mitmproxy. `net start` spawns the proxy, wires the device through
`adb reverse` and proxy settings, and decrypted HTTP(S) transactions then stream
as `http` events on the same timeline as `screen` when `watch` is running.
The pre-existing device proxy setting is persisted before wiring; a repeated
`net start` repairs wiring to an already-running daemon, while `net stop`
restores that exact setting and reports separate raw-IP and DNS connectivity
checks (`--canary-host` selects the neutral DNS probe).
Beyond observing, the agent can **intercept** a flow — `net intercept` pauses
matching requests/responses and emits them as `http_intercept` events on
`watch`; the agent inspects with `net show`, then releases with
`net resume --set-status/--body/…`, `net drop`, or `net respond` (a canned
reply). Repeated edits can be promoted to declarative `net rule`s (map-local /
map-remote / set-status / set-request-header / set-response-header / replace /
block / delay) or served
offline from a saved session with `net replay`. `net check <app>` reports
whether a build is interceptable; `net export har|curl|fixtures` hands flows to
other tools.

Header rules deliberately name their phase: use `set-request-header` before
upstream or `set-response-header` before returning to the app. The ambiguous
`set-header` kind is rejected instead of guessing.

The decrypted leg negotiates **HTTP/2 or HTTP/1.1** (h2 apps aren't
downgraded), streams **SSE / large bodies** through instead of buffering them —
both response and request (a big upload streams chunked; marked
`streamed`/`req_streamed` in the flow) — decodes `gzip`/`deflate`/`br`/`zstd`,
and raw-tunnels **WebSocket** upgrades. `net start --verify-upstream` validates
the real server certificate for both HTTPS and WSS (off by default for
self-signed dev backends);
`net start --redact` masks `authorization`/`cookie` in captured flows (the
session log is written `0600` either way).

Completed flows enter a bounded in-memory queue; `net status` exposes
`dropped_flows` if sustained traffic outruns storage. The session JSONL keeps
one 64 MiB current generation plus one rotated generation, bounding disk use to
roughly 128 MiB per device session.

By default the proxy signs with a CA it generates on first use. To reuse a CA
the device already trusts — an existing mitmproxy/Charles/corporate CA — run
`net ca import --cert <pem>` (the key can be a separate `--key`, or bundled in a
combined PEM like mitmproxy's `mitmproxy-ca.pem`); every downstream step then
signs and installs *your* CA. `net ca info` shows the active CA and
`net ca reset` returns to a generated one.

For in-process diagnostics in an app you can build, install the debug-only core
AAR with `shadowdroid aar install --build`. Add `--coroutine-probes` for
`aar coroutines`. HTTP capture is opt-in and OkHttp-specific:

```bash
shadowdroid aar install --okhttp --build
```

Then add `ShadowDroidCaptureInterceptor()` as an application interceptor to
each target debug `OkHttpClient`. `aar agent` reports whether that provider is
actually registered before `aar capture`/`aar intercept` are used. The core AAR
alone does not capture HTTP, and the companion does not instrument Cronet,
QUIC, or other stacks.

Run `shadowdroid commands --json --depth 1` for a compact catalog,
`commands --json --describe '<path>'` for one command, or `--help` for a human
view.

## Agent debugging

**This is the part nothing else gives an agent.** Driving a UI tells an agent
*what* happened on screen; debugging tells it *why*. ShadowDroid hands a coding
agent a live Android Studio debugger as plain JSON — so when a tap doesn't do
what the agent expected, it can set a breakpoint and read the actual program
state instead of guessing from screenshots. Reads are bounded, while attach,
pause/resume/step, breakpoint/watch changes, and evaluation have normal
debugger side effects. It is a debugger control surface, not a remote shell.

Backed by an optional Android Studio plugin:

- **`debug auto [app]`** — low-effort path: resolve an app alias/name/package,
  launch it, attach the Studio debugger when available, then return a full
  snapshot with setup guidance if the bridge is missing.
- **`debug`** — attach to the running app; set breakpoints (line, exception,
  method, field watchpoint; conditional, temporary, logpoints); read the call
  stack, local variables, and watches; evaluate/inspect expressions (`this`,
  locals, fields, array indexes) and follow object handles while the session
  remains suspended. Treat evaluation as real debugger evaluation rather than
  assuming arbitrary expressions are side-effect-free. Requests are bounded —
  they return structured failure instead of blocking without a suspended frame.
- **`debug snapshot`** — one shot: device + build, foreground app, screen tree,
  screenshot, recent logcat, and the live debugger stack / variables /
  breakpoints in a single JSON object.
- **`debug record` / `debug replay`** — JSONL timelines of screen changes,
  lifecycle, logcat, and replayable actions (taps, text, keys, swipes, drags).
- **`debug run-until-crash` / `step-until-screen-change` / `step-until-log`** —
  let the app run until something interesting happens, then return a full
  snapshot; crash waits emit parsed Java/native/ANR events and can write local
  bundles.
- **`debug native` / `debug tombstones` / `debug coroutines`** — native/mixed
  readiness, tombstone artifacts, and conservative suspended-state coroutine
  insight without arbitrary code execution. (For whole-process coroutine dumps
  from a *running* app with no debugger attached, see `aar coroutines`.)
- **`layout`** — UI-tree snapshots and diffs, enriched (when Studio's Layout
  Inspector is live) with Compose source locations, semantics, and recomposition
  counters.

Multiple devices debugged in one Studio are addressable: `debug sessions`
reports each session's device, stable `id` (for that Studio debug-session
lifetime), and current numeric index. Prefer `--session <id>`; the index remains
available for convenience but can change as sessions start/stop. Global `-d
<serial>` selects that device's session when no explicit session is supplied.

Everything degrades gracefully: with no Studio plugin running, the device and UI
commands still work and the debugger section just reports `available:false`.
Run `shadowdroid debug --help` and `shadowdroid layout --help` for the live
command surface.

## Agent integration

ShadowDroid is self-describing. `shadowdroid commands --json --depth 1` emits a
low-context top-level catalog; `commands --json --describe '<path>'` returns one
command with complete construction data; omitting `--depth` emits the full tree.
Schema version 2 carries canonical paths, global/command args, constraints,
output contracts, and agent decision hints straight from the CLI definition.

`shadowdroid init` installs/updates global agent skills automatically.
Project-scoped Codex `AGENTS.md` remains explicit so installers do not write
into an arbitrary current directory. `shadowdroid skill <agent>` is still
available when you want a specific integration file, project-scoped output, or a
dry run. Supported agents: `claude-code`, `cursor`, `codex`, `gemini`,
`antigravity`.

```bash
shadowdroid skill claude-code --install   # → ~/.claude/skills/shadowdroid/SKILL.md
shadowdroid skill cursor      --install   # → ~/.cursor/skills/shadowdroid/SKILL.md
shadowdroid skill gemini      --install   # → ~/.gemini/skills/shadowdroid/SKILL.md
shadowdroid skill antigravity --install   # → ~/.gemini/antigravity*/skills/shadowdroid/SKILL.md
shadowdroid skill codex                   # → prints an AGENTS.md section to stdout
shadowdroid skill codex --install         # → writes ./AGENTS.md after a safety check
```

Cursor `--install` creates a personal skill available across projects; pass
`--out /path/to/project/.cursor/rules/shadowdroid.mdc` to write a project-scoped
Cursor rule instead.

Installed skills are version-stamped. After upgrading the CLI, refresh them in
one shot. Pristine older files are rewritten; customized or markerless files
are preserved and reported. Explicit `--install` writes use the same safety
check. Pass `--force` only after reviewing the destination and intentionally
choosing to replace it:

```bash
shadowdroid skill --sync   # refresh every installed skill to this version
```

`connect` runs this refresh automatically (pristine skills only), so an upgraded
CLI keeps its installed skills current with no extra step.

## FAQ

**What is ShadowDroid?**
An open-source command-line tool that turns a real Android device or emulator
into a structured surface an AI agent can read, drive, debug, configure, and
instrument. It covers UI automation, app/device control, structured logcat and
crash triage, permissions, files, display profile, network interception, Android
Studio debugger state, Layout Inspector data, Compose recompositions, and an
optional in-app AAR for process/coroutine diagnostics plus explicit OkHttp
capture through its companion interceptor.

**Who is it for?**
Anyone pointing an AI or coding agent at a *running* Android app: building
agentic QA, reproducing bugs, automating end-to-end flows, or letting an agent
self-verify a UI change. It's equally handy by hand for quick scripted
automation.

**Is ShadowDroid a test framework?**
No. There's no assertion DSL or test runner to babysit — it's a fast, observable
control surface an agent drives live. It *can* launch your existing
instrumentation tests (`shadowdroid test`, which frees the `UiAutomation` slot
first), but it isn't a replacement for Espresso or JUnit.

**How is it different from Appium, Maestro, or Espresso?**
Those are built for authored test suites — WebDriver scripts, YAML flows,
compiled JUnit — running in CI. ShadowDroid is built for a *live agent loop*: a
persistent on-device service answers warm UI reads in tens of milliseconds, actions can fuse
their re-read (`--observe`), failures explain themselves, and the agent can
stream crash / toast / HTTP events or attach an Android Studio debugger. Use
those frameworks for regression suites; use ShadowDroid when an agent needs to
drive and reason about a running app right now.

**How is it different from `adb` and the `android` CLI?**
It complements them. Keep `adb` and the `android` CLI for scaffold, build,
deploy, and SDK management, then hand the *running* app to ShadowDroid. Raw
`adb shell uiautomator dump` is ~500 ms–1 s and stateless; `adb logcat` is an
unscoped firehose. ShadowDroid keeps a warm service, acts by selector,
turns logcat into structured `log`/`why` output, and streams events. See
[Why it exists](#why-it-exists).

**How do `why` and `log` differ from `adb logcat`?**
`adb logcat` is an unscoped text stream you have to grep and eyeball.
`shadowdroid log` scopes to the app, windows by time, dedups, and lifts crash
blocks out as parsed events with source-mapped frames — as JSON. `shadowdroid
why` goes one step further: instead of *lines*, it returns a *verdict* (was it a
crash, an ANR, a network failure, or just a different screen?) with the evidence
and next steps attached. Reach for `why` first, `log` for the detail behind it.

**Does it support Jetpack Compose?**
Yes — first-class, via AndroidX UI Automator 2.3.0+. Compose nodes appear in the
same element tree. When Android Studio's Layout Inspector is running,
`layout snapshot --compose --semantics --source-map` adds Compose semantics and
source locations, and `layout recompositions` reports recomposition counters.

**When should I use `net` versus `aar`?**
Use `net` first for proxy-aware HTTP(S): it is built into the host CLI, requires
no app code changes, and supports capture, intercept, mutation, rules, fixtures,
HAR/curl export, and replay. Use `aar` for apps you can build when you need the
debug-only in-app agent for process/coroutine diagnostics. For above-TLS HTTP,
install its optional OkHttp companion and explicitly add
`ShadowDroidCaptureInterceptor()` to the target debug clients; it handles
certificate-pinned OkHttp traffic but not Cronet, QUIC, or other HTTP stacks.

**Do I need Android Studio?**
Not for the core. The CLI plus `adb` cover UI automation, app lifecycle, network
capture, structured logs, and event streaming. Android Studio (via the optional
plugin) only adds the live debugger and Layout Inspector enrichment; without it
those sections report `available:false` and everything else keeps working.

**Which devices work? Emulators? Android TV?**
Real devices and emulators with USB debugging, plus Android TV / leanback, which
is focus + D-pad driven via `ui focus` and `ui key dpad_*`.

**Which agents can use it?**
Any agent that can run a shell command and read JSON. One-command skill install
ships for Claude Code, Cursor, Codex, Gemini, and Antigravity, and
`shadowdroid commands --json` emits the live catalog for anything else.

**What host platforms are supported?**
macOS, Linux, and Windows hosts (Homebrew, Scoop, or a one-line installer). The
target is always Android.

**Is it open source?**
Yes — licensed under Apache-2.0.

## License

Apache-2.0. See [LICENSE](LICENSE).
