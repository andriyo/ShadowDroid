# ShadowDroid — Checkpoint (M1 ✅, M2 ✅, M3 ✅, M4 implemented)

Last update: 2026-05-19 M3 crash proof + M4 implementation pass.

## TL;DR

- **M1 ships clean.** `shadowdroid connect/disconnect/devices` work end-to-end against the live emulator. Cold connect: 1.5s; warm: 130ms; steady-state `/v1/state`: ~18ms.
- **M2 is feature-complete on both sides** — server endpoints (`/v1/screen`, `/v1/tap`, `/v1/swipe`, `/v1/screenshot.png`, `/v1/shell`, all the others) + CLI dispatch for every subcommand. The full Livd demo (launch → screen dump → tap profile tab by id → screenshot → shell) ran end-to-end and worked.
- **M3 is now crash-proven against Livd.** `shadowdroid watch --app com.livd` captured Livd's built-in Crashlytics debug crash as a structured Java crash: `java.lang.RuntimeException`, message `Crashlytics DEV fatal test`, stack rooted at `CrashlyticsDebugScreen.kt:91`, plus logcat context and device info.
- **M4 is implemented.** Server routes now cover `find`, `find_tap`, limited xpath, toast buffering, and file push/pull. CLI verbs now cover `tap_text`, `tap_rid`, `tap_desc`, `xpath`, `xpath_tap`, `wait_for`, `toast`, `push`, and `pull`. `watch --watcher-file` and runtime watcher commands (`add_watcher`, `remove_watcher`, `list_watchers`, `clear_watchers`) are wired through the same dispatch path. Live validation covered selectors, xpath, file round-trip, and watcher-file firing; a real app-toast source is still the remaining toast proof.
- **One known blocker for repeated dev cycles**: UiAutomation is single-owner. The scary Android 16 "already registered" failure was reproduced, but the live cause in follow-up was an old host-side `movi`/openatx watcher respawning `/data/local/tmp/u2.jar` (`com.wetest.uia2.Main`) after ShadowDroid killed it. After stopping that watcher and killing the device process, `shadowdroid connect` worked again on the same Android 16 emulator without `-wipe-data`.

## What's in the repo

```
ShadowDroid/
├── README.md                                project pitch + repo map
├── .gitignore
├── docs/
│   ├── architecture.md                      big-picture design (current/final)
│   ├── protocol.md                          v1 HTTP API spec (source of truth)
│   ├── delivery-plan.md                     M1-M5 milestones (M1+M2 done)
│   ├── development.md                       dev workflow + APK precedence chain
│   └── checkpoint.md                        this file
├── cli/                                     Rust crate `shadowdroid`
│   ├── Cargo.toml                           clap 4.6 + tokio 1.52 + reqwest 0.13 + adb_client 3.2 + ...
│   └── src/
│       ├── main.rs                          tokio entry, tracing → stderr
│       ├── cli.rs                           clap defs + every subcommand dispatch
│       ├── proto.rs                         wire types
│       ├── events.rs                        stdout JSON event shapes
│       ├── dump.rs                          M3+ XML fallback parser stub
│       └── device/
│           ├── mod.rs
│           ├── adb.rs                       adb_client wrapper (list/shell/install/forward/am_instrument)
│           ├── client.rs                    reqwest HTTP client for /v1/* endpoints
│           ├── installer.rs                 APK resolver (6-source precedence chain) + ensure_ready
│           └── actions.rs                   M3+ stub
│       └── watch/                           M3 watch loop/crash/stdin; M4 watcher engine
└── server/                                  Gradle 9.4.1 + AGP 9.2.1 + Kotlin 2.2.0
    ├── settings.gradle.kts
    ├── build.gradle.kts                     plugin versions
    ├── gradle.properties
    ├── gradle/wrapper/                      wrapper at Gradle 9.4.1
    ├── gradlew
    └── app/
        ├── build.gradle.kts                 UI Automator 2.3.0, Ktor 3.2.0, JUnit 4.13.2
        ├── src/main/AndroidManifest.xml     INTERNET perm, cleartext, hasCode=true
        └── src/androidTest/java/io/github/andriyo/shadowdroid/
            ├── ShadowDroidServerTest.kt     @RunWith(AndroidJUnit4) — openatx's daemon pattern
            ├── HttpServer.kt                Ktor 3 / CIO, /v1 route group
            ├── BuildInfo.kt                 version constants
            ├── proto/Proto.kt               wire types (ServerState, Element, ScreenResponse, …)
            ├── dump/TreeWalker.kt           AccessibilityNodeInfo → flat element list
            └── routes/
                ├── Helpers.kt               currentFocusedActivity, pidForPackage
                ├── StateRoutes.kt           GET /v1/state
                ├── ScreenRoutes.kt          GET /v1/screen, /v1/screen?format=xml, /v1/screenshot.png
                ├── GestureRoutes.kt         POST tap/double_tap/long_tap/swipe/drag/swipe_ext
                ├── KeyTextRoutes.kt         POST key/text
                ├── AppRoutes.kt             app/start, stop, clear, wait, info, current
                ├── SystemRoutes.kt          screen_on/off, unlock, orientation, clipboard, shell, …
                ├── SelectorRoutes.kt        find/find_tap/limited xpath
                ├── ToastRoutes.kt           accessibility-event toast ring buffer
                └── FileRoutes.kt            push/pull/list under app storage or shared /sdcard paths
```

