<!-- Keep a Changelog guide -> https://keepachangelog.com -->

# ShadowDroid Android Studio Plugin Changelog

## [Unreleased]

### Added

- Breakpoint condition/log expressions that fail to evaluate no longer freeze the
  IDE behind a modal "Breakpoint Condition Error" dialog. A `breakpointBehaviorPolicy`
  extension answers the platform automatically for bridge-managed expressions:
  the session pauses at the breakpoint without any dialog, and the failure is
  recorded and surfaced via `last_evaluation_error` on `debug breakpoints` and
  `breakpoint_errors` on `studio status`. Expressions a human set in the IDE keep
  the stock dialog.
- Set-time **syntax** validation for breakpoint condition/log expressions: obvious
  mistakes (e.g. `count ==== 3`) are rejected up front with `error_code=invalid_expression`
  and a `problems[]` list, instead of failing later at hit time. Pass `--force`
  (CLI) to skip validation. Semantic/name-resolution errors are intentionally
  left to the (now non-blocking) runtime path to avoid rejecting valid expressions.
- Kotlin (`.kt`/`.kts`) files now get the Kotlin line-breakpoint type, so
  conditions are evaluated with the Kotlin evaluator rather than parsed as Java.
- `studio status` reports `blocked_dialogs`; bridge timeouts name a blocking IDE
  dialog when one is open, instead of a bare "did not answer".
