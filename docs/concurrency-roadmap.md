# Concurrent agents and resource ownership

Status: C0/C1 local cooperative sessions implemented and emulator-tested; C2/C3 remain conditional. The design below was written 2026-09-18 against source commit `add874bd004e313014889a420abcba555b0df070`. See the [session guide](sessions.md) for actual syntax and the [validation report](verification-validation.md) for tested boundaries.

The default should be **one active driver per device, multiple observers and advisers, and parallel drivers on isolated devices**. Agents divide engineering responsibility by task or feature; ShadowDroid coordinates the runtime resources those tasks actually touch. A project name, app package, or screen region is not a sufficient device-isolation boundary.

Concurrency is a foundation of the first verification release. Add identity, ownership enforcement, independent event subscriptions, and reliable handoff before allowing verification runs to share a host/device fleet. More elaborate subleases and fleet scheduling can follow.

Implementation tracking: the [session guide](sessions.md) describes the current local reservation gate, passive subscriptions, authority marker and explicit recovery. Distributed coordination and arbitrary-client endpoint enforcement remain outside that implementation.

## 1. Baseline before the session implementation

| Existing mechanism | Verified source behavior | Remaining boundary |
| --- | --- | --- |
| [Named AVD claims](../cli/src/device/target.rs) | Ownership records identify AVD, project root, and target; the same project can reuse its claim | No agent/run identity or reservation for an entire journey |
| [Device selection](../cli/src/cli.rs) | Explicit `--device` resolves directly; physical targets validate availability/form factor | Named-AVD project claims are not a universal gate on device access |
| [Lifecycle locks](../cli/src/device/installer.rs) | OS file locks serialize selected lifecycle operations; owner diagnostics and bounded acquisition are available | The lock is scoped to an operation, not the full read/think/act workflow |
| [UI action guard](../server/app/src/androidTest/java/io/github/andriyo/shadowdroid/routes/ActionGuard.kt) | Mutex covers guard-capable input routes and scroll; hashes reject stale observations | App/system operations and external transitions are outside that mutex; multiple valid actions can still form an invalid journey |
| [Port assignments](../cli/src/device/portmap.rs) | Per-serial/channel forwarding plus a host allocation lock | Agent/session identity, device reincarnation, and multiple ADB authorities need explicit modeling |
| [Network](../cli/src/net/paths.rs) and [video](../cli/src/video/paths.rs) state | Active daemon/control state is per serial; lifecycle ownership protects selected changes | These are shared services for one device, not independent services for each agent |
| [Crash notifications](../cli/src/crashscan.rs) | One persisted events-since-last cursor per serial advances across CLI calls | One agent can advance the cursor before another observes the same event; this needs a reproduction and subscriber-specific delivery |
| [Evidence bundles](../cli/src/cmd/evidence.rs) | Checkpoint writes use a bundle lock and separate artifact files | Atomic files do not attribute concurrent app mutations or reconcile competing requirement updates |
| [Debugger logpoints](../cli/src/cmd/debugger.rs) | Ownership labels exist; the default label is `shadowdroid` | Shared defaults are not distinct agent identities, and debugger process control still affects the app globally |
| [Effect catalog](../cli/src/cmd/introspect/effects.rs) | Commands declare possible effects, including automatic server bring-up | Observer admission must consider transitive effects, not just a read-sounding command name |

These are historical baseline source findings; the session implementation addresses the shared-authority and cursor gaps. Keep existing safeguards and extend their coverage rather than assuming a new session label alone prevents interference.

## 2. Demarcation lines

