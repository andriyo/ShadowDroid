<!-- Keep a Changelog guide -> https://keepachangelog.com -->

# ShadowDroid Android Studio Plugin Changelog

## [Unreleased]

### Added

- First-class logpoint bridge routes: `/v1/logpoints`, `/add`, `/events`,
  `/remove`, and `/clear`. Creation validates condition and log expressions,
  forces `SuspendPolicy.NONE`, applies configuration before enabling, rejects
  conflicts with manual/other-owner breakpoints, and rolls back failures.
- Versioned (`schema_version: 1`) structured logpoint callback events with
  project, session, device, cached app/process identity, source, breakpoint
  configuration, and Android Studio's composite rendered message.
  The process-local stream uses monotonic cursors, a 512-event bound, explicit
  overflow/eviction/rate-limit metadata, Unicode-safe configurable truncation,
  a hard 65,536-character message ceiling, and configurable per-breakpoint event
  acceptance limits. A known cursor overflow returns immediately even when
  retained events do not match a follower's filter.
- Bridge API v2 advertises structured-logpoint capacity and safety defaults in
  both `/v1/status` and the local bridge registry.
- Safe in-memory ownership: same-owner creation is idempotent, owner removal and
  clear cannot delete manual IDE logpoints, and external IDE edits relinquish
  cleanup ownership.
- Managed temporary logpoints atomically claim and retain only their first
  structured event before removing themselves asynchronously; owner checks
  prevent deleting a breakpoint edited or adopted in the IDE before cleanup.
- Newly created logpoints revalidate both IDE registration and their unchanged
  prepared fingerprint after expression validation and immediately before the
  configuration mutation.
- Breakpoint condition/log expressions that fail to evaluate no longer freeze the
  IDE behind a modal "Breakpoint Condition Error" dialog. A `breakpointBehaviorPolicy`
  extension answers the platform automatically for bridge-managed expressions:
  suspending breakpoints pause without a dialog, while non-suspending logpoints
  resume. The failure is emitted as a structured logpoint event and surfaced via
  `last_evaluation_error` on
  `debug breakpoints` and `breakpoint_errors` on `studio status`. Expressions a
  human set in the IDE keep the stock dialog.
- Set-time **syntax** validation for breakpoint condition/log expressions: obvious
  mistakes (e.g. `count ==== 3`) are rejected up front with `error_code=invalid_expression`
  and a `problems[]` list, instead of failing later at hit time. Pass `--force`
  (CLI) to skip validation. Semantic/name-resolution errors are intentionally
  left to the (now non-blocking) runtime path to avoid rejecting valid expressions.
- Kotlin (`.kt`/`.kts`) files now get the Kotlin line-breakpoint type, so
  conditions are evaluated with the Kotlin evaluator rather than parsed as Java.
- `studio status` reports `blocked_dialogs`; bridge timeouts name a blocking IDE
  dialog when one is open, instead of a bare "did not answer".
