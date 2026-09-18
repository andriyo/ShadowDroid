# Checked invocation and recovery

Check each child's exit status in batches. Copy this for one-shot invocations
(streams need incremental consumption):

```python
import subprocess
import sys

def checked_run(argv):
    result = subprocess.run(argv, capture_output=True, text=True)
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    if result.returncode:
        raise SystemExit(result.returncode if result.returncode > 0 else 128 - result.returncode)
    return result.stdout
```

For shell formatters use `set -o pipefail`; use `&&` for dependent actions.
Do not rely on `set -e` alone. Keep the full failure envelope, including `detail`,
`events`, snapshot consistency, input delivery, and postcondition evidence.
Serialize lifecycle mutations. Named AVD resolution also locks for read commands.
The default bounded lock wait is 2000 ms; `--lock-timeout-ms 0` fails immediately.
Never delete an active lock or use `--takeover` to bypass ordinary contention.

## UI guard recovery

After `screen_changed`, `interaction_changed`, or `stale_element`, inspect the
returned fresh screen (or run `ui dump`), require `snapshot_state:consistent`,
and reselect a unique target. Use `--if-screen` when screen content matters;
use `--if-interaction` when the interaction layout matters despite changing
text. A `--handle` identifies one element in that observed snapshot and must
be refreshed after staleness. Keep the relevant fresh guard and assert the
destination with `--expect-*`. If input was delivered or delivery is unknown,
observe the current outcome before deciding to retry; never blindly repeat it.

A screen guard protects the content you saw; an interaction guard protects the
layout/target identity when unrelated text is changing. Use both when both
matter. Select a handle only from the consistent snapshot you are acting on.
A postcondition timeout can follow successful input delivery; inspect the
current screen instead of repeating a potentially non-idempotent action.

## Common operations

```bash
shadowdroid app start com.example.app && shadowdroid app wait com.example.app --front
shadowdroid ui text 'hello world' --rid message_input --clear
shadowdroid device shell 'settings get global http_proxy'
```

`app launch` is an exact alias of `app start`; `ui type` aliases `ui text`.
`--wait-front` belongs to installation; for `app start` use `app wait --front`.
No alias guesses at the semantics of unsupported operations such as `app list`.
Use `commands --search '<operation>' --json --compact` before constructing them.

## Named targets

Resolving an AVD checks readiness and project ownership under a lifecycle lock,
even for `app info` and `net status`. Concurrent readers wait at most the
configured lock timeout (2000 ms by default). `--lock-timeout-ms 0` preserves
fail-fast behavior. `device_lifecycle_busy.detail` identifies the lock scope,
wait duration and recorded owner when available; owner_state:unknown does not
mean the lock is unowned. Cross-project reassignment still requires deliberate
`--takeover`; it is not a recovery for ordinary contention.

## Device selection

Prefer a project-configured named target (`--target mobile|tv`,
`default_target`) over persisting an ephemeral serial; a target reuses a
running emulator by AVD name and starts one only if its config says
`start: "if-needed"`. Otherwise never silently choose between attached devices
or start an emulator: read `devices`, pass global `-d <serial>`, or ask.
Explicit `-d/--device` overrides target selection; `--takeover` only when
reassigning another project's claimed AVD is intentional.


## Selector semantics

Selector actions are strict: multiple non-exact matches fail as
`ambiguous_match` rather than choosing one, and taps resolve a non-clickable
child to its nearest enabled clickable ancestor or fail typed
(`--coordinate-fallback` only when raw center injection is intended). Set
range controls with `ui set-progress --value/--percent`.