| Resource boundary | Safe default | Why this boundary matters / trade-off |
| --- | --- | --- |
| Independent device/AVD instances | Parallel writers, one driver for each instance | Strong runtime separation, with additional RAM/CPU/storage cost; shared backends can still couple them |
| Same device, different app packages | One device driver | Foreground task, keyboard, permission dialogs, display, navigation and instrumentation remain shared |
| Same app, different screens/components | Serialize whole journeys or use separate devices | Screen-local selectors do not isolate navigation, shared state, scrolling, dialogs, or recomposition |
| Device system configuration | Exclusive driver-owned changes | Rotation, locale, theme, proxy and animation settings can invalidate every observer's assumptions |
| App installation/private state | Exclusive device ownership in the first version | Installation, clear/restore, permissions and process lifecycle disrupt other checks even when package-scoped |
| UiAutomation/instrumentation slot | Exclusive execution phase | Release the ShadowDroid service for app tests; block attempts to reconnect until the phase ends |
| Network capture and rules | Shared read stream; one configuration owner | Capture is reusable; interception/rules alter another agent's experiment. Scoped flow delegation is a later capability |
| Debugger | One process-control owner; snapshots may be shared | Pause/resume/step/evaluate can change timing or state; an expression that looks like a getter is not guaranteed pure |
| Recorder | One recording lifecycle owner; many readers/attributed markers | An observer must not stop a recording another run still requires; resource use can affect timing |
| Evidence artifacts | Concurrent immutable reads; per-producer immutable writes and one reducer | Avoid last-writer-wins updates to a shared report or filenames such as `latest.png` |
| Source checkout and build outputs | Separate worktrees/build directories or explicit handoff | Different device leases do not stop agents from editing the same source, replacing an APK, or mixing test XML |
| Backend account, tenant, fixture server | Per-run data namespace or an explicit external-resource reservation | Two isolated emulators can still mutate the same cart, database row, account settings, or mock response queue |
| Host infrastructure | Shared capacity-managed services; narrow mutation ownership | ADB restarts, SDK/CA updates, ports, shared caches, and emulator launches can affect every device |

