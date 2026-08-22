# The output contract

[← README](../README.md)

Treat the process exit code as authoritative. Most one-shot commands print one
JSON object on stdout: action success as
`{"type":"action","ok":true,"cmd":…,…}`, a raw read such as `ui dump` as its
payload, and failure as `{"type":"error","ok":false,"stage":…,"code":…,
"msg":…}`. Every terminal JSON success and failure includes a non-empty,
command-specific `next_actions` array; failures also include `retryable` and
structured `detail`. Use those fields instead of parsing `msg`. Raw reads can
omit `ok`, so exit code zero is their success signal.

Streaming commands are explicit JSONL exceptions (`watch`, `log`, `net log`,
`debug logpoint follow`, and `debug replay`); `test` passes through the wrapped
command and adds a ShadowDroid trailer. Stream errors have the same `code`,
`retryable`, `detail`, and non-empty `next_actions` recovery fields, while
terminal stream summaries carry follow-up actions. Human, source, and
wrapped-command pass-through output cannot embed the JSON field; their exact
follow-ups remain available from `commands --json --describe '<path>'`. Interop
exports such as HAR, curl, and fixtures write an artifact and emit a small
terminal action naming its path and byte count. Some setup/report commands
default to human output and offer `--json`. Unknown-argument and
missing-command errors are JSON and exit 2; a spelling suggestion is included
when one is available. Explicit `--help` remains human-readable.

Runtime actions preserve the selected `-d <serial>` and shell-quote identifiers
copied from device/app output. If a required value is not yet known, the CLI
emits an exact `commands --json --describe '<path>'` discovery action rather
than a command that would immediately fail.

Inside a running `watch` stream, `{"type":"error","stage":…,"code":…,"msg":…,
"input":…,"retryable":…,"detail":…,"next_actions":[…],"ts":…}` is a
timeline event, not the terminal one-shot error envelope above. Keep consuming
unless the stream ends or the task says to stop.

## Crash events ride any response

When the app crashed or ANRed since your previous command, the next result
(action *or* error) carries an `events` array of parsed `{"type":"crash",…}`
objects. No `watch` required, no separate poll — the crash finds you.
(`SHADOWDROID_NO_EVENTS=1` opts out.) Delivery is one-shot: each staged event
is attached to exactly one result.

## Failures are self-describing

- `element_not_found` carries `top_texts` (what *is* on screen) and `closest`
  (ranked near-matches to your selector).
- `ambiguous_match` lists the candidate nodes.
- `screen_changed` carries the fresh compact screen.
- `ui wait` timeouts are non-zero `wait_timeout` errors carrying `top_texts`,
  `current_app`, and recovery commands.

Read `detail` before re-dumping.

## Logging and strictness

- **Logs go to stderr.** ShadowDroid's own operational logging is on
  **stderr**, so `… | jq` already sees clean JSON. Add `--quiet`/`-q` (or
  `SHADOWDROID_QUIET=1`) to silence it entirely — handy when you merge with
  `2>&1`.
- **Selector actions are strict.** Several matches and no exact hit is an
  `ambiguous_match` error listing candidates, never a silent guess.

## Verified mutations

Mutation commands verify the requested postcondition instead of trusting an
empty Android shell response. Permission/app-op changes, profile apply/reset,
explicit file modes, app clear/stop, install steps, and goal-directed
scroll/focus operations exit non-zero when readback disagrees, with requested
and observed state in `detail`. See [device-state.md](device-state.md).
