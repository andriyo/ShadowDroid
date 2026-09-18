# Requirement verification

The `verify` namespace connects task requirements with evidence. It provides offline plan validation, JUnit normalization, baseline comparison, and bounded execution with a restart-readable evidence ledger. Device runs use the shared [session authority](sessions.md). The [roadmap](agent-verification-roadmap.md) tracks additional adapters and evaluation work.

## Plan contract

A version 1 plan retains the original task and requirement text. Each requirement has a stable ID, a source location, and zero or more check IDs. A requirement with no checks remains visible as unmapped. An exclusion requires a nonempty `not_applicable` reason and cannot also reference checks. Unknown fields, duplicate IDs, missing references, invalid time limits and dependency cycles are rejected.

```json
{
  "schema_version": 1,
  "task": "Preserve the edited name after rotation.",
  "inputs": ["app/src/main/java/example/Profile.kt"],
  "requirements": [
    {"id": "name-state", "text": "Preserve the edited name after rotation.", "source": "task", "checks": ["rotation-test"]}
  ],
  "checks": [
    {
      "id": "rotation-test",
      "adapter": {
        "kind": "junit",
        "argv": ["./gradlew", ":app:testDebugUnitTest"],
        "cwd": ".",
        "timeout_ms": 120000,
        "reports": ["app/build/test-results/testDebugUnitTest/TEST-example.ProfileTest.xml"],
        "selection": ":app:testDebugUnitTest",
        "minimum_tests": 1
      }
    }
  ]
}
```

The example declares a test invocation; validation does not execute it or establish that the named test actually covers rotation. An independent reviewer must still compare requirements against the original task. Explicit input paths will delimit freshness checks; a Git revision alone cannot identify a dirty source tree or an installed APK.

```bash
shadowdroid verify plan validate verification.json
shadowdroid verify junit --report TEST-example.ProfileTest.xml --selection ':app:testDebugUnitTest' --out baseline.json
shadowdroid verify compare --baseline baseline.json --candidate candidate.json
```

## Test evidence

JUnit parsing retains suite/class/name identity, test duration, source location, failures, errors, skips and diagnostics. Multiple reports are allowed with repeated `--report`. Reports are bounded to 16 MiB each. Empty, missing, malformed, truncated or duplicate test evidence is unresolved. A skipped test is not a pass. Suite test counts are checked against the actual test cases.

`verify junit` reads saved reports, so it always labels execution as `not_observed` and freshness as `unknown`. A parser status of `passed` describes the XML cases only. It does not prove a successful fresh build, full-suite selection, or source-to-APK correspondence. The command exits successfully when it can report an observation, even when that observation is failed or blocked. Raw XML should be retained with the originating test invocation.

`verify compare` requires normalized JSON produced by `verify junit --out`. It reports regressions, fixes, missing tests, added tests and skips. Missing tests or changed test selections prevent a `regression_free` conclusion. Existing baseline failures remain visible. This comparison does not certify task completion.

These host-only commands remain available with malformed project configuration, so an agent can inspect evidence while repairing setup. See the live command catalog for exact options.

## Executable runs

```bash
shadowdroid verify run verification.json --host-only --out /tmp/verification-attempt-1
shadowdroid verify report /tmp/verification-attempt-1
# For instrumentation, select the dedicated device and set instrumentation: true in its check:
shadowdroid -s emulator-5556 verify run verification.json --out /tmp/verification-attempt-2
```

`source_root` defaults to the plan's directory. Run output must be a new directory outside that source root. Git inputs include tracked, dirty and untracked files; ignored build outputs are excluded. Explicit `inputs` add fixtures and references, or define the entire input scope outside Git. Missing inputs and symlinks are refused. Input snapshots are limited to 30,000 entries / 512 MiB. Include external references explicitly; no tool can infer every external build input.

A run reserves the source/build root and selected device through all checks. Check dependencies run in a stable topological order; a nonpassing prerequisite blocks its dependents. JUnit commands use explicit argv, cwd and a 1–3,600,000 ms timeout. Each output stream retains at most 32 MiB and records truncation. A passing test check requires successful execution and fresh, complete XML. Reused Gradle outputs are stale: use the project's appropriate rerun option when fresh execution is required. `baseline` optionally identifies normalized JSON for the same test selection. `instrumentation: true` releases UiAutomation around the command while retaining the device reservation. The service remains disconnected afterward until a later operation explicitly needs it.

The output directory contains the original plan, atomic manifest, append-only events, immutable check results, raw logs/XML, normalized test evidence, artifact hashes and a final report. `verify report` checks those hashes and current source/plan fingerprints after a restart. It preserves failed, blocked, stale, untested and excluded requirements; an omitted route cannot disappear. A changed source input conservatively invalidates mapped requirements. These are historical results: a report does not reobserve the device or establish source-to-installed-APK identity from JUnit alone.

A timeout or interruption retains partial evidence and marks the outcome unknown. The process group is terminated where supported, but arbitrary external Gradle daemons/services can outlive a child. Ownership therefore remains quarantined. After inspecting and stopping external workers, use:

```bash
shadowdroid verify recover /tmp/verification-attempt-1 --external-workers-stopped
```

