# ShadowDroid — Architecture

## 1. Goals & non-goals

**Goals**
- A single static Rust binary on the laptop. No Python, no Java/Node/Gradle to run it. Distribute via `cargo install` or GitHub release tarballs.
- Sub-100ms per UI dump in steady state (the on-device service stays alive and serves HTTP).
- A streaming JSON-line event model an LLM agent can read directly (`screen`, `action`, `crash`, `watcher_fired`).
- Latest AndroidX UI Automator (2.3.0+) for first-class Compose support.
- Built for one developer to drive their own app, scales to a fleet later if needed.

**Non-goals (initial)**
- Multi-device fleet management — we'll handle one device at a time. The HTTP layer makes this easy to lift later.
- Selenium-WebDriver / Appium protocol compatibility. The whole point is *not* being Appium.
- iOS, web, desktop. Android-only.
- A test framework (assertions, fixtures, etc.). ShadowDroid is the layer underneath.

## 2. Components

```
┌────────────────────────────────────────────────┐    adb forward     ┌──────────────────────────────────────────┐
│              Laptop                            │   tcp:7912 ⇆ 7912   │              Android device              │
│                                                │ ────────────────────│                                          │
│  ┌──────────────────────────────────────────┐  │                     │  ┌────────────────────────────────────┐  │
│  │  shadowdroid (Rust binary)               │  │                     │  │  io.github.andriyo.shadowdroid     │  │
│  │  ──────────────────────────────────────  │  │     HTTP + JSON     │  │  (Instrumentation APK)             │  │
│  │  • clap CLI subcommands                  │ <┼─────────────────────┼> │  • Ktor 3 / CIO (port 7912)        │  │
│  │  • tokio + reqwest HTTP client           │  │                     │  │  • UiDevice (AndroidX UA 2.3.0)    │  │
│  │  • adb_client (forward, install, push)   │  │                     │  │  • Toast accessibility monitor     │  │
│  │  • XML → flat-element JSON parser        │  │                     │  │  • Lifecycle: started via         │  │
│  │  • Watch loop (debounce + hash diff)     │  │                     │  │    `am instrument`                 │  │
│  │  • Logcat tail → crash event parser      │  │                     │  └────────────────────────────────────┘  │
│  │  • Watcher rule engine                   │  │                     │                                          │
│  │  • APK lifecycle manager                 │  │                     │              also:                       │
│  │  • Event-stream JSON emitter             │  │     adb logcat      │  ┌────────────────────────────────────┐  │
│  └──────────────────────────────────────────┘ <┼─────────────────────┤  │  Android system logcat            │  │
│                                                │                     │  │  (FATAL EXCEPTION, libc:F, ANR)   │  │
└────────────────────────────────────────────────┘                     │  └────────────────────────────────────┘  │
                                                                       └──────────────────────────────────────────┘
```

## 3. What lives where, and why

The line is drawn at "**is this an Android API or a pure-data transformation?**"

| Concern                          | Location | Why                                                                                                                                |
| -------------------------------- | -------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `UiDevice.click/swipe/dump`      | Server   | UI Automator is JVM-only and requires an Instrumentation context. Has to live on device.                                            |
| Toast monitor                    | Server   | Toasts surface as `AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED`; needs an on-device accessibility listener.                  |
| XPath query                      | Server   | We let the on-device `By.xpath(...)` do the matching against the live tree — cheaper than dumping the whole hierarchy first.        |
| ADB itself (push, install, forward) | CLI     | Standard developer surface; no need to be on device.                                                                                |
| Logcat tail + crash parsing      | CLI      | Logcat is per-device but the parsing is pure regex. Keeps the on-device APK tiny.                                                   |
| Watcher rule engine              | CLI      | Rules fire on each screen emit; no benefit from running on device. Lets us evolve rule syntax without re-shipping the APK.          |
| XML → flat-element JSON          | CLI      | Pure data transformation. Doing it on the laptop means the wire format can stay as raw UI Automator XML, which is stable.           |
| Debounce + hash-diff watch loop  | CLI      | Wall-clock orchestration. Easier to control + observe on the laptop.                                                                |
| APK lifecycle                    | CLI      | Install / version-check / restart on idle — only the laptop knows about versioning.                                                 |
| Streaming JSON event emission    | CLI      | The agent-facing API. CLI is the public surface; on-device APK is an implementation detail.                                         |

