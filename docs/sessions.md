# Coordinating agents on Android devices

Use one driver per device. Advisers can read immutable artifacts or use `session observe`; separate devices can have independent drivers. Different packages, activities, screens or Git worktrees on one device do not isolate Android's foreground state, input, permissions, debugger or instrumentation slot.

## Reserve, observe, act, release

Select an already running device. Open a reservation with an agent label, then pass the returned opaque `session` value through the global `--session` flag or `SHADOWDROID_SESSION` environment variable on every driver command.

```bash
shadowdroid -d emulator-5554 session open --agent implementer
shadowdroid -d emulator-5554 --session TOKEN session observe --subscription implementer
shadowdroid -d emulator-5554 --session TOKEN ui dump
shadowdroid -d emulator-5554 session observe --subscription reviewer
shadowdroid -d emulator-5554 --session TOKEN session close
```

Tokens coordinate cooperating clients; they are not an authentication boundary against someone with ADB or filesystem access. The agent label is diagnostic, not ownership proof. A new driver must observe fresh state before its first ordinary command. Observation captures device identity, foreground, the existing server's screen if available, and crashes for that subscription. An unavailable screen remains unavailable; observation never installs, starts, repairs, or reconnects the server. Capture timestamps describe a window, not an atomic screen/device sample.

Use a unique subscription for every adviser. Reading one subscription does not advance another one's cursor. Crash reads are bounded logcat observations, not a durable device event archive; initialize the subscription before the experiment, and retain evidence for long investigations. Logcat rotation can lose older events.

Unreserved ordinary commands acquire a temporary per-command gate. A reservation extends ownership across an entire multi-command journey. Conflicting commands receive `device_reserved` or `device_operation_busy`. Locks are independent per selected device and are held throughout `test -- ...`, including when ShadowDroid releases Android's UiAutomation slot. A passive adviser cannot reconnect during that handoff.

## One shared authority

The default ownership directory is `~/.shadowdroid/authority`. All cooperating clients must use the same physical directory and a filesystem with reliable cross-process locks. `--authority-dir` / `SHADOWDROID_AUTHORITY_DIR` supports a shared mounted directory. Sharing copies of the directory does not share locks. The implementation supports this host's ADB server at `127.0.0.1:5037`.

A device-side marker binds a live reservation to the authority and selected serial. It rejects a competing private registry and another transport alias to the same device. A local owner index protects control of an owned proxy/recorder when Android is offline. Markers are removed on clean release; persistent OS lock files are never unlinked. Reservations include the observed boot ID and a monotonically increasing generation. Reusing a serial after reboot does not silently reuse a running session.

This is a cooperative authority for current CLI clients. Direct ADB, older clients, Android Studio and human input can act outside it. Do not run those writers during an experiment. Remote authorities, access control, scoped specialist grants and device pools remain later [concurrency work](concurrency-roadmap.md). C1 endpoint-level enforcement against arbitrary HTTP/ADB clients is not claimed by this local gate.

## Handoff and recovery

Stop the device's proxy and recorder before closing or handing off. Long-running writers must finish first. A handoff context JSON object should include the candidate/build, verification plan, backend/account namespace, restored or intentionally transferred settings, relevant evidence, and the recipient's starting point.

```bash
shadowdroid -d emulator-5554 --session TOKEN session handoff --agent verifier --context handoff.json
shadowdroid -d emulator-5554 session status
```

Handoff returns a new token, invalidates the previous token, retains the context and its hash, and requires the recipient to observe again. The caller is responsible for declaring application/backend starting state correctly; the context itself is not proof of cleanup.

Reservations never expire automatically. A killed CLI or ambiguous transport failure leaves its operation journal unresolved. Neither elapsed time nor a missing PID authorizes reassignment. `session status` retains the request, command, start time and prior boot identity. No action is replayed automatically.

While an operation is quarantined, the current owner may still run `net stop`, `video stop`, or `disconnect` for cleanup. These commands retain the original unknown request.

Recovery is deliberately conservative: inspect the unknown outcome, stop all related external test/build/ADB workers, reboot the selected device, then use the current owner's token with `session recover --external-workers-stopped`. The command verifies a changed boot identity and records the external-worker attestation; it cannot prove that arbitrary external processes were stopped. Observe again before continuing. The interrupted request remains `unknown`, even after recovery succeeds. Do not delete authority state or lock files to bypass this check.

The real-device [session suite](../scripts/e2e-agent-sessions.py) exercises competing drivers, passive observers, private-authority rejection, separate devices, independent crash cursors, handoff, instrumentation handoff and interrupted-process recovery. It intentionally crashes the sample app and reboots its explicitly selected dedicated emulator.
