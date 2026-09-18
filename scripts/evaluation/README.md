# Coding-agent evaluation

This is an operator-run experiment harness, separate from the deterministic
`verify` command. It never treats an agent's own plan or completion claim as
independent acceptance. Do not describe results as Android Bench scores.

`prepare_smoke.py` freezes an Android source task, evaluator, same CLI binary,
existing guidance and improved guidance into a private directory. The task has
three defects (draft recreation, theme label, missing detail route) and an
existing editing/title regression contract. Independent instrumentation uses
random input, actual activity recreation and Android night configuration.
Only the declared Java file is copied to the evaluator's frozen build, so agent
changes to local tests or build scripts cannot change the acceptance tests.

```bash
python3 scripts/evaluation/prepare_smoke.py --cli cli/target/debug/shadowdroid \
  --device emulator-5556 --out /tmp/frozen-smoke
python3 scripts/evaluation/run.py /tmp/frozen-smoke/smoke.json \
  --out /tmp/smoke-attempts
```

Use a disposable, booted emulator, installed Android SDK 36, the project's
supported JDK in `JAVA_HOME`, working local Codex authentication, Python 3.11+,
and a POSIX host. The generated configuration runs local `codex exec` with
workspace-write sandboxing, JSONL events, no interactive approvals, and the
user-authorized two-minute agent budget. It uses the CLI's default model and
records that limitation, CLI version, runner argv, binary/guide/template/test
hashes, prompts, seed, schedule and host before the first attempt. The exact
server model revision may not be exposed. See [Codex non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode).

Every attempt retains stdout/stderr, timing, child CPU, tool-call count, available
token usage, candidate patch, completion claim, handoff and independent result.
Unknown token usage and unconfigured spend remain null, never zero. Timeouts and
infrastructure failures remain in the denominator; reruns are disabled. The
harness terminates process groups before judging and stops on surviving groups
or changed frozen inputs. Detached daemons are outside this group guarantee;
use disposable containers/workers for untrusted runners and inspect recovery.

The JSON configuration supports multiple templates, explicit requirement IDs,
variants, runner/evaluator argv, time budgets and a seeded randomized schedule.
Acceptance needs all expected IDs exactly once and passed, plus successful
evaluator execution. Missing/skipped/duplicate evidence cannot pass. Evaluator
exit and agent completion are separate. For a larger study, predeclare at least
five attempts per task/variant, independent applications, heldout task split,
resource limits and thresholds before outcomes are reviewed. Compare by task,
include all failed attempts, and use uncertainty clustered by task. The bundled
one-task/one-attempt-per-variant smoke cannot estimate a reliable effect size.

## Headless and concurrency boundary

The `android-verification` CI job supplies the headless Linux contract suite:
pinned Rust/JDK, CLI and APKs from the same checkout, API 36 emulator, explicit
serial, private evidence directory, lifecycle/SQL/matrix/visual/platform fixtures
and artifact upload on failure. Cold build/setup and warm journey timing are
separate logs. Run agent experiments only when authentication and spend are
explicitly configured; CI does not silently launch billable model trials.

For comparative trials use one candidate/build directory and one device per
writer, separate backend/test-account namespaces, and the shared ShadowDroid
authority for cooperating driver/observer agents. Keep acceptance tests and
outputs outside the coding workspace. The local smoke separation is procedural:
workspace-write limits edits but is not a hidden-evaluator read sandbox. Real
holdout evaluation needs separate containers/users and evaluator-only mounts;
never claim local prompt instructions provide that isolation. Give a dedicated
ADB server/device endpoint to a container and pin CLI/server APK checksums from
the same candidate; do not share host credentials or production app data.

Camera/Wear, semantic visual judges, multi-host leases, scoped helper writes and
new headless inspection backends remain conditional follow-on experiments.