**Design principle:** the on-device APK is *a stateless RPC over UI Automator*. All policy lives on the laptop. The APK is small enough to rev independently — and any time we need a new device-side capability (e.g., "give me the system UI overlay state"), we add one endpoint.

## 4. Lifecycle

### First run
1. User runs `shadowdroid connect` (or any other command).
2. CLI talks to `adb` (via `adb_client`) to enumerate devices. Picks one or errors with "use --device".
3. CLI **resolves which APK to install** by walking this precedence chain
   (first hit wins):
   1. `--apk PATH` flag (per-invocation override)
   2. `SHADOWDROID_APK` env var (per-shell override)
   3. **Repo auto-discovery**: walks up from `$CWD` looking for
      `server/app/build/outputs/apk/androidTest/debug/*-androidTest.apk`
      + the sibling main APK. Lets you `cd ShadowDroid && shadowdroid connect`
      and have your in-progress build picked up automatically.
   4. **Dev drop-in**: `~/.shadowdroid/apks/local/{main,test}.apk` (if you
      built the APK in one project and want to use it from anywhere).
   5. **Versioned cache**: `~/.shadowdroid/apks/<EXPECTED_APK_VERSION>/{main,test}.apk`
      (a previous download).
   6. **GitHub release** fallback: `https://github.com/andriyo/ShadowDroid/releases/download/v<version>/{shadowdroid-server-main.apk,shadowdroid-server-test.apk}`.
   Sources 1–4 are "dev mode" and skip the version check (we print
   "using local APK at <path> (dev mode)" so it's never silent). Sources 5–6
   enforce version match against the CLI's baked-in `EXPECTED_APK_VERSION`.
4. CLI checks the device's installed APK signature/hash. If absent or differs,
   `adb install -r` both APKs (main first, then test/instrumentation).
5. CLI runs `adb forward tcp:7912 tcp:7912`.
6. CLI starts the instrumentation: `adb shell am instrument -w -e debug false io.github.andriyo.shadowdroid.test/.ShadowDroidRunner`. This is fire-and-forget; the instrumentation backgrounds itself and keeps the HTTP server alive until killed.
7. CLI polls `GET /v1/state` until 200 OK (typically <500ms).
8. CLI writes `~/.shadowdroid/devices/<serial>.json` with `{port, apk_sha, apk_source, last_connected}`.

> See [development.md](development.md) for the day-to-day "I'm editing the Kotlin server" workflow that exercises the dev-mode sources.

### Subsequent runs
1. CLI reads cached state.
2. Probes `GET /v1/state` first. If it answers in <50ms with the right version, proceed.
3. If not, fall back to steps 3-6 above. This handles "device rebooted", "user force-stopped the APK", "APK was updated", etc.

### Shutdown
- The server runs as long as the instrumentation process lives. Killed by `am force-stop io.github.andriyo.shadowdroid`, device reboot, or `pm clear`.
- CLI never explicitly tears it down — that lets repeated invocations stay fast.
- A `shadowdroid disconnect` command stops the instrumentation and removes the forward.

## 5. The Android Instrumentation lifecycle problem

Instrumentations die when their hosting process dies. Android's process killer is aggressive — under memory pressure, when the screen is off for a long time, when the app is force-stopped, etc.

**Solution 1 (initial):** detect on the laptop side. Every CLI invocation probes `/v1/state`. If it doesn't respond, restart the instrumentation. Adds ~500ms-1s of latency on a cold call. Acceptable for interactive use.

**Solution 2 (later, if needed):** ship a small persistent on-device Service alongside the Instrumentation. The Service receives a `RESTART_INSTRUMENTATION` broadcast and re-launches via `am instrument`. The CLI broadcasts on reconnect. This is what openatx-agent does (its watchdog).

We'll start with Solution 1 and only build Solution 2 if real use surfaces pain.

## 6. Versioning & compatibility

- Single semver per repo. CLI 0.3.x ships APK 0.3.x. They MUST match on minor version.
- CLI stamps the expected APK version in its binary at build time.
- On every `/v1/state` probe, CLI compares the returned `apk_version` to its expected. Mismatch → reinstall.
- HTTP API is versioned in the URL (`/v1/...`). Breaking changes increment to `/v2/`; CLI supports both during a deprecation window.

## 7. Comparison with openatx / uiautomator2

|                              | uiautomator2 (openatx)                          | ShadowDroid                                              |
| ---------------------------- | ----------------------------------------------- | -------------------------------------------------------- |
| Laptop runtime               | Python + uiautomator2 + adbutils + requests     | Single Rust binary                                       |
| On-device service            | Go binary (atx-agent) + Java APK (uiautomator)  | Kotlin Instrumentation APK only                          |
| UI Automator version         | Bundled, older                                  | AndroidX 2.3.0+ (first-class Compose)                    |
| Wire format                  | JSON-RPC 2.0                                    | REST + JSON (cleaner, easier to curl)                    |
| Crash detection              | Not built in                                    | Built in (logcat tail + structured events)               |
| Watchers                     | On-device (limited)                             | Laptop-side (any movi command, max_fires, easy to evolve)|
| Streaming event model        | No                                              | Yes — JSON-lines, debounced, hash-diffed                  |
| Distribution                 | `pip install`                                   | `cargo install` or single binary download                |
| Initial dev effort to match  | Already shipping                                | ~3-4 weeks (this repo)                                   |
| Long-term maintenance        | Inherits upstream                               | Owned (good and bad)                                     |

## 8. What the agent's view looks like (unchanged from `movi`)

```json
{"type":"ready","device":"emulator-5554","viewport":{"w":1080,"h":2424},"server_version":"0.1.1","ts":...}
{"type":"screen_compact","ts":...,"screen_hash":"...","elements":[{"id":7,"rid":"main_tab_profile","tap":[980,2256],"clickable":true}]}
{"type":"screen","ts":...,"package":"...","activity":"...","screen_hash":"...","element_count":N,"elements":[...]}
{"type":"action","cmd":"tap","id":5,"x":540,"y":1800}
{"type":"action","cmd":"tap_and_wait","matched":true,"timeout":false,"screen_hash":"...","hash_changed":true}
{"type":"crash","kind":"java","exception":"...","stack":[...],"context":[...],"device_info":{...}}
{"type":"watcher_fired","name":"...","matched":{...},"ts":...}
{"type":"error","stage":"dump|parse|dispatch|watcher","msg":"..."}
```

The default `watch` screen payload is `screen_compact` because agents usually need ids, tap points, labels, resource ids, classes, and true state flags. Use `watch --screen-format full` when the agent needs the full UIAutomator-style dump with bounds and every boolean flag.

## 9. Resolved architectural decisions

1. **HTTP server: Ktor 3.x** (`io.ktor:ktor-server-*:3.2.x`, CIO engine).
   NanoHTTPD was the lighter option (~50KB vs Ktor's ~3MB), but it's effectively
   abandoned — 2.3.1 is from 2020 with no meaningful releases since. Ktor is
   actively maintained by JetBrains, coroutines-native, and gives us a clean
   typed routing DSL + first-class `kotlinx.serialization` integration so route
   handlers stay short and the wire types live in shared data classes. The
   3MB overhead is irrelevant for an instrumentation APK that ships once per
   device. Engine choice: CIO (pure-Kotlin, no Netty) — keeps deps to one
   coroutine-aware HTTP layer.
2. **No gzip on the wire.** Dumps are 50-200KB but loopback is essentially free;
   the CPU cost of gzip+decode is larger than the IO savings, and uncompressed
   bytes make `curl` debugging trivial.
3. **No WebSocket / no server-push.** The CLI does its own dump-then-emit loop with
   debounce + hash-diff. Single-request/single-response semantics keep the server
   tiny and the wire format `curl`-able. Revisit only if real workloads show the
   poll cost dominates (it doesn't today — dumps are ~25ms).
4. **No package allowlist on the server.** Trust boundary is the device, not the
   HTTP API. `--app` filtering happens on the CLI side (used to scope emitted
   `screen` events to a target package); the server itself accepts any package the
   ADB user could already drive. Devs need to be able to drive system UI dialogs
   (settings, permission prompts) to write meaningful flows.

See [protocol.md](protocol.md) for the wire format and [delivery-plan.md](delivery-plan.md) for the phasing.
