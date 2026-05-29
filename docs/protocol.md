# ShadowDroid — HTTP Protocol (v1)

The on-device server (Kotlin Instrumentation APK) exposes this HTTP API on **`localhost:7912`** after `adb forward tcp:7912 tcp:7912`. The laptop CLI is the only intended client.

This document is the contract — any change requires bumping `/v1/` → `/v2/` and shipping both server-side support for the deprecation window.

## Conventions

- **Base URL**: `http://127.0.0.1:7912/v1`
- **Encoding**: UTF-8 JSON request and response bodies. Content type `application/json`. PNG and raw XML use `image/png` / `application/xml`.
- **Idempotency**: GETs are pure reads. POSTs perform an action and may mutate device state.
- **Errors**: non-2xx returns `{"error": {"code": "string", "message": "human readable", "detail": {...optional...}}}`.
- **Coordinates**: pixels, integers, origin top-left, range `[0, viewport.w)` × `[0, viewport.h)`.
- **Durations**: milliseconds, integers. (No floats on the wire — units in field names.)
- **Bounds**: `[x1, y1, x2, y2]` integer arrays, inclusive of x1/y1 and exclusive of x2/y2 (UI Automator convention).

## 1. State & metadata

### `GET /v1/state`

Cheap probe used by the CLI to verify the server is alive and version-compatible. Should return in under 30ms.

**Response**
```json
{
  "server_version": "0.1.1",
  "api_version": "1",
  "ui_automator_version": "2.3.0",
  "android_sdk": 36,
  "android_release": "16",
  "viewport": {"w": 1080, "h": 2424},
  "current_app": {"package": "com.livd", "activity": "com.livdapp.client.MainActivity", "pid": 12345}
}
```

### `GET /v1/device`

One-shot detailed device info (model, manufacturer, build fingerprint, locale, density). Cached server-side.

## 2. Hierarchy / inspection

### `GET /v1/screen`

Default response is the **flat element list** (the agent-facing shape the CLI emits to stdout).

Query params:
- `format=elements` (default): flat JSON list, see below.
- `format=xml`: raw UI Automator XML dump.
- `compressed=true|false` (default `false`): pass-through to `UiDevice.dumpWindowHierarchy(compressed=...)`. Compressed elides non-significant nodes — faster but loses some text.

**Response (`format=elements`)**
```json
{
  "screen_hash": "1c2e022e3789d34a",
  "viewport": {"w": 1080, "h": 2424},
  "current_app": {"package": "...", "activity": "...", "pid": 12345},
  "element_count": 17,
  "elements": [
    {
      "id": 5,
      "text": "Sign in",
      "desc": null,
      "klass": "Button",
      "rid": "io.example.app:id/btn_signin",
      "bounds": [64, 600, 1016, 720],
      "tap": [540, 660],
      "clickable": true,
      "long_clickable": false,
      "scrollable": false,
      "checkable": false,
      "focusable": true,
      "enabled": true,
      "selected": false,
      "checked": false,
      "focused": false,
      "password": false,
      "input": false
    }
  ]
}
```

Element selection rule (server side): include a node if it is `clickable || long-clickable || scrollable || checkable || has non-empty text/contentDescription || is EditText`. Pure layout containers are dropped.

### `GET /v1/screenshot.png`

PNG screenshot of the current display. Binary `image/png` response. Query params:
- `quality=N` (`format=jpeg` only, default 90)
- `format=png|jpeg` (default `png`)
- `scale=0.5|1.0` (default `1.0`) — server-side downscale before encoding.

## 3. Gestures

All gesture endpoints return `{"ok": true}` on success or an error envelope on failure.

| Verb         | Endpoint                | Body                                                                                                     |
| ------------ | ----------------------- | -------------------------------------------------------------------------------------------------------- |
| Tap          | `POST /v1/tap`          | `{"x": int, "y": int}`                                                                                   |
| Double tap   | `POST /v1/double_tap`   | `{"x": int, "y": int}`                                                                                   |
| Long tap     | `POST /v1/long_tap`     | `{"x": int, "y": int, "duration_ms": int (default 600)}`                                                 |
| Swipe        | `POST /v1/swipe`        | `{"from": [int,int], "to": [int,int], "duration_ms": int (default 200)}`                                 |
| Drag         | `POST /v1/drag`         | `{"from": [int,int], "to": [int,int], "duration_ms": int (default 500)}`                                 |
| Swipe by dir | `POST /v1/swipe_ext`    | `{"direction": "up|down|left|right", "scale": float (default 0.9), "duration_ms": int (default 200)}`    |
| Pinch        | `POST /v1/pinch`        | `{"target_id": int, "direction": "in|out", "percent": int (1..100)}` — UiObject2.pinchIn/Out             |

## 4. Keys & text

### `POST /v1/key`

```json
{"name": "back|home|menu|enter|wakeup|power|volume_up|volume_down|...", "code": 4 /* alternative: raw keycode */}
```
Use `name` for the common cases; `code` for anything else. One or the other.

### `POST /v1/text`

```json
{"value": "any unicode text including \n and special chars", "clear": false}
```

The text is set via `UiObject2.setText(...)` on the focused element. If `clear: true`, calls `clear()` first.

## 5. Selectors (server-side find + act)

These run a UI Automator `By` query *on the live tree* — cheaper than dumping the whole hierarchy and re-finding on the laptop.

### `POST /v1/find`

