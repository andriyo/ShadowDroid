package io.github.andriyo.shadowdroid.agent.okhttp

import io.github.andriyo.shadowdroid.agent.Capture
import io.github.andriyo.shadowdroid.agent.CapturedFlow
import io.github.andriyo.shadowdroid.agent.Intercept
import okhttp3.Interceptor
import okhttp3.Response
import org.json.JSONObject
import java.io.IOException

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
 * Because it runs as an **OkHttp application interceptor** (above OkHttp's
 * TLS/connection layer), it observes decrypted OkHttp requests and responses,
 * including certificate-pinned OkHttp traffic that a host MITM proxy cannot
 * reach. It does not instrument other clients or stacks such as Cronet or QUIC.
 * Captured flows are buffered in [Capture] (drained by `shadowdroid aar
 * capture`); when armed via `shadowdroid aar intercept`, a matching OkHttp flow
 * is held and can be mutated or dropped ([Intercept]).
 *
 * Capture bookkeeping is best-effort. Explicit interception actions are the
 * only path intended to change the host call: resume may mutate the response,
 * and drop deliberately fails it with an [IOException].
 */
class ShadowDroidCaptureInterceptor : Interceptor {

    init {
        Capture.registerProvider(PROVIDER_NAME)
    }

    override fun intercept(chain: Interceptor.Chain): Response {
        val request = chain.request()
        val id = Capture.nextId()
        val startNs = System.nanoTime()

        val reqType = runCatching { request.body?.contentType()?.toString() }.getOrNull()
        val reqBody = captureRequestBody(request)
        val operationName = operationName(reqBody.text)

        val response = chain.proceed(request)
        val durMs = (System.nanoTime() - startNs) / 1_000_000

        val url = request.url
        val host = url.host
        val path = url.encodedPath
        val respType = runCatching { response.body?.contentType()?.toString() }.getOrNull()
        val respBody = readResponseBody(response, respType)

        // Decide (and possibly block) only when something is armed, keeping the
        // hold machinery out of the normal capture path.
        var out = response
        var modified = false
        var finalStatus: Int? = response.code
        var finalBody = respBody
        var finalRespType = respType
        if (Intercept.isArmed()) {
            val summary = JSONObject().apply {
                put("id", id)
                put("method", request.method)
                put("host", host)
                put("path", path)
                put("status", response.code)
                put("operationName", operationName ?: JSONObject.NULL)
                put("resp_preview", respBody.text?.take(PREVIEW_CAP) ?: JSONObject.NULL)
            }
            when (val action = Intercept.maybeHold(id, request.method, host, path, operationName, summary)) {
                is Intercept.Action.Drop -> {
                    response.close()
                    recordFlow(
                        id,
                        request,
                        url,
                        durMs,
                        reqType,
                        respType,
                        reqBody,
                        respBody.copy(streamed = false),
                        response.code,
                        true,
                        "dropped by ShadowDroid agent",
                    )
                    throw IOException("dropped by ShadowDroid agent")
                }

                is Intercept.Action.Mutate -> {
                    val mutation = applyResponseMutation(response, action, respBody, respType)
                    out = mutation.response
                    modified = true
                    finalStatus = mutation.response.code
                    finalBody = mutation.capturedBody
                    finalRespType = mutation.responseType
                }

                Intercept.Action.PassThrough -> Unit
            }
        }

        recordFlow(id, request, url, durMs, reqType, finalRespType, reqBody, finalBody, finalStatus, modified, null)
        return out
    }

    private fun recordFlow(
        id: String,
        request: okhttp3.Request,
        url: okhttp3.HttpUrl,
        durMs: Long,
        reqType: String?,
        respType: String?,
        reqBody: BodyCapture,
        respBody: BodyCapture,
        status: Int?,
        modified: Boolean,
        note: String?,
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
                    reqLen = reqBody.originalLength,
                    respLen = respBody.originalLength,
                    reqBody = reqBody.text,
                    respBody = respBody.text,
                    reqTruncated = reqBody.truncated,
                    respTruncated = respBody.truncated,
                    reqStreamed = reqBody.streamed,
                    streamed = respBody.streamed,
                    modified = modified,
                    error = note,
                ),
            )
        }
    }

    private fun readResponseBody(response: Response, responseType: String?): BodyCapture {
        val body = response.body ?: return metadataOnly(0)
        val declaredLength = runCatching { body.contentLength() }.getOrDefault(-1L)
        // Peeking a long-lived response would delay delivery until the cap is
        // filled (or indefinitely for a quiet stream). Preserve it untouched.
        if (isStreamingContentType(responseType)) return metadataOnly(declaredLength, streamed = true)
        if (!isTextualContentType(responseType)) return metadataOnly(declaredLength)
        return runCatching {
            // One extra byte distinguishes an exactly-at-cap body from a body
            // whose total size is unknown and exceeds the capture cap.
            val observed = response.peekBody(CAPTURE_BODY_CAP.toLong() + 1).bytes()
            captureObservedResponse(observed, declaredLength)
        }.getOrElse { metadataOnly(declaredLength, streamed = true) }
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

    private companion object {
        const val PROVIDER_NAME = "okhttp"
        const val PREVIEW_CAP = 512
    }
}
