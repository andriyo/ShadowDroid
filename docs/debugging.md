# Triage and agent debugging

[← README](../README.md)

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
captured even if no already-established on-device server session is reachable,
without installing, starting, repairing, or forwarding one. `collect` is
passive with respect to device lifecycle: the selected device/AVD must already
be online, and it never starts an AVD, server, or adb forward. With global
`--redact` (or `redaction.enabled` in config), every JSON/text artifact is
filtered and the manifest records privacy status per file. Screenshot pixels
are never silently treated as safe: they remain marked potentially sensitive;
add `--redact-screenshots` to explicitly black out accessibility bounds
matching the active policy.

## The agent debugger

Driving a UI tells an agent *what* happened on screen; debugging tells it
*why*. ShadowDroid hands a coding agent a live Android Studio debugger as plain
JSON — so when a tap doesn't do what the agent expected, it can set a
breakpoint and read the actual program state instead of guessing from
screenshots. Reads are bounded, while attach, pause/resume/step,
breakpoint/watch changes, and evaluation have normal debugger side effects. It
is a debugger control surface, not a remote shell.

Backed by the optional Android Studio plugin:

- **`debug auto [app]`** — low-effort path: resolve an app alias/name/package,
  launch it, attach the Studio debugger when available, then return a full
  snapshot with setup guidance if the bridge is missing.
- **`debug`** — attach to the running app; set breakpoints (line, exception,
  method, field watchpoint; conditional and temporary) and owner-scoped,
  non-suspending logpoints; read the call stack, local variables, and watches;
  evaluate/inspect expressions (`this`, locals, fields, array indexes) and
  follow object handles while the session remains suspended. Treat evaluation
  as real debugger evaluation rather than assuming arbitrary expressions are
  side-effect-free. Requests are bounded — they return structured failure
  instead of blocking without a suspended frame.
- **`debug snapshot`** — one shot: device + build, foreground app, screen tree,
  screenshot, recent logcat, the live debugger stack / variables / breakpoints,
  and a bounded page of recent logpoint events in a single JSON object.
- **`debug record` / `debug replay`** — JSONL timelines of screen changes,
  lifecycle, logcat, structured logpoint hits, and replayable actions (taps,
  text, keys, swipes, drags).
- **`debug run-until-crash` / `step-until-screen-change` / `step-until-log`** —
  let the app run until something interesting happens, then return a full
  snapshot; crash waits emit parsed Java/native/ANR events and can write local
  bundles.
- **`debug native` / `debug tombstones` / `debug coroutines`** — native/mixed
  readiness, tombstone artifacts, and conservative suspended-state coroutine
  insight without arbitrary code execution. (For whole-process coroutine dumps
  from a *running* app with no debugger attached, use `aar coroutines` after
  `aar install --coroutine-probes`.)
- **`layout`** — UI-tree snapshots and diffs, enriched (when Studio's Layout
  Inspector is live) with Compose source locations, semantics, and recomposition
  counters.

## Non-suspending logpoints

Logpoints observe a source line without leaving the app paused on successful
hits. They require the ShadowDroid Android Studio plugin, the matching Android
project open in Studio, and a debuggable app attached to the Studio debugger.
The source must match the installed build. Conditions and expressions run in
the app's debugger context and can call code, mutate state, block, or throw;
stack rendering and high-frequency hits add more overhead. "Non-suspending" is
not the same as free or side-effect-free.

| Command | Purpose |
| --- | --- |
| `debug logpoint add` | Transactionally add or update a line logpoint with `--expression`, `--log-message`, and/or `--log-stack`; optional `--condition`, `--pass-count`, `--temporary`, ownership, rate, and message-size bounds apply in the same operation. |
| `debug logpoint list` | List configured logpoints, optionally filtered by project, id, or owner. |
| `debug logpoint events` | Read one bounded page of structured hit events, optionally strictly after a paired `--after <cursor> --stream-id <stream_id>`. |
| `debug logpoint follow` | Follow new hits as JSONL; it starts at the live tail unless paired `--after` / `--stream-id` values or `--replay-existing` are supplied, and can stop by duration or event count. |
| `debug logpoint remove` | Remove one ShadowDroid-owned logpoint when its id and owner match. |
| `debug logpoint clear` | Remove only ShadowDroid-owned logpoints in the requested project/owner scope. |

