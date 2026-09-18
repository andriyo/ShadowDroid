# Requirement-driven verification (opt in)

Use this workflow for Android coding tasks, especially state restoration,
secondary screens, migrations, persistence, and configuration-sensitive UI.
Before changing code, preserve each explicit requirement and inventory its
reachable screens. Record the existing test selection and baseline failures.
Choose one representative journey early; reserve a small final-check budget.
Missing checks remain untested. A plan written by the coding agent still needs
an independent completeness review against the original task.

## Coordinate first

Discover `commands session --json` and select an explicit online device. Open
one driver session for the complete experiment, pass its `--session` token to
every managed command, then `session observe`. Other agents can use passive
`session observe --subscription <unique-name>` or inspect saved artifacts.
Use separate devices and source/build directories for parallel writers.
A target name or per-command lock does not reserve a multi-step journey.
Handoff requires no in-flight work and a fresh observation by the new owner.
Direct ADB, IDEs and external processes are outside this cooperative fence.

## Budget and plan

Discover `commands verify --json` before constructing commands. Write a schema
version 1 plan with original requirement text/source, check IDs, and dependencies.
Use task-specific checks; do not run every configuration after every edit.

```json
{
  "schema_version": 1,
  "task": "Preserve a draft through recreation",
  "source_root": ".",
  "requirements": [{"id":"draft","text":"Preserve entered draft after recreation","source":"user task","checks":["state-test"]}],
  "checks": [{"id":"state-test","adapter":{
    "kind":"junit","argv":["./gradlew",":app:testDebugUnitTest","--rerun"],
    "cwd":".","timeout_ms":120000,"reports":["app/build/test-results/testDebugUnitTest"],
    "selection":"project tests that assert draft restoration","minimum_tests":1
  }}]
}
```

This delegates the assertion to project tests; a generic passing test is not
proof of this requirement. Inspect the test's actual assertions. Run
`verify plan validate plan.json`, then `verify run plan.json --out /outside/source/run-001`.
The output directory must be new. `verify report /outside/source/run-001`
rechecks saved evidence and source freshness without driving the device.
Use `--host-only` for explicitly host-only plans.

## Choose the smallest useful evidence

- JUnit: existing unit/instrumentation tests; explicit selection and fresh reports.
  Missing, skipped, empty or stale reports cannot prove completion. Preserve
  baseline failures and compare regressions with `verify compare`.
- Build/install: fresh single-APK build output plus observed installed hashes.
  App checks must depend on that build to bind results to current edits. Split
  installation is unsupported by this build adapter.
- Journeys: stable selectors, assertions and declared destinations; remember and
  compare values across background/resume or saved-task process-kill simulation.
  Rotation alone does not prove recreation; force-stop is a separate cold launch.
- Matrices: a few relevant theme/orientation/font/display configurations, with
  explicit app-data reset policy. Configuration changes are not render assertions.
- SQLite: parameterized bounded reads of a quiescent private snapshot; requires
  debuggable run-as and stops the app. Keep it separate from transient-state work.
- Visual: comparable reference PNG plus metadata; deterministic pixel limits and
  possible accessibility findings. This is not a semantic visual judge.
- Constraints: source-pattern heuristics and actual resolved Gradle dependencies.
  Compilation alone does not prove runtime binding or architectural compliance.
- Platform tests: named instrumentation assertions for actual/stubbed intents,
  MediaSession release, hosted widget updates and PiP entry within declared APIs.
  Check the project test code and observation limits.

## Review and recovery

After two unchanged failures, inspect fresh evidence and change the diagnostic
approach. Preserve each failed attempt. Never blindly replay an action with
unknown delivery. Stop on unknown execution/cleanup; do not steal a lease or
remove its journal. Inspect `verify recover` and `session recover` prerequisites:
stop external workers, restore owned configuration, and when required reboot the
disposable device before reopening ownership. Instrumentation releases the
UiAutomation slot and leaves the server disconnected; reconnect explicitly.

Before finishing, review every requirement, unvisited destination, regression,
source/build binding, stale artifact, blocked check and cleanup result. Report
what passed and what remains unresolved. An agent's own completion claim is not
an independent acceptance result. Do not install this guide into user skills
without an explicit install/sync request.
