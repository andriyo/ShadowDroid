# JDWP spike results (Phase 1)

Date: 2026-10-08. Target: `emulator-5554`, Pixel_9 AVD, API 36
(`google/sdk_gphone64_arm64/emu64a:16/BE2A.250530.026.F3/13894323:userdebug`), sample app
`io.github.andriyo.shadowdroid.sample` (debug build). Host adb: `Android Debug Bridge version 1.0.41,
Version 37.0.1-15733141` (host:version `0029`). Android Studio was running with the ShadowDroid
bridge, holding no debugger on the sample except during Q10b.

Client: `jdwp.py`, a stdlib-only Python 3 JDWP codec with a reader thread that demuxes replies by
packet id and flattens Composite events. Run each probe with `python3 -I <script>.py` from this
directory. Raw transcripts are in the `*.out` transcripts kept in the session scratchpad; scripts live in [scripts/jdwp-spike](../scripts/jdwp-spike).

| Script | Questions |
| --- | --- |
| `q1_q2.py` | 1, 2 |
| `spike.py` | 3 (acceptance), 4, 5, 6, 7, 10 |
| `q3_coldstart.py`, `q3_patterns.py` | 3 (firing, pattern semantics) + deferred binding (§5.2 step 5, §5.1 `--wait-for-launch`) |
| `q7b.py`, `q7c.py` | 7 (the step INTO "escape" and its cause) + Kotlin local and SMAP survey |
| `q8_methodentry.py` | 8 |
| `q9_q10.py`, `q10b.py` | 9, 10 |
| `q_errors.py` | Error-code reference (frames on a running thread, stale frame ids, bogus ids) |

---

## 1. Transport: `jdwp:<pid>` over `host:transport:<serial>` (VERIFIED)

- `host:transport:emulator-5554` then `jdwp:<pid>` returns `OKAY`. `JDWP-Handshake` echoes back
  and normal JDWP traffic follows. No unsolicited packets arrived (no DDM chunks, cmdset 199)
  on either transport.
- The `jdwp` device service (pid list) works on the same transport. It streams `%04x`-framed,
  newline-separated pids and keeps the stream open.
- `adb forward tcp:0 jdwp:<pid>` with a plain TCP connection also works and behaves identically:
  same Version and IDSizes, same exclusivity behaviour (Q10).
- Latency, warm (`q1_rerun.out`):

| Path | Open | Handshake | IDSizes | Version RTT min/med/max |
| --- | --- | --- | --- | --- |
| adb service | 2.4 ms | 2.3 ms | 1.6 ms | 0.72 / 0.94 / 1.36 ms |
| TCP forward | 0.3 ms | 8.5 ms | 3.5 ms | 1.04 / 1.62 / 3.18 ms |

  The adb service path is slightly faster per request, presumably because it skips the
  forward's extra hop.
- **Surprise:** the very first JDWP command after the first attach in a process lifetime
  (IDSizes) took 57.7 ms; it took 1.6 ms on later attaches. Most likely ART loads the JDWP
  agent lazily on first connect. Give the first request a deadline of seconds, not
  milliseconds.
- Only one adb version (37.0.1) was available, so "every adb version ShadowDroid supports" is
  **not verified**. Nothing here suggests the TCP fallback is needed on current platform-tools.

## 2. VirtualMachine.Version / IDSizes / CapabilitiesNew (VERIFIED)

Version: `description="Java Debug Wire Protocol (Reference Implementation) version 1.8\nJVM Debug
Interface version 1.2\nJVM version 8 (Dalvik, )"`, `jdwpMajor=1`, `jdwpMinor=8`, `vmVersion="8"`,
`vmName="Dalvik"`. This is OpenJDK libjdwp over ART's JVMTI, as §4.3 assumes.

IDSizes: field=8, method=8, object=8, referenceType=8, frame=8.

CapabilitiesNew:

| Capability | Value |
| --- | --- |
| canWatchFieldModification | **true** |
| canWatchFieldAccess | **true** |
| canGetBytecodes | **true** |
| canGetSyntheticAttribute | **true** |
| canGetOwnedMonitorInfo | **true** |
| canGetCurrentContendedMonitor | **true** |
| canGetMonitorInfo | **true** |
| canRedefineClasses | false |
| canAddMethod | false |
| canUnrestrictedlyRedefineClasses | false |
| canPopFrames | **true** |
| canUseInstanceFilters | **true** |
| canGetSourceDebugExtension | **true** |
| canRequestVMDeathEvent | **true** |
| canSetDefaultStratum | **true** |
| canGetInstanceInfo | **true** |
| canRequestMonitorEvents | **true** |
| canGetMonitorFrameInfo | **true** |
| canUseSourceNameFilters | **false** (but the filter works, see Q3) |
| canGetConstantPool | false |
| canForceEarlyReturn | **true** |
| reserved22..31 | false |
| reserved32 | **true** (undocumented; ignore it) |

