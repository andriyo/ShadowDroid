# The agent loop

[← README](../README.md)

This is the canonical operating contract for LLMs and coding agents driving
ShadowDroid. The loop is **read → act → confirm**, and ShadowDroid is built so
each step costs as few round-trips as possible.

1. **Discover the surface once.** Start with `shadowdroid commands --json
   --depth 1`; use `commands --json --describe 'ui tap'` for one command,
   `commands --guide net|debugger|state` for a domain driving guide, or
   omit `--depth` for the full tree. Schema version 3 contains canonical paths,
   complete argument construction data (aliases, conflicts, requirements,
   groups, arity, and trailing/hyphen-value behavior), output contracts, and
   agent decision hints. Do not invent command names from memory or scrape
   `--help` prose.
2. **Put repeated context in config.** `shadowdroid config init ...` then
   `config validate --json`. Use an app alias instead of spending tokens on
   `--package`/`--project`/`--debugger` every call. See
   [configuration.md](configuration.md).
3. **Connect.** `shadowdroid connect`; if it fails, `shadowdroid doctor --json`,
   then `doctor --fix` only when repair side effects are acceptable.
4. **Read by dumping.** `shadowdroid ui dump` returns the actionable tree as a
   compact element list plus strict content identity, actionable interaction
   identity, screen-bound handles, and freshness metadata. Act only
   from `snapshot_state: "consistent"`; a lifecycle race is retried within a
   bounded window, then returned as `transitioning` with a warning. Cache the
   hash/version pairs; invalidate a cached hash if its version changes.
5. **Act by selector, not coordinates.** Prefer `--rid`, then `--desc`/exact
   `--text`. Add `--observe` to wait for an accessibility quiet period and get
   the post-action screen in the same response. Prefer one `--expect-*`
   destination condition when the outcome is known; it implies observation.
   Add `--if-interaction <hash>` to stable selectors on dynamic screens, or use
   a returned `--handle` when no stable selector exists. Use `--if-screen
   <hash>` when even display-only content must remain identical. A mismatch
   returns the fresh screen — that *is* your re-read.
6. **Confirm.** `ui wait --text/--rid/--pkg` blocks until the expected state and
   echoes the matched element; a timeout returns `top_texts` so you see what the
   screen became instead of guessing.
7. **Watch when timing matters.** `shadowdroid watch` streams screen diffs,
   crashes, ANRs, toasts, watcher actions, Android Studio logpoint hits, and
   (with `net` running) HTTP events on one JSONL timeline.
8. **Triage failures with one read.** After any surprise, `shadowdroid why`
   returns a verdict + evidence; `shadowdroid log --last 5m` gives the structured
   logcat behind it. You rarely need both plus a screenshot — start with `why`.
9. **Go deeper only when needed.** `shadowdroid debug ...` (Android Studio
   debugger as JSON) and `shadowdroid layout ...` (Compose semantics/source/
   recompositions) when UI polling can't answer *why*. See
   [debugging.md](debugging.md).
10. **Free the slot for instrumentation.** `shadowdroid test -- <cmd>` (or
    `disconnect` first) before Espresso / UI Automator runs — Android allows one
    `UiAutomation` owner at a time.

## Reading the screen

A typical agent reads `ui dump` once, acts by `--rid`/`--text`, and caches
strict `content_hash` and actionable `interaction_hash` together with their
versions. Legacy `screen_hash` remains the unprefixed strict identity for
backward compatibility. A hash is comparable only within the same version;
invalidate it when the version changes. `content_hash` changes for any visible
or actionable snapshot detail. `interaction_hash` ignores display-only content
(telemetry, timers, video surfaces) and a slider's current value, but includes
actionable hierarchy, bounds, stable selectors, enabled/selected/checked state,
range shape, and supported actions. Text participates when it is the control's
only selector.

