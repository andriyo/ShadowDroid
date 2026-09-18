# Verification implementation and validation

The September 2026 implementation is experimental and locally committed. It
adds deterministic tooling and evaluation infrastructure; no official Android
Bench score or improvement claim is made. The methodology's task set is private.
The expanded scope includes the later SQLite, visual, migration and platform
adapters requested after the original M2 release boundary.

## Implemented scope

| Work package | Delivered | Evidence and limits |
| --- | --- | --- |
| E01 | Frozen experiment runner, operator-owned Android evaluator, all-attempt records, randomized order and resource metadata | Small local Codex A/B smoke; independent evaluator first rejected three seeded defects and accepted the repaired app. Larger multi-app/holdout study remains needed. |
| E02 | Compiled opt-in verification guide and generated-skill pointer | Catalog/skill tests and customized-skill preservation; no user-scope skill installation. |
| E03–E04 | Plans, requirement coverage, dependencies, durable evidence, freshness, JUnit directories and baseline regression comparison | CLI integration tests reject omitted, missing, stale, empty, skipped, truncated and tampered evidence; interrupted runs require recovery. |
| E05–E06 | Guarded journeys, three distinct lifecycle experiments, per-cell configuration restoration | Real recreation identifier, state preservation, saved task/PID restoration, secondary route, dark/large-font/rotation matrix, interruption and restoration surviving reboot. App-enforced orientation can block an experiment. |
| E07 | Quiescent SQLite/Room snapshots and engine-restricted bounded queries | Real UI-only fake save fails while randomized persisted value passes; WAL, denied SQL and read-only limits have engine tests. Encrypted/live databases are unsupported. |
| E08 | Configuration-bound image comparison/review bundles and possible accessibility findings | Correct rendering passes; seeded theme defect fails; viewport mismatch blocks. Pixel contracts and tree findings are not a semantic visual or complete accessibility judge. |
| E09 | Fresh build/install chain, APK content identity, source constraints, resolved Gradle dependencies | Real clean sample build is bound to a UI check; an intentionally wrong resolved version fails. Source patterns remain heuristics; split installation is unsupported. |
| E10 | Named instrumentation contracts with API bounds and observation type | Real intent extras, MediaSession release callback, hosted widget second update and PiP entry: four correct passes and four seeded failures on API 36. No physical camera/Wear claim. |
| E13 C0/C1 | Shared local authority, exclusive drivers, passive observers, independent cursors, handoff, quarantine/recovery and source build ownership | Independent processes, second registry rejection, same crash to two subscribers, stale-owner rejection, UiAutomation handoff, reboot recovery and an independent second device. Direct ADB/HTTP/IDE clients bypass the cooperative fence. |

C1 uses fail-fast conflicts and bounded OS-lock waiting, non-expiring ownership,
and explicit handoff/recovery. Fair fleet queues, capability subleases, backend
reservations and multi-host coordination belong to C2/C3. Server endpoint
credentials do not fence arbitrary legacy/direct clients. This is a deliberate
local cooperation boundary, not distributed security enforcement.

## Reproduce and inspect

The [verification guide](verification.md) lists adapter syntax and E2E commands.
The [session guide](sessions.md) describes coordination/recovery. Run
`scripts/e2e-agent-sessions.py` with an explicitly selected disposable emulator
and optional independent `--peer` to reproduce isolation checks. The
[evaluation harness](../scripts/evaluation/README.md) freezes coding-agent trials
and keeps acceptance separate from the candidate workspace.

Local validation uses macOS, Rust 1.97, JDK 23 and the dedicated API 36 emulator
`ShadowDroid_Reliability_API36`; concurrency also exercised an independent API 36
TV emulator. The pre-existing real Compose/Views/WebView/DPAD/accessibility
surface suite passed. API 36 evidence does not establish every supported Android
version, physical-device behavior or every launcher.

The `android-verification` GitHub Actions job runs the headless Linux fixture
suite using artifacts from the same checkout and uploads failures. It is wired
but has not been executed remotely by this local task. Publication, installer
updates and official benchmark access are separate gates; nothing was pushed or
published by this implementation task.

## Local results (2026-09-18)

- Rust: 695 tests across unit and integration targets; strict Clippy, formatting,
  Rust 1.91 minimum-version check and Cargo package verification passed.
- Android: sample lint and seven unit tests passed; the final two-device session
  suite passed all eight listed coordination contracts.
- Python harness: missing/duplicate/skipped acceptance and timeout cleanup tests
  passed. Companion docs: 17 files, 190 local links, 54 routes, 118 CLI help
  invocations, zero errors.
- Lifecycle/SQLite/matrix/recovery, build/dependency, visual and platform fixture
  suites passed their expected positive and negative outcomes. Platform suite:
  four positive passes, four seeded failures. Each run preserves raw evidence.
- Independent evaluator preflight: broken app failed draft, theme and route while
  its regression passed; repaired app passed all four.

| Frozen smoke variant | Agent wall time | Agent terminal status | Independent acceptance | Tool calls |
| --- | --- | --- | --- | --- |
| A: existing guide, same binary | 120.26 s | Timed out; no final completion claim | 4/4 requirements passed | 24 |
| B: improved guide, same binary | 120.27 s | Timed out; no final completion claim | 4/4 requirements passed | 21 |

Both attempts are retained, including timeout status and incomplete token usage.
Independent evaluation took 22.05/21.08 seconds after the agent budget. Codex CLI
was 0.136.0 using its default model selection; its model-list refresh emitted a
compatibility warning, so the exact server model revision is not established.
The sandbox blocked Gradle socket setup during agent verification. Operator
instrumentation subsequently built and tested each submitted source independently.
Token spend and financial cost are unknown, not zero. One task and one attempt
per guide, shared caches and host resources, procedural evaluator separation and
these runner limits do not support any measured uplift or broad effectiveness
claim. The useful result is that timeout patches remain reviewable and are
judged separately from agent completion.
