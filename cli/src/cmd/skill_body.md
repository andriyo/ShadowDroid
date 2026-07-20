`shadowdroid` is the agent-facing control and debugging layer for a running
Android app: deploy, control app/device state, inspect and act on UI, triage
logs/crashes, read debugger/layout state, observe network traffic. Gradle or
the `android` CLI builds; ShadowDroid verifies the result on a device or
emulator.

## Discover before constructing a command

The live CLI is the source of truth — prefer its machine catalog to remembered
syntax or scraped help:

```bash
shadowdroid commands --json --depth 1
shadowdroid commands --json --describe 'ui tap'
shadowdroid commands --guide net
```

The catalog carries canonical paths, argument constraints, output modes, and
hand-authored agent hints. Domain depth is served on demand — before first use
of a domain, read its driving guide: `--guide net` (proxy, TLS trust, capture
sessions, rules, in-app OkHttp AAR), `--guide debugger` (Studio plugin, debug
sessions, Layout Inspector fallbacks, recompositions), or `--guide state` (app
state snapshot/restore, appops scoping, profile files, private files).
Covered groups alias to their guide (`aar` → net, `layout` → debugger).

## First contact and device selection

```bash
shadowdroid devices
shadowdroid --target mobile connect
shadowdroid -d emulator-5554 doctor --json
```

Prefer a project-configured named target (`--target mobile|tv`,
`default_target`) over persisting an ephemeral serial; a target reuses a
running emulator by AVD name and starts one only if its config says
`start: "if-needed"`. Otherwise never silently choose between attached devices
or start an emulator: read `devices`, pass global `-d <serial>`, or ask.
Explicit `-d/--device` overrides target selection; `--takeover` only when
reassigning another project's claimed AVD is intentional.

`connect` may install the instrumentation APKs and claims Android's single
`UiAutomation` slot; wrap Espresso/UI Automator runs in
`shadowdroid test -- <command>` to release and reclaim it, or `disconnect`.

## Output and exit contract

Treat stdout as data and the process exit code as authoritative:

- Action success: one object with `type:"action"`, `ok:true`, `cmd`, non-empty
  `next_actions`. Raw reads (`ui dump`) return the payload directly; exit zero
  is success even without an envelope.
- Failure: one object with `type:"error"`, `ok:false`, `stage`, `code`, `msg`,
  `retryable`, `detail`, non-empty `next_actions`.
- `watch`, `log`, `net log`, and `debug replay` stream JSONL; large exports
  (HAR, curl, fixtures) write an artifact and return a small JSON summary; a
  few setup/report commands default to human output — request `--json`.

Branch on `ok`/`code`, inspect `detail`, follow the most relevant
`next_actions` entry; never parse `msg` to recover state. Inside a `watch`
stream a `type:"error"` record is a timeline event, not the one-shot envelope
— keep consuming. Operational logs go to stderr (`--quiet` silences).

## Project config and recovery

`config init --project --app Example --package com.example.app --json` stores
repeated device/app/debugger values (user `~/.shadowdroid/config.json`;
project `.shadowdroid/config.json` discovered from ancestors; nearer values
override, CLI flags win). Config is data, not shell code — identifier fields
are validated and quoted at the device-shell boundary; shell syntax fails
typed. Run `config validate --json` after editing a committed config;
validate/paths/schema and `commands --json` still run when a malformed config
blocks everything else.

## Predictable read, act, confirm loop

Start each UI decision from the structured tree:

```bash
shadowdroid ui dump
shadowdroid ui tap --rid btn_sign_in --expect-text "Welcome" --timeout-ms 3000
```

Prefer selectors in this order: stable `--rid`, Compose test tag/resource id,
`--desc`, exact `--text`, then XPath; coordinates only for a genuinely
gesture-only surface. `--text`/`--desc` match normalized case-insensitive
substrings (`--exact` requires the full value; values starting with `-` need
the equals form, `--text=-50%`).

Selector actions are strict: multiple non-exact matches fail as
`ambiguous_match` rather than choosing one, and taps resolve a non-clickable
child to its nearest enabled clickable ancestor or fail typed
(`--coordinate-fallback` only when raw center injection is intended). Set
range controls with `ui set-progress --value/--percent`.

Check-act-observe (full flag semantics: `commands --describe 'ui tap'`):

- Acting on a previously read screen: pass `--if-screen <screen_hash>`; a
  changed UI prevents the action and returns the fresh screen. Act only from a
  `consistent` snapshot.
- Destination known: pass exactly one `--expect-*` flag (implies observation).
  An unmet destination fails as `postcondition_timeout`; its `detail.screen`
  is evidence only — never reuse element ids from an unproven destination.
- Otherwise `--observe`, then read `input_delivered`, `stable`, and
  `screen_changed` separately — a valid action may leave the screen unchanged.
- `ui wait` timeouts are typed non-zero `wait_timeout` failures; never treat
  one as successful polling.

Use global `--redact` for UI/log/network/watch/collect output; screenshots
are pixel-masked only when explicitly requested (`--redact-pixels`,
`--redact-screenshots`) and stay labeled potentially sensitive. On
TV/leanback prefer `ui focus` and `ui key dpad_*`.

## Failure triage

- `why` — one bounded, non-mutating diagnosis (crash, ANR, network, or just a
  different screen); never installs or starts the server.
- `log --last 5m --level e` — bounded app-scoped logcat with crash/ANR blocks
  parsed into events.
- `collect --app <pkg>` — handoff bundle; degrades to adb evidence without the
  server.

UI and app results may carry an `events` array for a crash/ANR detected since
the previous invocation; inspect it before probing further.

## App, device, permission, and file operations

Prefer the dedicated typed verbs over ad hoc shell:

```bash
shadowdroid app install ./app-debug.apk --grant-all --launch --wait-front
shadowdroid appops set com.example.app CAMERA ignore --scope uid
shadowdroid files pull --run-as --app com.example.app files/state.json local.json
```

These verbs verify readback: permission/app-op changes, profile apply/reset,
explicit file modes, app clear/stop, and install steps fail typed, non-zero
when the requested state was not reached. Use `device shell` only when no
typed verb exists. Private file/state access requires a debuggable package
with working `run-as` and never prints file contents. Read
`commands --guide state` before appops scoping, `profile apply --file`, or
`app state` snapshot/restore work.

## Android Studio debugger and layout

The optional Studio plugin adds debugger control and Layout Inspector data
(start from `debug auto Example`). Expression evaluation is real evaluation
with possible side effects. With several debug sessions prefer stable session
`id`s; if ambiguous, stop and choose — never act on an arbitrary session.
Read `commands --guide debugger` before debugger or layout work.

## Network debugging

`net` is a host-side MITM proxy: `net start` changes the device proxy,
`net stop` restores it. Run `net check <pkg>` before assuming HTTPS will
decrypt; `tls_error` means the app rejected the MITM path. Pinned OkHttp
traffic needs the optional in-app AAR companion. Read `commands --guide net`
before `net` or `aar` work.

## Maintenance and self-improvement

`skill --sync` refreshes pristine installed skills after upgrades (hand-edits
preserved). Opt-in `usage enable` + `usage report` builds a local friction
backlog — local-only, no argument values recorded.
