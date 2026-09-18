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