```json
{"text": "...", "rid": "...", "desc": "...", "klass": "...", "xpath": "...", "all": false}
```
Returns one or many elements in the same shape as `/v1/screen` elements. Substring match on text/rid/desc by default; pass `"exact": true` for exact.

### `POST /v1/find_tap`

Same body as `/v1/find` but performs a tap on the first match. Returns `{"matched": {...element}, "x": int, "y": int}`.

### `POST /v1/xpath`

XPath against the dump. The on-device `By.xpath(...)` is more limited than W3C XPath — `//*[@text='...']`, `//*[@resource-id='...']`, `contains(@text,'...')` and similar all work; complex axes do not.

```json
{"query": "//*[@text='Sign in']", "tap": false}
```

## 6. App lifecycle

| Verb        | Endpoint                | Body                                                  |
| ----------- | ----------------------- | ----------------------------------------------------- |
| Launch      | `POST /v1/app/start`    | `{"package": "..."}`                                  |
| Force stop  | `POST /v1/app/stop`     | `{"package": "..."}`                                  |
| Clear data  | `POST /v1/app/clear`    | `{"package": "..."}`                                  |
| Wait launch | `POST /v1/app/wait`     | `{"package": "...", "timeout_ms": 20000, "front": false}` |
| Info        | `GET  /v1/app/info?package=...` | (none) → `{"version_name": "...", "version_code": int, "label": "..."}` |
| Current     | `GET  /v1/app/current`  | (none) → `{"package": "...", "activity": "...", "pid": int}` |

## 7. System & device

| Verb              | Endpoint                       | Body                                                       |
| ----------------- | ------------------------------ | ---------------------------------------------------------- |
| Screen on         | `POST /v1/screen/on`           | (empty)                                                    |
| Screen off        | `POST /v1/screen/off`          | (empty)                                                    |
| Wakeup            | `POST /v1/wakeup`              | (empty)                                                    |
| Unlock keyguard   | `POST /v1/unlock`              | (empty) — wakes + dismisses                                |
| Orientation get   | `GET  /v1/orientation`         | → `{"value": "natural|left|right|upsidedown"}`             |
| Orientation set   | `POST /v1/orientation`         | `{"value": "..."}`                                         |
| Open notif. shade | `POST /v1/notifications/open`  | (empty)                                                    |
| Open quick sett.  | `POST /v1/quick_settings/open` | (empty)                                                    |
| Open URL          | `POST /v1/url/open`            | `{"url": "https://..."}`                                   |
| Shell             | `POST /v1/shell`               | `{"cmd": "...", "timeout_ms": 30000}` → `{"stdout": "...", "stderr": "...", "exit_code": int}` |

## 8. Clipboard

```
GET  /v1/clipboard          → {"value": "..."}
POST /v1/clipboard          {"value": "..."}      # 400 on Android 13+ if permission denied
```

Note: writing the clipboard is broken on Android 13+ for any background process (`SecurityException`). The server returns a structured error and the CLI surfaces it without retrying.

## 9. Toast

Toasts surface as accessibility events; the server keeps a small ring buffer when monitoring is active.

```
POST /v1/toast/start    {"buffer_size": 50}            # default 50
POST /v1/toast/stop
GET  /v1/toast/recent?since_ts=<unix_ms>               # default since_ts=now-5s
   → {"toasts": [{"package": "...", "text": "...", "ts": 1234}, ...]}
```

The CLI's `shadowdroid toast --wait 5` does: `start` (once per session, idempotent) → `GET recent` polling loop.

## 10. Files

Both endpoints stream to/from the on-device `/sdcard/` area (or wherever the server's app data dir is, see error response).

| Verb  | Endpoint              | Body / response                                                                                          |
| ----- | --------------------- | -------------------------------------------------------------------------------------------------------- |
| Push  | `PUT  /v1/files{path}`| Raw bytes in the request body. Response: `{"path": "...", "bytes": int, "mode": int}`                    |
| Pull  | `GET  /v1/files{path}`| Raw bytes in the response body, `application/octet-stream`. 404 if missing.                              |
| List  | `GET  /v1/files{dir}?list=true` | `{"entries": [{"name": "...", "size": int, "is_dir": bool}]}`                                  |

For files outside the server's accessible storage, the CLI falls back to `adb push` / `adb pull`.

## 11. Errors

```json
{"error": {
  "code": "permission_denied|element_not_found|timeout|xpath_invalid|device_busy|...",
  "message": "Human readable explanation.",
  "detail": {"query": "...", "...optional context..."}
}}
```

HTTP status codes: 400 (bad request body), 404 (no such element / route / file), 408 (operation timed out), 422 (semantic error like xpath_invalid), 500 (unexpected). The CLI maps these to typed Rust errors.

## 12. What we deliberately don't expose

- **Direct accessibility-event subscription** — the laptop CLI already polls + diffs efficiently; pushing every accessibility event would flood the wire.
- **Long-lived dump streaming** (WebSocket / SSE) — out of scope; the CLI's poll-with-debounce gives equivalent latency at ~25ms per round-trip, and request/response semantics make the protocol trivially `curl`-able. See architecture.md §9.3.
- **Multi-touch gestures beyond pinch** — UI Automator's `PointerInput` API is exposed via `swipe_ext`/`drag`; we'll add `multi_touch` if a real flow needs it.
- **Anything that requires root** — explicitly out of scope. ShadowDroid runs as the standard instrumentation UID 2000 (shell-level).
