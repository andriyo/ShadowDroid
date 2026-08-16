package io.github.andriyo.shadowdroid.agent

import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * In-app, agent-in-the-loop interception — the same model as the host proxy
 * (`net intercept`/`resume`/`drop`), but through a registered in-app capture
 * provider. The current companion is an OkHttp application interceptor, so it
 * can handle certificate-pinned OkHttp calls but not Cronet, QUIC, or arbitrary
 * HTTP clients.
 *
 * A single immutable matcher configuration is armed at a time. When the HTTP
 * hook reports a matching flow at the response phase it is **held** (the app's
 * call blocks) until the CLI issues `resume`/`drop`, or the hold deadline
 * expires — in which case it **fails open** (resumes unmodified) so a
 * slow/forgotten operator never bricks the app under test.
 *
 * Held-flow lifecycle changes are serialized by one registry lock. A flow can
 * make exactly one transition from `held` to `released`, `deadline_expired`, or
 * `client_interrupted`; a late control action can therefore never claim success
 * after the app call already resumed.
 */
object Intercept {

    /** All fields optional; null = match anything. Host/path are substrings. */
    class Matcher(
        val host: String?,
        val path: String?,
        val method: String?,
        val operationName: String?,
    )

    /** Matcher and deadline policy published as one indivisible generation. */
    internal data class ArmConfig(
        val matcher: Matcher,
        val holdMs: Long,
        val armedAtMs: Long,
    )

    sealed class Action {
        /** Release unmodified. */
        object PassThrough : Action()

        /** Fail the call (the app sees an IOException). */
        object Drop : Action()

        /** Replace status and/or body before returning to the app. */
        class Mutate(val status: Int?, val body: String?, val contentType: String?) : Action()
    }

    internal enum class ResultCode(val wireValue: String) {
        RELEASED("released"),
        DEADLINE_EXPIRED("deadline_expired"),
        CLIENT_INTERRUPTED("client_interrupted"),
        ALREADY_RELEASED("already_released"),
        UNKNOWN_ID("unknown_id"),
    }

    internal enum class TerminalState(val wireValue: String) {
        RELEASED("released"),
        DEADLINE_EXPIRED("deadline_expired"),
        /** The Java thread blocked in [maybeHold] was interrupted. */
        CLIENT_INTERRUPTED("client_interrupted"),
    }

    internal data class TerminalRecord(
        val id: String,
        val state: TerminalState,
        val heldAtMs: Long,
        val expiresAtMs: Long,
        val terminalAtMs: Long,
        val action: String?,
        val summary: JSONObject,
    ) {
        fun toJson(): JSONObject = copyJson(summary).apply {
            put("id", id)
            put("state", state.wireValue)
            put("heldAtMs", heldAtMs)
            put("expiresAtMs", expiresAtMs)
            put("terminalAtMs", terminalAtMs)
            put("action", action ?: JSONObject.NULL)
        }
    }

    internal data class ResolveResult(
        val code: ResultCode,
        val terminal: TerminalRecord?,
    ) {
        val released: Boolean
            get() = code == ResultCode.RELEASED

        fun toJson(id: String): JSONObject = JSONObject().apply {
            put("ok", released)
            put("id", id)
            put("resultCode", code.wireValue)
            terminal?.let {
                put("state", it.state.wireValue)
                put("heldAtMs", it.heldAtMs)
                put("expiresAtMs", it.expiresAtMs)
                put("terminalAtMs", it.terminalAtMs)
                put("action", it.action ?: JSONObject.NULL)
            }
        }
    }

