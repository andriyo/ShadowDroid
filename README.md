<p align="center">
  <img src="docs/assets/shadowdroid-hero.png" alt="A Shadow Droid — a glossy black Imperial droid starfighter with a red sensor slash and segmented weapon booms" width="720">
</p>

# ShadowDroid

**Give your coding agent eyes, hands, and a debugger for Android.**

ShadowDroid is an open-source Android automation and debugging CLI for AI coding agents. It lets any shell-capable
agent — Claude Code, Codex, Cursor, Gemini, Antigravity etc — drive, inspect, and debug real Android apps, emulators,
and devices through structured JSON. While it's possible for AI agents to achieve this with just commonly available CLIs
like adb and android, it’s the only CLI that is optimized for speed and has an expertise layer built-in into it.
ShadowDroid doesn’t replace adb completely (only the slowest parts). It also has a built-in network proxy
specifically designed for AI agents. It also has a built-in debugger so agents can debug your code without inserting log
statements and recompiling. ShadowDroid can analyze app layouts, Compose recomposition counts etc. And all of that
information is available as a stream of JSON lines that are easy and inexpensive (token cost wise) for an agent to parse.

Most importantly, each output line has recommended next steps for the agent to do so the agent is informed contextually
about possible and suggested commands to run. ShadowDroid is an Android development expert system wrapped into a CLI
tool.

ShadowDroid is also optimized to run multiple debug automations at the same time. So your AI agents can work in parallel
on different devices/emulators on different projects.


