# ShadowDroid — Phased Delivery Plan

Five milestones. Each one ships a runnable artifact you can use, even if it doesn't yet match `movi` feature-for-feature. The principle: **end-to-end loop before depth in any one layer**.

| Phase | Goal                                          | Done when…                                                                  | Estimate |
| ----- | --------------------------------------------- | --------------------------------------------------------------------------- | -------- |
| M1    | Hello-world round trip                        | `shadowdroid connect` installs the APK, starts it, gets `/v1/state` back    | 1 week   |
| M2    | Inspection + core gestures                    | All the read/tap/swipe/text/launch verbs from the protocol implemented      | 1 week   |
| M3    | Watch loop + crash detection                  | `shadowdroid watch` emits the full JSON event stream incl. crashes          | 5 days   |
| M4    | Watchers + Toast + selectors + xpath          | Feature parity with `movi`                                                  | 4 days   |
| M5    | Distribution                                  | `cargo install shadowdroid` works; APK lives in GitHub releases             | 3 days   |

Total: **~3-4 weeks** of focused work. Each phase below lists what's in, what's out, and the smallest meaningful demo that proves it.

---

## M1 — Hello-world round trip (1 week)

**In:**
- Server: bare Gradle project that builds, `ShadowDroidRunner` that brings up a Ktor (CIO engine) server on 7912 and exposes **only** `GET /v1/state` returning version + viewport.
- CLI: `shadowdroid devices`, `shadowdroid connect`, `shadowdroid disconnect`. The `connect` command does the full lifecycle: ADB enumeration → resolve APK (see precedence chain in architecture.md §4) → install both APKs if needed → `adb forward` → `am instrument` → poll `/v1/state` until ready.
- **Dev-mode APK sources** (the first four of the six in the precedence chain) are wired in from M1 — that's how we'll iterate on the Kotlin server. GitHub-release download is a *stub* that errors with "not yet implemented; use --apk in dev"; the real download lands in M5.

**Out:** any UI operation. This phase is entirely about the *pipe* being usable.

**Demo (dev workflow):**
```bash
# In the repo:
cd ShadowDroid/server && ./gradlew :app:assembleDebug :app:assembleDebugAndroidTest

# Auto-discovery: from inside ShadowDroid/, the CLI finds the freshly-built APKs
cd .. && cargo run -p shadowdroid -- connect
# → "using local APK at server/app/build/outputs/.../app-debug-androidTest.apk (dev mode)"
# → {"type":"connected","server_version":"0.1.1",...}

# Explicit path (from anywhere):
shadowdroid --apk ~/Downloads/shadowdroid-test.apk connect

# Or env var, sticky for the shell:
export SHADOWDROID_APK=~/Downloads/shadowdroid-test.apk
shadowdroid connect

# Reconnect is fast — no reinstall when on-device hash matches
shadowdroid connect             # ~50ms

# Force reinstall (e.g., after rebuilding):
adb shell pm uninstall io.github.andriyo.shadowdroid
shadowdroid connect             # → reinstalls, ~3s
```

**Validates:** the lifecycle manager, the APK build, the Instrumentation-as-daemon pattern, the Rust↔Kotlin HTTP plumbing.

---

## M2 — Inspection + core gestures (1 week)

**In:**
- Server: implement `ScreenRoutes`, `GestureRoutes`, `KeyTextRoutes`, `AppRoutes`, `SystemRoutes` (everything except Toast/xpath/find — those are M4).
- CLI: every one-shot subcommand from the legacy `movi` CLI — `screen`, `tap`, `swipe`, `text`, `launch`, etc.
- Element model: server flattens the UI Automator XML into our element JSON shape *on device* (cheaper than shipping XML over loopback).

**Out:** the watch loop, crash detection, watchers, xpath, find.

**Demo:**
```bash
shadowdroid launch com.livd
shadowdroid screen | jq '.elements | length'    # → 17
shadowdroid tap_text Profile                    # (uses /v1/find_tap)
shadowdroid screenshot /tmp/profile.png
shadowdroid shell "getprop ro.product.model"
```

**Validates:** UI Automator coverage, error envelopes, the round-trip latency we promised (target: <100ms for a tap+screen cycle).

---

## M3 — Watch loop + crash detection (5 days)

**In:**
- CLI: `shadowdroid watch [--app PKG] [--no-stdin] [--no-crash-detect]` — the streaming subcommand.
- CLI: `watch::loop` — wake on logcat events / safety-net poll / stdin commands, debounce, hash-diff, emit.
- CLI: `watch::logcat` — port `movi/crash.py` regex parser line-for-line. Same structured `crash` event shape (Java/native/ANR, stack, context, device_info).
- CLI: `watch::stdin` — port `parse_command` shorthand + JSON forms.

**Out:** watchers, toasts, xpath.

**Demo:**
```bash
shadowdroid watch --app com.livd | jq -c .
# In another shell:
adb shell am crash com.livd
# In the watch stream:
# {"type":"crash","kind":"java","exception":"android.app.RemoteServiceException$CrashedByAdbException",
#  "stack":[...], "context":[...], "device_info":{"android_release":"16",...}}
```