Consequences for the design:
- Field watchpoints (FieldAccess/FieldModification) are available.
- The JDWP-level redefinition path is closed: `canRedefineClasses=false`. This matches the
  existing ART JDWP / RedefineClasses limitation (§8, open question 5). The error-60 retest was
  not run here.

## 3. SourceNameMatch ClassPrepare (design Q1): VERIFIED WORKING despite `canUseSourceNameFilters=false`

- EventRequest.Set(ClassPrepare, modifier 12 SourceNameMatch) is **accepted with error 0** for
  `"MainActivity.kt"`, `"*.kt"`, `"Main*"`, `"*Activity.kt"` and other patterns. ART does not
  reject it, even though the capability bit reports false.
- **It fires correctly.** Cold start under `am set-debug-app -w`, with requests set right after
  attach (`q3_patterns.out`; 5,311 classes prepared after attach):

| Pattern | Matches | Notes |
| --- | --- | --- |
| `MainActivity.kt` | 21 | Exactly MainActivity, its `$Companion`, and the 19 `MainActivity$onCreate$1$1$N$1` classes. No false positives. |
| `*MainActivity.kt` | 21 | Same as above |
| `MainActivity*` | 21 | Same as above |
| `*Activity.kt` | 34 | MainActivity plus androidx `ComponentActivity*` |
| `ComponentActivity.kt` | 10 | |
| `LabTheme.kt` | 1 | `LabThemeKt` |
| `*.kt` | 4,041 | |
| `*.java` | 358 | |
| `LabThemeKt`, `*Kt`, `io.github.andriyo.shadowdroid.sample.*`, `*sample*` | **0** | |

  The match is on the SourceFile basename (or the SMAP file table) only, never on the class
  name. Patterns follow JDWP rules: a single leading or trailing `*`.
- Not matched by a file filter: D8 synthetic lambda classes report `SourceFile="D8$$SyntheticClass"`
  (for example `MainActivity$$ExternalSyntheticLambda2`). They are line-less trampolines, so
  this does not matter for line breakpoints.
- `R$id` and `R$string` return 101 (ABSENT_INFORMATION) from ReferenceType.SourceFile. These
  are the only 2 of 295 package classes with no source file; the resolver must skip error 101.
- **Deferred binding works end to end** (`q3_coldstart.out`):
  1. With the app in `waitForDebugger`, set ClassPrepare
     `ClassMatch io.github.andriyo.shadowdroid.sample.MainActivity` with SUSPEND_EVENT_THREAD.
  2. MainActivity is prepared 1.6 s after attach.
  3. On that event, set Breakpoint `onCreate` line 61, then ThreadReference.Resume.
  4. The breakpoint hits 63 ms later in `MainActivity.onCreate@38 line=61`, and ThisObject works.
- §5.1 note: under `am set-debug-app -w`, the main thread is in `Debug.waitForDebugger` →
  `Thread.sleep` with status SLEEPING and suspendCount 0. The VM is **not** JDWP-suspended, so
  VirtualMachine.Resume is unnecessary (harmless if sent). `waitForDebugger` returns about 1.5 s
  after attach, once debugger traffic goes idle. Requests must be in place before that window
  closes.
- Design implication: deferred binding can use `SourceNameMatch <basename>` instead of a package
  `ClassMatch`. That gives 21 events instead of 295 for this app. Keep ClassMatch as the
  fallback.
- **Codec requirement found here:** a single ClassPrepare arrives as **one Composite carrying one
  event per matching request**. The first harness version kept only the first event of each
  Composite and wrongly reported "SourceNameMatch never fires". The Rust demux must dispatch
  every event in a Composite.

## 4. Breakpoint resolution path (§5.2): VERIFIED

- AllClassesWithGeneric returns 35,166 classes in 126–213 ms. Of these, 295 match the prefix
  `Lio/github/andriyo/shadowdroid/sample/`. Calling SourceFile on each of the 295 is fast; the
  whole resolve takes well under 1 s.