`snapshot_state`, `captured_at_ms`, `current_app.sampled_at_ms`, and `ui_tree`
make lifecycle freshness explicit. Do not derive an action from a
`transitioning` snapshot; retry or wait for the expected package/activity.

Every `ui dump` also reports `accessibility_completeness`. UIAutomator cannot
prove that custom-drawn or unexported Compose controls are represented, so the
normal tree is explicitly marked `unverified` instead of looking deceptively
complete. With Android Studio Layout Inspector attached, ask for the deeper
comparison:

```bash
shadowdroid ui dump --deep
shadowdroid ui tap --fallback-id cs:12345 --if-screen <screen-hash>
```

Missing Compose nodes are returned under `fallback.elements` with an id,
bounds/tap point, `source` (`compose_semantics` or `compose_layout`), confidence,
and `stable_selector:false`. High-confidence `cs:` semantics results can be
targeted directly by fallback id. Layout-only `cl:` results (including custom
drawing without exported semantics) require both `--coordinate-fallback` and a
same-snapshot `--if-screen` guard. OCR is not run automatically; direct `X Y`
taps remain an explicit coordinate target and support the same screen guard.

## Selectors

Selectors are consistent across commands: `--text`, `--rid` (resource id),
`--desc` (content description), and `--xpath`.

