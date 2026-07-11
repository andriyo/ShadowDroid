package io.github.andriyo.shadowdroid.agent.okhttp

import io.github.andriyo.shadowdroid.agent.Intercept
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.RequestBody
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import okio.BufferedSink
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.atomic.AtomicInteger

class ResponseMutationTest {
    @Test
    fun statusOnlyMutationPreservesOriginalBodyAndHeaders() {
        val originalBody = "complete body that was not captured".toResponseBody()
        val response = response(originalBody)
        val preview = BodyCapture("complete", originalBody.contentLength(), truncated = true)

        val result = applyResponseMutation(
            response,
            Intercept.Action.Mutate(status = 418, body = null, contentType = "text/plain"),
            preview,
            "application/json",
        )

        assertEquals(418, result.response.code)
        assertSame(originalBody, result.response.body)
        assertEquals("gzip", result.response.header("Content-Encoding"))
        assertEquals(originalBody.contentLength().toString(), result.response.header("Content-Length"))
        assertSame(preview, result.capturedBody)
        assertEquals("complete body that was not captured", result.response.body!!.string())
    }

    @Test
    fun explicitBodyMutationReplacesBodyAndFramingHeaders() {
        val response = response("old".toResponseBody())

        val result = applyResponseMutation(
            response,
            Intercept.Action.Mutate(status = null, body = "new body", contentType = "text/plain"),
            captureUtf8("old"),
            "application/json",
        )

        assertEquals(200, result.response.code)
        assertEquals(null, result.response.header("Content-Encoding"))
        assertEquals(null, result.response.header("Content-Length"))
        assertEquals("text/plain; charset=utf-8", result.response.header("Content-Type"))
        assertEquals("text/plain; charset=utf-8", result.response.body!!.contentType().toString())
        assertEquals("new body", result.response.body!!.string())
        assertEquals(8L, result.capturedBody.originalLength)
        assertFalse(result.capturedBody.truncated)
    }

    @Test
    fun captureReportsOriginalUtf8LengthAndTruncation() {
        val captured = captureUtf8("abcdef", cap = 3)

        assertEquals("abc", captured.text)
        assertEquals(6L, captured.originalLength)
        assertTrue(captured.truncated)
    }

    @Test
    fun replacementRemainsCompleteWhileCaptureIsBounded() {
        val replacement = "x".repeat(CAPTURE_BODY_CAP + 1)
        val result = applyResponseMutation(
            response("old".toResponseBody()),
            Intercept.Action.Mutate(status = null, body = replacement, contentType = "text/plain"),
            captureUtf8("old"),
            "text/plain",
        )

        assertEquals(replacement, result.response.body!!.string())
        assertEquals((CAPTURE_BODY_CAP + 1).toLong(), result.capturedBody.originalLength)
        assertEquals(CAPTURE_BODY_CAP, result.capturedBody.text!!.length)
        assertTrue(result.capturedBody.truncated)
    }

    @Test
    fun longLivedResponseTypesAreClassifiedAsStreaming() {
        assertTrue(isStreamingContentType("text/event-stream; charset=utf-8"))
        assertTrue(isStreamingContentType("multipart/x-mixed-replace"))
        assertTrue(isStreamingContentType("application/grpc+proto"))
        assertFalse(isStreamingContentType("application/json"))
    }

    @Test
    fun cappedUnknownLengthIsStreamedInsteadOfReportingLowerBoundAsExact() {
        val unknown = captureObservedResponse(ByteArray(5), declaredLength = -1, cap = 4)
        assertEquals(null, unknown.text)
        assertEquals(0L, unknown.originalLength)
        assertTrue(unknown.streamed)

        val known = captureObservedResponse(ByteArray(5), declaredLength = 12, cap = 4)
        assertEquals(12L, known.originalLength)
        assertEquals(4, known.text!!.length)
        assertTrue(known.truncated)
        assertFalse(known.streamed)
    }

    @Test
    fun largeAndUnknownRequestsAreNeverBufferedForCapture() {
        val writes = AtomicInteger()
        fun request(declaredLength: Long): Request =
            Request.Builder()
                .url("https://example.test/upload")
                .post(
                    object : RequestBody() {
                        override fun contentType() = "application/json".toMediaType()

                        override fun contentLength(): Long = declaredLength

                        override fun writeTo(sink: BufferedSink) {
                            writes.incrementAndGet()
                            sink.writeUtf8("x".repeat(CAPTURE_BODY_CAP + 100))
                        }
                    },
                )
                .build()

        val large = captureRequestBody(request(CAPTURE_BODY_CAP.toLong() + 1))
        val unknown = captureRequestBody(request(-1))

        assertEquals(0, writes.get())
        assertEquals((CAPTURE_BODY_CAP + 1).toLong(), large.originalLength)
        assertTrue(large.streamed)
        assertEquals(0L, unknown.originalLength)
        assertTrue(unknown.streamed)
    }

    @Test
    fun inaccurateSmallLengthHintStillCannotOverflowCaptureBuffer() {
        val request = Request.Builder()
            .url("https://example.test/upload")
            .post(
                object : RequestBody() {
                    override fun contentType() = "application/json".toMediaType()

                    override fun contentLength(): Long = 1

                    override fun writeTo(sink: BufferedSink) {
                        sink.writeUtf8("x".repeat(CAPTURE_BODY_CAP + 100))
                    }
                },
            )
            .build()

        val captured = captureRequestBody(request)

        assertEquals(null, captured.text)
        assertEquals(0L, captured.originalLength)
        assertTrue(captured.streamed)
    }

    private fun response(body: okhttp3.ResponseBody): Response =
        Response.Builder()
            .request(Request.Builder().url("https://example.test/path").build())
            .protocol(Protocol.HTTP_1_1)
            .message("OK")
            .code(200)
            .header("Content-Encoding", "gzip")
            .header("Content-Length", body.contentLength().toString())
            .body(body)
            .build()
}
