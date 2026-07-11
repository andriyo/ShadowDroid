package io.github.andriyo.shadowdroid.agent.okhttp

import okhttp3.Request
import okio.Buffer
import okio.Sink
import okio.Timeout
import okio.buffer
import java.io.IOException
import java.nio.charset.StandardCharsets
import kotlin.math.min

internal const val CAPTURE_BODY_CAP: Int = 64 * 1024

/**
 * A bounded text snapshot plus the best-known full byte length. The length is
 * exact for buffered requests, replacement bodies, and responses with a known
 * content length. A capped response of unknown length is marked streamed and
 * reports length zero rather than presenting a lower bound as exact.
 */
internal data class BodyCapture(
    val text: String?,
    val originalLength: Long,
    val truncated: Boolean,
    val streamed: Boolean = false,
)

internal fun captureUtf8(
    bytes: ByteArray,
    originalLength: Long = bytes.size.toLong(),
    cap: Int = CAPTURE_BODY_CAP,
): BodyCapture {
    require(cap >= 0) { "cap must be non-negative" }
    val observedLength = bytes.size.toLong()
    val length = originalLength.coerceAtLeast(observedLength)
    val captured = if (bytes.size > cap) bytes.copyOf(cap) else bytes
    return BodyCapture(
        text = captured.takeUnless { it.isEmpty() }?.toString(StandardCharsets.UTF_8),
        originalLength = length,
        truncated = length > cap,
    )
}

internal fun captureUtf8(text: String, cap: Int = CAPTURE_BODY_CAP): BodyCapture =
    captureUtf8(text.toByteArray(StandardCharsets.UTF_8), cap = cap)

internal fun captureObservedResponse(
    observed: ByteArray,
    declaredLength: Long,
    cap: Int = CAPTURE_BODY_CAP,
): BodyCapture {
    // A cap+1 peek proves truncation but not the full size. Treat it as a
    // streamed response with an unknown length instead of publishing the lower
    // bound as an exact FlowRecord resp_len.
    if (declaredLength < 0 && observed.size > cap) return metadataOnly(0, streamed = true)
    val length = if (declaredLength >= 0) {
        declaredLength.coerceAtLeast(observed.size.toLong())
    } else {
        observed.size.toLong()
    }
    return captureUtf8(observed, length, cap)
}

internal fun metadataOnly(length: Long, streamed: Boolean = false): BodyCapture =
    BodyCapture(
        text = null,
        originalLength = length.coerceAtLeast(0),
        truncated = false,
        streamed = streamed,
    )

internal fun isStreamingContentType(contentType: String?): Boolean {
    val type = contentType?.substringBefore(';')?.trim()?.lowercase() ?: return false
    return type == "text/event-stream" ||
        type == "multipart/x-mixed-replace" ||
        type.startsWith("application/grpc")
}

internal fun isTextualContentType(contentType: String?): Boolean {
    val type = contentType?.lowercase() ?: return false
    return TEXTUAL_HINTS.any { type.contains(it) }
}

internal fun captureRequestBody(
    request: Request,
    cap: Int = CAPTURE_BODY_CAP,
): BodyCapture {
    require(cap >= 0) { "cap must be non-negative" }
    val body = request.body ?: return metadataOnly(0)
    val declaredLength = runCatching { body.contentLength() }.getOrDefault(-1L)
    if (body.isOneShot() || body.isDuplex() || declaredLength < 0 || declaredLength > cap) {
        return metadataOnly(declaredLength, streamed = true)
    }
    if (!isTextualContentType(runCatching { body.contentType()?.toString() }.getOrNull())) {
        return metadataOnly(declaredLength)
    }

    val sink = CappedCaptureSink(cap)
    return try {
        val buffered = sink.buffer()
        body.writeTo(buffered)
        buffered.flush()
        val observedLength = sink.captured.size
        val bytes = sink.captured.readByteArray(min(observedLength, cap.toLong()))
        captureUtf8(bytes, observedLength, cap)
    } catch (_: CaptureLimitExceeded) {
        // A body that wrote more than its declared length is still bounded and
        // truthfully represented as streamed rather than trusting the bad hint.
        metadataOnly(0, streamed = true)
    } catch (_: Throwable) {
        metadataOnly(declaredLength)
    }
}

private class CappedCaptureSink(private val cap: Int) : Sink {
    val captured = Buffer()

    override fun write(source: Buffer, byteCount: Long) {
        val remaining = (cap.toLong() + 1 - captured.size).coerceAtLeast(0)
        val keep = min(byteCount, remaining)
        if (keep > 0) captured.write(source, keep)
        val discard = byteCount - keep
        if (discard > 0) source.skip(discard)
        if (captured.size > cap) throw CaptureLimitExceeded()
    }

    override fun flush() = Unit

    override fun close() = Unit

    override fun timeout(): Timeout = Timeout.NONE
}

private class CaptureLimitExceeded : IOException()

private val TEXTUAL_HINTS =
    listOf("json", "text", "xml", "graphql", "x-www-form-urlencoded", "javascript")
