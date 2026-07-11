package io.github.andriyo.shadowdroid.agent

import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicLong

/**
 * A captured HTTP(S) flow. Serialized in the **same JSON shape as the host
 * `FlowRecord`** (`cli/src/net/flow.rs`) so the CLI's `net log` /
 * `net export fixtures` consume agent-captured and proxy-captured flows
 * identically — the whole point of in-app capture is to feed that pipeline for
 * certificate-pinned OkHttp calls when the optional OkHttp companion is wired.
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
    val reqTruncated: Boolean,
    val respTruncated: Boolean,
    val reqStreamed: Boolean,
    val streamed: Boolean,
    val modified: Boolean,
    val error: String?,
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
            put("req_truncated", reqTruncated)
            put("resp_truncated", respTruncated)
            put("req_streamed", reqStreamed)
            put("streamed", streamed)
            put("matched", if (modified) "agent" else JSONObject.NULL)
            put("modified", modified)
            put("error", error ?: JSONObject.NULL)
        }
}

/**
 * In-memory ring buffer of captured flows, drained over the control channel by
 * the CLI (`aar capture`). Thread-safe and bounded so a long session can't grow
 * unbounded inside the host app.
 */
object Capture {
    const val MISSING_PROVIDER_HINT =
        "Run `shadowdroid aar install --okhttp`, add ShadowDroidCaptureInterceptor " +
            "to every debug OkHttpClient, rebuild, and relaunch. Other HTTP stacks are not instrumented."

    private const val MAX_FLOWS = 200
    private val flows = BoundedDrainBuffer<CapturedFlow>(MAX_FLOWS)
    private val seq = AtomicLong(0)

    @Volatile
    private var providerName: String? = null

    fun registerProvider(name: String) {
        require(name.isNotBlank()) { "capture provider name must not be blank" }
        providerName = name
    }

    fun providerAvailable(): Boolean = providerName != null

    fun status(): JSONObject {
        val provider = providerName
        return JSONObject()
            .put("available", provider != null)
            .put("provider", provider ?: JSONObject.NULL)
            .put("buffered", size())
            .put("missing_provider_hint", if (provider != null) JSONObject.NULL else MISSING_PROVIDER_HINT)
    }

    fun nextId(): String = "a${seq.incrementAndGet()}"

    fun record(flow: CapturedFlow) {
        flows.record(flow)
    }

    /** Snapshot the buffered flows as a JSON array; optionally clear afterward. */
    fun drain(clear: Boolean): JSONArray {
        val array = JSONArray()
        flows.snapshot(clear).forEach { array.put(it.toJson()) }
        return array
    }

    fun size(): Int = flows.size()

    fun clear() = flows.clear()
}
