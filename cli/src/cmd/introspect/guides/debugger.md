# Android Studio debugger and layout guide

The optional Studio plugin adds debugger and Layout Inspector data. Begin with:

```bash
shadowdroid studio status --json
shadowdroid debug auto Example
shadowdroid debug snapshot --depth 1
shadowdroid layout snapshot --compose --semantics --source-map
```

`ui dump` marks accessibility completeness as unverified because UIAutomator
cannot prove all drawn controls are exported. If visible Compose content is
missing, attach Android Studio Layout Inspector and run `ui dump --deep`.
Fallback elements report `id`, bounds, source, confidence, and selector
stability. Tap a high-confidence semantics result with `ui tap --fallback-id
cs:<draw-id>`; a lower-confidence `cl:` layout result requires both
`--coordinate-fallback` and `--if-screen <hash>`. OCR is never implicit.

Debugger commands can attach, pause/resume/step, and mutate breakpoint/watch
state. Treat expression evaluation as real debugger evaluation: keep it bounded
and do not assume an arbitrary expression is free of side effects.

## Non-suspending logpoints

Use a logpoint to capture locals, fields, or a stack at an exact source line
while the app keeps running. Creation is one transactional Studio request and
always uses `suspend_policy=NONE`; supply at least one output:

```bash
shadowdroid debug logpoint add \
  --file app/src/main/java/example/Foo.kt \
  --line 42 \
  --expression '"user=" + user.id' \
  --condition 'state == FAILED' \
  --pass-count 10

shadowdroid debug logpoint follow --max-events 20
```

Configuration failure rolls back a new logpoint and leaves an existing one
unchanged. JetBrains does not expose create-disabled registration, so on an
already attached, extremely hot line there is a theoretical interval before
the same IDE task applies the initial disable. Add the logpoint before attach
when even that narrow platform-level race is unacceptable.

`follow` starts at the live tail by default. Add `--replay-existing` to emit the
current bounded history first, or use
`debug logpoint events --after <cursor> --stream-id <stream_id>` for one bounded
page. Copy both values from the prior page: a numeric cursor is valid only in
the stream that issued it. Each page reports `stream_id`, `next_cursor`, buffer
overflow/eviction counters, and rate-limit totals. A follower emits an explicit
warning if Studio restarts the stream or if buffer eviction creates a cursor
gap; it never silently presents the remaining events as complete history.

Android Studio supplies one rendered message for a hit. Combining
`--log-message`, `--log-stack`, and `--expression` produces a composite rendered
message; do not parse it as independently structured expression and stack data.

Structured capture defaults to 100 events/second and 16,384 message characters
per logpoint. Override those bounds with `--max-events-per-second` and
`--max-message-chars`; the plugin clamps them to safe ranges and reports
suppression/truncation metadata. Message retention accepts 256 through 65,536
characters. Global `--redact` applies at the CLI output and
recording boundary. Expressions can still expose or mutate sensitive runtime
state before that boundary, so keep them narrow.

Logpoints created by this command default to owner `shadowdroid`. Cleanup is
owner checked and will not remove an unowned IDE breakpoint:

```bash
shadowdroid debug logpoint list --owner shadowdroid
shadowdroid debug logpoint remove --id <id>
shadowdroid debug logpoint clear --owner shadowdroid
```

With several debug sessions, run `debug sessions`. Prefer each entry's stable
`id` (stable for that Studio debug-session lifetime) over its current numeric
index:

```bash
shadowdroid debug sessions
shadowdroid debug stack --session session_2
shadowdroid debug variables --session session_2 --depth 2
shadowdroid debug resume --session session_2
```

Global `-d <serial>` selects the session attached to that device when no
explicit session is supplied. If selection remains ambiguous, stop and choose
an id; do not act on an arbitrary session.

Use `layout source` to map a UIAutomator id or Inspector draw id back to source.
Use `layout recompositions --reset`, perform one interaction, then read
`layout recompositions` to isolate Compose churn.