## How to pick this up

```bash
cd /Users/andrii/Work/ShadowDroid
git log --oneline                             # check the M1+M2 commit
git status                                    # should be clean

# Build the world:
(cd server && ./gradlew :app:assembleDebug :app:assembleDebugAndroidTest)
(cd cli && cargo build)

# Smoke test (after `emulator -wipe-data -avd Pixel_9` if the UA slot is stuck):
cli/target/debug/shadowdroid devices
cli/target/debug/shadowdroid connect          # ~1.5s cold, ~130ms warm
cli/target/debug/shadowdroid screen | jq '{element_count, package: .current_app.package}'
cli/target/debug/shadowdroid screenshot /tmp/x.png
cli/target/debug/shadowdroid shell "id && getprop ro.product.model"
cli/target/debug/shadowdroid disconnect
```

If `connect` fails with "server did not become ready", check `adb shell cat /sdcard/shadowdroid-instr.log` — if it shows `UiAutomationService ... already registered!`, first look for a competing owner:

```bash
adb shell "ps -A -o USER,PID,PPID,NAME,ARGS | grep -E 'app_process|uiautomator|shadowdroid|wetest|atx'"
```

If you see `com.wetest.uia2.Main`, stop the host-side `uiautomator2`/`movi` watcher that is respawning it, then kill the device process and retry. If there is no visible owner and the slot still survives cleanup, use the heavier AVD reset:

```bash
# Stop emulator, wipe data, restart fresh:
adb emu kill
~/Library/Android/sdk/emulator/emulator -avd Pixel_9 -no-snapshot -wipe-data -no-boot-anim &
# wait for boot, then reinstall Livd APK if you need it:
adb install -r -t ~/Work/Livd/androidApp/build/outputs/apk/debug/androidApp-debug.apk
```

## What works end-to-end (proven this session)

### M1 (`devices` / `connect` / `disconnect`)
- `shadowdroid devices` → emulator-5554
- `shadowdroid connect` cold → ~1.5s, installs both APKs, starts JUnit-based server, polls /v1/state
- `shadowdroid connect` warm → ~130ms (just probes /v1/state)
- `shadowdroid disconnect` → cleanly force-stops + removes port forward
- APK source precedence chain: `--apk` flag works; repo auto-discovery works; cached + GH release stubbed for M5

