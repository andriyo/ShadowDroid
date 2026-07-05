# ShadowDroid Test App

A deliberately small Android app with many surfaces that are useful for testing
ShadowDroid commands end to end.

## Build

From this directory:

```bash
./gradlew :app:assembleDebug
```

Or from the repository root:

```bash
./server/gradlew -p samples/shadowdroid-test-app :app:assembleDebug
```

The debug APK is written to:

```text
app/build/outputs/apk/debug/app-debug.apk
```

## Install And Launch

```bash
shadowdroid app reinstall app/build/outputs/apk/debug/app-debug.apk --grant-all --wait-front
```

The package is:

```text
io.github.andriyo.shadowdroid.sample
```

## What It Exercises

| Area | App surface | Useful commands |
| --- | --- | --- |
| App launch | `MainActivity` and `AltLauncherActivity` both declare launcher intents | `app start`, `app start --activity .MainActivity`, `app current` |
| UI selectors | Stable resource IDs, content descriptions, duplicate text, disabled controls, scrollable content | `ui dump`, `ui find`, `ui tap`, `ui scroll-to`, `ui audit`, `ui gen` |
| Text input | Name, URL, and request-body fields | `ui text`, `ui focus`, `ui key` |
| Popups | Alert dialog and popup window | `watch`, `ui tap --text`, watcher rules |
| Toasts | Toast button | `ui toast`, `watch` |
| Permissions | Camera and notification permission requests | `perm list`, `perm grant`, `perm revoke`, `perm reset` |
| Notifications | Local notification with app pending intent | `device notifications`, `ui dump` |
| App lifecycle | Detail activity, deep link activity, explicit finish buttons | `app start`, `app wait`, `device open-url` |
| Files | Writes deterministic files under app data and cache | `files ls`, `files pull`, `app clear` |
| Clipboard | Writes a sample clip | `device clipboard` |
| Logs/crashes/ANR | Log spam, deliberate crash, deliberate main-thread block | `watch`, `collect`, `debug run-until-crash`, `doctor` |
| Network MITM | HTTP GET, HTTPS GET, JSON POST, GraphQL-shaped POST, error status, slow response, large body | `net check`, `net trust`, `net start`, `net log`, `net show`, `net export fixtures`, `net rule`, `net override` |
| WebView | Loads the configured URL into a platform WebView | `net log`, `ui dump`, `watch` |
| Coroutine dumps | `CoroutinesActivity` starts a misbehaving coroutine zoo: a leaked heartbeat, parked channel workers, a clogged no-buffer `SharedFlow` (slow collector + suspended emitter), plus a button that grows the pool | `aar coroutines`, `aar agent` |

The coroutine zoo needs the agent AAR with probes activation
(`shadowdroid aar install --coroutine-probes`, already wired in this sample);
open the "Open coroutine workload" screen (or `am start …/.CoroutinesActivity`)
and every coroutine shows up named in `shadowdroid aar coroutines`.

The debug build uses a Network Security Config with `debug-overrides` that trusts
user-installed CAs, so it is suitable for `shadowdroid net trust --auto` on
rootable emulator images and `shadowdroid net trust --ui` on locked devices.