- Classes with `SourceFile == "MainActivity.kt"`: 21, listed below. There is **no `MainActivityKt`
  facade** (the file has no top-level functions) and no `MainActivity$...` lambda containing
  line 100.
  - `MainActivity`
  - `MainActivity$Companion`
  - `MainActivity$startUnstableUpdates$updater$1` (status=3, verified but not initialized)
  - 18 × `MainActivity$onCreate$1$1$N$1` (Compose function-reference classes)

  Status is 7 (VERIFIED|PREPARED|INITIALIZED) unless noted.
- Locations containing line 100: **exactly one**,
  `MainActivity.onNewIntent(Landroid/content/Intent;)V` at codeIndex 5. The method range is
  0..40 with 5 line entries.
- Breakpoint EventRequest (SUSPEND_EVENT_THREAD, LocationOnly) is set as request id 6. Firing
  `am start ... -f 0x20000000 --es tag bp1` produces a Composite with one Breakpoint event
  **38 ms** later:
  - `{'kind': 2, 'req': 6, 'policy': 1, 'thread': 35167, 'loc': (1, 32927, 213, 5)}`
  - Thread `main`, status (RUNNING=1, suspended=1), suspendCount 1.
  - Location `MainActivity.onNewIntent@5 line=100`.
- IDs are **per connection**: the main thread was 35167 in one session and 35344 in another.
  Never persist a raw JDWP id across attaches.

## 5. Frame reads while suspended: VERIFIED

- ThreadReference.Frames returns 21 frames. The top is
  `MainActivity.onNewIntent@5 line=100`, then `Activity.onNewIntent(Intent, ComponentCaller)`,
  `Activity.performNewIntent`, `Instrumentation.callActivityOnNewIntent`, and so on.
- StackFrame.ThisObject returns tag `L` and an object id of class `MainActivity`.
- VariableTableWithGeneric for `onNewIntent` has argCnt=2 and 2 entries. **`this` is listed as an
  ordinary slot:**
  - slot 3, `this`, range 0+41
  - slot 4, `intent`, range 0+41

  Register-based slot numbers start at 3, not 0, so slots must come from the table, not be
  computed. GetValues on those slots returns the same `this` id as ThisObject, plus the intent.
- After stepping INTO `intentSummary`: 4 entries (`data` slot 1, range 18+57; `source` slot 2,
  range 27+48; `this` slot 5; `intent` slot 6). At codeIndex 0, `data` and `source` are out of
  range. GetValues on an out-of-range slot returns **error 35 (INVALID_SLOT)**, and so does
  bogus slot 99. Filter slots by `[codeIndex, codeIndex+length)` before reading.
- Kotlin synthetic locals (static survey of all `MainActivity*` methods, `q7b.out`). The Kotlin 2.x
  names carry `\N\M` suffixes, so the renderer has to filter or rename them:
  - `$i$f$getValue\1\51`: inline function marker
  - `$i$a$-cache-MainActivity$onCreate$1$1$1\3\473\0` and `$i$a$-let-ComposerKt$cache$1\2\471\1`:
    inline lambda markers
  - `$this$cache\1` and `$this$dp`: extension receivers
  - `this$0`: outer instance
  - `$composer` and `$changed`: Compose
  - `$update` and `$receiver`: captured variables in an anonymous object `<init>`

  No `$continuation` appeared because the sample's MainActivity has no suspend functions. Some
  `<init>` methods return error **101** from VariableTable.
- Intent object reads:
  - ObjectReference.ReferenceType returns `Landroid/content/Intent;`.
  - FieldsWithGeneric returns **583 declared fields**, mostly static `ACTION_*` constants. The
    renderer should drop statics by `modBits & 0x8`.
  - GetValues: `mAction=null` (am start without an action), `mFlags=805306368` (0x30000000),
    `mExtras` → `Bundle`, `mData=null`, `mComponent` → `ComponentName`.
  - StringReference.Value on `ComponentName.mClass` returns
    `"io.github.andriyo.shadowdroid.sample.MainActivity"`, and on `mPackage` returns
    `"io.github.andriyo.shadowdroid.sample"`.
- `this` fields show the Kotlin delegated-property shape: `statusMessage$delegate`
  (`ParcelableSnapshotMutableState`), `counter$delegate` (`ParcelableSnapshotMutableIntState`),
  `events`, `mainHandler`, `networkBusy$delegate`, plus statics `$stable`, `Companion`, `TAG`,
  and so on. Showing `statusMessage` as a value means unwrapping the `$delegate`.