    /**
     * The lifecycle registry is a separate type so timeout/race behavior can be
     * tested with a deterministic monotonic clock without sleeping for a real
     * interception deadline.
     */
    internal class Registry(
        private val nanoTime: () -> Long = System::nanoTime,
        private val wallTimeMs: () -> Long = System::currentTimeMillis,
        private val terminalHistoryCap: Int = TERMINAL_HISTORY_CAP,
        private val maxHeldFlows: Int = MAX_HELD_FLOWS,
    ) {
        private enum class Lifecycle {
            HELD,
            RELEASED,
            DEADLINE_EXPIRED,
            CLIENT_INTERRUPTED,
        }

        private class Held(
            val id: String,
            val config: ArmConfig,
            val summary: JSONObject,
            val heldAtMs: Long,
            val expiresAtMs: Long,
            val deadlineNanos: Long,
        ) {
            val latch = CountDownLatch(1)
            var lifecycle: Lifecycle = Lifecycle.HELD
            var action: Action = Action.PassThrough
        }

        private sealed class WaitStep {
            class Await(val remainingNanos: Long) : WaitStep()
            class Done(val action: Action) : WaitStep()
        }

        private val lock = Any()

        @Volatile
        private var armed: ArmConfig? = null

        // Linked maps make status/history ordering stable and let history evict
        // the oldest terminal record deterministically.
        private val held = linkedMapOf<String, Held>()
        private val terminal = linkedMapOf<String, TerminalRecord>()
        private var rejectedHolds: Long = 0L

        init {
            require(terminalHistoryCap > 0) { "terminalHistoryCap must be positive" }
            require(maxHeldFlows > 0) { "maxHeldFlows must be positive" }
        }

        fun arm(spec: JSONObject) {
            val matcher = Matcher(
                host = spec.optStringOrNull("host"),
                path = spec.optStringOrNull("path"),
                method = spec.optStringOrNull("method"),
                operationName = spec.optStringOrNull("operationName"),
            )
            val requestedHoldMs = spec.optLong("holdMs", DEFAULT_HOLD_MS)
                .coerceIn(MIN_HOLD_MS, MAX_HOLD_MS)
            synchronized(lock) {
                armed = ArmConfig(
                    matcher = matcher,
                    holdMs = requestedHoldMs,
                    armedAtMs = wallTimeMs(),
                )
            }
        }

        fun disarm() {
            synchronized(lock) {
                armed = null
            }
        }

        fun isArmed(): Boolean = armed != null

        fun status(): JSONObject = synchronized(lock) {
            pruneExpiredLocked(nanoTime())
            val config = armed
            val matcherJson = if (config == null) {
                JSONObject.NULL
            } else {
                JSONObject().apply {
                    put("host", config.matcher.host ?: JSONObject.NULL)
                    put("path", config.matcher.path ?: JSONObject.NULL)
                    put("method", config.matcher.method ?: JSONObject.NULL)
                    put("operationName", config.matcher.operationName ?: JSONObject.NULL)
                    put("holdMs", config.holdMs)
                    put("armedAtMs", config.armedAtMs)
                }
            }
            val heldArray = JSONArray()
            held.values.forEach { heldArray.put(heldJson(it)) }
            val terminalArray = JSONArray()
            terminal.values.forEach { terminalArray.put(it.toJson()) }
            JSONObject().apply {
                put("armed", config != null)
                put("matcher", matcherJson)
                put("held", heldArray)
                put("terminal", terminalArray)
                put("rejectedHolds", rejectedHolds)
            }
        }

        fun resolve(id: String, actionJson: JSONObject): ResolveResult {
            val action = parseAction(actionJson)
            val actionName = actionName(action)
            return synchronized(lock) {
                // Expiration is checked against the absolute monotonic deadline
                // while holding the same lock as release. A delayed timer can
                // therefore never let an at/after-deadline release win.
                pruneExpiredLocked(nanoTime())
                val active = held[id]
                if (active != null) {
                    val record = transitionLocked(
                        active,
                        TerminalState.RELEASED,
                        action,
                        actionName,
                    )
                    ResolveResult(ResultCode.RELEASED, record)
                } else {
                    val prior = terminal[id]
                    when (prior?.state) {
                        TerminalState.RELEASED -> ResolveResult(ResultCode.ALREADY_RELEASED, prior)
                        TerminalState.DEADLINE_EXPIRED ->
                            ResolveResult(ResultCode.DEADLINE_EXPIRED, prior)
                        TerminalState.CLIENT_INTERRUPTED ->
                            ResolveResult(ResultCode.CLIENT_INTERRUPTED, prior)
                        null -> ResolveResult(ResultCode.UNKNOWN_ID, null)
                    }
                }
            }
        }

        fun maybeHold(
            id: String,
            method: String,
            host: String,
            path: String,
            operationName: String?,
            summary: JSONObject,
        ): Action {
            val active = synchronized(lock) {
                val config = armed ?: return Action.PassThrough
                if (!matches(config.matcher, method, host, path, operationName)) {
                    return Action.PassThrough
                }
                pruneExpiredLocked(nanoTime())
                // Flow ids are expected to be unique. Never overwrite a live
                // waiter if a buggy provider reuses one; the duplicate fails
                // open and the original remains actionable.
                if (held.containsKey(id)) return Action.PassThrough
                // A broad matcher must not park an unbounded number of OkHttp
                // calls (and their response/socket/thread state). Matchers over
                // capacity fail open, just like an expired hold.
                if (held.size >= maxHeldFlows) {
                    if (rejectedHolds < Long.MAX_VALUE) rejectedHolds += 1L
                    return Action.PassThrough
                }

                val heldAtMs = wallTimeMs()
                val nowNanos = nanoTime()
                val value = Held(
                    id = id,
                    config = config,
                    summary = copyJson(summary),
                    heldAtMs = heldAtMs,
                    expiresAtMs = saturatingAdd(heldAtMs, config.holdMs),
                    deadlineNanos = nowNanos + TimeUnit.MILLISECONDS.toNanos(config.holdMs),
                )
                terminal.remove(id)
                held[id] = value
                value
            }
            return awaitOutcome(active)
        }

        private fun awaitOutcome(active: Held): Action {
            while (true) {
                val step = synchronized(lock) {
                    when (active.lifecycle) {
                        Lifecycle.RELEASED -> WaitStep.Done(active.action)
                        Lifecycle.DEADLINE_EXPIRED,
                        Lifecycle.CLIENT_INTERRUPTED,
                        -> WaitStep.Done(Action.PassThrough)
                        Lifecycle.HELD -> {
                            val remaining = active.deadlineNanos - nanoTime()
                            if (remaining <= 0L) {
                                transitionLocked(
                                    active,
                                    TerminalState.DEADLINE_EXPIRED,
                                    Action.PassThrough,
                                    null,
                                )
                                WaitStep.Done(Action.PassThrough)
                            } else {
                                WaitStep.Await(remaining)
                            }
                        }
                    }
                }
                when (step) {
                    is WaitStep.Done -> return step.action
                    is WaitStep.Await -> {
                        try {
                            active.latch.await(step.remainingNanos, TimeUnit.NANOSECONDS)
                        } catch (_: InterruptedException) {
                            val result = synchronized(lock) {
                                if (active.lifecycle == Lifecycle.HELD) {
                                    transitionLocked(
                                        active,
                                        TerminalState.CLIENT_INTERRUPTED,
                                        Action.PassThrough,
                                        null,
                                    )
                                    Action.PassThrough
                                } else if (active.lifecycle == Lifecycle.RELEASED) {
                                    active.action
                                } else {
                                    Action.PassThrough
                                }
                            }
                            Thread.currentThread().interrupt()
                            return result
                        }
                    }
                }
            }
        }

        /** Caller holds [lock]. */
        private fun pruneExpiredLocked(nowNanos: Long) {
            val expired = held.values
                .filter { it.lifecycle == Lifecycle.HELD && it.deadlineNanos - nowNanos <= 0L }
                .toList()
            expired.forEach {
                transitionLocked(
                    it,
                    TerminalState.DEADLINE_EXPIRED,
                    Action.PassThrough,
                    null,
                )
            }
        }

        /** Caller holds [lock]; this is the sole held -> terminal transition. */
        private fun transitionLocked(
            active: Held,
            state: TerminalState,
            action: Action,
            actionName: String?,
        ): TerminalRecord {
            check(active.lifecycle == Lifecycle.HELD) { "held flow already terminal" }
            check(held[active.id] === active) { "held flow is not the active registry entry" }
            active.lifecycle = when (state) {
                TerminalState.RELEASED -> Lifecycle.RELEASED
                TerminalState.DEADLINE_EXPIRED -> Lifecycle.DEADLINE_EXPIRED
                TerminalState.CLIENT_INTERRUPTED -> Lifecycle.CLIENT_INTERRUPTED
            }
            active.action = action
            held.remove(active.id)
            val record = TerminalRecord(
                id = active.id,
                state = state,
                heldAtMs = active.heldAtMs,
                expiresAtMs = active.expiresAtMs,
                terminalAtMs = wallTimeMs(),
                action = actionName,
                summary = active.summary,
            )
            recordTerminalLocked(record)
            // Publish action/state/history before waking the application call.
            active.latch.countDown()
            return record
        }

        /** Caller holds [lock]. */
        private fun recordTerminalLocked(record: TerminalRecord) {
            terminal.remove(record.id)
            terminal[record.id] = record
            while (terminal.size > terminalHistoryCap) {
                val oldest = terminal.keys.firstOrNull() ?: break
                terminal.remove(oldest)
            }
        }

        private fun heldJson(active: Held): JSONObject = copyJson(active.summary).apply {
            put("id", active.id)
            put("state", "held")
            put("heldAtMs", active.heldAtMs)
            put("expiresAtMs", active.expiresAtMs)
            put("holdMs", active.config.holdMs)
        }
    }