This releases source/build ownership without changing the old results or replaying actions. Device recovery remains a separate `session recover` operation requiring a changed boot identity. A known test failure permits a new run; an unknown execution does not. Evidence failure or incomplete cleanup never becomes a passing result.

External commands run with the user's permissions; this is coordination, not a sandbox. They receive `ANDROID_SERIAL` and `SHADOWDROID_DEVICE` but must themselves honor that selection. Isolate source roots, Gradle output directories, ports, app accounts and backend data when running independent agents. The authority cannot stop an IDE or direct ADB client from changing inputs. Artifacts are private local files and may contain application data. `--redact` applies configured output/log redaction, but the plan and source identity are retained for reproducibility; avoid secrets in task text and argv.

## UI journeys, lifecycle and configuration

A `journey` adapter contains `package`, `destinations`, a bounded `timeout_ms` (default 120000), and sequential `steps`. It requires an explicitly connected server. Targets are stable exact selectors: `{"by":"rid","value":"example.app:id/name"}`, `text`, or `description`. Actions are `start` (activity), guarded `tap`/`text`, `key`, `assert`, `remember`, `compare`, `configure`, `lifecycle`, and `capture`. A failed step blocks later steps; every declared destination remains visible until a passing assertion visits it. The observed graph covers only the declared experiment.

```json
{"kind":"journey","journey":{
  "package":"example.app","destinations":["profile"],"steps":[
    {"action":"start","activity":".MainActivity"},
    {"action":"text","target":{"by":"rid","value":"example.app:id/name"},"value":"generated-name-42"},
    {"action":"configure","configuration":{"rotation":1}},
    {"action":"assert","target":{"by":"rid","value":"example.app:id/name"},"text":"generated-name-42","destination":"profile"}
  ]
}}
```

An assertion checks exact match count (default 1), optional text, enabled, selected and checked state. `count: 0` is an absence check in the observed accessibility tree, not proof of complete app semantics. `remember` records a named element's text; `compare` checks it against `memory` with optional `different: true`. A test app can expose an activity-instance identifier to prove recreation separately from rotation. Capture-only journeys remain untested; screenshots are review evidence and do not establish visual compliance.

Lifecycle modes are `background_resume`, `background_kill_restore`, and `force_stop_cold_launch`, each with an explicit `resume_activity`. They record foreground component, task ID and main PID. Background experiments reorder the prior top activity to the front; they do not use the normal `app start` task-clearing behavior. Process termination requires a saved, stopped activity, working `run-as`, an explicit SIGKILL of the original PID, observed process absence, and restoration of the same task/top activity with a new PID. This is a debuggable-app simulation, not every low-memory condition. Unavailable saved state, denied access, a process that never died, or an unverified task restoration is blocked. Force-stop is recorded as a separate cold-launch experiment.

Configuration steps support rotation (0–3), font scale (0.5–3), system night mode, display size, and density. They journal exact prior settings, including unset values and absent display overrides, before mutation and verify readback. Cleanup runs after success and known failures. A value changed by another owner produces a conflict instead of being overwritten. An interrupted run retains its journal; `verify recover ... --external-workers-stopped` restores only still-owned values and releases build ownership. It does not clear an unknown device operation: the separate session recovery remains required. Recovery changes the journal, so old evidence is never silently upgraded to a pass.

A `matrix` adapter supplies a `journey` and 1–32 named `cells`, each with `id` and `configuration`. Each cell restores its baseline before the next starts. `reset_app_data: true` explicitly clears the target package before each cell; otherwise app data is shared across cells and this is recorded. Animations are unchanged. System theme readback alone does not establish correct rendering or an app-specific theme override: include corresponding UI assertions or visual review.

## SQLite and Room persistence

The `sqlite` adapter declares `package`, a private `database` path under `databases/`, and a `query` with SQL, scalar JSON `parameters`, and exact `expected_rows`. Queries should specify `ORDER BY` when row ordering matters. Discovery/schema information comes from SQLite's schema catalog; compatible Room databases use the same mechanism.

```json
{"kind":"sqlite","package":"example.app","database":"databases/app.db","query":{
  "sql":"SELECT name FROM profiles WHERE id=?1 ORDER BY id","parameters":[42],"expected_rows":[["generated-name-42"]]
}}
```

This adapter **force-stops the app** and uses the existing protected app-state snapshot machinery, preserving SQLite sidecars. It checks that package processes remain stopped and rechecks file bytes/membership before querying the copy. Snapshots are capped at 64 MiB/100 files. Keep this experiment separate from transient saved-state tests. Cooperating device ownership excludes writers; an external process that bypasses ownership remains outside that guarantee.

SQLite opens the copy read-only, with query-only mode, trusted schema disabled and an authorizer denying writes, attachments, pragmas, extensions and other mutation capabilities. A quick integrity check precedes the query. The engine enforces time/size limits; default limits are 100 rows and 2 seconds (maximum 1000 rows/10 seconds), 64 KiB text values and 1 MiB projected output. BLOBs are projected as byte counts and content hashes. Missing access, unsupported encryption, corruption, denied SQL or truncated results are blocked, never an inferred empty result. The source database is not queried or modified by SQLite. Full private snapshots stay local with restricted permissions; projected summaries follow `--redact`.