## 6. DisableCollection (design Q2): VERIFIED. Pinning is required, and it works.

Procedure (`spike.py` Q6):
1. At hit 1, pin intent A and `this` with DisableCollection.
2. Fire three more intents (hits bp2, bp3, bp4). Record each intent id without pinning it, and
   resume after each.
3. Clear the breakpoints.
4. Force GC 3× with `adb shell run-as <pkg> kill -10 <pid>` (SIGUSR1 forces a GC in ART; rc 0).

Results:

| id | pinned | IsCollected | GetValues |
| --- | --- | --- | --- |
| A (35169) | yes | **false** | works (`mFlags=805306368`) |
| bp2 (35179) | no | **true** | **error 20 (INVALID_OBJECT)** |
| bp3 (35180) | no | **true** | **error 20** |
| bp4 (35181) | no | false (still `mIntent`, reachable) | works |
| `this` | yes | false | — |

EnableCollection on both returns OK. Reproduced identically on a second full run.

Conclusion:
- **ART libjdwp holds object ids weakly.** An id that is not pinned is collected as soon as the
  app drops it, between hits.
- DisableCollection reliably keeps the object alive.
- `debug handles`, watches, and any id that outlives one suspension must DisableCollection on
  creation and EnableCollection on release or detach. Error 20 / `IsCollected=true` must map to a
  `handle_collected` error instead of a generic failure.
- Re-validating with IsCollected before every read is only needed for unpinned ids.

## 7. Stepping: VERIFIED, with one ordering trap

- Step LINE (size 1) + OVER (depth 1), Count(1), SUSPEND_EVENT_THREAD, then ThreadReference.Resume:
  - Step event in 32–39 ms at `onNewIntent@8 line=101`.
  - Again in 8–10 ms at `onNewIntent@11 line=102`.
- Step INTO (depth 0) at line 102 with ClassExclude `kotlin.*`, `java.*`, `android.*`, `androidx.*`
  lands in `MainActivity.intentSummary@0 line=436`. The `StringBuilder` calls are skipped.
- Step OUT (depth 2) from there returns to `onNewIntent@14 line=102` (mid-line, as expected).
- **Surprise: modifier order is semantic. Count must come after ClassExclude.**
  - With `[Step, Count(1), ClassExclude×4]`, a step INTO from line 100 (`super.onNewIntent` →
    `androidx.activity.ComponentActivity.onNewIntent`) **never produces an event**. The
    single-shot Count expires on the first, excluded, step location. The thread then runs free
    back to `Looper` with suspendCount 0, and the request is silently gone. Reproduced 2/2, also
    with only `androidx.*` excluded.
  - With `[Step, ClassExclude×4, Count(1)]` or no Count, the same INTO lands on
    `onNewIntent@8 line=101` in 2–29 ms. A second INTO goes into
    `android.app.Activity.setIntent@0 line=1179` if `android.*` is not excluded, or to line 102
    if it is.

  This is JDWP-spec behaviour ("filters are applied in the order specified"), but easy to get
  wrong. **Rule for the codec: always emit Count last.**
- Step INTO without excludes from line 100 gives four steps:
  1. `androidx.activity.ComponentActivity.onNewIntent@0` (line **None**: no line entry at
     index 0)
  2. `kotlin.jvm.internal.Intrinsics.checkNotNullParameter@0 line=130`
  3. `@5 line=133`
  4. `ComponentActivity.onNewIntent@6 line=893`

  Default step excludes (kotlin/java/android/androidx) are therefore needed for "step into my
  code".
- Clearing a Count-expired step request returns OK (no error), so clearing unconditionally after
  each step is safe.

## 8. MethodEntry cost (design Q4): CONFIRMED EXPENSIVE. Use a different design.

Setup: MethodEntry with ClassMatch `io.github.andriyo.shadowdroid.sample.*`, SUSPEND_NONE. Each
3 s window measures app CPU (utime+stime ticks/s at 100 Hz, about % of one core), `dumpsys
gfxinfo` frames, and the time for `adb shell input swipe` to return. The interaction is
alternating swipes plus an intent every third swipe. Taps were avoided because the Field Lab has
crash and ANR buttons.

