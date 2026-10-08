# ShadowDroid — Standalone Debugger Design (`debug --backend jdwp`)

> Status: **IMPLEMENTED through P1c** (2026-10-08; P0 and P1b are on main,
> P1c on branch `feat/jdwp-debugger-p1c`). The standalone backend lives in
> [cli/src/jdwp/](../cli/src/jdwp); the Studio plugin bridge
> ([cmd/debugger.rs](../cli/src/cmd/debugger.rs),
> [shadowdroid-plugin](../shadowdroid-plugin)) remains the other backend.
> This document supersedes the E11 decision in the
> [agent verification roadmap](agent-verification-roadmap.md) that declined to
> build a JDWP stack. User-facing docs:
> [debugging.md](debugging.md#standalone-debugger-no-android-studio).

> **What ships (P0–P1c).** `attach|detach|sessions|status`; `break
> line|exception|method|field|update|remove` with conditions, pass counts
> (native Count, reported `expired`), temporary/disabled, suspend policy;
> `logpoint add|list|events|follow|remove|clear` with Studio's cursor
> contract; `pause|resume|step-*|continue-until`, `stack|threads|variables|
> eval|inspect`, handles; `watch add|list|remove|clear`; `coroutines
> snapshot|threads|continuation|flow`; the composed `auto [--from-start]`,
> `snapshot`, `run-until-crash`, `step-until-log`, `step-until-screen-change`;
> launch-time breakpoints via `attach --wait-for-launch --break`.
> Measured behaviour that shaped the design, each a change from the sections
> below:
>
> - **Invoke tier** (`--invoke` on eval/inspect/conditions/logpoints/
>   `break update`): INVOKE_SINGLE_THREADED, 1.7–3.1 ms per call on the
>   emulator; property sugar `x` → `getX()`/`isX()` only with `--invoke`;
>   a thrown exception is a result (`thrown: {type, message}`), not an error.
>   Every daemon-owned breakpoint and step request is cleared for the call and
>   re-armed after; events during it resume their thread unrecorded. ART
>   refuses invokes on a thread stopped by `debug pause` (error 10), so those
>   are rejected up front (`invoke_requires_event_stop`). A call past its
>   deadline returns `invoke_timeout` and marks the thread busy until the late
>   reply (frame reads refuse `thread_busy_invoking`).
> - **Method breakpoints** are line breakpoints at the first line and at every
>   return (DEX return opcodes from Method.Bytecodes), never
>   MethodEntry/MethodExit, which deoptimize the whole app.
> - **Field breakpoints** default to the Kotlin accessors' lines; a real field
>   watch needs `--accept-slowdown`, warns on every response, and auto-clears
>   after `--duration-ms` (60 s); `status.slow_requests` counts armed ones.
> - **Lambdas**: a location is a lambda when its method contains `$lambda$`
>   (depth = count), or is `invoke`/`invokeSuspend` of a Lambda/SuspendLambda/
>   function-reference class; bridges are skipped. `--variant
>   all|outer|lambda` (lambda = deepest).
> - **step-until-screen-change** runs `mode: "run_to_frame"`
>   ([§5.5](#55-stepping-and-threads)).
> - **Coroutines**: `snapshot` also discovers coroutines process-wide with
>   ReferenceType.Instances (capped at 100 per class, class discovery cached
>   per session); source lines are not read (they need an invoke) and the
>   response points at `aar coroutines`.
> - **Backend policy** ([§4.4](#44-backend-selection)): `auto` resolves per
>   command and reports `backend_reason`.
> - Earlier decisions stand: flat `debug_backend` config key, unix hosts
>   only, `--uncaught` meaning "not caught by app code", keep-alive Version
>   pings during wait-for-launch, suspending hits capped at 20/s by default
>   (ceiling 100/s), ANR warning after 4 s for attach-to-running sessions.
>
> Not done: `debug record`, `debug native`/mixed mode, and `debug clients`
> (Studio only); the doctor check; SMAP inline-body resolution; coroutine
> source lines; `step-until-screen-change --stop-at frame`; Windows hosts.

Design and phased plan for a debugger that needs **no Android Studio**: the CLI
speaks JDWP to the app itself, through the same adb connection it already owns,
and keeps the existing `debug` verbs and JSON contract.

---

## 1. Thesis

Today ShadowDroid's debugger is a JSON front over Studio's debugger. That buys
four things from the IDE:

1. the JDWP transport and the JDI object model;
2. source-to-bytecode breakpoint resolution with Kotlin awareness;
3. expression evaluation;
4. value renderers.

The bridge already narrows item 3 to **deterministic path expressions**
(`this`, locals, fields, array indexes). That was a safety decision, but it
also removes the one component that genuinely needs an IDE: a Kotlin fragment
compiler. What remains is protocol work, class/line lookup, and rendering,
all of which fit the single Rust binary the way the MITM proxy did
([net-proxy-plan](../../ShadowDroid-docs/net-proxy-plan.md)).

Three commitments shape the design:

1. **Same verbs, same JSON.** `debug attach/break/step/stack/variables/eval/...`
   keep their names, flags, ids, and envelopes. A `backend` field appears in
   every response. Agents and the generated skill do not learn a second
   debugger.
2. **A daemon owns the socket.** JDWP is a stateful connection; CLI calls are
   stateless. A per-process `debugd` holds the session and answers RPCs over a
   unix socket, exactly like the proxy daemon in
   [net/daemon.rs](../cli/src/net/daemon.rs) and [net/control.rs](../cli/src/net/control.rs).
3. **Studio stays an equal backend.** Native/mixed mode, Layout Inspector,
   Compose recomposition counts, and IDE-compiled expressions remain Studio
   features. `--backend auto` picks whichever debugger already holds the
   process.

---

## 2. Goals / non-goals

**Goals**

- Attach, breakpoints (line, exception, method, field), logpoints with the
  structured hit stream, pause/resume/step, stack/threads/variables, path
  evaluation and object handles, watches, continue-until, run-until-crash and
  the step-until helpers, coroutine continuation reads: all without Studio.
- Catch code that runs **before the first frame** (`Application.onCreate`,
  first activity) through `am set-debug-app -w`. Studio's attach-to-running
  flow cannot do this through the bridge today.
- Work on the headless emulator in CI, where no IDE exists.
- Keep the bounded, non-blocking contract: a read without a suspended frame
  returns structured `ok:false`, never hangs.
- Per-serial scoping so concurrent sessions on different devices do not share
  a debugger ([sessions](sessions.md), [concurrency roadmap](concurrency-roadmap.md)).

**Non-goals (for this design)**

- Native / LLDB debugging. `debug native` and `debug tombstones` are unchanged.
- Layout Inspector replacement. `layout recompositions` and `layout source`
  keep requiring the Studio bridge; a headless path through the app inspection
  transport is a separate workstream.
- Arbitrary Kotlin/Java expression compilation. The path grammar stays; a
  bounded, opt-in method invocation tier is specified in [§5.6](#56-evaluation-and-handles).
- Hot code replacement, Apply Changes, class redefinition.
- Debugging release or non-debuggable builds. JDWP requires `debuggable=true`
  or a `ro.debuggable=1` image, same as Studio.

---

## 3. Options considered

| Option | Shape | Verdict |
| --- | --- | --- |
| **A. Own JDWP client in Rust** | Speak JDWP over the in-tree ADB wire `jdwp:<pid>` stream | **Chosen.** Full control of contract and failure modes; no new runtime on the host. |
| B. On-device JDWP client (server APK) | Kotlin client inside the ShadowDroid server | Not viable. On Android the app's JDWP endpoint is reachable only through `adbd`; another on-device process has no path to it. |
| C. In-app AAR "cooperative breakpoints" | ASM-injected checkpoints that park a thread when armed | No locals without pervasive instrumentation, no stepping, debug-variant-only. The AAR keeps its current role (OkHttp capture, coroutine dumps). |
| D. JVMTI agent via `am attach-agent` | Native `.so` pushed into the app's `code_cache`, exposing a control socket | Strong second phase for things JDWP cannot do (field watch breadth, heap queries, retransformation). Too much native risk inside the app process to be the foundation. |
| E. Sidecar JDI or DAP server on a JVM | Bundle a JDI daemon or reuse `kotlin-debug-adapter` | Fast prototype, but adds a JVM dependency, a lossy protocol hop, and weak Kotlin fidelity in existing adapters. |

A is the foundation; D is an accelerator gated on measured need ([§8](#8-phase-2-jvmti-agent-conditional)).

---

## 4. Architecture

```
 shadowdroid debug <verb>  ──unix socket──▶  shadowdroid-debugd (one per attached pid)
      (stateless CLI)                             │
                                                  │ JDWP over adb:
                                                  │   host:transport:<serial>  →  jdwp:<pid>
                                                  ▼
                                            adb server ─▶ adbd ─▶ app process (libjdwp)
```

### 4.1 Transport

The in-tree ADB client ([device/adb_wire.rs](../cli/src/device/adb_wire.rs))
already opens `host:transport:<serial>` streams. JDWP adds one device service,
`jdwp:<pid>`, which adbd connects to the app's debugger socket. No TCP forward,
no host port allocation, nothing for [portmap.rs](../cli/src/device/portmap.rs)
to reconcile. Pid discovery uses the `jdwp` / `track-jdwp` services, which list
debuggable pids; names come from `/proc/<pid>/cmdline` via shell.

Fallback when the wire path fails on an unusual adb build:
`adb forward tcp:0 jdwp:<pid>` and a plain TCP connection, removed on detach.

### 4.2 Daemon

`shadowdroid-debugd` is the same binary (`shadowdroid __debugd`, hidden), one
instance per attached process:

- Registry: `~/.shadowdroid/debug/<serial>/<pid>.json` (pid of the daemon,
  socket path, package, attach time, startup id). `debug sessions` enumerates
  registries, probes liveness, and prunes stale entries, mirroring the net
  daemon's `await_ready` / log-tail pattern.
- Control: JSON-RPC over a `0600` unix socket. Each CLI verb is one request.
  Long waits (`continue-until`, `run-until-crash`, `logpoint follow`) are
  server-side long-polls with the caller's `--timeout-ms`.
- Lifetime: exits on `debug detach`, on JDWP EOF (process died), or after an
  idle timeout with no breakpoints and no suspended threads (default 30 min,
  configurable). A crash of one daemon never affects another device's session.
- Log: `~/.shadowdroid/debug/<serial>/<pid>.log`, tailed into attach failures
  the way `net start` tails the proxy log.

### 4.3 JDWP core

A hand-rolled codec, like the proxy's HTTP stack. Required command sets:

| Set | Commands used |
| --- | --- |
| VirtualMachine (1) | Version, ClassesBySignature, AllClassesWithGeneric, AllThreads, IDSizes, Suspend, Resume, Dispose, CapabilitiesNew, CreateString (invoke tier only) |
| ReferenceType (2) | Signature, SourceFile, MethodsWithGeneric, FieldsWithGeneric, GetValues (statics), SourceDebugExtension, Status, Interfaces, ClassLoader |
| ClassType (3) | Superclass; InvokeMethod only in the opt-in tier |
| Method (6) | LineTable, VariableTableWithGeneric |
| ObjectReference (9) | ReferenceType, GetValues, DisableCollection, EnableCollection, IsCollected; InvokeMethod only in the opt-in tier |
| StringReference (10) | Value |
| ThreadReference (11) | Name, Suspend, Resume, Status, Frames, FrameCount, SuspendCount, ThreadGroup |
| ArrayReference (13) | Length, GetValues |
| EventRequest (15) | Set, Clear, ClearAllBreakpoints |
| StackFrame (16) | GetValues, ThisObject |
| Event (64) | Composite |

Event kinds: Breakpoint, Step, Exception, MethodEntry, MethodExit,
FieldAccess, FieldModification, ClassPrepare, ThreadStart/Death, VMDeath.

One reader task demultiplexes replies by packet id and pushes Composite events
into the session state machine. Every request carries a deadline; a missing
reply surfaces as `debugger_timeout` with the pending command named.

**Baseline: API 28 and newer.** From Android 9 ART loads OpenJDK's `libjdwp`
through its JVMTI implementation, so command coverage and error codes match
the JDWP specification. Older ART shipped its own JDWP server with different
gaps; it is detected and reported as `unsupported_api_level`, not worked around.

### 4.4 Backend selection

`debug --backend auto|studio|jdwp`, config key `debug.backend` (default
`auto`). JDWP attach is **exclusive per process**: Studio and ShadowDroid cannot
both hold the same pid. `auto` therefore follows the process:

1. a live `debugd` registry for the target (the `--pid`/`--package` when the
   verb names one, else any live session on the device) → `jdwp`;
2. else a reachable Studio bridge (a TCP connect, so a busy IDE still
   counts) → `studio`;
3. else `jdwp`.

Studio cannot report that ShadowDroid holds a pid (it shows
`debugger_attached=false`), so the registry is the only signal for rule 1,
and rule 2 does not ask Studio about the pid. Verbs or options only one
backend serves skip the probes: `record`, `native`, `clients`,
`attach --dialog`, and `--mode native|mixed` go to Studio (keeping Studio's
errors when it is not running); `--wait-for-launch`, `--break*`,
`--from-start`, `--invoke`, `--variant outer|lambda`, and
`--accept-slowdown` go to jdwp. Results and errors of an `auto` decision
carry `backend_reason`: `jdwp_session_holds_target`,
`studio_bridge_reachable`, `studio_bridge_unreachable`, `studio_only_verb`,
`studio_only_option`, or `jdwp_only_option`. An explicit `--backend` (or
config `debug_backend`) carries none.

An attach that fails because the other debugger holds the process returns
`debugger_already_attached` naming the holder and the two ways out
(`debug detach`, or detach in Studio). Every response carries
`"backend": "jdwp" | "studio"`; `debug status` reports both backends'
availability.

---

## 5. Behaviours

### 5.1 Attach and launch

- `debug attach [--pid|--pkg]` resolves the process as today (alias → package →
  process name, with the existing ambiguity error listing candidates), spawns
  `debugd`, performs the `JDWP-Handshake`, reads IDSizes/CapabilitiesNew, and
  returns the session id. The VM is **not** suspended on attach.
- `debug attach --wait-for-launch` and `debug auto --from-start` set
  `am set-debug-app -w <pkg>` (plus `--persistent` only for the duration of
  the command), launch, attach to the pid that appears in `track-jdwp`, then
  issue VirtualMachine.Resume and `am clear-debug-app`. Breakpoints queued
  before launch bind through ClassPrepare ([§5.2](#52-breakpoint-resolution)).
- `debug detach` disposes the VM connection (which clears all event requests
  and resumes the app), removes the registry, and exits the daemon. A dying
  daemon takes the same path from a `Drop` guard so an app is never left
  suspended by a host crash.

### 5.2 Breakpoint resolution

Input is `--file <path | name | suffix> --line N`, as today. Resolution without
an IDE:

1. **Locate the source file** in the project source index already built for
   crash frame mapping (`crashscan::project_frames`, `--project-root`, the
   `source_roots` config). Ambiguous suffixes return candidates, as the Studio
   path does.
2. **Read the `package` declaration** from the file (one regex, both languages).
3. **Candidate classes** = loaded classes whose signature starts with the
   package and whose `SourceFile` equals the file's basename. This naturally
   includes Kotlin file facades (`FooKt`), nested/inner classes, and lambda
   classes (`Foo$bar$1`), all of which keep the outer `SourceFile`.
4. **Locations** = every method whose `LineTable` contains N. A line that
   lives in a lambda and in its enclosing method yields several locations; all
   are set and reported in `locations[]`, one breakpoint id.
5. **Deferred binding.** If nothing is loaded yet, register a ClassPrepare
   request with a `ClassMatch` on `<package>.*` and bind on prepare. The
   breakpoint reports `bound: false` with `pending_reason: class_not_loaded`
   until then, mirroring Studio's hollow-circle state.
6. **Kotlin inline functions.** `SourceDebugExtension` carries the JSR-45
   SMAP. Phase 1 parses it so that a line *inside* an inline function body is
   resolved to the synthetic line ranges in each caller class that inlined it.
   Phase 1a may ship with this reported as `unsupported_location: inline_body`;
   it is on the acceptance list for phase 1b.

The same lookup serves `continue-until --file/--line`, which sets a temporary
breakpoint exactly as it does now.

Exception breakpoints map to an Exception request with `ClassOnly` plus
caught/uncaught flags. Method breakpoints map to MethodEntry/MethodExit with a
`ClassMatch` and a daemon-side method-name filter (wildcards allowed, as today).
Field watchpoints map to FieldModification/FieldAccess on the backing field,
which also fixes the known gap where Kotlin properties never fired through the
Java breakpoint type.

### 5.3 Suspend policy, conditions, pass counts

- Suspend policy `all|thread|none` maps directly to JDWP `SUSPEND_ALL`,
  `SUSPEND_EVENT_THREAD`, `SUSPEND_NONE`.
- **Pass count** uses the native `Count` modifier; no host round-trip.
- **Conditions** are evaluated in the daemon. The request is set with
  `SUSPEND_EVENT_THREAD`; on hit the daemon evaluates the path expression in
  the top frame and resumes that thread when it is falsy. A condition that
  fails to evaluate is recorded as `last_evaluation_error` on the breakpoint
  and the thread is left **suspended**, matching the behaviour-policy fix in
  the plugin (an agent would rather find a paused app than a silently skipped
  breakpoint). No modal dialog exists to block anything.
- Truthiness: boolean `true`, non-null reference, non-zero number, non-empty
  string. Phase 1b adds `==`, `!=`, `<`, `>`, and string/number literals on
  the right-hand side. Still no method calls, assignments, or arithmetic.

Hot breakpoints are a real cost here: each conditional hit is an adb round
trip. The daemon tracks hits per second per breakpoint and, above
`max_events_per_second`, reports `throttled: true` with the drop count; it
does not silently remove the breakpoint.

### 5.4 Logpoints

`SUSPEND_EVENT_THREAD` → evaluate the log expression and optional condition →
resume. Events go into the same bounded ring with cursor and stream id that
`debug logpoint events/follow` already expose, so the CLI's paging and
`--after` semantics are unchanged. `--owner` scoping, `--max-message-chars`,
and `--max-events-per-second` keep their meaning.

### 5.5 Stepping and threads

- Step in/over/out → StepRequest `size=LINE`, `depth=INTO|OVER|OUT`, on the
  suspended thread, with a bounded wait for the Step event.
- Step-into filters (configurable, on by default): `ClassExclude` for
  `java.*`, `kotlin.*`, `kotlinx.coroutines.*`, `android.*`, `androidx.*`,
  `dalvik.*`, and classes without line tables. This is what stops
  "step into" from landing in `Intrinsics.checkNotNullParameter`.
- `step-until-log` loops over step-over exactly as it does now; only the
  step primitive changes.
- `step-until-screen-change` cannot work that way on jdwp: the screen only
  changes once the main thread returns to its message loop and draws, and
  the UI tree cannot be read while the main thread is suspended (every
  accessibility request times out, about 11 s per read). The jdwp verb
  therefore runs `mode: "run_to_frame"`: step out until the top frame is
  framework code (usually one step-out), hash a server screenshot with the
  status bar excluded, resume, poll that hash every ~120 ms (a device
  screenshot costs 9–33 ms while running), and pause as soon as it changes.
  The result keeps the Studio shape and adds `mode`, `step_outs`, `polls`,
  `ran_ms`, the position it started from, and the main thread's stack at the
  pause (normally idle in the message loop). A one-shot breakpoint on
  `Choreographer.doFrame` to stop inside the drawing frame is a possible
  refinement, not implemented.
- `threads` reports name, status, suspend count, and frames; the dispatcher
  hints used by `debug coroutines threads` are derived from thread names as
  today.

### 5.6 Evaluation and handles

- `variables`, `eval`, `inspect`, and `watch` use StackFrame.GetValues,
  ObjectReference.GetValues, and the array/string commands. The grammar is the
  existing deterministic path grammar.
- Kotlin hygiene: hide `$i$f$...` / `$i$a$...` inline markers, surface
  `$continuation` and spilled `L$n`/`I$n` slots under a `spilled` group so
  suspend-function locals are readable rather than invisible.
- **Handles.** `obj_<id>` handles are valid while the session stays suspended,
  as today. The daemon calls DisableCollection on handed-out objects and
  EnableCollection on resume, so a handle cannot silently point at a collected
  object between two CLI calls.
- **Renderers** (phase 1): `String`, boxed primitives, arrays, `ArrayList`,
  `LinkedList`, `HashMap`/`LinkedHashMap`, Kotlin data classes (fields),
  `StateFlow`/`MutableStateFlow` (`_state` value), `Pair`/`Triple`, enums,
  `Throwable` (message + cause chain). Everything else prints type, identity,
  and the first `--max-fields` fields.
- **Opt-in invoke tier:** `--invoke` on `eval`/`inspect` permits
  `toString()` and zero-argument getters through InvokeMethod with
  `INVOKE_SINGLE_THREADED`, each bounded by `--timeout-ms`. Off by default and
  declared as an effectful action in the command catalog.

### 5.7 Crash and ANR waits

`run-until-crash` registers an uncaught Exception request so Java crashes are
caught with the live frame **before** the process dies, in addition to the
logcat/tombstone/ANR scan it performs now. The snapshot therefore gains the
exception object, its message, and the throwing frame's locals.

### 5.8 Coroutines

`debug coroutines snapshot|threads|continuation|flow` read the same
continuation-shaped objects from suspended frames; the implementation moves
from JDI to the daemon's object reader. Whole-process dumps of a running app
remain `aar coroutines` ([agent docs](../../ShadowDroid-docs/agent.md)).

### 5.9 Multi-device and concurrency

One daemon per attached pid, registries keyed by serial. `--device` and
`--session` resolution rules from [sessions](sessions.md) and
[agent-debugging](../../ShadowDroid-docs/agent-debugging.md) apply unchanged. A
debugger attach counts as a device-level side effect under the E13 lease model:
the attaching session owns it, passive observers may read
`debug sessions`/`debug status` only.

### 5.10 Security and redaction

- No TCP listener on the host. Control is a `0600` unix socket under the
  user's `~/.shadowdroid`.
- Values flow through the existing redaction policy before they reach stdout
  or artifacts ([security](security-and-redaction.md)).
- The path grammar cannot call methods unless `--invoke` is given; the
  catalog marks the invoke tier as `effect: mutating`.

---

## 6. Command and config surface

No new top-level verbs. Additions:

| Surface | Change |
| --- | --- |
| `debug attach` | `--backend auto\|studio\|jdwp`, `--wait-for-launch` |
| `debug auto` | `--backend`, `--from-start` |
| `debug detach` | new; disposes the JDWP session (Studio backend: existing `stop`) |
| `debug status` | `backends: {studio: {...}, jdwp: {available, daemons[]}}` |
| `debug sessions` | lists JDWP daemons alongside Studio sessions, each with `backend` |
| `debug eval` / `inspect` | `--invoke` (opt-in method tier) |
| all `debug` responses | `backend` field |
| config | `debug.backend`, `debug.step_filters[]`, `debug.idle_timeout_ms` |
| `doctor` | `debug` check: adb jdwp service reachable, debuggable target, API ≥ 28, Studio bridge presence |

Error codes (added to the diagnostic catalog): `debugger_already_attached`,
`process_not_debuggable`, `unsupported_api_level`, `breakpoint_unresolved`,
`unsupported_location`, `debugger_timeout`, `daemon_unreachable`.

---

## 7. Hard edges

- **Exclusive attach.** Studio and `debugd` cannot share a pid. Detection is by
  attempt; the error names the holder. Quitting Studio while attached can kill
  the debuggee, a pre-existing hazard that now has an explicit alternative.
- **ART JDWP gaps.** `RedefineClasses`, `PopFrames`, `ForceEarlyReturn`, and
  `SetValues` on some slots are unsupported or unreliable; the design uses none
  of them in phase 1. Capabilities are read at attach and reflected in
  `debug status` so an agent can see what is available.
- **Source lookup needs a project root.** Without one, `break line` accepts
  `--class` + `--line` as a fully-qualified fallback.
- **Kotlin shapes.** Lambdas and `$default` methods multiply locations;
  inline bodies need SMAP; `suspend` locals are spilled. Each is handled as
  described above and each has an explicit `unsupported_location` reason when
  it is not.
- **Compose.** The Compose compiler emits ordinary line tables; breakpoints in
  composables work. Recomposition counters remain a Layout Inspector feature.
- **Host-side conditions are slower than in-VM ones.** Throttling reports the
  cost instead of hiding it. Pass counts are free.
- **Collection.** Handles pin objects via DisableCollection only while
  suspended; a long-held suspended session with many handles is an app-side
  memory cost and is capped (`max_live_handles`, default 512).
- **Emulator vs device.** `ro.debuggable=1` images (userdebug, most emulators)
  expose every process; production devices expose only `debuggable=true`
  apps. `doctor` says which case applies.

---

## 8. Phase 2: JVMTI agent (conditional)

A Rust `cdylib` with `Agent_OnAttach`, built for `arm64-v8a` and `x86_64`,
pushed with `run-as` into the app's `code_cache` and loaded through
`am attach-agent`. It talks to `debugd` over an abstract unix socket forwarded
by adb. Candidate capabilities, each gated on a measured need:

- field watches and method tracing without per-hit host round trips;
- `IterateOverInstancesOfClass` for "how many `FooViewModel` instances exist"
  style evidence (leak checks for E05);
- class retransformation at runtime. Worth re-testing first: the earlier spike
  recorded JDWP error 60, which is `INVALID_CLASS_FORMAT`, not
  `NOT_IMPLEMENTED`; ART's redefinition expects **DEX** bytes, not JVM class
  bytes. If retransformation works with DEX input, `DebugProbesKt` could be
  swapped at runtime and the build-time ASM step in `aar install
  --coroutine-probes` becomes optional.

Risks: native code inside the app process, per-ABI builds, SELinux and
`code_cache` permissions across OEMs. The agent must never be required for
any phase-1 verb.

---

## 9. Phased delivery

| Phase | Scope | Exit gate |
| --- | --- | --- |
| **P0 — Spike (≈2 days)** | Codec + handshake + daemon skeleton; attach to the sample app; `break line MainActivity.kt:68`; fire the known intent trigger; read locals and `this`; step over; resume; detach cleanly | All seven steps pass on the emulator; a written note of every JDWP/ART surprise |
| **P1a — Core parity** | attach/detach/sessions/status; line + exception breakpoints; pause/resume/step; stack/threads/variables/eval/inspect/handles; watches; renderers; backend selection; doctor check | Contract test: identical JSON shape for the same scenario on `studio` and `jdwp`; existing `debug` e2e script passes with `--backend jdwp` |
| **P1b — Breadth** | logpoints + hit stream; conditions with comparisons; pass counts; method and field breakpoints; `--wait-for-launch`; SMAP inline resolution; `run-until-crash` with live exception frame; coroutine verbs; redaction | Field Lab journeys for each; Kotlin-property watchpoint fires; startup breakpoint in `Application.onCreate` hits |
| **P1c — Release** | docs (README, debugging.md, agent-debugging.md, skill body), catalog effects, release contract script coverage, CI job on the headless emulator with `--backend jdwp` | Published claims match measured behaviour; Studio plugin remains optional and documented as such |
| **P2 — JVMTI (conditional)** | [§8](#8-phase-2-jvmti-agent-conditional) | Each capability earns inclusion through a fixture that the JDWP backend cannot satisfy |

Testing layers: proptest round-trips for the codec; a fake JDWP server in
`cli/tests` for session-state and failure paths (timeouts, EOF mid-suspend,
unknown event kinds); the live e2e script against `samples/shadowdroid-test-app`
following the `scripts/e2e-*.sh` conventions; a studio-vs-jdwp contract test.

Size estimate for P0–P1c, for scoping only: roughly 8–12k lines of Rust plus
tests, comparable to the existing `debug`/`debugger`/`studio` host code plus
the plugin's bridge.

---

## 10. Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Where the client lives | Host, in the CLI binary | JDWP is only reachable through adb; keeps the single-binary story |
| Transport | In-tree ADB wire `jdwp:<pid>` | No forwards, no port collisions, per-serial by construction |
| Session ownership | One daemon per attached pid | Crash isolation; same lifecycle pattern as the proxy daemon |
| Expression language | Keep the deterministic path grammar; opt-in invoke tier | Removes the need for an IDE compiler; keeps reads side-effect free by default |
| Condition evaluation | Daemon-side, thread-suspend | Correct and simple first; throttling makes the cost visible |
| Studio | Equal backend chosen by who holds the process | Native, Layout Inspector, and compiled expressions stay available |
| Minimum API | 28 | OpenJDK `libjdwp` under ART; coherent error codes |
| JVMTI | Phase 2, conditional | Native risk in-process; only for what JDWP cannot do |
| Verb surface | Unchanged names and ids | Agents and the skill keep one mental model |

## 11. Open questions for the spike

> Answered on the emulator on 2026-10-08; see [jdwp-spike-2026-10-08.md](jdwp-spike-2026-10-08.md) (probe scripts in `scripts/jdwp-spike/`). In short: Q1 yes, SourceNameMatch works despite the capability flag; Q2 pinning is required, unpinned ids are collected between hits; Q3 the transport works on adb 37 and is faster than a forward; Q4 MethodEntry is far too costly, use first-line breakpoints; Q5 `canRedefineClasses` is false, the route is closed.

1. Does ART honour the `SourceNameMatch` ClassPrepare modifier? If yes,
   deferred binding can filter by file instead of by package prefix.
2. Does `DisableCollection` behave on ART under memory pressure, or must
   handles be re-validated with `IsCollected` before each read?
3. Does the adb server accept `jdwp:<pid>` on a `host:transport` stream for
   every adb version ShadowDroid supports, or is the TCP-forward fallback
   needed in practice?
4. Does MethodEntry with `ClassMatch` cost enough on ART to require a
   different design for method breakpoints (for example a line breakpoint on
   the method's first line table entry)?
5. The error-60 retest for DEX-format redefinition ([§8](#8-phase-2-jvmti-agent-conditional)).