`debug logpoint add` immediately disables a newly registered breakpoint,
validates the condition and expression, applies every logging option plus
suspend policy `none`, then enables it in one Studio operation. A validation or
configuration failure rolls back a newly created logpoint and leaves an existing
one unchanged, so no partially configured breakpoint survives the request. The
public JetBrains API does not expose create-disabled registration; on an already
attached, extremely hot line there is therefore a theoretical interval before
the same IDE task applies the initial disable. Prefer adding instrumentation
before attach when even that narrow platform-level race is unacceptable. At
least one of `--expression`, `--log-message`, or `--log-stack` is required.
`--force` skips set-time expression validation; a bad expression can then fail
at runtime, where the failure is recorded and a non-suspending logpoint resumes
without an IDE-blocking dialog. Use it deliberately because evaluation can
still have side effects. `--temporary` retains the first structured hit, then
asynchronously removes the still-owned logpoint because Studio does not
auto-remove non-suspending actions.

Each hit is a structured event with a monotonic cursor plus logpoint, source,
project, debugger-session/device, and ownership context. The payload contains
JetBrains' composite rendered message: expression output, the optional default
hit message, and an optional rendered stack can share that one string and are
not promised as separately parsed fields. `debug snapshot` includes a bounded
recent page; `debug record` and `watch` interleave hits as ordered timeline
records; the dedicated `events` and `follow` commands support narrower project/
session/id/owner filters.

The Studio-side history is bounded. Pages report a `stream_id` and
`oldest_cursor`, `latest_cursor`, and `next_cursor`; `--after` means strictly
newer than that cursor. Cursors belong to one stream, so resuming requires both
`--after <cursor>` and the page's `--stream-id <stream_id>`. A one-shot read
rejects a mismatched stream instead of returning ambiguous events; a follower
discards the mismatched page and resets to the new live tail with a warning.
Studio restart changes the stream id. If a consumer falls behind the retained
history, `overflowed:true`, `evicted_total`, and a cursor-gap warning make the
loss explicit. Bound hot sites with `--condition`, `--pass-count`, or
`--max-events-per-second`; page-level `rate_limited_total` reports suppressed
hits instead of allowing memory to grow without limit. `--max-message-chars`
marks oversized events with `message_truncated:true` and retains
`original_message_chars`; its supported range is 256 through 65,536. Global
`--redact` scrubs supported JSON/JSONL output, including snapshots and
recordings, but it cannot stop the expression from being evaluated or the
unredacted value from reaching Android Studio first.

Ownership is the cleanup boundary. The default is `shadowdroid`; concurrent
agents should use distinct `--owner` labels and pass the same label to `remove`
or `clear`. These lifecycle commands neither repurpose nor delete ordinary
human-created Studio breakpoints or logpoints owned by somebody else. Prefer
them over generic `debug break remove` for temporary agent instrumentation. If
a human edits an owned logpoint in Studio, ShadowDroid relinquishes ownership.
Ownership is also intentionally process-local: after Studio restarts, a
persisted logpoint becomes unowned/manual rather than eligible for a later
owner-wide deletion.

## Addressing multiple devices

Multiple devices debugged in one Studio are addressable: `debug sessions`
reports each session's device, stable `id` (for that Studio debug-session
lifetime), and current numeric index. Prefer `--session <id>`; the index remains
available for convenience but can change as sessions start/stop. Global `-d
<serial>` selects that device's session when no explicit session is supplied.

## Graceful degradation

Everything degrades gracefully: with no Studio plugin running, the device and UI
commands still work and the debugger section just reports `available:false`.
Run `shadowdroid debug --help` and `shadowdroid layout --help` for the live
command surface.
