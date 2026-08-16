package io.github.andriyo.shadowdroid.agent.okhttp

import io.github.andriyo.shadowdroid.agent.Capture
import io.github.andriyo.shadowdroid.agent.Intercept
import okhttp3.Call
import okhttp3.Connection
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.IOException
import java.util.concurrent.ExecutionException
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

class HeaderCaptureTest {
    @Before
    fun resetCaptureState() {
        Intercept.disarm()
        Capture.clear()
    }

    @After
    fun clearCaptureState() {
        Intercept.disarm()
        Capture.clear()
    }

    @Test
    fun `request and response header repetitions retain tuple order in flow JSON`() {
        val request = requestWithRepeatedHeaders()
        val response = response(
            request,
            listOf(
                "Warning" to "199 first",
                "X-Middle" to "between",
                "Warning" to "299 second",
            ),
        )

        ShadowDroidCaptureInterceptor().intercept(StaticChain(request, response))

        val flow = onlyCapturedFlow()
        assertEquals(
            listOf(
                "X-Request" to "one",
                "X-Middle" to "between",
                "X-Request" to "two",
            ),
            flow.getJSONArray("req_headers").headerPairs(),
        )
        assertEquals(
            listOf(
                "Warning" to "199 first",
                "X-Middle" to "between",
                "Warning" to "299 second",
            ),
            flow.getJSONArray("resp_headers").headerPairs(),
        )
    }

    @Test
    fun `mutated flow JSON contains final response headers in order`() {
        val request = requestWithRepeatedHeaders()
        val response = response(
            request,
            listOf(
                "Warning" to "199 first",
                "Content-Type" to "application/json",
                "Warning" to "299 second",
                "Content-Length" to "3",
            ),
        )
        val executor = Executors.newSingleThreadExecutor()
        Intercept.arm(JSONObject().put("host", "example.test").put("holdMs", 2_000))
        try {
            val pending = executor.submit<Response> {
                ShadowDroidCaptureInterceptor().intercept(StaticChain(request, response))
            }
            val heldId = awaitHeldId()
            assertTrue(
                Intercept.resolve(
                    heldId,
                    JSONObject()
                        .put("status", 299)
                        .put("body", "replacement")
                        .put("contentType", "text/plain"),
                ),
            )

            assertEquals(299, pending.get(2, TimeUnit.SECONDS).code)
            assertEquals(
                listOf(
                    "Warning" to "199 first",
                    "Warning" to "299 second",
                    "Content-Type" to "text/plain; charset=utf-8",
                ),
                onlyCapturedFlow().getJSONArray("resp_headers").headerPairs(),
            )
        } finally {
            executor.shutdownNow()
        }
    }

    @Test
    fun `dropped flow JSON retains original response header repetitions`() {
        val request = requestWithRepeatedHeaders()
        val response = response(
            request,
            listOf(
                "Set-Cookie" to "a=1",
                "X-Middle" to "between",
                "Set-Cookie" to "b=2",
            ),
        )
        val executor = Executors.newSingleThreadExecutor()
        Intercept.arm(JSONObject().put("host", "example.test").put("holdMs", 2_000))
        try {
            val pending = executor.submit<Response> {
                ShadowDroidCaptureInterceptor().intercept(StaticChain(request, response))
            }
            val heldId = awaitHeldId()
            assertTrue(Intercept.resolve(heldId, JSONObject().put("drop", true)))

            val thrown = assertThrows(ExecutionException::class.java) {
                pending.get(2, TimeUnit.SECONDS)
            }
            assertTrue(thrown.cause is IOException)
            assertEquals(
                listOf(
                    "Set-Cookie" to "a=1",
                    "X-Middle" to "between",
                    "Set-Cookie" to "b=2",
                ),
                onlyCapturedFlow().getJSONArray("resp_headers").headerPairs(),
            )
        } finally {
            executor.shutdownNow()
        }
    }

    private fun requestWithRepeatedHeaders(): Request =
        Request.Builder()
            .url("https://example.test/resource")
            .addHeader("X-Request", "one")
            .addHeader("X-Middle", "between")
            .addHeader("X-Request", "two")
            .build()

    private fun response(request: Request, headers: List<Pair<String, String>>): Response {
        val builder = Response.Builder()
            .request(request)
            .protocol(Protocol.HTTP_1_1)
            .message("OK")
            .code(200)
            .body("old".toResponseBody("application/json".toMediaType()))
        headers.forEach { (name, value) -> builder.addHeader(name, value) }
        return builder.build()
    }

    private fun onlyCapturedFlow(): JSONObject {
        val flows = Capture.drain(clear = true)
        assertEquals(1, flows.length())
        return flows.getJSONObject(0)
    }

    private fun awaitHeldId(): String {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2)
        while (System.nanoTime() < deadline) {
            val held = Intercept.status().getJSONArray("held")
            if (held.length() == 1) return held.getJSONObject(0).getString("id")
            Thread.sleep(5)
        }
        throw AssertionError("flow was not held before the test deadline")
    }
}

private fun JSONArray.headerPairs(): List<Pair<String, String>> =
    List(length()) { index ->
        val pair = getJSONArray(index)
        pair.getString(0) to pair.getString(1)
    }

private class StaticChain(
    private val request: Request,
    private val response: Response,
) : Interceptor.Chain {
    private val call: Call = OkHttpClient().newCall(request)

    override fun request(): Request = request

    override fun proceed(request: Request): Response = response

    override fun connection(): Connection? = null

    override fun call(): Call = call

    override fun connectTimeoutMillis(): Int = 10_000

    override fun withConnectTimeout(timeout: Int, unit: TimeUnit): Interceptor.Chain = this

    override fun readTimeoutMillis(): Int = 10_000

    override fun withReadTimeout(timeout: Int, unit: TimeUnit): Interceptor.Chain = this

    override fun writeTimeoutMillis(): Int = 10_000

    override fun withWriteTimeout(timeout: Int, unit: TimeUnit): Interceptor.Chain = this
}
