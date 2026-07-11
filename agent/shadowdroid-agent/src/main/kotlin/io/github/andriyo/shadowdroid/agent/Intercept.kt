package io.github.andriyo.shadowdroid.agent

import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * In-app, agent-in-the-loop interception — the same model as the host proxy
 * (`net intercept`/`resume`/`drop`), but through a registered in-app capture
 * provider. The current companion is an OkHttp application interceptor, so it
 * can handle certificate-pinned OkHttp calls but not Cronet, QUIC, or arbitrary
 * HTTP clients.
 *
 * A single matcher is armed at a time. When the HTTP hook reports a matching
 * flow at the response phase it is **held** (the app's call blocks) until the
 * CLI issues `resume`/`drop`, or the hold deadline expires — in which case it
 * **fails open** (resumes unmodified) so a slow/forgotten operator never bricks
 * the app under test.
 */
object Intercept {

    /** All fields optional; null = match anything. Host/path are substrings. */
    class Matcher(
        val host: String?,
        val path: String?,
        val method: String?,
        val operationName: String?,
    )

    sealed class Action {
        /** Release unmodified. */
        object PassThrough : Action()

        /** Fail the call (the app sees an IOException). */
        object Drop : Action()

        /** Replace status and/or body before returning to the app. */
        class Mutate(val status: Int?, val body: String?, val contentType: String?) : Action()
    }

    private class Held(val id: String, val summary: JSONObject) {
        val latch = CountDownLatch(1)

        @Volatile
        var action: Action = Action.PassThrough
    }

    @Volatile
    private var matcher: Matcher? = null

    @Volatile
    private var holdMs: Long = DEFAULT_HOLD_MS

    private val held = ConcurrentHashMap<String, Held>()

    /** Arm interception from a JSON spec: `{host,path,method,operationName,holdMs}`. */
    fun arm(spec: JSONObject) {
        matcher = Matcher(
            host = spec.optStringOrNull("host"),
            path = spec.optStringOrNull("path"),
            method = spec.optStringOrNull("method"),
            operationName = spec.optStringOrNull("operationName"),
        )
        holdMs = spec.optLong("holdMs", DEFAULT_HOLD_MS).coerceIn(100L, MAX_HOLD_MS)
    }

    fun disarm() {
        matcher = null
    }

    fun isArmed(): Boolean = matcher != null

    /** Status snapshot for `aar agent`: armed matcher + currently-held flows. */
    fun status(): JSONObject {
        val m = matcher
        val matcherJson = if (m == null) {
            JSONObject.NULL
        } else {
            JSONObject().apply {
                put("host", m.host ?: JSONObject.NULL)
                put("path", m.path ?: JSONObject.NULL)
                put("method", m.method ?: JSONObject.NULL)
                put("operationName", m.operationName ?: JSONObject.NULL)
                put("holdMs", holdMs)
            }
        }
        val heldArray = JSONArray()
        held.values.forEach { heldArray.put(it.summary) }
        return JSONObject().apply {
            put("armed", m != null)
            put("matcher", matcherJson)
            put("held", heldArray)
        }
    }

    /** Resolve a held flow by id. `action` JSON: `{drop}` or `{status,body,contentType}`. */
    fun resolve(id: String, action: JSONObject): Boolean {
        val h = held[id] ?: return false
        h.action = when {
            action.optBoolean("drop", false) -> Action.Drop
            action.has("status") || action.has("body") -> Action.Mutate(
                status = if (action.has("status")) action.optInt("status") else null,
                body = action.optStringOrNull("body"),
                contentType = action.optStringOrNull("contentType"),
            )
            else -> Action.PassThrough
        }
        h.latch.countDown()
        return true
    }

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
    ): Action {
        val m = matcher ?: return Action.PassThrough
        if (!matches(m, method, host, path, operationName)) return Action.PassThrough
        val h = Held(id, summary)
        held[id] = h
        try {
            h.latch.await(holdMs, TimeUnit.MILLISECONDS)
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
        } finally {
            held.remove(id)
        }
        return h.action
    }

    private fun matches(
        m: Matcher,
        method: String,
        host: String,
        path: String,
        operationName: String?,
    ): Boolean {
        m.host?.let { if (!host.contains(it, ignoreCase = true)) return false }
        m.path?.let { if (!path.contains(it, ignoreCase = true)) return false }
        m.method?.let { if (!method.equals(it, ignoreCase = true)) return false }
        m.operationName?.let { if (!it.equals(operationName, ignoreCase = false)) return false }
        return true
    }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (has(key) && !isNull(key)) optString(key).ifEmpty { null } else null

    private const val DEFAULT_HOLD_MS = 30_000L
    private const val MAX_HOLD_MS = 120_000L
}
