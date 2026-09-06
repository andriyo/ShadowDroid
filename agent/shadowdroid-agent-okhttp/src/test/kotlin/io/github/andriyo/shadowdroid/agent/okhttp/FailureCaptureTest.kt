package io.github.andriyo.shadowdroid.agent.okhttp

import io.github.andriyo.shadowdroid.agent.Capture
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.IOException
import java.io.InterruptedIOException
import java.lang.reflect.Proxy
import java.net.ConnectException
import java.net.ServerSocket
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import javax.net.ssl.SSLHandshakeException

class FailureCaptureTest {
    @Before
    @After
    fun clearCapture() = Capture.clear()

    @Test
    fun ioFailuresAreRecordedOnceAndRethrownUnchanged() {
        val payload = "{\"operationName\":\"Refresh\"}"
        val request = Request.Builder()
            .url("https://example.test:8443/refresh?attempt=2")
            .header("X-Request-Id", "attempt-2")
            .post(payload.toRequestBody("application/json".toMediaType()))
            .build()
        for (failure in listOf(
            UnknownHostException("DNS lookup failed"),
            SSLHandshakeException("certificate rejected"),
            ConnectException("connection refused"),
            SocketTimeoutException("read timed out"),
            InterruptedIOException("Canceled"),
            IOException(),
        )) {
            val caught = runCatching {
                ShadowDroidCaptureInterceptor().intercept(chain(request) { throw failure })
            }.exceptionOrNull()

            assertSame(failure, caught)
            val flows = Capture.drain(clear = true)
            assertEquals(1, flows.length())
            val flow = flows.getJSONObject(0)
            assertEquals("POST", flow.getString("method"))
            assertEquals("https", flow.getString("scheme"))
            assertEquals("example.test", flow.getString("host"))
            assertEquals(8443, flow.getInt("port"))
            assertEquals("/refresh?attempt=2", flow.getString("path"))
            assertEquals(payload, flow.getString("req_body"))
            assertEquals(payload.length.toLong(), flow.getLong("req_len"))
            val header = flow.getJSONArray("req_headers").getJSONArray(0)
            assertEquals("X-Request-Id", header.getString(0))
            assertEquals("attempt-2", header.getString(1))
            assertTrue(flow.getLong("dur_ms") >= 0L)
            assertEquals(failure.toString(), flow.getString("error"))
            assertTrue(flow.isNull("status"))
            assertTrue(flow.isNull("resp_body"))
            assertTrue(flow.isNull("resp_type"))
            assertEquals(0, flow.getJSONArray("resp_headers").length())
            assertEquals(0L, flow.getLong("resp_len"))
            assertFalse(flow.getBoolean("modified"))
            assertFalse(flow.getBoolean("streamed"))
        }
    }

    @Test
    fun successfulResponseStillReachesTheCallerAndIsRecordedOnce() {
        val request = Request.Builder().url("https://example.test/ok").build()
        val response = Response.Builder()
            .request(request)
            .protocol(Protocol.HTTP_1_1)
            .code(200)
            .message("OK")
            .body("response body".toResponseBody("text/plain".toMediaType()))
            .build()
        val actual = ShadowDroidCaptureInterceptor().intercept(chain(request) { response })

        assertSame(response, actual)
        assertEquals("response body", actual.body!!.string())
        val flows = Capture.drain(clear = true)
        assertEquals(1, flows.length())
        assertEquals(200, flows.getJSONObject(0).getInt("status"))
        assertEquals("response body", flows.getJSONObject(0).getString("resp_body"))
        assertTrue(flows.getJSONObject(0).isNull("error"))
    }

    @Test
    fun realOkHttpCallRecordsAPeerClosingWithoutAResponse() {
        val executor = Executors.newSingleThreadExecutor()
        val client = OkHttpClient.Builder()
            .addInterceptor(ShadowDroidCaptureInterceptor())
            .retryOnConnectionFailure(false)
            .callTimeout(5, TimeUnit.SECONDS)
            .build()
        try {
            ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress()).use { server ->
                server.soTimeout = 5_000
                val peer = executor.submit {
                    server.accept().use { socket ->
                        socket.soTimeout = 5_000
                        val reader = socket.getInputStream().bufferedReader()
                        while (true) {
                            val line = reader.readLine() ?: break
                            if (line.isEmpty()) break
                        }
                        // Close after consuming the request, before any status line.
                    }
                }
                val request = Request.Builder()
                    .url("http://localhost:${server.localPort}/disconnect")
                    .build()
                val failure = runCatching { client.newCall(request).execute().use { } }.exceptionOrNull()
                peer.get(5, TimeUnit.SECONDS)

                assertTrue(failure is IOException)
                val flows = Capture.drain(clear = true)
                assertEquals(1, flows.length())
                assertEquals("/disconnect", flows.getJSONObject(0).getString("path"))
                assertTrue(flows.getJSONObject(0).isNull("status"))
                assertEquals(failure.toString(), flows.getJSONObject(0).getString("error"))
            }
        } finally {
            executor.shutdownNow()
            client.connectionPool.evictAll()
            client.dispatcher.executorService.shutdownNow()
        }
    }

    private fun chain(request: Request, proceed: () -> Response): Interceptor.Chain =
        Proxy.newProxyInstance(
            Interceptor.Chain::class.java.classLoader,
            arrayOf(Interceptor.Chain::class.java),
        ) { _, method, _ ->
            when (method.name) {
                "request" -> request
                "proceed" -> proceed()
                else -> error("unexpected chain call: ${method.name}")
            }
        } as Interceptor.Chain
}
