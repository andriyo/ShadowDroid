# ShadowDroid

**A fast Android IDE for your AI agents** — because your coding agent deserves the
best, fastest, most reliable tools to drive and debug Android apps.

ShadowDroid turns a real Android device or emulator into a structured surface an
AI agent can read and act on. It pairs a single static binary on your laptop
with a tiny Kotlin instrumentation service on the device, and exposes the whole
app UI as JSON. Your agent reads the screen as a flat list of elements, taps /
types / swipes / scrolls **by selector**, waits for state to settle, streams
crashes and toasts as events, and drops into Android Studio-backed debugging and
layout inspection — all at roughly **25 ms per UI dump**, fast enough that the
agent loop never stalls.

There's no test DSL and no extra runtime to babysit — just the CLI and `adb`.
Every action is one command that prints one line of JSON: `shadowdroid ui …` for
live UI automation, `shadowdroid app …` for app lifecycle, `shadowdroid net …`
for HTTP(S) traffic, and so on. That makes it trivial to wire into any agent —
point it at the command catalog and let it drive.

```jsonc
$ shadowdroid ui dump
{"screen_hash":"a1b2…","viewport":{"w":1080,"h":2424},"current_app":{…},"element_count":42,
 "elements":[{"id":7,"rid":"main_tab_profile","tap":[980,2256],"clickable":true}, …]}

$ shadowdroid ui tap --rid main_tab_profile
{"type":"action","cmd":"tap","via":"selector","x":980,"y":2256,"matched":true}

$ shadowdroid ui wait --text "Welcome back" --timeout-ms 5000
{"type":"action","cmd":"wait","matched":true,"gone":false,"screen_hash":"c3d4…"}
```

> Android-only by design, and not a test framework — ShadowDroid is the fast,
> observable layer an agent drives directly.

## Why it exists

To drive a *running* app in a tight agent loop, the tools you'd otherwise reach
for each fall short:

| Tool                              | Gap for a live agent loop                                                          |
| --------------------------------- | --------------------------------------------------------------------------------- |
| `adb shell uiautomator dump`      | ~500ms–1s per dump — the loop stalls between every step.                           |
| `adb shell input tap`             | Stateless: no idea what's on screen, fragile to any layout change.                 |
| `android` CLI (`layout`/`screen`) | Built for project create / build / run / SDK — and great at it. But for live UI, each `layout` call runs a fresh `ui-dump` (the slow path): no persistent service, no streaming loop, no interaction-by-selector, no crash/popup events, no agent debugger. |