### M2 (everything one-shot from the legacy `movi` CLI)
- `/v1/state` → version + viewport + current_app (✓)
- `/v1/screen` → flat element list with stable IDs, screen_hash, viewport, current_app (✓ — got 16 elements from the launcher)
- `/v1/screenshot.png` → ~1.2MB PNG 1080x2424 (✓)
- `/v1/tap`, `/v1/double_tap`, `/v1/long_tap`, `/v1/swipe`, `/v1/drag`, `/v1/swipe_ext` (✓ — all return `{"ok":true}`)
- `/v1/key` (back/home/enter/wakeup/…), `/v1/text` (✓ — uses focused EditText)
- `/v1/app/start` (Intent first, falls back to `monkey -p PKG -c LAUNCHER 1`), `/v1/app/stop`, `/v1/app/clear`, `/v1/app/wait`, `/v1/app/info` (PackageManager → dumpsys fallback), `/v1/app/current`
- `/v1/screen/on`, `/v1/screen/off`, `/v1/wakeup`, `/v1/unlock`, `/v1/orientation` (get/set), `/v1/clipboard` (get/set), `/v1/notifications/open`, `/v1/quick_settings/open`, `/v1/url/open`, `/v1/shell` (returns stdout, no exit_code — UiDevice.executeShellCommand doesn't expose it)
- CLI subcommand for every one of the above; element-id-based tap (`tap N` does fresh dump → look up id → tap center) works

### Verified Livd demo (earlier this session)
```
$ shadowdroid screen | jq '{element_count, package: .current_app.package}'
{"element_count": 16, "package": "com.google.android.apps.nexuslauncher"}

$ shadowdroid screen | jq -c '.elements[] | select(.clickable)' | head
{"id":4,"text":"Gmail","rid":null,"tap":[416,1633],...}
{"id":11,"text":"LIVD","rid":null,"tap":[910,1994],...}

$ shadowdroid screenshot /tmp/x.png  → {bytes: 1242027}
$ shadowdroid shell "getprop ro.product.model"  → "sdk_gphone64_arm64"
```

## Validated tech stack

| Layer | Version | Notes |
|---|---|---|
| Rust | 1.95 (rustc 2026-04-14) | min via `rust-version = "1.82"` |
| Gradle | **8.14 → 9.4.1** (bumped during M2) | AGP 9.2.1 requires 9.4.1+ |
| AGP | **9.2.1** | latest stable. AGP 9.x built-in Kotlin pattern applied |
| Kotlin | **2.2.0** (via built-in Kotlin from AGP 9) | `org.jetbrains.kotlin.plugin.serialization` 2.2.0 |
| compileSdk / targetSdk | **37 / 36** | Android 17 SDK platform / Android 16 GA |
| Java | 21 source/target via `kotlin.compilerOptions` | |
| Ktor | 3.2.0 (CIO engine) | |
| UI Automator | **2.3.0** (downgraded from 2.4.0-beta02) | beta02 raced with AndroidJUnitRunner.onStart |
| AndroidX test runner / rules / ext-junit | 1.7.0 / 1.7.0 / 1.3.0 | |
| kotlinx-coroutines-android | 1.10.2 | |
| kotlinx-serialization-json | 1.9.0 | |
| adb_client | 3.2 | pure-Rust ADB protocol — no shell-out to `adb` binary |
| reqwest | 0.13 (features `["json","rustls","stream"]`) | |
| clap | 4.6 | derive-based CLI |
| tokio | 1.52 | full features |

## Major decisions made (locked in)

1. **HTTP server: Ktor 3.x** (CIO engine). NanoHTTPD considered but abandoned (effectively dead since 2020).
2. **No gzip on the wire.** Loopback makes it a CPU loss.
3. **No WebSocket / server-push.** CLI does its own dump-then-emit loop, single-request semantics keep `curl` debugging trivial.
4. **No package-allowlist on the server.** Trust boundary is the device.
5. **AGP 9 built-in Kotlin** (don't apply `kotlin.android` separately).
6. **openatx's JUnit `@Test` pattern** for the Instrumentation daemon (not a custom AndroidJUnitRunner subclass — that races UiAutomation init).
7. **Standard AndroidJUnitRunner as testInstrumentationRunner**; we run a single long-lived `@Test fun runServerForever()` to keep the process alive.
8. **6-source APK precedence chain**: --apk flag → SHADOWDROID_APK env → repo auto-discovery → ~/.shadowdroid/apks/local → versioned cache → GitHub release.

## Open items

| # | Item | Status |
|---|---|---|
| #27 | Investigate UiAutomation slot leak on Android 16 emulator | **INVESTIGATED** — current evidence points to slot contention from a respawned openatx/u2 process, not a proven Android 16 leak. Keep the `-wipe-data` path as last resort only. |
| Future | M3: streaming `watch` + crash detection | **SHIPPED** — Livd built-in debug crash emitted a structured `crash` event with `java.lang.RuntimeException`, `Crashlytics DEV fatal test`, stack, context, and device_info. |
| Future | M4: SelectorRoutes (find/find_tap/xpath) + ToastRoutes + FileRoutes + watchers | **IMPLEMENTED; mostly live-validated** — selectors/xpath/file round-trip/watcher-file/runtime watcher list passed on the emulator. Toast route compiles and CLI is wired; still needs a known real app-toast source for visible proof. |
| Future | M5: GitHub Actions APK build/sign, `cargo install shadowdroid`, GH releases auto-download | Not started. |

## Three suggested next moves

1. **Finish the last M4 toast proof** (~20 min). Use or build a tiny known-toast source app, then verify `shadowdroid toast --wait-ms ...` captures it from the accessibility-event ring buffer.

2. **Polish M4 docs/examples** (~30 min). Add example watcher files for Android permission dialogs and a short selector cookbook for Compose testTags.

3. **Start M5 distribution** (~3 days). Build release APKs and CLI artifacts in GitHub Actions, embed/download the APK version from GitHub releases, then make `cargo install shadowdroid` work for a clean machine.

My pick: **toast proof → docs/examples → M5**. The tool is now genuinely agent-usable; the next big value jump is making it easy to install outside this repo.
