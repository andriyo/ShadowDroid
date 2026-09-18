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

A `key` step requires an immediately following `assert` or `compare` for its
postcondition. The key is guarded by a fresh stable screen and sent once.
Android's raw injection result is advisory: `false` can accompany a delivered
key, so it is retained without claiming failed delivery or pressing again.
Only the following observation establishes the requested outcome; a missing
postcondition is rejected before execution, and a failing one fails the journey.

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

## Build identity and migration constraints

`build_install` accepts a `build` object with explicit `argv`, `cwd`, `timeout_ms`, `apk` (relative to cwd), and `package`. It runs the build, requires a freshly produced APK, installs it, and independently hashes the installed bytes. An up-to-date old APK is stale; choose the project's correct clean/rerun invocation. This installer supports a single APK; installed split sets are observed and cannot silently pass as a matching base APK. The evidence records the build command and its limits: an observed build/install chain is not a hermetic build attestation.

Device checks hash the installed APK set before and after their observations. A change invalidates the check. `current_edits_verified_at_run` requires successful build provenance, unchanged source inputs, and device checks linked through dependencies to the matching build. Historical reports still do not reobserve the current device. Plain JUnit instrumentation cannot by itself establish this binding. `connect` is an explicit setup adapter with optional `server_apk`, useful after instrumentation released UiAutomation; it may install/start the server and does not verify app requirements.

`source_constraints` takes `rules.files`, `required_patterns`, `forbidden_patterns`, and/or `forbidden_extensions`. It reports exact file/line matches and labels the result as a text-search heuristic, not architectural proof. Rules come from the task, with no invented universal restrictions. Files and references are automatically included in the input fingerprint.

`resolved_dependencies` takes a `dependencies` object: Gradle `argv`, absolute project `module` (for example `:app`), resolvable `configuration`, `timeout_ms`, and `required`/`forbidden` coordinates (`group`, `name`, `version`). A generated init script reads Gradle's resolved component graph. Declaration text is not treated as resolution. Only the selected configuration is covered; use project tests/build checks for other variants and secondary UI journeys for runtime DI bindings.

## Visual contracts and review bundles

A journey `capture` produces PNG and metadata with their shared hash, before/after UI observations, configuration, timestamps, consistency, and conservative accessibility findings. Unstable captures are blocked. `--redact` applies pixel redaction using the observed tree; an unstable image is not written when it cannot be redacted safely. Bounds can suggest small controls, missing labels or off-screen geometry, but findings alone do not prove touch bounds, clipping, contrast or full Compose semantics.

`visual_comparison` takes a `comparison` object with `reference_png`, `reference_metadata`, `capture_check`, `capture_name`, optional matrix `cell`, an explicit `max_changed_fraction`, optional `channel_tolerance` (0–255), and optional masks (`bounds: [left, top, right, bottom]`, `reason`). The check must depend on its capture check. Reference and candidate configuration/viewport, image hashes and capture consistency must match before comparison. The bundle retains reference, actual, difference image, metadata and findings. Masks and thresholds remain visible; masking every pixel is rejected. Inputs are capped at 32 MiB/16 megapixels per PNG.

A passing comparison means the declared pixel contract passed. It is not a semantic visual judgment or an Android Bench score. Review the image pairs with the calling vision-capable agent, and keep behavior checks separate. System image variation can cause differences even in a correct app; masks must be explicit and justified. No model API, network cost or uncalibrated visual judge is introduced.

## Reproduce the fixture checks

Build the CLI, server APK pair and sample app, then use an explicitly selected disposable emulator:

```bash
python3 scripts/e2e-verification.py --device emulator-5556 --exercise-recovery --out /tmp/verification-fixtures
python3 scripts/e2e-verification-build.py --device emulator-5556 --out /tmp/verification-build
python3 scripts/e2e-verification-visual.py --device emulator-5556 --out /tmp/verification-visual
```

The build fixture requires the sample's supported JDK in `JAVA_HOME`. The recovery fixture reboots the selected emulator. These suites preserve successful and defective attempts: state loss, missing secondary binding, light-only rendering, UI-only persistence, wrong resolved dependency, stale artifacts, viewport mismatch, and interruption. They validate tool behavior; they do not measure coding-agent effectiveness on the private Android Bench dataset.

## Platform boundary contracts

`platform_test` takes a `test` object with `package`, `min_api`, `max_api`,
`argv`, `cwd`, `timeout_ms`, `reports`, `selection`, and named `contracts`.
Each contract declares `boundary` (`outbound_intent`, `media_session`,
`widget_update`, `picture_in_picture`), exact JUnit `class` and `test`, and
`observation` (`actual`, or `stubbed` for intents only). Missing, duplicate,
skipped or unsupported cases stay blocked. Only those named project assertions
establish the stated boundary; unrelated passing tests cannot satisfy it.

The adapter checks the device API, releases UiAutomation around instrumentation,
and records installed APK contents before/after. Configure the test runner to
leave the tested app installed for the final identity check. With AGP's connected
test task, use `-Pandroid.injected.androidTest.leaveApksInstalledAfterRun=true`.
Reinstallation into a different Android directory is allowed if all APK content
hashes and sizes remain identical. A missing or different APK prevents a pass.
JUnit `reports` accept files or bounded directories of XML, including dynamic
AGP device report names; existing cached XML does not become fresh evidence.

The sample's actual instrumented assertions exercise randomized intent payloads,
MediaController session-destruction callbacks, a real AppWidgetHost receiving a
second RemoteViews update, and Activity PiP state. Correct and seeded defective
variants run with:

```bash
python3 scripts/e2e-verification-platform.py --device emulator-5556 --out /tmp/platform-fixtures
```

A released MediaSession does not prove every audio/decoder resource was released.
One widget host does not cover all launchers. These fixtures require API 29+ and
were locally exercised on API 36; physical camera and Wear behavior are outside
this adapter's demonstrated scope. Camera/Wear and deeper headless Compose
inspection remain conditional roadmap investigations.

Configuration restoration waits for effective font scale and settled display
rotation as well as stored values. A matrix starting with `start` applies its
configuration after the initial launch, so a portrait-only launcher cannot mask
a landscape app experiment. Unsupported app-enforced orientation stays blocked.

Font-scale restoration waits for a pending owned change to settle before
overwriting it, then requires stored and effective values to agree for two
seconds. This guards against delayed framework write-back; it is not a portable
proof that every vendor's settings storage has flushed to disk. Recovery fixtures
also reboot the disposable emulator and check the restored value again.
Night-mode baselines retain Android's distinct `custom_schedule` and
`custom_bedtime` values when restoring a temporary light/dark override.