ShadowDroid is the **complement, not a replacement**. Keep using the `android`
CLI to scaffold, build, deploy, and manage the SDK — then hand the *running* app
to ShadowDroid. A persistent on-device service keeps dumps at ~25ms, a streaming
JSON event model lets the agent follow the app live, and it ships with
**first-class Jetpack Compose support** (AndroidX UI Automator 2.3.0+),
**built-in crash detection**, **declarative popup watchers**, and — uniquely — an
**agent-facing Android Studio debugger** (see [Agent debugging](#agent-debugging)).
It even follows the `android` CLI's own conventions (`init`, `skills`, `layout`,
`studio`), so it slots in right beside it.

## How it works

```
        Laptop                         adb forward                Android device
  ┌───────────────────────┐         tcp:7912 ⇆ 7912        ┌───────────────────────────┐
  │  shadowdroid (Rust)   │  ── HTTP + JSON (loopback) ──▶  │  instrumentation APK      │
  │  • clap CLI           │                                 │  • Ktor 3 / CIO server    │
  │  • XML → element JSON  │ ◀────────  adb logcat  ──────── │  • UiDevice (AndroidX     │
  │  • watch/crash/watcher │                                 │    UI Automator 2.3.0+)   │
  └───────────────────────┘                                 └───────────────────────────┘
```

The on-device APK is a **stateless RPC over UI Automator** — it just exposes
`UiDevice.click / swipe / dump` and a toast monitor over HTTP. All *policy* lives
on the laptop: the dump-then-diff watch loop, crash parsing from logcat, the
watcher rule engine, and the XML→JSON transform. That keeps the APK tiny and lets
it rev independently of the CLI.

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
> Run `shadowdroid disconnect` before the test run, then `shadowdroid connect`
> again. `connect` reports this in its `ui_automation` field and `doctor` shows
> the current slot owner.

Initialize host integrations (Android Studio plugin for debugger + layout,
plus agent skills):

```bash
shadowdroid init                    # install/update Studio plugin + agent skills
shadowdroid init --no-studio-plugin # only inspect Studio and install skills
```

Put repeated values in config instead of spending prompt/context on every
command. ShadowDroid loads `~/.shadowdroid/config.json` first, then
`.shadowdroid.json` files from the current directory's ancestors, with the
nearest project file winning:

```bash
shadowdroid config schema --json
shadowdroid config init --project --app Livd --package com.livd --project-path /Users/you/Work/Livd
shadowdroid config validate --json
```

```json
{
  "device": "emulator-5554",
  "app": "Livd",
  "project": "/Users/you/Work/Livd",
  "apps": {
    "Livd": {
      "package": "com.livd",
      "run_configuration": "app",
      "debugger": "Android Debugger"
    }
  }
}
```

## What you can drive

Every command prints a single JSON event. Selectors are consistent across commands:
`--text`, `--rid` (resource id), `--desc` (content description), and `--xpath`.
A typical agent reads `ui dump` once, acts by `--rid`/`--text`, and re-reads only
when `screen_hash` changes.

Text/desc selectors match as a case-insensitive **substring** by default. On
`ui find`/`ui tap`, add `--exact` to require a full match (so `--text Allow` won't
hit a label reading "Allow Disney+…") and `--clickable` to skip non-clickable
labels in favor of the actual button. `--rid` is the most reliable target when a
stable resource id exists. Curly and straight quotes/apostrophes are matched
interchangeably, so `--text "Don't allow"` matches UI text rendered with a
typographic apostrophe.

Results go to **stdout**; ShadowDroid's own logs go to **stderr**, so `… | jq`
already sees clean JSON. Add `--quiet`/`-q` (or `SHADOWDROID_QUIET=1`) to silence
those logs entirely — handy when you pipe with `2>&1`.

| Group            | Commands                                                                                          |
| ---------------- | ------------------------------------------------------------------------------------------------- |
| **UI**           | `ui dump`, `ui screenshot`, `ui find`, `ui tap`, `ui double-tap`, `ui long-tap`, `ui swipe`, `ui drag`, `ui swipe-ext`, `ui pinch`, `ui scroll-to`, `ui text`, `ui key`, `ui back`, `ui home`, `ui wait`, `ui toast` |
| **Authoring**    | `ui audit` (flag elements with no stable selector), `ui gen` (scaffold a Screen Object), `net export fixtures` (replayable response set + `manifest.json`, GraphQL keyed by operationName), `test` (run an instrumentation test with the slot freed), `debug replay --repeat --diff` (flake hunt) |
| **Watch**        | `watch` (screen changes, crashes, toasts, watcher actions, and HTTP events when `net start` is running) |
| **Layout**       | `layout snapshot` / `layout diff` / `layout source` / `layout recompositions`                      |
| **App**          | `app start` / `stop` / `install` / `reinstall` / `clear` / `info` / `wait` / `current`             |
| **Permissions**  | `perm grant` / `revoke` / `list` / `reset`, `appops get` / `set`                                   |
| **Device**       | `device info` / `shell` / `wake` / `sleep` / `unlock` / `orientation` / `clipboard` / `notifications` / `quick-settings` / `open-url` |
| **Files**        | `files ls` / `push` / `pull`                                                                       |
| **Network**      | `net check` / `trust` / `start` / `stop` / `status`, `net log` / `show` / `export`, `net intercept` / `resume` / `drop` / `respond`, `net rule …` / `replay` |
| **Agent (AAR)**  | `aar install` / `status` / `remove` (wire the in-app debug agent), `aar capture` (in-app, **above-TLS** flows → `net export fixtures`), `aar intercept` / `resume` / `drop` / `agent` (in-app agent-in-the-loop modify — works on **pinned / Cronet / QUIC**, no CA) |
| **Display**      | `profile snapshot` / `apply` / `reset` (animations, font, density, size, rotation)                 |
| **Debug**        | `debug auto` / `snapshot` / `record` / `replay`, `debug attach` / `break` / `stack` / `variables` / `eval` / `inspect`, `debug native` / `tombstones` / `coroutines`, `debug run-until-crash` |
| **Session**      | `devices`, `connect`, `disconnect`, `test`, `doctor`, `collect`, `config`, `update`, `commands`, `skill`, `studio`, `init` |

`watch` is the streaming workhorse — it emits debounced, hash-diffed `screen`
events plus `crash`, `toast`, `watcher_fired`, and `http` events when a `net`
proxy is running. If network capture is not available, `watch` emits a structured
`warning` event with the suggested recovery command, so an agent can decide
whether to run `net start` or continue UI/crash-only (`watch --no-net`).

`net` is an embedded MITM proxy (built into the single binary — no Python, no
external mitmproxy). `net start` points the device at it; decrypted HTTP(S)
transactions then stream as `http` events on the same timeline as `screen`
when `watch` is running. Beyond observing, the agent can **intercept** a flow —
`net intercept` pauses matching requests/responses and emits them as
`http_intercept` events on `watch`; the agent inspects with `net show`, then releases with
`net resume --set-status/--body/…`, `net drop`, or `net respond` (a canned
reply). Repeated edits can be promoted to declarative `net rule`s (map-local /
map-remote / set-status / set-header / replace / block / delay) or served offline
from a saved session with `net replay`. `net check <app>` reports whether a build
is interceptable; `net export har|curl` hands a flow to other tools.

Run `shadowdroid commands` for the full command tree, or `shadowdroid --help` on
any subcommand for its flags.

## Agent debugging

**This is the part nothing else gives an agent.** Driving a UI tells an agent
*what* happened on screen; debugging tells it *why*. ShadowDroid hands a coding
agent a live Android Studio debugger as plain JSON — so when a tap doesn't do
what the agent expected, it can set a breakpoint and read the actual program
state instead of guessing from screenshots. It's a bounded, read-only surface
designed for autonomous use, not a remote shell.

Backed by an optional Android Studio plugin:

- **`debug auto [app]`** — low-effort path: resolve an app alias/name/package,
  launch it, attach the Studio debugger when available, then return a full
  snapshot with setup guidance if the bridge is missing.
- **`debug`** — attach to the running app; set breakpoints (line, exception,
  method, field watchpoint; conditional, temporary, logpoints); read the call
  stack, local variables, and watches; evaluate/inspect read-only expressions
  (`this`, locals, fields, array indexes) and follow object handles while the
  session remains suspended. Requests are bounded — they return a structured
  `ok:false` instead of blocking when no suspended frame is available.
- **`debug snapshot`** — one shot: device + build, foreground app, screen tree,
  screenshot, recent logcat, and the live debugger stack / variables / breakpoints
  in a single JSON object.
- **`debug record` / `debug replay`** — JSONL timelines of screen changes,
  lifecycle, logcat, and replayable actions (taps, text, keys, swipes, drags).
- **`debug run-until-crash` / `step-until-screen-change` / `step-until-log`** —
  let the app run until something interesting happens, then return a full snapshot;
  crash waits emit parsed Java/native/ANR events and can write local bundles.
- **`debug native` / `debug tombstones` / `debug coroutines`** — native/mixed
  readiness, tombstone artifacts, and conservative suspended-state coroutine
  insight without arbitrary code execution.
- **`layout`** — UI-tree snapshots and diffs, enriched (when Studio's Layout
  Inspector is live) with Compose source locations, semantics, and recomposition
  counters.

Everything degrades gracefully: with no Studio plugin running, the device and UI
commands still work and the debugger section just reports `available:false`.
Run `shadowdroid debug --help` and `shadowdroid layout --help` for the live
command surface.

## Agent integration

ShadowDroid is self-describing. `shadowdroid commands --json` emits the full
command catalog (names, nesting, args, help, and agent-facing decision hints)
straight from the CLI definition — the machine-readable counterpart to `--help`
that an agent reads once to discover the whole tool.

`shadowdroid init` installs/updates global agent skills automatically.
Project-scoped Codex `AGENTS.md` remains explicit so installers do not write
into an arbitrary current directory. `shadowdroid skill <agent>` is still
available when you want a specific integration file, project-scoped output, or
a dry run. Supported agents: `claude-code`, `cursor`, `codex`, `gemini`,
`antigravity`.

```bash
shadowdroid skill claude-code --install   # → ~/.claude/skills/shadowdroid/SKILL.md
shadowdroid skill cursor      --install   # → ~/.cursor/skills/shadowdroid/SKILL.md
shadowdroid skill gemini      --install   # → ~/.gemini/skills/shadowdroid/SKILL.md
shadowdroid skill antigravity --install   # → ~/.gemini/antigravity*/skills/shadowdroid/SKILL.md
shadowdroid skill codex                   # → prints an AGENTS.md section to stdout
```

Cursor `--install` creates a personal skill available across projects; pass
`--out /path/to/project/.cursor/rules/shadowdroid.mdc` to write a project-scoped
Cursor rule instead.

Installed skills are version-stamped. After upgrading the CLI, refresh them in
one shot — unmodified skills are rewritten in place, hand-edited ones are left
alone (pass `--force` to overwrite those too):

```bash
shadowdroid skill --sync   # refresh every installed skill to this version
```

`connect` runs this refresh automatically (pristine skills only), so an upgraded
CLI keeps its installed skills current with no extra step.


## License

Apache-2.0. See [LICENSE](LICENSE).