| Window | CPU ticks/s | Frames | Janky | p90 | Swipe cmd | Events |
| --- | --- | --- | --- | --- | --- | --- |
| detached, idle | 4 | — | — | — | — | — |
| detached, interact | 80 | 140 | 34% | 48 ms | 216 ms | — |
| attached, no requests, idle | 2 | — | — | — | — | — |
| attached, no requests, interact | 76 | 151 | 28% | 46 ms | 215 ms | — |
| **MethodEntry, idle** | **48** | — | — | — | — | **0 events** |
| **MethodEntry, interact** | **95** | **18** | **94%** | **350 ms** (p99 450 ms) | **430 ms** | **104 (34/s)** |
| after Clear, interact | 66 | 142 | 33% | 48 ms | 215 ms | — |
| after Dispose, interact | 71 | 148 | 29% | 46 ms | 216 ms | — |

Top methods hit: `LabThemeKt.getLabTextMuted` (9), `getLabViolet` (5), `LabUiKt.EventRow`, and
similar.

Conclusion:
- The ClassMatch filter is applied in libjdwp **after** ART raises a JVMTI MethodEntry for every
  method in the process.
- Only 34 events/s match, yet the idle app jumps from about 4% to 48% of a core with zero
  matching events, and rendering collapses about 8× (140 → 18 frames, p90 7×). The UI stays
  usable but visibly janky.
- Clearing the request restores full performance immediately: set takes 3 ms, clear takes 1 ms.
- **Method breakpoints should be line breakpoints at each matched method's first line-table entry
  (or codeIndex 0).** Method-exit can be a breakpoint on each `return` location or a short-lived
  MethodExit used only while a single thread is stepping. Raw MethodEntry/MethodExit should be
  reserved for explicit opt-in tracing, with the cost surfaced.

## 9. Clean detach: VERIFIED

- With a ClassPrepare SUSPEND_ALL request active and the VM suspended (VirtualMachine.Suspend),
  VirtualMachine.Dispose replies OK in 1 ms. **The VM then closes the socket:** the reader sees
  EOF right after the Dispose reply. The daemon must treat post-Dispose EOF as a clean close.
- The app stays alive (same pid) and is resumed: `onNewIntent` logs 100 ms after a later
  `am start`, and gfxinfo after Dispose matches the baseline (148 frames, p90 46 ms).
- Re-attaching immediately after Dispose works.
- Studio visibility: `shadowdroid debug clients` shows the pid with
  `debugger_attached=false, debugger_status=DEFAULT` before, **during**, and after a raw JDWP
  attach. **Studio cannot see a ShadowDroid JDWP attach.** `auto` backend selection (§4.4) must
  rely on the debugd registry, not on Studio's client list, to know ShadowDroid holds a pid.
  Conversely, when Studio holds the pid, its client list does show `debugger_attached=true,
  debugger_status=ATTACHED` (Q10b), so step 2 of §4.4 is sound.

## 10. Exclusivity: VERIFIED. Detection signature: "OKAY, then EOF before the handshake echo"

- While our client is attached, a second `host:transport` + `jdwp:<pid>` gets an **ADB-level
  `OKAY`** in about 1 ms. Sending `JDWP-Handshake` then gets an immediate **EOF (0 of 14 bytes)**
  within 1–2 ms. There is no ADB `FAIL` message and no timeout. Repeated attempts (3×) behave the
  same, and the first connection is unaffected.
- The same holds through `adb forward tcp:0 jdwp:<pid>`: TCP connect succeeds, and the handshake
  gets EOF in 2 ms.
- Q10b, Studio holding the pid: after `shadowdroid debug attach --project shadowdroid-test-app
  --pid 31062` (Studio debugger attached), our `jdwp:<pid>` connection gets the identical
  signature, OKAY then handshake EOF in 6 ms. Studio's session survived our attempt. It was then
  detached with `shadowdroid debug stop --session session_1`, and the app stayed alive.
- **Detection rule for `debugger_already_attached`:** ADB `OKAY` on `jdwp:<pid>` followed by EOF
  before the 14-byte handshake echo, given that the pid is still alive and listed by the `jdwp`
  service. A dead pid looks different. To name the holder, check the local debugd registry first.
  If it has no entry, check the Studio bridge's `debug clients` (`debugger_attached=true`). If
  neither holds the pid, report "another debugger".
- Not tested: what Studio shows if it tries to attach while ShadowDroid holds the pid. It was
  skipped to avoid leaving a modal or error balloon in the running Studio.

