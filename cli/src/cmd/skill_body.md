`shadowdroid` deploys, controls, and debugs Android apps. Gradle or `android`
builds; ShadowDroid verifies on devices.

## Discover before constructing a command

Discover syntax from the live catalog:

```bash
shadowdroid commands --json --depth 0  # shared globals/effect definitions, once per version
shadowdroid commands ui --json --compact
shadowdroid commands --search 'response body' --json --compact
shadowdroid commands --json --describe 'ui tap' --compact
shadowdroid commands --guide net
```

Reuse syntax within one CLI/schema version.
`--compact` references shared metadata; full JSON remains available.
Before first use, read `--guide net` (proxy/AAR), `--guide debugger` (Studio/layout),
`--guide state` (private files/app state), or `--guide evidence` (checkpoints
and video coverage). Use `--guide verification` for coding-task checks and concurrent agents.
Aliases: `aar` → net, `video` → evidence.

## First contact and device selection

```bash
shadowdroid devices
shadowdroid --target mobile connect
shadowdroid -d emulator-5554 doctor --json
```

Prefer a named target; otherwise discover `devices` and pass `-d`.
Explicit `-d` overrides targets. Never guess devices or boot an AVD unless
its target allows `start:if-needed`. `--takeover` reassigns ownership.

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

For one-shot batches, preserve every child's output and exit status:

```python
import subprocess
import sys

def checked_run(argv):
    r = subprocess.run(argv, capture_output=True, text=True)
    sys.stdout.write(r.stdout)
    sys.stderr.write(r.stderr)
    if r.returncode:
        raise SystemExit(r.returncode if r.returncode > 0 else 128 - r.returncode)
    return r.stdout
```

Use `pipefail` and `&&` for dependencies; `set -e` alone is insufficient.
Keep error/detail, events, and input/postcondition evidence. Serialize
lifecycle mutations. Reserve multi-step work with a driver session; observers
remain passive. Named-target operations also lock: the default wait is 2000 ms
(`--lock-timeout-ms 0` fails immediately). Never delete an active lock.

Branch on `ok`/`code`, inspect `detail`, follow `next_actions`;
never parse `msg` to recover state. Inside a `watch`
stream a `type:"error"` record is a timeline event, not the one-shot envelope
— keep consuming. Operational logs go to stderr (`--quiet` silences).

## Project config and recovery

Use `config init --project`, then `config validate --json` after edits. CLI flags
win. Discovery and config validate/paths/schema remain usable
when malformed config blocks other commands. See `commands --guide state`.

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

Selectors fail on ambiguity. Taps require an enabled clickable target/ancestor;
`--coordinate-fallback` explicitly permits raw input. Use `ui set-progress` for
range controls; see `commands --guide workflow` for selector details.

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

After guard rejection, reobserve, require consistency, reselect, and use a fresh
guard. Check input delivery before retrying. See `commands --guide workflow`
for interaction hashes, handles, and recovery examples.

`--redact` covers structured output; screenshot pixel masking requires
`--redact-pixels`/`--redact-screenshots`. On TV use `ui focus` and `ui key dpad_*`.

## Failure triage

- `why` — one bounded, non-mutating diagnosis (crash, ANR, network, or just a
  different screen); never installs or starts the server.
- `log --last 5m --level e` — bounded app-scoped logcat with crash/ANR blocks
  parsed into events.
- `collect --app <pkg>` — passive bundle; the device/AVD must be online. It
  reads an existing server session or degrades to adb evidence; never starts an
  AVD/server or forward.

Inspect UI/app `events` for crashes/ANRs before probing further.

## App, device, permission, and file operations

Prefer the dedicated typed verbs over ad hoc shell:

```bash
shadowdroid app install ./app-debug.apk --grant-all --launch --wait-front
shadowdroid appops set com.example.app CAMERA ignore --scope uid
shadowdroid files pull --run-as --app com.example.app files/state.json local.json
```

Mutations verify readback and fail typed/non-zero when state was not reached. Use `device shell` only when no
typed verb exists; quote the entire shell command. Read `commands --guide workflow`
for app start/wait and text-entry recipes. Private file/state access requires a debuggable package
with working `run-as` and never prints file contents. Read
`commands --guide state` before appops scoping, `profile apply --file`, or
`app state` snapshot/restore work.

## Android Studio debugger and layout

Use `commands --guide debugger` before Studio/debugger/layout work.
`debug auto Example` starts the workflow. Expression evaluation can have side
effects. With multiple sessions, select an observed stable `id`; never choose
arbitrarily. `debug logpoint` observes without suspension.

## Network debugging

`net` is a host-side MITM proxy: `net start` changes the device proxy,
`net stop` attempts owned restoration and reports `proxy_restoration`. Run `net check <pkg>` before assuming HTTPS will
decrypt; `tls_error` means the app rejected the MITM path. Pinned OkHttp
traffic needs the optional in-app AAR companion. Read `commands --guide net`
before `net` or `aar` work: it covers unknown observations, unresolved cleanup,
and prepared response rules. Ping/DNS never prove application HTTP connectivity.

## Maintenance and self-improvement

`skill --sync` updates pristine skills, preserves customizations, and retires
pristine duplicates. Opt-in `usage enable` + `usage report` records local command
metadata without argument values.