**Validates:** the streaming event model, the agent-facing contract, the crash parser.

---

## M4 — Watchers + Toast + selectors + xpath (4 days)

**In:**
- Server: `ToastRoutes` (accessibility-event listener, ring buffer). `SelectorRoutes` (find / find_tap / xpath). `FileRoutes` (push/pull within accessible storage).
- CLI: `watch::watcher` rule engine. `--watcher-file` flag on `watch`. Add/remove/list at runtime via JSON commands. Watcher fires dispatch through the normal action path.
- CLI: `tap_text`, `tap_rid`, `tap_desc`, `tap_and_wait`, `tap_text_and_wait`, `tap_rid_and_wait`, `tap_desc_and_wait`, `xpath`, `xpath_tap`, `toast`, `wait_for`, `swipe_ext`, `open_url`, `push`, `pull` — all the verbs we added in `movi` after M2, plus fast agent-loop helpers.

**Out:** the `MoviSession` Python adapter — that's not a ShadowDroid concern. Agents drive the CLI directly via stdin/stdout, or via the JSON-line stream from `shadowdroid watch`. (If we miss the in-process adapter, an `agent` crate could add it later, but `movi`'s agent layer was Python-only and doesn't need to carry forward.)

**Demo:**
```bash
shadowdroid watch --app com.livd --permission-dialogs allow | jq -c .

# Reset permissions in another shell:
adb shell pm reset-permissions com.livd
# Watch stream emits:
# {"type":"watcher_fired","name":"builtin_permission_allow","matched":{...},"ts":...}
# {"type":"action","cmd":"tap","x":540,"y":1331}
# {"type":"screen", ...}
```

Use `--permission-dialogs deny` for negative-path tests. Custom `--watcher-file`
rules remain the escape hatch for app-specific dialogs, but Android
PermissionController buttons are covered by built-in resource-id rules so agents
do not need to shell out to ADB for the common permission flow.

Fast agent loop:
```bash
shadowdroid watch --app com.livd --permission-dialogs deny --debounce-ms 0
```

`screen_compact` is the default screen event. Agents should read the latest
compact event, use cached element ids or `tap` coordinates from that event, and
send `tap_and_wait` when the next decision depends on a screen change. JSON
commands can include `screen_hash` to reject stale cached ids. Use
`--screen-format full` only when an agent needs a full screen dump.

**Validates:** end-to-end feature parity with `movi`. At this point the legacy Python tool can be retired.

---

## M5 — Distribution (3 days)

**In:**
- GitHub Actions: build APKs on tag and upload release assets. Build CLI binaries for macOS arm64/x86_64, Linux x86_64/arm64, and Windows x86_64.
- CLI: embed the expected APK version and optional release SHA-256 values at build time. On first run, fetch `SHA256SUMS`, download the matching APK pair, verify, cache, and install.
- Copy-paste installers: `shadowdroid-installer.sh` for macOS/Linux and `shadowdroid-installer.ps1` for Windows.
- Publish to crates.io: `cargo install shadowdroid` should work once the matching GitHub Release exists.
- Homebrew tap: optional, can come later.
- Release notes + a short `docs/getting-started.md`.

**Out:** F-Droid publication, Linux distro packaging (.deb/.rpm), Windows MSI. All deferred.

**Demo:**
```bash
cargo install shadowdroid
shadowdroid connect              # downloads APK from GitHub on first run
```

**Validates:** the project is shippable to a stranger with no Android SDK installed.

---

## Risk register

| Risk                                                                  | Severity | Mitigation                                                                                                                |
| --------------------------------------------------------------------- | -------- | ------------------------------------------------------------------------------------------------------------------------- |
| AndroidJUnitRunner subclassing pattern breaks in a future AndroidX rev| Medium   | The pattern is in wide use (openatx, automate, etc.). If it breaks, fall back to a true `Service` + Shizuku/root path.    |
| `adb_client` crate doesn't support some quirk of `am instrument`      | Low      | Add a thin shell-out fallback for `am instrument` only; everything else stays pure-Rust.                                  |
| First-time APK download from GitHub releases is rate-limited          | Low      | Cache aggressively under `~/.shadowdroid/`. Ship APK alongside CLI tarball as a fallback.                                 |
| Instrumentation gets killed when device sleeps                        | Medium   | M5+ optional: ship a small persistent on-device Service that re-`am instrument`s on a broadcast (the openatx watchdog pattern). |
| Ktor APK weight / Android-specific quirks                             | Low      | Route handlers are kept thin; the routing DSL + serialization plugin layer is interchangeable with NanoHTTPD or hand-rolled `ServerSocket` if needed. ~1-day swap because handlers themselves are JSON-in/JSON-out closures.       |

## What's deliberately *not* in scope

- **iOS support.** ShadowDroid is Android-only by design. iOS would need a totally different on-device stack (XCTest, idb).
- **Webview-specific selectors.** We rely on whatever the WebView exposes through the accessibility tree. No `chrome devtools` integration.
- **Recording / replay.** The streaming JSON already gives you what you need to script replays externally. We won't bake a recorder in.
- **Test framework.** No assertions, fixtures, reporters. Compose with `pytest` or `cargo test` at the use-site.
