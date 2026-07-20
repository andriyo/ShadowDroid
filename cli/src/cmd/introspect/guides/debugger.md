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
