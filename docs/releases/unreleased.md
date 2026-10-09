# Unreleased

Draft notes for the next release. Everything below is on `main` and not yet
in a published version.

This release adds a debugger that works without Android Studio. An agent can
now attach to a debuggable app, stop it, read its state and step through it
using only the adb connection ShadowDroid already owns.

- **Standalone debugger:** the `debug` verbs talk JDWP to the app through
  adb, held by a small per-process daemon. Attach to a running app, or launch
  it under the debugger with `debug attach --wait-for-launch --break
  File.kt:LINE` so breakpoints are in place before the first line runs.
  Line, exception and method breakpoints support conditions, pass counts,
  temporary and disabled states, and lambda selection with `--variant`.
  Logpoints share the Studio event stream and cursors and are rate-limited.
  Stepping, stacks, threads, variables, `eval` and `inspect` with object
  handles work, as do watches, coroutine listing and crash capture with
  `run-until-crash`. See the "Standalone debugger" section of
  [debugging](../debugging.md) and the
  [design](../jdwp-debugger-design.md).
- **Method calls are opt-in:** `--invoke` lets `eval`, conditions, logpoints,
  watches and coroutine snapshots call methods such as `toString()`, getters
  and Kotlin properties. A thrown exception comes back as a result. Without
  the flag, nothing runs app code.
- **Property watches without slowing the app:** Kotlin properties are watched
  through their setter or getter. A field with no setter is watched at the
  instructions that write it, found in the app's dex files, so the app keeps
  full speed. A real field watch, which slows the whole app, needs
  `--accept-slowdown` and expires after `--duration-ms`.
- **ANRs:** a stop in an app that was already running can trigger Android's
  "not responding" dialog. The session reports it and suggests
  `debug attach --relaunch`, which restarts the app under the debugger with the
  same breakpoints. Apps launched under the debugger never raise ANRs.
- **Default backend:** with no flag, `debug` follows the process: a
  standalone session that holds the target answers; otherwise a running
  Android Studio answers; otherwise the standalone debugger. Every result
  carries `backend` and `backend_reason`. Pin a backend with `--backend` or
  config `debug_backend`. Android Studio remains the backend for native and
  mixed debugging and for Layout Inspector data.
- **Doctor:** an advisory `debugger` check reports whether the standalone
  debugger can run, whether the app is debuggable, who holds it, and which
  backend `auto` would pick. Missing Android Studio is reported as optional.
- **Testing:** a CI job runs the standalone debugger end to end on an API 36
  emulator (`scripts/e2e-jdwp-debugger.sh`).

Compatibility: `debug` results on the Studio path now carry `backend` and,
under `auto`, `backend_reason`. Without Android Studio running, `debug`
commands that used to fail now use the standalone debugger. On Windows the
standalone debugger is unavailable and `auto` stays on Studio.
