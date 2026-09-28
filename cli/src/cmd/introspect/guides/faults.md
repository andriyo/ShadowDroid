# Fault injection guide

`fault` puts the device or app into a failure condition on purpose and undoes
it reliably. `fault kinds` lists every kind with its category, whether it is a
**state** (stays until cleared) or an **action** (happens once), its scope
(`device`, `emulator`, `proxy`), whether it needs `--app`, and the minimum API.
`commands --json --describe 'fault inject <kind>'` shows a kind's exact flags.

```bash
shadowdroid fault kinds
shadowdroid fault inject airplane-mode --duration-ms 10000
shadowdroid fault inject process-death --app com.example --relaunch
shadowdroid fault list
shadowdroid fault clear --all
```

## What to expect from every kind

- Refusals change nothing: a missing app, an emulator-only kind on a phone,
  too old an API, or a conflict with an active fault on the same setting fail
  first (`fault_requires_app`, `fault_app_not_installed`,
  `fault_requires_emulator`, `fault_unsupported_api`, `fault_conflict`).
- A fault that would change nothing fails with `fault_no_effect`.
- A state fault records how to undo itself before changing the device, then
  verifies it took effect; if it can't, it is rolled back and fails with
  `fault_verification_failed`.
- The reply's `fault` has `id`, `state` (`active` for states, `completed` for
  actions), `params`, `observed` (what the device showed afterwards) and
  `restore_plan`. Read `observed`, not your assumptions.
- `fault clear` runs the plan and verifies each step. A fault it can't restore
  stays listed as `restore_failed` and the command fails with
  `fault_restore_failed`; clear it again once the device is reachable.
- `--duration-ms` clears a state fault on its own; `fault list` marks one whose
  timer did not run as `overdue`. `disconnect` clears everything left.
- Destructive kinds (`storage-full`, `clock`) need `--allow-physical` on a
  real device; `emulator-crash` is emulator-only.

## Reading the app's reaction

Faults are recorded in the device's history: `watch` streams `fault` events
(`injected`, `cleared`, `expired`, `completed`, `rolled_back`) beside UI and
crash events. `watch` holds the device, so while it runs send faults through
its stdin as `{"cmd":"fault","args":["inject","<kind>",…]}`; it clears due
`--duration-ms` faults itself. `why` lists active and recent faults, so a
crash reads as "crashed 1.2 s after airplane-mode". Proxy faults (`http-errors`,
`http-latency`, `bandwidth`, `connection-reset`, `truncated-response`,
`tls-failure`) need `net start`; `net log` marks every hit request with
`fault_ids`, and `fault list` shows `proxy_hits`. `--percent` below 100 picks
requests from `--seed`: the same seed hits the same requests in order.

## Useful combinations

- Saved state: `process-death --relaunch`, `dont-keep-activities`, then
  navigate away and back.
- Offline handling: `airplane-mode`, `network-flap --period-ms 3000`,
  `dns-failure`, or `http-errors --status 503 --percent 30 --seed 1`.
- Background work: `doze`, `standby-bucket --bucket restricted`,
  `battery --level 5 --saver`.
- Layout: `font-scale --scale 1.5`, `display-size --size 1080x1200`,
  `app-locale --locales ar`, `split-screen`, `orientation --rotation 90`.

## Scenarios

`fault run scenario.json [--seed N]` runs steps in order: `{"run": [argv]}`
(any ShadowDroid command), `{"fault": [argv]}` (a `fault …` command),
`{"wait_ms": N}`, and `{"pick": [[argv], …]}` (one option chosen from the
seed). Steps take the device lock themselves and must not pass `-d`. A step
that exits non-zero stops the run unless it has `"allow_failure": true`; the
command then fails with `fault_scenario_failed` and `detail.steps`. Faults the
scenario injected are cleared at the end either way. On an emulator,
`fault snapshot save <name>` before and `fault snapshot load <name>` after
return the whole device to a known state.
