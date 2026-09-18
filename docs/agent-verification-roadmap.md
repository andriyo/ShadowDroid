# Agent verification roadmap

Status: E01–E10 and local E13/C0–C1 implementation delivered as experimental capabilities, with local validation recorded in the [validation report](verification-validation.md). E01 effectiveness measurement is a small pilot, and E12 Linux CI/publication gates remain separate. E11, model judging, physical camera/Wear and distributed coordination are conditional follow-ons. The [verification guide](verification.md) records actual command contracts; the original work packages below preserve the broader acceptance targets. Written 2026-09-18 against source commit `add874bd004e313014889a420abcba555b0df070` (`main`, source version `1.1.0`). The baseline below describes inspected source, not an installed binary or a new device-validation result.

The objective is to help an AI coding agent finish Android engineering tasks with evidence that the requested behavior works. The first investment is a persistent requirement checklist connected to lifecycle checks and existing tests. Subsequent work adds visual review, database checks, migration diagnostics, and Android system-boundary probes.

Concurrent operation is a first-release requirement. The companion [concurrency design](concurrency-roadmap.md) defines resource boundaries, agent cooperation, ownership, event subscriptions and recovery. The default is one driver per device, shared observers, and parallel drivers on isolated devices.

Android Bench 2.0 motivates this direction: it evaluates app creation, migrations, new features, and conversions using functional, regression, requirements, and visual checks. It reports incomplete secondary screens, theme support, state restoration, and runtime dependency failures among remaining gaps. Tasks run in containers, and the dataset is private. These observations guide our hypotheses; this roadmap is not a reproduction of the official evaluator or a claim of measured score improvement. [Android Bench methodology](https://developer.android.com/bench/methodology/2)

## 1. Outcomes and product decisions

The desired outcome is a higher independently verified task pass rate. Better failure localization, fewer forgotten requirements, and lower cost per successful task are supporting outcomes. More tool calls, larger evidence bundles, and an agent's own completion claim are not success measures.

Decisions for this roadmap:

- Keep Gradle, JUnit, Espresso, Compose testing, and existing project test frameworks responsible for builds and test execution. Add bounded orchestration, evidence, and a small set of typed checks to ShadowDroid.
- Let the calling agent translate a task into requirements and journeys. Core ShadowDroid stays deterministic and does not require a model API key.
- Run the first release with the CLI, ADB, and the existing server on a headless emulator. Studio enrichment and in-app instrumentation are optional capabilities with explicit availability.
- Require session ownership for concurrent managed work, including multiple agents in one project. Project/target names and per-command locks do not reserve an entire device journey.
- Treat every check as a claim with provenance, scope, and limitations. Missing evidence never becomes a pass.
- Preserve existing command behavior and output compatibility. New command names and schemas below are proposals until implementation review and catalog registration.
- Deliver by acceptance gates, not calendar promises. Effort sizes indicate relative implementation scope; scheduling follows the capability spikes and baseline measurements.

This is a deliberate extension of the current control-surface positioning. It adds narrow assertions and orchestration without a general-purpose testing language. Update the README FAQ and product description when this capability ships; the historical delivery plan remains a historical record.

## 2. Current foundation and actual gaps

| Area | Existing source capability | Addition needed |
| --- | --- | --- |
| UI actions | Selectors, guards, postconditions, consistency metadata, bounded waits in [CLI](../cli/src/cli.rs) and [agent loop](agent-loop.md) | Associate observations with requirements and repeatable journeys |
| Display configuration | Rotation, size, density, fonts, animations in [profiles](../cli/src/cmd/device_profile.rs) | Theme control, exact restoration of prior settings, matrix orchestration |
| App lifecycle | Start, stop, foreground waits and lifecycle observations in [CLI](../cli/src/cli.rs) and [debugging](debugging.md) | Separate verified lifecycle transitions and state-preservation checks |
| Layout | Tree snapshots/diffs and optional Studio enrichment in [layout](../cli/src/cmd/layout.rs) | Reference-image bundles, configuration-aware comparisons, semantic findings |
| Test authoring | Selector-quality audit and generated screen-object scaffolding in [authoring](../cli/src/cmd/authoring.rs) | Broader accessibility checks and journey assertions |
| Evidence | UI/network/video correlation and selected JSON/SharedPreferences fields in [checkpoints](evidence.md) | Requirement ledger, freshness rules, SQLite probes, assertion outcomes |
| App data | Private files and transactional snapshots/restores, including database sidecars, in [state handling](../cli/src/cmd/app_state.rs) | Queryable, consistent, bounded read-only database evidence |
| Existing tests | UiAutomation handoff, subprocess status, reconnect/cleanup in [test wrapper](../cli/src/cli.rs) | Structured report ingestion, baseline regression comparison, no-tests detection |
| Agent guidance | Generated skills and discovery/effect metadata in [skill body](../cli/src/cmd/skill_body.md) and [catalog](../cli/src/cmd/introspect) | Task planning, coverage review, verification-budget and completion guidance |
| Validation | Field Lab, [Android surface journeys](../scripts/e2e-android-surfaces.sh), [reliability suite](../scripts/e2e-reliability.py), [CI](../.github/workflows/ci.yml) | Defective/correct fixture pairs and independent agent-task evaluation |

Existing recording/replay should supply reusable action execution. Existing `collect` and `evidence checkpoint` remain passive; they must not silently gain orchestration or lifecycle side effects.

## 3. Architecture and contracts

The proposed flow is: task requirements → versioned verification plan → bounded execution using existing commands/test adapters → immutable evidence → requirement report → targeted next action.

| Component | Responsibility | Proposed implementation location |
| --- | --- | --- |
| Plan and ledger | Validate requirements, checks, dependencies, statuses, evidence references | New `cli/src/verify/` modules |
| Execution coordinator | Device ownership, bounded steps, cancellation, test handoff, recovery | New verification coordinator reusing device/config and debug-replay logic |
| Shared runtime authority | Agent sessions, fenced device leases, passive observers, independent subscriptions | E13 coordination layer shared by verification and ordinary managed CLI operations |
| Check adapters | UI, lifecycle, external test results, later SQL and visual findings | Small modules around existing command implementations |
| Artifact store | Atomic result writes, fingerprints, schema versions, summaries | Reuse `cmd/artifact.rs` and evidence conventions |
| Discovery and guidance | Command schemas, effect declarations, recovery hints, agent skill | Existing introspection modules and generated skill sources |
| Device capabilities | Supported transitions and observations, with explicit prerequisites | Existing server routes; optional app test adapter only where necessary |

Implementation should extract reusable operations from CLI handlers where necessary instead of recursively launching the CLI for every step. External test/build commands use explicit executable/argument arrays, working directories, timeouts, and recorded exit status. Their effects remain declared as external and potentially unbounded; argv execution is not a security sandbox.

### Requirement and result semantics

- Requirements have stable IDs, source text/location, applicability, and check IDs. An agent-authored plan must preserve the original requirement text and expose unmapped requirements.
- Each result is scoped to a plan revision, requirement revision, installed artifact, source/build provenance, device configuration, fixture/input seed, and check implementation version.
- Requirement statuses are `passed`, `failed`, `untested`, `blocked`, `stale`, and `not_applicable`. Non-applicability needs a recorded reason; unsupported required capabilities are blocked.
- A requirement passes only when all its required applicable checks have current evidence accepted by their declared evaluator. Coverage counts and excluded checks are always visible.
- Record deterministic results separately from visual/model assessments and human review. A visual assessment records the evaluator, prompt/rubric revision, input hashes, and uncertainty; it is not a deterministic fact or an official benchmark score.
- Keep execution completion, requirement satisfaction, and cleanup status separate. A run can finish normally with failed requirements. The checking command exits nonzero when required checks fail or remain unresolved; read-only reports can successfully describe such a run.
- Never infer a passing test suite from a zero process exit alone. Missing, stale, malformed, or unexpectedly empty reports remain unresolved.

### Freshness and evidence integrity

Fingerprint the plan, references, fixture inputs, effective device configuration, test invocation, and actual APK set, including splits where applicable. Record source revision plus dirty/untracked source inputs, relevant build/dependency configuration, tool versions, and provenance limitations. A matching version name or Git commit alone does not establish that the installed app matches the edited source.

Start with conservative invalidation: changed app/build inputs invalidate app-dependent results; changed requirements or references invalidate their dependent checks. Preserve old results as history. Optimize selective invalidation only after correctness is demonstrated. If source-to-APK identity cannot be established, the report must not claim that the current edits were verified.

Capture UI structure, images, and external observations with individual timestamps and bounded capture windows. Recheck the UI identity around image capture when practical; mark unstable or mismatched captures. Do not claim simultaneous sampling. Store large artifacts locally and return compact summaries with paths and hashes.

### Device ownership and recovery

Serialize independent writers for a selected device at journey/test-phase scope. E13 supplies the shared session/lease authority used by E03; avoid building a second verifier-only ownership system. Compose this authority with existing target/lifecycle locks without holding a non-reentrant lock across a nested acquisition. Release the UiAutomation slot through the existing test handoff for instrumentation steps while retaining the higher-level device reservation. Passive observers cannot reconnect or repair the service during that phase.

Journal owned configuration changes and interrupted execution. Restore the exact prior values, including unset/override distinctions, after success, failure, or cancellation. If another owner changed a setting, report an ownership conflict instead of overwriting it. Cleanup failure remains visible even if application checks passed. Do not automatically replay a non-idempotent action after interruption; require a fresh observation and an explicit restart point.

Record agent/session identity, canonical device instance, lease generation, candidate identity and independent event cursors in run evidence. Handoff drains earlier operations, transfers or restores the declared starting state, and requires a fresh observation from the recipient. Independent agents may inspect immutable artifacts concurrently; later scoped specialists operate only within a driver's explicitly delegated experiment. See [concurrency contracts and trade-offs](concurrency-roadmap.md).

## 4. Delivery sequence

P0 is required for the first verification release, P1 expands the core based on measured value, and P2 is conditional. Sizes are S (contained change), M (multiple modules/adapters), and L (cross-layer work or material feasibility uncertainty). They are not elapsed-time estimates.

| Milestone | Scope | Dependencies | Exit gate |
| --- | --- | --- | --- |
| M0 — Baseline and design | E01 evaluation baseline; E02 skill-only trial; lifecycle/backend and E13/C0 concurrency spikes | Current source | Frozen baseline, fixture specifications, measured bottlenecks, resource/ownership contracts |
| M1 — Evidence and regressions | E03 ledger/coordinator; E04 test reports; E13/C1 coordinated sessions | M0 | Requirement-to-report path plus exclusive driver, passive observers and safe recovery |
| M2 — Lifecycle verification | E05 transitions; E06 configuration matrix; revised skill | M1 | Demonstrated detection of state-loss and theme failures with recovery; first release candidate |
| M3 — Data and presentation | E07 SQL; E08 visual/accessibility | M2 contracts | Persistence and visual defects localized with trustworthy evidence |
| M4 — Migration and platform depth | E09 constraints; E10 boundary adapters; optional E11 deeper headless inspection | M1 contracts; relevant M2/M3 adapters | Each adapter earns inclusion through fixtures and task-level evaluation |
| M5 — Validated release | E12 packaging, docs, independent holdout evaluation | Applicable milestone gates | Published claims match measured evidence and packaged capabilities |

E01 measurement and E12 release discipline apply throughout; they are not postponed until the end. E04 and E05 can be implemented independently after E03's contracts settle. E07 and E08 can proceed independently once artifact and matrix contracts stabilize. E09 is not blocked on all visual/storage work. E11 must not block the first release.

E13/C0 and E03 settle contracts together. The E03 offline ledger can ship independently, but its live coordinator must use E13/C1 ownership. Advanced delegation/pooling in E13/C2 and remote coordination in E13/C3 are later expansions, not M2 prerequisites.

## 5. Work packages

### E01 — Baseline, fixtures, and independent evaluation

**Priority / size:** P0 / M. **Depends on:** none.

Create a reproducible local evaluation suite with fixed repository revisions, task instructions, visible agent inputs, independent acceptance checks, emulator images, budgets, and result collection. Keep Field Lab tool-contract fixtures separate from coding tasks used to measure agent effectiveness.

Add a container setup recipe that pins the CLI and matching server APKs, connects to the assigned AVD, and records cold setup separately from warm tool latency. Respect the task's tool/network policy and keep evaluator artifacts outside the agent's visible workspace. End the agent phase with an explicit instrumentation-slot handoff and owned-state cleanup before independent evaluation. Verify this integration locally before assuming an official harness can load ShadowDroid.

Start with small, independently authored tasks covering form-state retention, an omitted secondary route, a theme defect, persistence, a dependency-binding crash, and a pre-existing regression. Expand to all four engineering categories before broad claims. Maintain development and held-out task sets; the coding agent may run its own tests but must not read or edit the independent acceptance evaluator.

**Acceptance:** replay one baseline end to end from a clean checkout; preserve all attempts, environment failures, agent completion claims, patches, tool versions, resource consumption, and evaluator outcomes. Seeded tool fixtures must have an independently specified correct result and both defective and repaired app variants. Freeze the experimental plan before comparing variants.

### E02 — Agent guidance and verification planning

**Priority / size:** P0 / S initially. **Depends on:** E01 task definitions; E03/E05 for later guidance.

Add a short generated guide that teaches the agent to extract explicit requirements, inventory reachable screens, establish existing tests, verify one representative journey early, check applicable lifecycle/configuration behavior, and review unresolved requirements before finishing. Reuse existing commands in the first trial; only teach new commands when available in the live catalog.

Teach a bounded recovery rule: after repeated unchanged failures, inspect fresh evidence or switch diagnostic strategy. Preserve uncertain outcomes instead of asserting completion. Suggest checks according to task requirements and risk; do not run every matrix cell after every edit.

**Acceptance:** run a skill-only comparison with the same binary and task budgets; validate generated skills against the catalog and preserve customized installed skills. Each new feature ships with use-when, prerequisites, effects, failure recovery, and a short worked example. Do not silently install experimental guidance into user scopes.

### E03 — Requirement ledger and bounded coordinator

**Priority / size:** P0 / L. **Depends on:** E01 contracts.

Implement plan validation, requirement/check IDs, check dependencies, reusable journeys, result ingestion, freshness, a compact report, and bounded execution. Initial adapters support existing UI observations/actions and externally executed tests. Track expected destinations and intermediate failures; a broken first navigation step blocks downstream checks rather than making them disappear.

Record observed routes and navigation edges against the plan. A discovered UI graph is partial: screen hashes alone cannot establish screen identity or exhaustive app coverage. Associate observations with declared destinations and show routes still unvisited.

**Acceptance:** a deliberately omitted requirement remains untested; an unreachable route remains blocked; a missing artifact cannot pass; new source/APK/reference inputs invalidate results; repeated equivalent failures are visible; interruption preserves evidence and run ownership. Report paths must remain usable after the agent restarts. An agent-authored pass flag cannot replace adapter evidence.

### E04 — Structured build and regression results

**Priority / size:** P0 / M. **Depends on:** E03.

Extend the existing test wrapper with explicit report locations and parsers for JUnit-style unit/instrumentation results. Retain raw logs, exit codes, test identity, failures, skipped tests, timing, source locations where available, and infrastructure errors. Compare a baseline revision with the candidate, using the same intended test selection and configuration.

Separate compile failures, assertion failures, device failures, runner crashes, empty test selections, and reconnect/cleanup failures. Group duplicate compiler diagnostics into concise causal candidates while retaining the originals. Report failed-to-passed and passed-to-failed tests; missing or renamed tests require review and cannot silently improve regression status.

**Acceptance:** fixtures cover zero exit with no expected tests, stale XML, truncated reports, failed build with no XML, an actual regression, a baseline failure, skipped tests, and interrupted instrumentation. Test completion must preserve the existing UiAutomation handoff and child exit semantics. Running one filtered test never proves the whole suite passed.

### E05 — Verified lifecycle transitions

**Priority / size:** P0 / L. **Depends on:** E03; capability spike in M0.

Add explicit operations/check recipes for background/resume, rotation, activity recreation where supported, background process termination and task restoration, and cold launch after force-stop. Use plan-specific expectations for transient form state, scroll position, selected tab, back stack, persistent records, and background work.

These are different experiments. Configuration changes and system-initiated process death have different state-saving requirements; a ViewModel alone does not survive process death. [Android state-saving guidance](https://developer.android.com/develop/ui/compose/state-saving)

An ADB-induced background kill is a simulation, not proof of every low-memory behavior. Record PID/task observations and the mechanism used. Rotation may be handled without recreation, so it cannot establish that recreation occurred. Direct activity recreation may need an app test adapter; the M0 spike decides the supported backend. A force-stop/relaunch cannot be mislabeled as saved-task restoration.

**Acceptance:** run defective and repaired form/back-stack fixtures; verify the requested transition actually occurred before checking state; detect a process that never died; distinguish new task launch from restored task. Keep transient-state experiments separate from app-data snapshots that force-stop the app. Unsupported modes are blocked with a reason, and prior device settings are restored on interruption.

### E06 — Configuration matrix and restoration

**Priority / size:** P0 / M. **Depends on:** E03 and supported E05 transitions.

Extend profiles with verified theme controls and a matrix over light/dark, orientation, display size, and font scale. Model system theme and application-specific overrides separately. Add locale/RTL only after a backend and restoration spike; it is an extension, not a first-release dependency.

Provide a small task-selected matrix and a broader final verification matrix. Identify each cell in results, isolate starting app data as specified, and restore the baseline after every case. Preserve normal animation settings for animation/transition checks; an automation preset that disables animations cannot validate transition behavior.

For M2, render validation comes from the project's existing tests or an explicitly recorded agent/human image review. Capture and configuration control alone leave appearance requirements untested. The automated review bundles and additional visual checks arrive in E08.

**Acceptance:** find a dark-theme defect and a rotated/large-font layout defect; restore non-default and unset settings; report skipped/unavailable cells; handle an interruption or a second owner changing configuration. Applying a theme setting proves the setting changed, not that the app renders correctly; render validation is a separate check.

### E07 — SQLite and persistence evidence

**Priority / size:** P1 / L. **Depends on:** E03; E05 for persistence journeys.

Add database discovery, schema inspection, parameterized read-only queries, bounded row/value projection, and typed comparisons. Use working `run-as` for debuggable apps where available. SQLite-backed Room stores use the same file-level path; do not assume every Room configuration or encrypted database is readable.

Start with an explicitly requested quiescent snapshot that uses existing app-state machinery and retains relevant sidecars. Verify consistency before opening the copy. This mode stops the app and must be declared as such. A later live mode needs a supported transaction/backup backend; sequential copies of a changing database and WAL are insufficient. SQLite's backup API is one candidate for a consistent live snapshot. [SQLite backup documentation](https://www.sqlite.org/backup.html)

Use engine-enforced read-only access and restricted SQL capabilities, including restrictions on attachments, writable pragmas, and extension loading. Retain selected fields and artifact provenance; bound bytes, rows, time, and locking waits. Missing access, encryption, or inconsistency is unavailable evidence, not an empty result set.

**Acceptance:** verify randomized input survives a declared restart; catch a UI that displays saved data while the database is unchanged; test WAL-only committed data, concurrent writes, migration failure, denied `run-as`, and read-only enforcement. Prove the check does not mutate the source database. Keep full private snapshots protected and separate from redacted summaries.

### E08 — Visual comparison and accessibility checks

**Priority / size:** P1 / L. **Depends on:** E03 and E06.

Create review bundles containing declared reference images, actual screenshots, accessibility structure, viewport/insets/density/theme/font metadata, and capture consistency. Extend layout diffs with semantic findings and extend the existing selector audit with checks that the available tree can support. Preserve the distinction between weak selectors and accessibility defects.

Use deterministic checks for observable labels, roles, selected/enabled state, and measurable geometry. Flag possible clipping, overlap, or undersized controls with evidence and documented limits; accessibility bounds do not always equal actual touch bounds. Do not claim complete Compose semantics, contrast compliance, or pixel visibility from the accessibility tree alone.

Let the calling vision-capable agent review image pairs first. A pluggable judge is a later adapter with versioned rubrics, declared model/network cost, repeatability measurements, and calibrated outcomes. Keep raw images; normalization/masking is explicit and must not hide relevant app content. A single image comparison cannot validate behavior or an animation.

**Acceptance:** correct/defective pairs cover missing controls, wrong theme, clipping, missing semantics, and viewport mismatch. Include benign system-image variation to measure false positives. Mark incomplete tree coverage and unavailable source mapping. A static visual imitation that cannot complete the journey must fail behavioral checks. Add temporal/video checks separately for transitions; do not infer frame accuracy from current video markers.

### E09 — Migration constraints and failure localization

**Priority / size:** P1 / M. **Depends on:** E03 and E04.

Attach explicit task constraints to checks: required dependency versions, prohibited APIs/files, module boundaries, and expected build targets. Inspect resolved build information when available; a dependency declaration alone may not describe the runtime graph. Reuse project lint/build tooling and targeted source analysis before considering a new parser stack.

Give the agent a compact migration report joining build failures, source locations, failing journeys, and crash evidence. Exercise declared routes after dependency-injection changes; successful launch does not cover every binding. Reference installed/resolved API versions when suggesting a diagnostic next step.

**Acceptance:** detect a task-specific prohibited API, a wrong resolved dependency, an excluded build variant, and a binding failure reachable only on a secondary screen. A text search is labeled heuristic and cannot prove architectural compliance. The tool must not invent universal restrictions or prescribe an API version absent from the task/project.

### E10 — Android boundary adapters

**Priority / size:** P1 / L, delivered one adapter at a time. **Depends on:** E03/E04 and relevant lifecycle checks.

Prioritize outbound intents and media lifecycle, then widgets/Picture-in-Picture, then Wear OS and camera workflows if measured task demand justifies them. Each adapter declares supported Android/device versions, observation completeness, required instrumentation, and how it interacts with UiAutomation ownership.

Use existing app tests to verify intent extras where possible; Espresso-Intents supplies verification and stubbing facilities. Stubbing is appropriate for controlled fixtures, while acceptance of actual dispatch must record what was really observed. [Espresso-Intents](https://developer.android.com/training/testing/espresso/intents)

For media, distinguish visible playback, active sessions, and actual resource-release evidence. For widgets, verify a post-initial-render data change propagates. For Wear OS, record both devices, event identity, and observation boundaries. Mocks cannot establish physical camera/peripheral behavior.

**Acceptance:** each adapter catches an independently seeded defect and passes the repaired case. Absence from a limited `dumpsys` or log sample is not proof of release or non-delivery. A generic intent sniffer, universal leak detector, and physical-device guarantees are outside the initial adapter scope.

### E11 — Optional deeper headless inspection

**Priority / size:** P2 / L. **Depends on:** measured E01/E08/E09 gaps.

Investigate a headless path for missing Compose semantics or targeted runtime diagnostics only if task failures show that basic accessibility, logs, test adapters, and existing Studio integration are insufficient. Compare a debug-only app bridge, supported inspection interfaces, and test-framework adapters before attempting a new debugger implementation.

**Acceptance:** a feasibility spike demonstrates useful additional evidence on an independent app, documents version/app changes and lifecycle effects, and preserves truthful fallback behavior. Do not gate M2 on Studio installation, a new JDWP stack, a model service, or mandatory application instrumentation.

### E12 — Release, documentation, and measured claims

**Priority / size:** P0 for each shipped milestone / M. **Depends on:** the milestone being released.

Add effect contracts, error codes, command metadata, schema compatibility, generated-skill examples, operator guidance, and supported-capability tables with every new command. Keep the public source docs and companion docs synchronized; label experimental commands and limitations.

Run existing required formatting/lint/tests plus changed Android components and the relevant real-emulator journeys. Exercise interruption, stale data, unsupported capabilities, and cleanup failures. Validate headless Linux/container operation; preserve other shipped CLI platform builds. Choose a tested Android version matrix from actual support claims, including a supported lower API where practical; an API 36 result does not prove every supported API.

**Acceptance:** the packaged candidate passes its consumer smoke journeys; command discovery and generated skills describe only shipped behavior; artifact/schema compatibility is documented; independent evaluation is attached to any effectiveness claim. Publication additionally follows the existing release process, including artifacts, checksums, provenance, package managers, and installation checks. Completion of this roadmap document does not authorize publishing a release.

### E13 — Concurrent agents and resource ownership

**Priority / size:** P0 / L for C0/C1; later stages P1/P2. **Depends on:** joint E03 contract design; independently useful to normal CLI workflows.

Implement the [concurrency roadmap](concurrency-roadmap.md): canonical resource identities, one local execution authority, agent sessions, exclusive journey-level device ownership, passive observers, independent event cursors, fenced operations, bounded queues, handoffs and interrupted-run recovery. Cover direct serials, named targets, automatic server bring-up, instrumentation, daemons and ordinary managed commands; a verifier-only lock is insufficient.

**Acceptance:** independent agent processes cannot interleave unauthorized mutations on one device; a late former-owner request cannot affect the next experiment; observer reads cannot steal the instrumentation slot; both subscribers receive the same crash; one device's long action does not serialize other devices; ambiguous in-flight work blocks reassignment. Capability-scoped helper mutations, device pooling and multi-host coordination are separate later-stage gates.

## 6. Proposed CLI and artifacts

This table preserves the original design candidates. Implemented syntax is documented in the verification/session guides and live command catalog; do not infer unimplemented commands from this proposal.

| Proposed surface | Responsibility |
| --- | --- |
| `verify plan validate` | Validate an agent-authored plan offline; show missing requirement mappings and effects |
| `verify run` | Execute selected checks with declared prerequisites, time limits, device ownership, and evidence |
| `verify report` | Read results and currentness; list failed, untested, blocked, stale, and excluded requirements |
| `verify recover` | Inspect an interrupted run and restore still-owned settings; never blindly replay inputs |
| Session/lease/observation surfaces | Shared ownership and independent subscriptions for all managed agents; see E13 |
| `test` report options | Preserve existing execution behavior while adding report ingestion/baseline comparison |
| Lifecycle/profile extensions | Typed transitions and configuration support used by both agents and verification plans |
| Evidence/layout extensions | SQL projections and visual review artifacts, reusing current artifact conventions |

Keep the initial plan vocabulary small: existing UI actions, bounded observations, typed equality/presence checks, supported transitions, and external test adapters. No arbitrary branching language, embedded JavaScript, unbounded retries, or independent concurrent writers on one device. Later E13 grants may permit coordinated specialist actions within a single experiment. Sophisticated test logic stays in the project's existing framework.

An illustrative report fragment shows the intended agent experience. This is a schema sketch, not a currently emitted payload:

```json
{
  "schema_version": 1,
  "run_id": "example-run",
  "execution_status": "completed",
  "requirements_satisfied": false,
  "cleanup_status": "restored",
  "requirements": [
    {
      "id": "profile.rotation",
      "status": "failed",
      "check_id": "profile-form-rotation",
      "expected": {"display_name": "generated-input-42"},
      "observed": {"display_name": ""},
      "evidence_refs": ["checks/profile-form-rotation.json"]
    },
    {
      "id": "statistics.navigation",
      "status": "blocked",
      "reason": "required navigation control was not found",
      "evidence_refs": ["checks/open-statistics.json"]
    }
  ],
  "next_actions": [
    "inspect the failed form-state check and its lifecycle evidence",
    "inspect the saved navigation screen before retrying the statistics journey"
  ]
}
```

Store checked-in plans/fixture definitions separately from generated run artifacts. Prefer a host artifact directory outside the application's submitted patch; make paths configurable and do not silently edit ignore files. A run contains a manifest, an append-only event log, immutable per-check results, and referenced images/logs/test reports. Record schema/provenance/capture limits and apply existing redaction policies. Screenshots, video, and private database snapshots retain explicit sensitivity labels.

## 7. How improvement will be measured

Use staged ablations with the same tasks, agent/model versions, tools unrelated to this change, resource budgets, and environment policy:

| Variant | Purpose |
| --- | --- |
| A: existing ShadowDroid + existing skill | Baseline for incremental product benefit |
| B: existing binary + improved skill | Isolate the effect of guidance |
| C: B + ledger/test reports/lifecycle/matrix | Measure the first release's incremental benefit |
| D: C + one additional adapter at a time | Attribute gains and costs to visual, SQL, constraints, or boundary work |
| Optional E: ordinary Android tools without ShadowDroid | Measure total product benefit separately from feature uplift |

Use five independent attempts per task as an initial experimental design, then choose any expansion before reviewing comparative outcomes. Pair task/environment/fixture seeds where possible and randomize variant order. Model sampling remains stochastic; fixed fixture seeds do not make model runs deterministic. Record model revision, harness prompt, skill/catalog hashes, evaluator version, and all enabled tools.

Choose task count and uncertainty analysis from the M0 baseline and desired detectable improvement. Five attempts on a few tasks are a pilot, not sufficient evidence for a broad effectiveness claim. Include multiple independent applications before generalizing beyond the sample app.

Analyze task-level paired outcomes and uncertainty, clustered by task rather than pretending repeated runs of one task are independent task samples. Reserve held-out tasks for milestone decisions. Avoid changing fixtures, rubrics, or budgets after seeing which variant benefits. If a baseline run was skipped because of infrastructure, preserve that record and apply a predeclared rerun rule to all variants.

| Metric | Definition / use | Target |
| --- | --- | --- |
| Task pass rate | Independently accepted complete tasks divided by all attempted runs; also break down infrastructure failures | Baseline + [X] percentage points |
| Requirement completion | Independently satisfied applicable requirements; show dimensions and denominators | Baseline + [X] |
| False completion | Agent claims done while independent acceptance still fails | At most [X] |
| Regression rate | Previously passing applicable checks broken by the candidate | At most [X] |
| False pass/false alarm | Verification-tool result disagrees with independently known fixture outcome | No false passes in mandatory fixture suite; broader rates reported |
| Cost per successful task | All attempt costs, including failed attempts, divided by accepted successes; report spend and successes separately if zero | At most [X] |
| Latency and context use | End-to-end task time, tool time, model tokens, and report sizes | At most [X] overhead for comparable outcomes |
| Recovery reliability | Interrupted/failed runs restoring owned settings and retaining usable evidence | At least [X] |
| Coverage honesty | Required stale/missing/blocked observations incorrectly counted as passed | Zero in mandatory contract fixtures |
| Concurrent throughput and isolation | Accepted work per wall-clock time, total compute, queue/idle time, subscriber overhead, cross-owner writes | Baseline + [X] throughput; zero cross-owner writes in mandatory fixtures |

Calibrate every [X] after M0 and before feature-comparison results are examined. Store the selected thresholds with the experiment definition. Do not assign an official Android Bench score to this local suite. If official task access becomes available, document allowed tools, visibility boundaries, and harness integration before running the same comparisons there.

## 8. First release boundary

The M2 release candidate includes E01–E06, E13/C0–C1 and the applicable E12 packaging work. Its end-to-end demonstration is:

1. An agent reads a small Android task and writes a plan preserving all explicit requirements.
2. ShadowDroid validates the plan and captures existing test results.
3. The agent builds/installs a candidate with recorded source-to-artifact provenance.
4. ShadowDroid drives declared primary and secondary journeys and runs the selected lifecycle/configuration checks.
5. A missing route and a state-loss defect appear as separate unresolved requirements with evidence.
6. After a fix/rebuild, old results are stale; affected checks must run again.
7. Independent app checks accept the repaired behavior; interruption/cleanup checks also pass.
8. A fresh agent session can read the saved report and continue without reconstructing the investigation.
9. A second agent reviews the same immutable evidence and receives its own crash events without changing device state; a conflicting driver is queued or rejected. A controlled handoff invalidates the former driver's authority, and an independent device continues concurrently.

SQL, model-based visual judging, platform adapters, deeper Compose inspection, and a new debugger are outside this first release. Visual artifacts may still be captured and reviewed by the agent using existing tools. Completion is scoped to the checks actually implemented and executed.

## 9. Risks and decision gates

| Risk / open decision | Resolution owner area | Gate and response |
| --- | --- | --- |
| Tool mostly adds overhead without improving outcomes | Evaluation | Compare B and C; simplify guidance/orchestration if benefit is not demonstrated |
| Headless activity recreation cannot be proven externally | Lifecycle | M0 backend spike; advertise supported transitions and require an optional test adapter for the rest |
| Verification becomes a competing test framework | CLI architecture | Keep vocabulary bounded and delegate custom logic to external tests |
| Ledger certifies an incomplete agent-authored plan | Plan/skill | Preserve task text, expose unmapped requirements, review plan completeness independently |
| Wrong installed build receives fresh-looking evidence | Artifact provenance | Source/APK association required for claims about edited code; otherwise unresolved provenance |
| Settings/proxy changes leak between runs | Coordinator | Journal owned mutations; interrupted/contended cleanup fixtures block release |
| Project-level locks are mistaken for agent isolation | E13 coordinator | Canonical device leases cover named targets, direct serials and transitive effects |
| Expired owner or surviving child changes the next experiment | E13 recovery | Enforce fencing and drain admitted work; unresolved execution blocks reassignment |
| An observer consumes another agent's crash or repairs its server | E13 subscriptions | Independent cursors and mechanically passive observer paths |
| Database snapshot is inconsistent or requires stopping the app | Storage | Explicit consistency mode and effects; keep it out of transient-state experiments |
| Visual judge produces confident but unstable results | Visual | Calibrate on correct/defective pairs; retain deterministic/subjective distinction and uncertainty |
| Agent optimizes for development fixtures | Evaluation | Independent held-out coding tasks and fixed acceptance evaluators |
| Parsing depends on one Gradle/Android report format | Test adapters | Versioned parser fixtures, raw evidence retention, typed unsupported outcomes |
| An optional inspector changes the app under test | Inspection | Declare instrumentation/build identity; test the intended candidate and record adapter effects |
| Roadmap commands are mistaken for shipped behavior | Documentation | Proposed-status labels; actual catalog and current guides remain authoritative |

## 10. First implementation backlog

These are implementation-sized starting slices, not completed work or created issue-tracker tickets. Keep code, meaningful regression coverage, catalog/skill updates, and both documentation surfaces together in each delivered slice.

| Order | Deliverable | Depends on | Reviewable proof |
| --- | --- | --- | --- |
| 1 | Evaluation specification and baseline runner | None | One complete baseline artifact with independent acceptance |
| 2 | Lifecycle/backend, theme-restoration and E13/C0 concurrency spikes | None | Capability/resource table, cursor reproduction and ownership design |
| 3 | Skill-only verification guide experiment | 1 | A/B result using the unchanged binary |
| 4 | Plan/result schema, validator, offline ledger/report | 1 | Missing/unmapped/stale evidence cases and valid report examples |
| 5 | JUnit ingestion and baseline comparison | 4 | Real project run plus empty/stale/failed-report fixtures |
| 6 | E13/C1 runtime authority, independent observers and E03 coordinator integration | 2, 4 | Competing/expired owners, crash subscribers, late commands and interrupted emulator journeys |
| 7 | First lifecycle journey and state assertions | 2, 4, 6 | Defective/repaired form-state app pair |
| 8 | Theme/orientation matrix with exact restoration | 2, 6, 7 | Dark-theme failure plus cancellation restoration |
| 9 | Headless end-to-end concurrent-agent journeys and revised guide | 3, 5, 7, 8 | C-versus-B comparison, driver/reviewer handoff and independent-device throughput |
| 10 | First release-candidate review | 9 | Contract, emulator, packaging, docs, and evaluation gates |

Once M2 is measured, choose E07/E08/E09 ordering from the actual residual failures. Start a platform adapter only with an identified task class, a feasible evidence source, and an independent defective/repaired fixture. Update the roadmap's status per milestone with links to implementation and validation; never convert a proposed item to shipped based on design completion alone.
