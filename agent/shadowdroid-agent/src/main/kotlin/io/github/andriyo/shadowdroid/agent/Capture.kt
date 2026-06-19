package io.github.andriyo.shadowdroid.agent

import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.ConcurrentLinkedDeque
import java.util.concurrent.atomic.AtomicLong

/**
 * A captured HTTP(S) flow. Serialized in the **same JSON shape as the host
 * `FlowRecord`** (`cli/src/net/flow.rs`) so the CLI's `net log` /
 * `net export fixtures` consume agent-captured and proxy-captured flows
 * identically — the whole point of in-app capture is to feed that pipeline for
 * cert-pinned / non-proxyable apps.
 */
class CapturedFlow(
    val id: String,
    val tsSeconds: Double,
    val method: String,
    val scheme: String,
    val host: String,
    val path: String,
    val status: Int?,
    val durMs: Long?,
    val reqType: String?,
    val respType: String?,
    val reqLen: Long,
    val respLen: Long,
    val reqBody: String?,
    val respBody: String?,
    val modified: Boolean,
) {
    fun toJson(): JSONObject =
        JSONObject().apply {
            put("id", id)
            put("ts", tsSeconds)
            put("method", method)
            put("scheme", scheme)
            put("host", host)
            put("path", path)
            put("status", status ?: JSONObject.NULL)
            put("dur_ms", durMs ?: JSONObject.NULL)
            put("req_headers", JSONArray())
            put("resp_headers", JSONArray())
            put("req_type", reqType ?: JSONObject.NULL)
            put("resp_type", respType ?: JSONObject.NULL)
            put("req_len", reqLen)
            put("resp_len", respLen)
            put("req_body", reqBody ?: JSONObject.NULL)
            put("resp_body", respBody ?: JSONObject.NULL)
            put("req_truncated", false)
            put("resp_truncated", false)
            put("matched", if (modified) "agent" else JSONObject.NULL)
            put("modified", modified)
            put("error", JSONObject.NULL)
        }
}

/**
 * In-memory ring buffer of captured flows, drained over the control channel by
 * the CLI (`aar capture`). Thread-safe and bounded so a long session can't grow
 * unbounded inside the host app.
 */
object Capture {
    private const val MAX_FLOWS = 200
    private val flows = ConcurrentLinkedDeque<CapturedFlow>()
    private val seq = AtomicLong(0)

    fun nextId(): String = "a${seq.incrementAndGet()}"

    fun record(flow: CapturedFlow) {
        flows.addLast(flow)
        while (flows.size > MAX_FLOWS) flows.pollFirst()
    }

    /** Snapshot the buffered flows as a JSON array; optionally clear afterward. */
    fun drain(clear: Boolean): JSONArray {
        val array = JSONArray()
        flows.forEach { array.put(it.toJson()) }
        if (clear) flows.clear()
        return array
    }

    fun size(): Int = flows.size

    fun clear() = flows.clear()
}
