# ShadowDroid Test App

A deliberately small Android app with many surfaces that are useful for testing
ShadowDroid commands end to end.

## Build

From this directory:

```bash
./gradlew :chat-server:test :app:assembleDebug
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

## Run The WebSocket Chat Server

The `chat-server` module is a real Ktor server with a shared chat room, a
health endpoint, server-initiated greetings, ping/pong traffic, and
`permessage-deflate`. It starts cleartext and self-signed TLS connectors:

```bash
./gradlew :chat-server:run
curl http://shadowdroid.localhost:18080/health
```

The Android fixture uses these endpoints:

```text
ws://shadowdroid.localhost:18080/chat?name=android
wss://shadowdroid.localhost:18443/chat?name=android
```

`shadowdroid.localhost` deliberately has a dot so Android's proxy selector does
not treat it as an unproxied loopback URL. The host-side proxy resolves the
special-use `.localhost` name back to the local Ktor process.

Start ShadowDroid's proxy with that host in scope, then open the exported
`WebSocketChatActivity` or tap **Open WebSocket chat** in the Network section:

```bash
shadowdroid net start --host shadowdroid.localhost
shadowdroid app start io.github.andriyo.shadowdroid.sample \
  --activity .WebSocketChatActivity
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
| WebSocket proxy | Native chat UI backed by a local Ktor WS/WSS room; client and server messages, ping/pong, compression, and normal close | `net log --protocol websocket`, `net ws`, `net show w1.1`, `watch` |
| WebView | Loads the configured URL into a platform WebView | `net log`, `ui dump`, `watch` |
| Coroutine dumps | `CoroutinesActivity` starts a misbehaving coroutine zoo: a leaked heartbeat, parked channel workers, a clogged no-buffer `SharedFlow` (slow collector + suspended emitter), plus a button that grows the pool | `aar coroutines`, `aar agent` |

The coroutine zoo needs the agent AAR with probes activation
(`shadowdroid aar install --coroutine-probes`, already wired in this sample);
open the "Open coroutine workload" screen (or `am start …/.CoroutinesActivity`)
and every coroutine shows up named in `shadowdroid aar coroutines`.

The debug build uses a Network Security Config with `debug-overrides` that trusts
user-installed CAs, so it is suitable for `shadowdroid net trust --auto` on
rootable emulator images and `shadowdroid net trust --push` followed by manual
Settings installation on locked devices (a screen-lock credential is required).
It also handles ShadowDroid's package-scoped HTTPS canary and requests the exact
unique URL, making it a real target for `net start` followed by
`net check --probe io.github.andriyo.shadowdroid.sample`.