---

## Extra: error-code reference observed on ART (`q_errors.out`)

| Call | Result |
| --- | --- |
| ThreadReference.Frames / FrameCount on a running thread | **13** THREAD_NOT_SUSPENDED |
| ObjectReference.ReferenceType with a bogus id | **20** INVALID_OBJECT |
| ReferenceType.Signature with a bogus id | **20** (not 21 INVALID_CLASS) |
| Breakpoint with codeIndex outside the method range (9999) | **24** INVALID_LOCATION |
| Breakpoint at an arbitrary in-range index (6) | Accepted. ART does not validate instruction boundaries, so always use line-table indices. |
| EventRequest.Clear of an unknown request id | OK (silently) |
| ThreadReference.Resume on a non-suspended thread | OK (no-op) |
| StackFrame.ThisObject with a frame id after its thread was resumed | **13** |
| ... after the thread was re-suspended | **30** INVALID_FRAMEID |
| StackFrame.GetValues on an out-of-range or bogus slot | **35** INVALID_SLOT |
| ReferenceType.SourceFile on `R$*` | **101** ABSENT_INFORMATION |
| VariableTable on some `<init>` | **101** ABSENT_INFORMATION |

- Frame ids encode a suspension epoch: `262144+n` (4<<16) at one hit, `327680+n` (5<<16) after
  re-suspend. They are valid only for the current suspension.
- Suspend counts nest: an extra ThreadReference.Suspend raises the count to 2, and each Resume
  decrements by 1.

## Design assumptions: scorecard

| Assumption | Result |
| --- | --- |
| §4.1 in-tree `jdwp:<pid>` device service, no forward needed | **Held** on adb 37.0.1. The forward fallback works too, with identical semantics. |
| §4.3 OpenJDK libjdwp on ART, spec error codes | **Held** (JDWP 1.8 "Reference Implementation"; codes 13/20/24/30/35/101 per spec). The capability bit for source filters is wrong (false, but it works). |
| §5.1 attach does not suspend; `--wait-for-launch` + ClassPrepare | **Held**. VM.Resume is not needed after `-w`; requests must be set within about 1.5 s. |
| §5.2 resolution via prefix + SourceFile + LineTable | **Held**. Skip error 101; D8 synthetics have `D8$$SyntheticClass`. |
| §5.2 step 5 deferred binding via ClassMatch | **Held**. SourceNameMatch also works and is about 14× narrower (open question 1: yes). |
| §11 Q2 DisableCollection | **Held**, and it is *mandatory*: unpinned ids are weak and do get collected (error 20). |
| §11 Q4 MethodEntry with ClassMatch is cheap enough | **Did not hold**. It costs the whole process; use first-line breakpoints. |
| §4.4 `auto` can see a ShadowDroid holder via Studio | **Did not hold**. Studio shows `debugger_attached=false`, so the registry is the only source. Studio-held pids are visible. |
| Exclusive attach yields a recognisable failure | **Held**: OKAY then handshake EOF, about 1–6 ms. No ADB FAIL text to parse. |
| New codec requirements found | Flatten every event of a Composite; emit Count as the last modifier; treat EOF after the Dispose reply as clean; give the first request a generous deadline (lazy agent load, about 60 ms observed). |

---

## Errata and additions from Phase 2 live validation

- **Q1 correction:** the plain `jdwp` device service is **not** framed. On adb 37 / API 36 it
  sends a bare newline-separated pid list (`4748\n22736\n31062\n31088\n`) and keeps the stream
  open. My `jdwp_pids()` parsed it as `%04x`-framed and silently dropped the first pid (4748).
  The Rust reader now ends the read after a 150 ms quiet period once data has arrived.
- **Q11 (new, `q11_uncaught.py`): an Android crash is never "uncaught" to JDWP.** A
  RuntimeException from a Compose click handler (`MainActivity.crashNow`, line 240) reports
  catch location `androidx.compose.ui.input.pointer.PointerInputEventProcessor.process@256
  line=150`. Looper.loopOnce also catches and rethrows. An ExceptionOnly(caught=false,
  uncaught=true) request never fires, and the process dies. The jdwp backend therefore treats
  `--uncaught` as "not caught by app code".
- Non-debuggable pid (system_server) and missing pid both get an adb `FAIL closed` on
  `jdwp:<pid>`. A bogus serial fails earlier, at `host:transport`
  (`device 'bogus-serial' not found`).