Text/desc selectors match as a **normalized, case-insensitive substring** by
default: before comparing, surrounding whitespace is collapsed, curly
quotes/apostrophes/ellipsis are folded to ASCII, and zero-width characters are
stripped — so `--text "sign in"` matches a `SIGN IN` button and `--text "Don't
allow"` matches text rendered with a typographic apostrophe. Add `--exact` (on
`ui find`/`tap`/`text`/`wait`/`focus`) to require a full match (so `--text
Allow` won't hit a label reading "Allow Disney+…"), and `--clickable` to skip
non-clickable matches instead of resolving their clickable ancestor. `--rid` is
the most reliable target when a stable resource id exists. Matching is
**literal** — `*`, `.`, `?` and other symbols match themselves, with no
wildcards or regex (a value starting with `-` needs the `--text=-50%` equals
form so it isn't read as a flag).

Selector **actions** are **strict**: if `ui tap`/`text`/`focus` matches several
elements and none is an exact match, they fail with a structured
`ambiguous_match` error listing the candidates rather than guessing — narrow
with `--exact`, `--rid`, or `--clickable`. On a hit, `ui tap`/`wait`/`focus`
echo back the matched element so you can confirm the right node was targeted.

## Element handles

Each actionable element also has a screen-bound `handle` such as
`i:6b4f20feab9812c3/e:2`. Prefer a stable `--rid` or `--desc` plus
`--if-interaction`; use `--handle` when no stable selector exists. Handles are
accepted by `ui tap`, `ui set-progress`, and `ui text`. They are resolved to the
fresh numeric element id only after validating their embedded interaction hash,
so stale navigation/recomposition or reused numeric ids fail as `stale_element`
without delivering input. Agent-facing `next_actions` follow the same order:
stable selector first, handle second, and numeric ids only as a strictly
`--if-screen`-guarded compatibility fallback.

## Acting

Selector taps are semantic by default. A non-clickable match resolves to its
nearest enabled clickable ancestor and reports both `matched_element` and
`activated_element`; a disabled target fails with `element_disabled`, and a
label with no safe ancestor fails with `element_not_clickable`. Raw center
injection requires explicit `--coordinate-fallback` (or direct `X Y`
coordinates). Tap results separate `selector_matched`, `actionable_resolved`,
`input_delivered`, `screen_changed` (with `--observe`), and
`postcondition_satisfied`, so a valid no-op action is not confused with an
undelivered input.

Loop-fusion action verbs (`ui tap`, `ui set-progress`, coordinate gestures,
`ui pinch`, `ui text`, `ui key`, `ui back`, and `ui home`) accept `--observe`
(wait for a 500 ms accessibility-event quiet period, then return the stable
compact screen), `--if-screen <hash>` (strict optimistic concurrency), and
`--if-interaction <hash>` (ignore display-only volatility while guarding
actionable structure). The device server re-captures and validates that state
in the same guarded request that resolves the target and injects input. A
server too old to support this contract fails the distinct guarded route before
injecting anything. Both refuse changed state and return the fresh screen.

A single `--expect-text`, `--expect-desc`, `--expect-rid`, `--expect-package`,
or `--expect-activity` postcondition implies observation; `--expect-exact`,
`--observe-delay-ms`, and `--timeout-ms` refine matching and timing. An unmet
condition fails with `postcondition_timeout`; a screen that never settles fails
with `observation_unstable`. Both preserve the freshest screen under
`detail.screen` as diagnostic evidence only, so start a new interaction cycle
instead of reusing its element ids.

`ui wait` also syncs on the foreground app, not just elements: `--pkg <package>`
blocks until that app reaches the foreground (e.g. a Custom Tab or share sheet
opened), and `--pkg-not <package>` blocks until the screen leaves it.

## Sliders and ranges

Platform and Compose sliders expose their accessibility `range` (`type`,
`min`, `max`, `current`, and nullable `step`) plus stable `actions` in the
normal `ui dump` shape. Android's range API does not expose a declared discrete
step, so ShadowDroid leaves `step:null` unless a future platform surface can
prove it. Set a slider semantically and verify the resulting range readback:

```bash
shadowdroid ui set-progress --desc "Follow stand-off slider" --value 0.45 --observe
shadowdroid ui set-progress --rid distance_slider --percent 80 --if-interaction <interaction-hash>
shadowdroid ui set-progress --handle <handle> --percent 50 --observe
```

Out-of-range values fail unless `--clamp` is explicit. Missing range semantics
or `ACTION_SET_PROGRESS` is a typed error; `--coordinate-fallback` opts into an
approximate track click and reports when readback could not verify it.

## Entering a PIN without echoing it

Numeric system keypads can be driven without returning the secret in action
JSON: `shadowdroid ui pin "$DEVICE_PIN" --if-interaction <interaction-hash>`.
The command preflights every required digit before entering anything, activates
each label through its clickable ancestor, and presses Enter by default;
`--no-submit` leaves the digits entered for controlled validation. Its output
contains only a digit count and delivery/submission state. Unlike ordinary
loop-fusion actions, `ui pin` intentionally exposes only the pre-action
`--if-screen` / `--if-interaction` guards — not `--observe` or postconditions,
whose returned screen could reveal an unmasked keypad value. The PIN is still
present in the local process invocation, so use the host's normal shell-history
and process-visibility protections. When either guard is present, every digit
and the optional Enter revalidate the original interaction identity inside the
on-device action request; partial-entry failures remain PIN-redacted.

## Android TV / leanback

Android TV is focus + D-pad driven, not touch driven: `/v1/state` reports
`is_television: true`, each element carries a `focused` flag, and
`ui focus --text/--rid/--desc [--center]` walks the D-pad to a selector (then
optionally activates it) — the TV analog of `ui tap` / `ui scroll-to`. Prefer
it (and `ui key dpad_*`) over coordinate taps there.

## The live timeline

`watch` is the streaming workhorse — it emits debounced, hash-diffed `screen`
events plus `crash`, `toast`, `watcher_fired`, structured Android Studio
`logpoint` hits, and `http` events when a `net` proxy is running (plus a
`tls_error` when an app rejects the proxy CA, so a failed interception is
visible instead of just missing). A missing Studio bridge or proxy produces a
structured warning while the other producers continue; use `--no-logpoints` or
`--no-net` when that source is intentionally absent.

## The command surface

| Group | Commands |
| --- | --- |
| **Discovery/setup** | `commands --json --depth 1`, `commands --json --describe '<path>'`, `commands --guide '<topic>'`, `config paths` / `schema` / `explain` / `init` / `validate`, `skill`, `studio status` / `install`, `init`, `update`, `usage` |
| **Session/diagnostics** | `devices`, `connect`, `disconnect`, `test`, `doctor`, `collect`, `why`, `log` |
| **UI automation** | `ui dump`, `ui audit`, `ui gen`, `ui screenshot`, `ui find`, `ui tap`, `ui set-progress`, `ui double-tap`, `ui long-tap`, `ui swipe`, `ui drag`, `ui swipe-ext`, `ui pinch`, `ui scroll-to`, `ui focus`, `ui text`, `ui pin`, `ui key`, `ui hide-keyboard`, `ui back`, `ui home`, `ui wait`, `ui toast` (action verbs take `--observe`, `--if-screen`, and `--if-interaction`; tap/progress/text also accept `--handle`) |
| **Triage** | `why` (one-read verdict + evidence), `log` (structured app-scoped logcat + parsed crashes) |
| **Live timeline** | `watch` (screen changes, crashes, ANRs, toasts, watcher actions, Android Studio logpoint hits, and HTTP events when network capture is active) |
| **Screen video** | `video record -o DIR [--duration 30s]`; detached `video start -o DIR`, `video status`, `video mark LABEL`, `video stop` |
| **Layout / Compose** | `layout snapshot`, `layout diff`, `layout source`, `layout recompositions` |
| **Debugger** | `debug auto`, `snapshot`, `record`, `replay`, `status`, `sessions`, `clients`, `attach`, `break`, `breakpoints`, `logpoint add` / `list` / `events` / `follow` / `remove` / `clear`, `pause`, `resume`, `step-in`, `step-over`, `step-out`, `stop`, `stack`, `threads`, `variables`, `eval`, `inspect`, `coroutines`, `continue-until`, `watch`, `step-until-screen-change`, `step-until-log`, `run-until-crash`, `native`, `tombstones` |
| **App lifecycle/state** | `app start`, `stop`, `install`, `reinstall`, `clear`, `info`, `wait`, `current`; `app state snapshot`, `restore`, `recover`, `cleanup` |
| **Permissions/app-ops** | `perm grant`, `revoke`, `list`, `reset`; `appops get`, `set` |
| **Device/system** | `device info`, `shell`, `wake`, `sleep`, `unlock`, `orientation`, `clipboard`, `notifications`, `quick-settings`, `open-url` |
| **Display profile** | `profile snapshot`, `apply`, `reset` (animations, font, density, size, rotation) |
| **Files** | `files ls`, `push`, `pull` (add `--run-as --app <pkg>` for private debuggable-app paths) |
| **Network MITM** | `net check`, `trust`, `ca import/info/reset`, `start`, `stop`, `status`, `log`, `checkpoint`, `show`, `export`, `intercept`, `resume`, `drop`, `respond`, `rule`, `rules`, `replay` |
| **In-app AAR agent** | `aar install` (`--okhttp`, `--coroutine-probes`, `--build`), `status`, `remove`, `capture`, `intercept`, `resume`, `drop`, `agent`, `coroutines` |
| **Authoring/testing helpers** | `ui audit` (selector gaps), `ui gen` (Screen Object scaffold), `net export fixtures` (replayable response set + `manifest.json`, GraphQL keyed by operationName), `test` (instrumentation command with the slot freed), `debug replay --repeat --diff` (flake hunting) |

Run `shadowdroid commands --json --depth 1` for the live catalog,
`commands --json --describe '<path>'` for one command, or `--help` for a human
view.

## Related contracts

- [output-contract.md](output-contract.md) — envelopes, exit codes, `events`,
  streaming.
- [device-state.md](device-state.md) — app/device/permission/file/profile/video
  controls and their verified postconditions.
- [network.md](network.md) — HTTP(S) and WebSocket capture, interception,
  rules, and replay.
- [debugging.md](debugging.md) — `why`/`log`/`collect`, the Android Studio
  debugger surface, logpoints, and layout inspection.
