# Requirement verification

The `verify` namespace connects task requirements with evidence. The first implementation chunk provides offline plan validation, JUnit normalization, and baseline comparison. These commands do not connect to Android or execute tests. The [roadmap](agent-verification-roadmap.md) describes the remaining live execution, lifecycle and concurrency work.

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