[![Latest release](https://img.shields.io/github/v/release/andriyo/ShadowDroid?sort=semver&display_name=tag&label=release&color=blue)](https://github.com/andriyo/ShadowDroid/releases/latest)
[![CI](https://github.com/andriyo/ShadowDroid/actions/workflows/ci.yml/badge.svg)](https://github.com/andriyo/ShadowDroid/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/github/license/andriyo/ShadowDroid?color=blue)](LICENSE)
[![Platform: Android](https://img.shields.io/badge/platform-Android-3DDC84?logo=android&logoColor=white)](#install)

[Install](#install) · [Quickstart](#quickstart) ·
[Why ShadowDroid](#why-shadowdroid) · [Docs](#documentation) ·
[Field Lab](#the-field-lab-sample)

## The loop, in three commands


**1. The agent reads the screen** — parsed elements with stable selectors, not
a screenshot to squint at:

```jsonc
$ shadowdroid ui dump
{"screen_hash":"c50fd462de4304be","interaction_hash":"i:178f2b73ce0e1447",
 "snapshot_state":"consistent","viewport":{"w":1080,"h":2424},
 "current_app":{"package":"io.github.andriyo.shadowdroid.sample",
                "activity":"io.github.andriyo.shadowdroid.sample.MainActivity"},
 "element_count":39,
 "elements":[…,
   {"id":3,"text":"Recover the\nsilent relay.","tap":[336,777]},
   {"id":30,"rid":"nav_mission","tap":[403,2256],"clickable":true,
    "handle":"i:178f2b73ce0e1447/e:3"}, …]}
```

**2. It acts by selector and proves the destination in the same call** — tap,
wait for the screen to settle, check the postcondition, and return the new
screen, one round-trip:

```jsonc
$ shadowdroid ui tap --rid nav_mission --expect-text "Night Shift: Signal Recovery"
{"type":"action","cmd":"tap","ok":true,
 "matched_element":{"id":30,"rid":"nav_mission","tap":[403,2256], …},
 "action":"accessibility_click","action_delivered":true,
 "settle_ms":1878,"screen_changed":true,
 "postcondition":{"kind":"text","expected":"Night Shift: Signal Recovery",
                  "matched":true, …},
 "postcondition_satisfied":true,
 "screen":{"screen_hash":"87332662a59bfb79","element_count":45,"elements":[…]}}
```

**3. When the app breaks, the error is the diagnosis.** After tapping the
Field Lab's deliberate crash fixture, one bounded read returns the verdict —
no logcat spelunking:

```jsonc
$ shadowdroid why
{"type":"action","cmd":"why","ok":true,"verdict":"app_crashed",
 "explanation":"the app process crashed — see evidence.crash (project_frames point into your code)",
 "evidence":{"crash":{"kind":"java","exception":"java.lang.RuntimeException",
   "message":"Deliberate ShadowDroid sample crash",
   "stack":["io.github.andriyo.shadowdroid.sample.MainActivity.crashNow(MainActivity.kt:240)", …]}},
 "hints":["shadowdroid log --last 5m   # full crash context",
          "shadowdroid app start       # relaunch after the fix"]}
```

Every response ends with machine-readable `next_actions`, and if the app
crashed since your previous command, the next response — success or error —
carries the parsed crash as an `events` array. The crash finds the agent, not
the other way around. Full details: [the output
contract](docs/output-contract.md).

## Install

Homebrew:

```bash
brew install andriyo/tap/shadowdroid
```

Shell installer (macOS / Linux):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.sh | sh
```

Windows (Scoop, or the PowerShell installer):

```powershell
scoop bucket add andriyo https://github.com/andriyo/scoop-bucket
scoop install shadowdroid
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/andriyo/ShadowDroid/releases/latest/download/shadowdroid-installer.ps1 | iex"
```

You also need Android Platform Tools (`adb`) on PATH — the installers print a
hint if it's missing (macOS: `brew install --cask android-platform-tools`;
Windows: `scoop install adb`). Manual binaries are attached to each [GitHub
Release](https://github.com/andriyo/ShadowDroid/releases/latest).

## Quickstart

Start an emulator or plug in a device with USB debugging, then:

```bash
shadowdroid devices     # list attached devices and emulators
shadowdroid connect     # install the on-device service, forward, verify
shadowdroid ui dump     # read the current screen as JSON
```

On first `connect`, the CLI installs a version-matched, SHA-256-verified APK
pair on the device (downloaded from the matching GitHub Release and cached
under `~/.shadowdroid/`). Later calls reuse the live server. If anything is
off, `shadowdroid doctor` diagnoses the pipe and `doctor --fix` repairs it.

Now act by selector and verify the outcome in one call — selectors here are
illustrative; use your app's:

```bash
shadowdroid ui tap --text "Sign in" --expect-text "Welcome"
```

And when something surprises you:

```bash
shadowdroid why
```

### Point your agent at it

One command installs the agent skill and the Android Studio plugin (skip the
plugin with `--no-studio-plugin`):

```bash
shadowdroid init
```

Then ask your agent to drive: the skill teaches it the loop, and
`shadowdroid commands --json --depth 1` gives any agent the full,
self-describing command catalog — canonical paths, argument construction data,
output contracts, and decision hints. The operating contract agents follow is
written down in [the agent loop](docs/agent-loop.md).

> **Running Espresso / UI Automator tests?** While connected, ShadowDroid holds
> the device's single `UiAutomation` slot, so instrumentation tests fail with
> `UiAutomationService ... already registered!`. Wrap them in
> `shadowdroid test -- ./gradlew connectedDebugAndroidTest`, which disconnects
> first and reconnects after — or run `disconnect`/`connect` yourself.

## Why ShadowDroid

The tools you'd otherwise reach for each fall short in a live agent loop:

| Tool | Gap for a live agent loop |
| --- | --- |
| `adb shell uiautomator dump` | A fresh dump process per read, writing raw XML to `/sdcard` for you to pull and parse. No selectors, no actions, no events. |
| `adb shell input tap` | Stateless coordinate injection: no idea what's on screen, fragile to any layout change. |
| `adb logcat` | An unscoped text firehose — no app scoping, no structure, crash blocks buried in noise. |
| Appium / Maestro / Espresso | Built for authored test suites running in CI, with a server, DSL, or compiled tests between the agent and the device. |

ShadowDroid keeps a **persistent service on the device**, so reads are warm:
on our reference setup (Apple-silicon macOS host, arm64 API 36 emulator,
August 2026), a warm `ui dump` returns parsed JSON in **~60–70 ms end-to-end**
including CLI process start, while a single raw `uiautomator dump` takes
~130–190 ms before you've pulled or parsed anything. Cold paths (first
connect, APK install) are slower. Fast reads matter because the agent observes
after every action — and every round-trip saved is an LLM inference saved.

Speed is the smallest part. The persistent service is what makes the rest
possible:

- **Act + observe fused**: postconditions, stale-screen guards, and the
  resulting screen in the same response.
- **Failures that explain themselves**: what *is* on screen, ranked
  near-matches, crash events riding the next response.
- **A debugger an agent can read**: Android Studio breakpoints, stacks,
  variables, and Layout Inspector data as JSON.

ShadowDroid is a **complement, not a replacement**: keep `adb`, Android
Studio, and the `android` CLI for scaffold/build/deploy/SDK work, and keep
Espresso for regression suites. ShadowDroid takes over once the app is
running — it is the live runtime control plane for an AI development and
debugging loop. It even follows the `android` CLI's conventions (`init`,
`skill`, `layout`, `studio`), so it slots in beside it.

## What your agent gets

**Robust selector actions with guards.** Tap / type / swipe / scroll /
long-press by `--rid`, `--text`, `--desc`, or `--xpath` — never brittle
coordinates. Ambiguity is a structured error listing candidates, never a
guess. `--observe` fuses act + settle + re-read into one call; `--expect-*`
proves the destination; `--if-screen`/`--if-interaction` refuse to act on a
screen that changed since you read it; screen-bound handles catch stale
elements before input is delivered. First-class Jetpack Compose and Android
TV (D-pad focus) support. → [docs/agent-loop.md](docs/agent-loop.md)

**Failures that explain themselves.** A missed selector returns `top_texts`
(what the screen actually shows) and `closest` (ranked near-matches); a
timeout reports what the screen became; a crash since your last command rides
the next response as an `events` array:

```jsonc
$ shadowdroid ui tap --text "Sign in"
{"type":"error","ok":false,"code":"element_not_found","retryable":true,
 "detail":{"top_texts":["DETERMINISTIC FIXTURES","Challenge catalog", …],
           "closest":[{"id":43,"text":"Signals","score":0.5, …}, …], …},
 "next_actions":["inspect detail.top_texts and detail.closest when present", …]}
```

→ [docs/output-contract.md](docs/output-contract.md)

**Structured logs and one-verb triage.** `log` turns logcat into bounded,
app-scoped, deduplicated JSON with crash/ANR blocks parsed out and stack
frames mapped to your source files. `why` fuses crash + logs + screen +
network failures into a single verdict with evidence and next steps.
`collect` bundles everything for a hand-off. →
[docs/debugging.md](docs/debugging.md)

**The debugger, as JSON.** Through the optional Android Studio plugin, the
agent gets a live debugger it can read: breakpoints (line, exception, method,
field; conditional and temporary), call stacks, threads, variables, watches,
expression evaluation, non-suspending logpoints with structured hit streams,
coroutine insight, and Layout Inspector data — Compose source locations and
recomposition counts. UI polling tells an agent *what* happened; this tells it
*why*. → [docs/debugging.md](docs/debugging.md)

**The network, observed and shaped.** A host-side MITM proxy built into the
binary captures decrypted HTTP(S) — HTTP/2, SSE, streaming bodies,
`gzip`/`deflate`/`br`/`zstd` — and WebSocket frames. The agent can intercept
and mutate flows in-flight, declare rewrite rules, inject or rewrite WS
frames, export HAR/curl/fixtures, and replay recorded backends. An optional
debug-only in-app companion captures certificate-pinned OkHttp traffic above
TLS. → [docs/network.md](docs/network.md)

**A full operator console.** App lifecycle and state snapshot/restore, runtime
permissions and app-ops, files (including private app dirs via `--run-as`),
display profiles, device controls, crash-safe segmented screen video with
timeline markers, and a live `watch` stream of screen diffs, crashes, toasts,
and HTTP events. Mutations verify their postconditions instead of trusting a
silent shell. → [docs/device-state.md](docs/device-state.md)

## How it works

```
        Laptop                          adb forward                Android device
  ┌───────────────────────┐    (per-device host port → 7912)   ┌───────────────────────────┐
  │  shadowdroid (Rust)   │  ──── HTTP + JSON (loopback) ───▶  │  instrumentation APK      │
  │  • clap CLI           │                                    │  • Ktor server            │
  │  • watch/log/why      │  ◀────────  adb logcat  ─────────  │  • UiDevice (AndroidX     │
  │  • net MITM proxy     │                                    │    UI Automator 2.3.0+)   │
  └───────────────────────┘                                    └───────────────────────────┘
```

The on-device APK answers low-latency UI reads and performs UI/device actions.
The host CLI owns orchestration: watch diffing, logcat parsing for
`log`/`why`/crash events, act+observe fusion, source mapping, and recovery —
so host-side evidence keeps working even when the on-device server is down.
The wire protocol is loopback HTTP/JSON, but the supported public interface
for agents is the CLI and `shadowdroid commands --json`.

Three optional integrations extend the same command surface, and everything
degrades gracefully without them:

- **Android Studio plugin** — exposes the debugger and Layout Inspector to
  `shadowdroid debug ...` and `shadowdroid layout ...` (installed by
  `shadowdroid init` or `studio install`).
- **Built-in MITM proxy** — `shadowdroid net ...` wires the device through
  `adb reverse` and restores the previous proxy state on stop.
- **Debug-only in-app AAR** — `shadowdroid aar ...` adds process/coroutine
  diagnostics and explicit above-TLS OkHttp capture to apps you build.

## The Field Lab sample

[`samples/shadowdroid-test-app`](samples/shadowdroid-test-app) is a stateful
showcase app for driving ShadowDroid against a real package — the same app the
transcripts above ran against:

| Destination | What it exercises |
| --- | --- |
| **Overview** | Mission progress, live posture, workspace shortcuts, event trail |
| **Mission** | A three-gate "Night Shift: Signal Recovery" run: validation, semantic sliders, gated state, a long press |
| **Signals** | HTTP request composer, live WS/WSS channel, WebView |
| **Lab** | Deterministic fixtures: selectors, ranges, windows, permissions, lifecycle, storage, coroutines, logs, crash and ANR paths |

```bash
./server/gradlew -p samples/shadowdroid-test-app :app:assembleDebug
shadowdroid app reinstall \
  samples/shadowdroid-test-app/app/build/outputs/apk/debug/app-debug.apk \
  --grant-all --wait-front
shadowdroid app start io.github.andriyo.shadowdroid.sample --activity .MainActivity
```

Command-by-command walkthroughs — the Night Shift mission, a selector
gauntlet with deliberate ambiguity, and a live WebSocket channel — are in
[`samples/README.md`](samples/README.md).

## Agent integrations

Skills ship for five agents; `shadowdroid init` installs/updates user-scoped
skills automatically, or install one explicitly:

```bash
shadowdroid skill claude-code --install # → ~/.claude/skills/shadowdroid/SKILL.md
shadowdroid skill cursor      --install # → ~/.cursor/skills/shadowdroid/SKILL.md
shadowdroid skill codex       --install # → ~/.agents/skills/shadowdroid/SKILL.md
shadowdroid skill gemini      --install # → ~/.gemini/skills/shadowdroid/SKILL.md
shadowdroid skill antigravity --install # → ~/.gemini/config/skills/shadowdroid/SKILL.md
```

Add `--scope project` for per-repository installs (Claude Code uses
`.claude/skills/`; the others share `.agents/skills/`). Installed skills are
version-stamped: `shadowdroid skill --sync` refreshes pristine installs after
an upgrade (`connect` does this automatically for user scope), while
customized files are preserved and reported — `--force` is required to
replace them. Any other agent that can run a shell command can bootstrap from
`shadowdroid commands --json`.

## Documentation

- [The agent loop](docs/agent-loop.md) — the canonical read → act → confirm
  contract: selectors, hashes, guards, handles, fusion flags, TV, and the full
  command surface.
- [The output contract](docs/output-contract.md) — envelopes, exit codes,
  `next_actions`, `events`, streaming JSONL, self-describing failures.
- [Configuration](docs/configuration.md) — folder config, app aliases, named
  device targets, AVD claims, source mapping, per-project proxy CAs.
- [Network](docs/network.md) — HTTP(S)/WebSocket capture, interception,
  rules, fixtures and replay, CA management, the OkHttp AAR companion.
- [Triage and debugging](docs/debugging.md) — `why`/`log`/`collect`, the
  Android Studio debugger surface, logpoints, layout/Compose inspection.
- [Device and state controls](docs/device-state.md) — app state
  snapshot/restore, permissions/app-ops, files, display profiles, screen
  video evidence.
- [Security and redaction](docs/security-and-redaction.md) — the redaction
  policy, pixel boundaries, trust model, and what stays local.
- [Field Lab walkthroughs](samples/README.md) — scripted journeys against the
  sample app.


## Contributing

If ShadowDroid improves your Android agent loop, starring the repository
helps other Android developers find it.

Three concrete ways to help:

- **Report a compatibility gap.** Ran it against a device, OS version, or app
  where something misbehaved? [Open an
  issue](https://github.com/andriyo/ShadowDroid/issues) — `shadowdroid
  collect` produces the diagnostic bundle worth attaching (review it for
  private data first).
- **Extend the Field Lab.** Add a fixture or journey to
  [`samples/shadowdroid-test-app`](samples/shadowdroid-test-app) that breaks
  selectors, capture, or recovery in an interesting way.
- **Improve an agent integration.** The skill templates and install targets
  live in the CLI (`shadowdroid skill`); a new agent or a sharper skill is a
  contained contribution.

Developing the CLI needs stable Rust (`cargo test --locked --all-targets` in
[`cli/`](cli/)); the on-device server, sample app, and Android Studio plugin
are Gradle projects (`server/`, `samples/`, `shadowdroid-plugin/`).

## FAQ

**Is ShadowDroid a test framework?**
No. There's no assertion DSL or test runner — it's a fast, observable control
surface an agent drives live. It *can* launch your existing instrumentation
tests (`shadowdroid test`, which frees the `UiAutomation` slot first), but it
isn't a replacement for Espresso or JUnit.

**How is it different from Appium, Maestro, or Espresso?**
Those are built for authored test suites — WebDriver scripts, YAML flows,
compiled JUnit — running in CI. ShadowDroid is built for a *live agent loop*:
a persistent on-device service answers warm UI reads fast, actions fuse their
re-read (`--observe`), failures explain themselves, and the agent can stream
crash/toast/HTTP events or attach a debugger. Use those frameworks for
regression suites; use ShadowDroid when an agent needs to drive and reason
about a running app right now.

**How is it different from `adb` and the `android` CLI?**
It complements them. Keep them for scaffold, build, deploy, and SDK
management, then hand the *running* app to ShadowDroid: warm structured reads
instead of one-shot XML dumps, selector actions instead of raw coordinates,
scoped structured `log`/`why` instead of an unscoped logcat firehose. See
[Why ShadowDroid](#why-shadowdroid).

**Does it support Jetpack Compose?**
Yes — first-class, via AndroidX UI Automator 2.3.0+. Compose nodes appear in
the same element tree. With Android Studio's Layout Inspector running,
`layout` adds Compose semantics, source locations, and recomposition
counters; `ui dump --deep` cross-checks the accessibility tree against
Compose data and can target nodes the normal tree misses.

**Do I need Android Studio?**
Not for the core. The CLI plus `adb` cover UI automation, app/device control,
network capture, structured logs, and event streaming. The optional Studio
plugin adds the live debugger and Layout Inspector enrichment; without it
those sections report `available:false` and everything else keeps working.

**Which devices work?**
Real devices and emulators with USB debugging, plus Android TV / leanback
(focus + D-pad driven via `ui focus` and `ui key dpad_*`). Projects can bind
named targets (e.g. `mobile`, `tv`) to stable AVDs or serials — see
[configuration](docs/configuration.md).

**Which agents can use it?**
Any agent that can run a shell command and read JSON. Skills ship for Claude
Code, Cursor, Codex, Gemini, and Antigravity; `shadowdroid commands --json`
is the live catalog for everything else.

**When do I use `net` versus `aar` for HTTP?**
`net` first — no app changes needed. `aar` when the app pins its certificates
(OkHttp only) or you want in-process diagnostics. See
[docs/network.md](docs/network.md).

## Credits

The name — and the banner above — come from the **Shadow Droid**, the Imperial
droid starfighter piloted by a surgically implanted brain in the *Star Wars*
expanded universe ([Wookieepedia](https://starwars.fandom.com/wiki/Shadow_Droid)).

The banner is a render of
[*-Star Wars- Dark Empire Shadow Droid*](https://sketchfab.com/3d-models/star-wars-dark-empire-shadow-droid-290d87db98d24fc199243031cfb4df04)
by **ARKON MAREK**, used and modified under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/). The model is his;
the materials, lighting, and camera are this project's.

ShadowDroid is an independent project, not affiliated with or endorsed by
Lucasfilm Ltd. or The Walt Disney Company. *Star Wars* and related marks are
trademarks of Lucasfilm Ltd.

## License

Apache-2.0. See [LICENSE](LICENSE).