    private val registry = Registry()

    /** Arm interception from a JSON spec: `{host,path,method,operationName,holdMs}`. */
    fun arm(spec: JSONObject) = registry.arm(spec)

    fun disarm() = registry.disarm()

    fun isArmed(): Boolean = registry.isArmed()

    /** Status snapshot: armed matcher, live holds, and bounded terminal history. */
    fun status(): JSONObject = registry.status()

    /**
     * Resolve a held flow by id. Kept as a Boolean API for existing embedded
     * integrations; the control server uses [resolveDetailed] so it can report
     * the exact terminal result instead of a generic "not found".
     */
    fun resolve(id: String, action: JSONObject): Boolean = registry.resolve(id, action).released

    internal fun resolveDetailed(id: String, action: JSONObject): ResolveResult =
        registry.resolve(id, action)

    /**
     * Called by the HTTP hook at the response phase. If a matcher is armed and
     * this flow matches, block until the CLI resolves it or the hold expires
     * (fail-open). Returns the action to apply.
     */
    fun maybeHold(
        id: String,
        method: String,
        host: String,
        path: String,
        operationName: String?,
        summary: JSONObject,
    ): Action = registry.maybeHold(id, method, host, path, operationName, summary)

    private fun matches(
        matcher: Matcher,
        method: String,
        host: String,
        path: String,
        operationName: String?,
    ): Boolean {
        matcher.host?.let { if (!host.contains(it, ignoreCase = true)) return false }
        matcher.path?.let { if (!path.contains(it, ignoreCase = true)) return false }
        matcher.method?.let { if (!method.equals(it, ignoreCase = true)) return false }
        matcher.operationName?.let { if (!it.equals(operationName, ignoreCase = false)) return false }
        return true
    }

    private fun parseAction(action: JSONObject): Action = when {
        action.optBoolean("drop", false) -> Action.Drop
        action.has("status") || action.has("body") -> Action.Mutate(
            status = if (action.has("status")) action.optInt("status") else null,
            body = action.optStringOrNull("body"),
            contentType = action.optStringOrNull("contentType"),
        )
        else -> Action.PassThrough
    }

    private fun actionName(action: Action): String = when (action) {
        Action.PassThrough -> "resume"
        Action.Drop -> "drop"
        is Action.Mutate -> "mutate"
    }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (has(key) && !isNull(key)) optString(key).ifEmpty { null } else null

    private fun copyJson(source: JSONObject): JSONObject = JSONObject(source.toString())

    private fun saturatingAdd(left: Long, right: Long): Long =
        if (left > Long.MAX_VALUE - right) Long.MAX_VALUE else left + right

    private const val MIN_HOLD_MS = 100L
    private const val DEFAULT_HOLD_MS = 30_000L
    private const val MAX_HOLD_MS = 120_000L
    private const val MAX_HELD_FLOWS = 32
    private const val TERMINAL_HISTORY_CAP = 256
}