Use separate writable AVD state for separate runtime instances. Android stores per-instance app/settings state in writable AVD data; copying a target alias does not create a new instance. Validate any clone/snapshot provisioning mechanism before supporting it. [Android emulator storage](https://developer.android.com/studio/run/emulator-commandline#avd-data-directory)

The initial scheduling unit is a **bounded journey or test phase**. Locking only each tap allows agents to interleave logically incompatible steps. Reserving a device for an entire multi-hour coding task wastes capacity. Release at declared safe checkpoints or when the agent is returning to source-only work; do not preempt halfway through an unknown action.

## 3. Cooperation patterns and trade-offs

| Pattern | Appropriate use | Cost / limitation | Recommendation |
| --- | --- | --- | --- |
| Isolated worker | Each agent has a worktree, device, build and backend namespace | Highest resource use; integration still needs fresh validation | Preferred for independent implementation or matrix shards |
| Driver with advisers | One agent drives; reviewers inspect immutable UI/log/network evidence and suggest the next step | Runtime actions are serial; advisers can reason concurrently | Default when only one device is available |
| Shared-device queue | Agents submit complete journeys to one execution coordinator | Queue time and setup/reset cost; each job must declare starting state | Support in the first coordinated release |
| Driver with scoped specialist | Network/debug specialist receives a bounded grant within the driver's experiment | More complex conflict rules, revocation and causal attribution | Add after the basic ownership model is proven |
| Optimistic independent writers | Each agent acts with a screen hash and retries on conflict | Hashes do not express hidden state or whole-journey intent; retry loops waste tokens | Do not offer as a correctness guarantee |
| Distributed device fleet | Agents on several machines share a registry/scheduler | Network partitions, host identity, authentication and recovery complexity | Later extension; each device still has one execution authority |

A useful initial team is a builder in an isolated checkout, a runtime driver for the installed candidate, and a reviewer reading that candidate's saved evidence. The reviewer returns findings tagged with the artifact/requirement revision. The builder's next APK becomes a new candidate; observations from the old candidate remain historical.

ShadowDroid should expose work descriptions, reservations, observations, handoffs and results. The calling agent framework chooses agents, models, prompts and task allocation. The CLI should not grow a general multi-agent conversation system.

## 4. Identity and the local execution authority

Recommended first implementation: a lightweight local coordinator/supervisor serving short-lived CLI invocations. It owns a per-device mutation queue, reservation journal, active child processes and event subscriptions. It can start lazily for managed sessions; offline report reading remains independent.

Use distinct identifiers with distinct meanings:

| Identity | Purpose |
| --- | --- |
| `project_id` / `worktree_id` | Source/configuration provenance, not device ownership |
| `task_id` / `run_id` | Logical engineering task and one execution attempt |
| `agent_id` / `session_id` | Participant label and restart-aware runtime membership |
| `authority_id` | Coordinator and ADB-server authority for a device connection |
| `device_instance_id` | Resolved physical/virtual instance, with connection/boot generation |
| `lease_id` / `lease_generation` | Current exclusive reservation and its fencing generation |
| `operation_id` / `request_id` | Execution record and retry deduplication identity |
| `subscription_id` | One consumer's event cursor and delivery policy |
| `artifact_id` / `candidate_id` | Immutable evidence and tested application identity |

Canonicalize target aliases to the same runtime resource before checking ownership. Include the ADB authority in the identity: `emulator-5554` is not globally unique. Reconnects, reboots and serial reuse require revalidation; an old handle must not silently bind to a different instance. Configuration fingerprints are attributes, not unique physical-device identifiers.

All cooperating clients reaching the same device must use the same authority. Separate containers or user homes with private lock files do not coordinate merely because their lock filenames match. For the first release, require an explicitly shared coordinator endpoint for such clients or refuse the shared-device configuration. Defer multi-host consensus rather than claiming local files solve it.

The coordinator's own single-instance guarantee uses an OS lock whose file is never removed while held. A short transactional registry update is acceptable; a host-global lock must not serialize slow actions on different devices. Persist reservation/operation transitions and recover them before resuming dispatch after a crash.

This model protects cooperating ShadowDroid clients. Raw ADB, direct server HTTP, IDE actions, humans and other automation can bypass parts of the system unless the environment restricts those paths. In managed mode, route supported mutations through the authority, require session context at the server/daemon boundaries, and reject unmanaged clients for a reserved device. Unknown external activity invalidates affected observations where detected; do not promise perfect detection or isolation from arbitrary external processes.

## 5. Leases, fencing and recovery

A lease is the right to drive a resource for a bounded period. A fencing generation lets the execution side reject requests from a previous owner after a handoff. Expiry alone is insufficient: a paused client can resume after another client becomes owner. This is the reason to validate ownership at execution, not only when a client obtains a lock. [Fencing-token rationale](https://docs.hazelcast.com/hazelcast/5.7/data-structures/fencedlock)

Required semantics:

1. Acquire resolves the device, checks policy/capabilities, and returns a lease handle bound to session, resource set and coordinator epoch. PID and an arbitrary owner string are diagnostic labels, not sufficient authority.
2. Validate session, resource scope and generation before dispatch and at supported mutation boundaries. UI-server validation belongs inside the action mutex; network and debugger mutations need their corresponding execution boundary.
3. Keep an epoch across coordinator restart/recovery and monotonically ordered generations within it. Old epochs/leases fail closed until explicitly reconciled. Do not infer ownership from wall-clock timestamps or PID reuse.
4. Renewal comes from the controlling session/explicit supervisor contract. Do not treat the mere survival of the coordinator as proof that an agent still wants a device. Use monotonic local deadlines and configurable idle limits long enough for normal model reasoning.
5. Expiry stops admission of new commands. It does not prove previously dispatched input, ADB work, instrumentation or delayed network actions have stopped.
6. Before giving the device to another writer, drain or cancel all admitted operations and reconcile owned state. For ADB operations without endpoint fencing, the authority must supervise dispatch and refuse reassignment while execution remains uncertain. A host timeout is not proof of cancellation.
7. If quiescence cannot be established, mark the device `recovery_required` and block new writers. Restore only state still attributable to the expired owner; a user/another owner change is a conflict, not a value to overwrite.
8. Parent expiry/release revokes delegated grants. A helper cannot extend or reacquire its parent's authority. Preserve pending-flow and test-run outcomes according to their declared bounded recovery policy.
9. Record ownership changes and operation outcomes durably. A client retry with the same request ID and payload returns the recorded result where known. A mismatched payload is rejected. If an action may have happened before an acknowledgement was lost, return `outcome_unknown` and reobserve; do not promise exactly-once Android side effects.
10. A normal `release` completes after safe draining and required cleanup. Emergency recovery is an explicit, auditable operation. Existing `--takeover` must not silently become permission to interrupt a live managed lease.

An active lease covers the read/think/act loop, but the low-level mutex does not stay locked during model inference. This lets permitted observers receive evidence and other devices continue working. Heartbeat cadence, queue deadlines, and recovery timeouts are calibrated from measured inference and command latency rather than guessed constants.

## 6. Observers and event subscriptions

Expose three distinct read modes:

- **Artifact read:** inspect an immutable screenshot, tree, log page or report without touching the device. Safe and cheap for many reviewers.
- **Passive live observation:** use an existing connection, bounded polling or stream subscription. It cannot start/repair servers, boot devices, reconnect instrumentation, install packages, change capture settings, or execute watchers.
- **Barrier capture:** ask the driver/coordinator for a checkpoint at a declared safe point. New managed mutations pause briefly around the capture; Android can still change asynchronously, so record per-source time ranges and consistency.

Do not classify `ui dump`, `watch`, `debug inspect`, or a storage snapshot as automatically read-only. Current server-backed reads can ensure the server is running; watch can trigger popup actions; debugger expressions can have side effects; an app-state snapshot stops the app. Observer variants must mechanically restrict these effects. An unavailable source returns blocked/unavailable, not an attempt to repair the driver session.

Replace shared notification consumption with an append-only event stream and independent subscriptions. Record authority/device/capture epochs, sequence IDs, timestamps, subscription cursors and retention gaps. Two agents should both receive the same crash if subscribed; one agent acknowledging it must not acknowledge it for another. Resuming after retention expiry produces an explicit gap and incomplete-observation status.

Namespace crash cursors, logpoint ownership, bookmarks, report outputs and request histories by session/run as appropriate. Keep device-wide services canonical; blindly creating a proxy or recorder for every agent would introduce new conflicts. Subscriber teardown only releases that subscriber. Service stop belongs to the owning session and must account for active consumers or report their interruption.

Bound observer overhead using shared capture, rate limits, cached snapshots and backpressure. Slow readers should not stop the driver or make event loss invisible. Include dropped-record counts and degraded capture quality in evidence. Shared capture can reduce work, but two requests with different filters/redaction requirements must not be silently conflated.

## 7. Scoped cooperation within one device lease

Initial release: advisers submit findings or operation proposals; the driver/coordinator executes mutations. A later release can delegate narrow capabilities while retaining one owner for the overall experiment:

| Delegated role | Allowed scope | Excluded by default |
| --- | --- | --- |
| Network observer | Read a particular capture/session and bounded bodies | Rule changes, starting/stopping proxy, trust/proxy changes |
| Flow responder | Resolve one held flow identified by capture ID, flow ID, phase and deadline | Resolving another flow or replacing global rules |
| Diagnostic observer | Read saved stack/log/layout evidence | Pause/resume/evaluate or attaching an inspector that changes state |
| Debug specialist | Explicit process/session grant during a coordinated debug phase | Resuming a process while the driver expects it paused; changing another owner's breakpoints |
| Evidence reviewer | Produce findings referencing candidate/check/artifact hashes | Overwriting evidence, setting deterministic pass flags, changing plan requirements |

Network intervention can intentionally overlap a waiting UI action when the plan permits it; this is coordinated cooperation within one experiment. It is not permission for two independent journeys to share the device. An urgent held-flow decision needs its own bounded control path so it cannot deadlock behind the UI operation waiting for that response.

Treat popup watchers, permission automation, delayed daemon rules, and scheduled cleanup as writers attributable to a session. Revocation must cover these background behaviors, not just the next foreground CLI call. Rule-set edits use expected revision plus owner-scoped changes; a helper cannot clear another session's rules or use a stale snapshot to overwrite a newer configuration.

## 8. Handoff and task coordination

A planned handoff has a mechanical state transition and a small context package:

1. The current owner requests a yield at a named checkpoint. The coordinator stops admitting its new mutations and drains admitted work.
2. Save candidate/APK identity, requirement/plan revision, device and app configuration, fixture/backend namespace, current UI observation, event watermarks, active services, pending flows, unresolved checks and cleanup obligations. Store artifacts by reference; do not put bearer credentials in the handoff record.
3. The coordinator restores the agreed baseline or explicitly transfers the existing experiment state. Merely transferring a lease does not reset the app. The receiving job must accept the handoff mode and starting-state contract.
4. Revoke the old generation and delegated grants, grant the new owner a new generation, and emit the ownership event. Do this only after prior execution is reconciled.
5. The new owner acknowledges the candidate/state contract and obtains a fresh observation before acting. A previous owner's element handles, screenshots or cleanup rights are not automatically current.

Unexpected owner death goes through recovery, not this optimistic transfer path. A heartbeat expiry should not silently undo a valuable reproduction or clear app data. Keep recovery evidence and expose the choice of preserving the scene versus resetting it to the controlling workflow.

Task records should carry a task ID, requirement IDs, immutable source/candidate inputs, requested resources, starting state, effects, deadline, output artifacts and completion predicates. Allow requirement review and source implementation to be sharded across agents; reconcile plan changes by revision checks, and run integration checks after their changes are combined.

For a shared checkout, use one edit/integration owner and explicit file/module handoffs if the agent framework cannot provide worktrees. ShadowDroid should record source identity and report mismatches; it should not attempt to solve arbitrary concurrent Git edits. Build agents must publish immutable APK/report artifacts before a device job consumes them.

## 9. Scheduling and resource accounting

Use fair bounded queues with cancellation, queue position and retry-after diagnostics. Never make agents spin on `busy` or repeatedly invoke `--takeover`. Allow a waiting agent to inspect artifacts, work on source, or select an available equivalent device.

Allocate all resources for a multi-device test together, or use a deterministic ordering with bounded acquisition and release on failure. A Wear OS phone/watch run cannot keep one device indefinitely while waiting for the other. Extend this to declared backend/fixture reservations; undeclared remote effects remain outside the guarantee.

Acquire device ownership before subordinate service locks, with a documented order and no lock upgrades while holding partial resource sets. Keep child test execution outside non-reentrant lifecycle locks while retaining the higher-level device lease. Track and reap supervised child process groups where possible; if a child can still touch the device after cancellation, block reassignment until reconciled.

Separately budget emulator slots, CPU/RAM, build workers, recording/inspection load, disk retention and backend quotas. No broad host lock should serialize independent devices. Share verified immutable SDK/APK dependencies where appropriate, but isolate mutable build directories and app/user-data images. Do not reset the host ADB server as routine per-agent recovery.

Choose the cheapest useful parallelism first: parallel source/artifact review, then independent device journeys, then narrowly coordinated live specialists. Cloning devices for every small observation can cost more than it saves. Shard configuration matrices by complete case with isolated data and exact artifact identity; aggregate only compatible results.

## 10. Proposed interfaces and compatibility

All names below are proposals; current CLI discovery remains authoritative.

| Surface | Purpose |
| --- | --- |
| `session open/status/close` | Identify a participant and its task/run; expose active obligations |
| `lease acquire/status/release` | Reserve canonical runtime resources with bounded waiting |
| `lease handoff/recover` | Controlled transfer or recovery with explicit state handling |
| Explicit session/lease context | Bind every managed operation to its permitted owner and generation |
| `observe subscribe/resume` | Independent event cursors and passive subscriptions |
| `verify run` resource requirements | Queue one declared journey/test phase on an eligible device set |

Use structured errors such as `resource_busy`, `lease_expired`, `lease_revoked`, `observer_mutation_forbidden`, `candidate_mismatch`, `outcome_unknown`, and `recovery_required`. Include resource identity, holder's non-secret identity, conflict scope, wait/retry guidance and evidence references. Never echo capability credentials.

An unreserved device can retain standalone behavior through an implicit short session. Once a managed lease exists, all participating command paths—including explicit serial selection, aliases, cleanup, repair and test reconnect—must honor it. An implicit per-command session does not reserve a multi-call journey; agent guidance must make that distinction clear.

Managed sessions require compatible CLI/server/daemon capability negotiation. Old tools unable to enforce ownership must be rejected or isolated on a different device. An observer missing a compatible service must not upgrade it under an active driver. Global installs, skill/config/CA changes and other host mutations get their own narrow ownership/atomic-update rules; do not attribute device isolation to them.

## 11. Validation and effectiveness

Test with independent OS processes and delayed real transports, not only threads or a mock lock map. Unit/model checks cover state transitions; actual emulator/server/daemon tests establish the execution boundary.

| Scenario | Required result |
| --- | --- |
| Two agents, one project, one device | Only the admitted driver mutates; the other receives a bounded conflict |
| Different aliases or direct serial for the same device | Both resolve to one reservation; no selection-path bypass |
| Same serial text on separate ADB authorities | Different resources; no accidental cross-wiring |
| Device reboot/reconnect or serial reuse | Old handles and generations require revalidation |
| Driver and passive observer during instrumentation | Observer never reconnects the ShadowDroid server or steals the slot |
| Two crash subscribers | Both receive the event; acknowledgement and resume are independent |
| Parent session expires with pending helper/watcher work | New writes are rejected; queued/in-flight behavior is drained or reported unresolved |
| Old client resumes after lease transfer | Its delayed UI, ADB, network and debugger mutations are rejected or prevent transfer until reconciled |
| Timeout after an input may have been delivered | Retry cannot silently inject the input twice; outcome remains unknown until resolved |
| Coordinator crashes/restarts | Epoch recovery rejects stale requests and preserves pending operations/evidence |
| Lost heartbeat with a live test child | Device cannot be reassigned while the child may still mutate it |
| Rule-set or settings cleanup after another owner changes state | Revision/ownership mismatch prevents destructive cleanup |
| Two devices running independently | Long work on one does not block the other's mutation queue |
| Slow observer and high-volume events | Bounded driver overhead; subscriber lag/gaps visible |
| Shared backend account deliberately causes interference | Missing isolation is detected by the fixture; no claim that device isolation solved it |
| Two-device job contends with single-device jobs | No deadlock, starvation or indefinite partial reservation |
| Raw ADB/human changes the app | Detectable drift invalidates evidence; unobservable external changes remain an explicit limit |
| Separate containers use different private registries | Shared-device mode is refused or routed through one authority |

Compare one, two and four participating agents in controlled trials, with fixed work and recorded total compute. Measure completed tasks per wall-clock time, total cost per accepted task, queue time, lease idle time, conflict/retry counts, observer overhead, duplicate/missed events, unknown outcomes, and recovery time. Add shared-device and isolated-device variants so extra emulator capacity is not mistaken for better coordination. Set numerical performance thresholds after baseline calibration; mandatory fixtures permit zero cross-owner writes and zero false passes from lost or stale evidence.

## 12. Delivery stages and decisions

This is E13 in the main roadmap. C0/C1 are part of its P0 scope; C2 is P1 and C3 is P2.

| Stage | Deliverable | Gate / relationship to main roadmap |
| --- | --- | --- |
| C0 — Resource and effect model | Canonical identities, conflict/effect classification, same-device cursor reproduction, coordinator feasibility spike | M0; settle with E03's data contracts |
| C1 — Basic coordinated sessions | Single local authority, exclusive device driver, passive observers, independent cursors, fencing/draining, recovery and handoff | M1 foundation; all mandatory C1 concurrency cases pass before M2 release |
| C2 — Scoped specialists and pooling | Narrow delegated flow/debug roles, service sharing, fair device-pool and multi-device scheduling | M3/M4; each new grant/resource combination adds its own tests |
| C3 — Remote coordination | Multiple hosts/users, remote authorities and partitions, explicit access policy | Later; only when use cases justify the operational cost |

Implement C1 with preconfigured devices; automatic AVD cloning is not required. Keep pure artifact reviewers available early. Do not claim C2/C3 guarantees merely because IDs and lock files exist. The mandatory first-release cases are those reachable through C1 interfaces; delegated-role and multi-device-pool cases become release gates when C2 is exposed.

Decisions to settle in C0 are coordinator persistence/storage, endpoint protocol, capability distribution, restart epochs, lease renewal/idle policy, direct-ADB supervision, observer admission/rate limits, and stable device identity on reconnect. Prefer a small same-host authority over adding a distributed lock service as a new baseline dependency. The contract must allow later remote clients without requiring that infrastructure now.
