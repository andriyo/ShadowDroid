package io.github.andriyo.shadowdroid.agent.okhttp

import io.github.andriyo.shadowdroid.agent.Capture
import io.github.andriyo.shadowdroid.agent.CapturedFlow
import io.github.andriyo.shadowdroid.agent.Intercept
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import okio.Buffer
import org.json.JSONObject
import java.io.IOException
import java.util.zip.GZIPInputStream

/**
 * The single piece of in-app capture that needs the host app's cooperation:
 * add this to your OkHttp client in debug builds —
 *
 * ```kotlin
 * OkHttpClient.Builder()
 *     .addInterceptor(ShadowDroidCaptureInterceptor()) // debug-only
 *     .build()
 * ```
 *
 * Because it runs as an **application** interceptor (above the TLS/connection
 * layer), it observes the decrypted request/response regardless of certificate
 * pinning, Cronet, or QUIC — the exact traffic the host MITM proxy cannot reach.
 * Captured flows are buffered in [Capture] (drained by `shadowdroid aar
 * capture`); when armed via `shadowdroid aar intercept`, a matching flow is
 * held and can be mutated or dropped ([Intercept]).
 *
 * Best-effort and self-contained: any failure in capture/intercept falls back
 * to passing the original response through, so the debug hook can never break
 * the host app's networking.
 */
class ShadowDroidCaptureInterceptor : Interceptor {

    override fun intercept(chain: Interceptor.Chain): Response {
        val request = chain.request()
        val id = Capture.nextId()
        val startNs = System.nanoTime()

        val reqType = runCatching { request.body?.contentType()?.toString() }.getOrNull()
        val reqBody = readRequestBody(request)
        val operationName = operationName(reqBody)

        val response = chain.proceed(request)
        val durMs = (System.nanoTime() - startNs) / 1_000_000

        val url = request.url
        val host = url.host
        val path = url.encodedPath
        val respType = runCatching { response.body?.contentType()?.toString() }.getOrNull()
        // As a *network* interceptor the body can still be gzip-encoded (the
        // transparent gunzip runs above us), so decode by Content-Encoding.
        val gzipped = response.header("Content-Encoding")?.contains("gzip", ignoreCase = true) == true
        val respBody = if (isTextual(respType)) {
            runCatching {
                val peeked = response.peekBody(BODY_CAP.toLong())
                if (gzipped) {
                    GZIPInputStream(peeked.byteStream()).bufferedReader(Charsets.UTF_8).use { it.readText() }
                } else {
                    peeked.string()
                }
            }.getOrNull()
        } else {
            null
        }

        // Decide (and possibly block) only when something is armed — keeps the
        // pure-capture path allocation-free and non-blocking.
        var out = response
        var modified = false
        var finalStatus: Int? = response.code
        var finalBody = respBody
        if (Intercept.isArmed()) {
            val summary = JSONObject().apply {
                put("id", id)
                put("method", request.method)
                put("host", host)
                put("path", path)
                put("status", response.code)
                put("operationName", operationName ?: JSONObject.NULL)
                put("resp_preview", respBody?.take(PREVIEW_CAP) ?: JSONObject.NULL)
            }
            when (val action = Intercept.maybeHold(id, request.method, host, path, operationName, summary)) {
                is Intercept.Action.Drop -> {
                    response.close()
                    recordFlow(id, request, url, durMs, reqType, respType, reqBody, null, null, true, "dropped")
                    throw IOException("dropped by ShadowDroid agent")
                }

                is Intercept.Action.Mutate -> {
                    val newStatus = action.status ?: response.code
                    val newBody = action.body ?: respBody.orEmpty()
                    val mediaType = (action.contentType ?: respType ?: "application/json").toMediaTypeOrNull()
                    out = response.newBuilder()
                        .code(newStatus)
                        // We hand back a plaintext body, so strip framing headers
                        // that would make the layers above us mis-decode it.
                        .removeHeader("Content-Encoding")
                        .removeHeader("Content-Length")
                        .body(newBody.toResponseBody(mediaType))
                        .build()
                    response.body?.close()
                    modified = true
                    finalStatus = newStatus
                    finalBody = newBody
                }

                Intercept.Action.PassThrough -> Unit
            }
        }

        recordFlow(id, request, url, durMs, reqType, respType, reqBody, finalBody, finalStatus, modified, null)
        return out
    }

    private fun recordFlow(
        id: String,
        request: okhttp3.Request,
        url: okhttp3.HttpUrl,
        durMs: Long,
        reqType: String?,
        respType: String?,
        reqBody: String?,
        respBody: String?,
        status: Int?,
        modified: Boolean,
        @Suppress("UNUSED_PARAMETER") note: String?,
    ) {
        runCatching {
            Capture.record(
                CapturedFlow(
                    id = id,
                    tsSeconds = System.currentTimeMillis() / 1000.0,
                    method = request.method,
                    scheme = url.scheme,
                    host = url.host,
                    path = url.encodedPath,
                    status = status,
                    durMs = durMs,
                    reqType = reqType,
                    respType = respType,
                    reqLen = (reqBody?.toByteArray()?.size ?: 0).toLong(),
                    respLen = (respBody?.toByteArray()?.size ?: 0).toLong(),
                    reqBody = reqBody,
                    respBody = respBody,
                    modified = modified,
                ),
            )
        }
    }

    private fun readRequestBody(request: okhttp3.Request): String? {
        val body = request.body ?: return null
        if (body.isOneShot() || body.isDuplex()) return null
        if (!isTextual(runCatching { body.contentType()?.toString() }.getOrNull())) return null
        return runCatching {
            val buffer = Buffer()
            body.writeTo(buffer)
            val text = buffer.readUtf8()
            if (text.length > BODY_CAP) text.substring(0, BODY_CAP) else text
        }.getOrNull()
    }

    private fun operationName(reqBody: String?): String? {
        val body = reqBody?.trim() ?: return null
        if (!body.startsWith("{")) return null
        return runCatching {
            val json = JSONObject(body)
            val name = json.optString("operationName")
            name.ifEmpty { null }
        }.getOrNull()
    }

    private fun isTextual(contentType: String?): Boolean {
        val ct = contentType?.lowercase() ?: return false
        return TEXTUAL_HINTS.any { ct.contains(it) }
    }

    private companion object {
        const val BODY_CAP = 64 * 1024
        const val PREVIEW_CAP = 512
        val TEXTUAL_HINTS = listOf("json", "text", "xml", "graphql", "x-www-form-urlencoded", "javascript")
    }
}
