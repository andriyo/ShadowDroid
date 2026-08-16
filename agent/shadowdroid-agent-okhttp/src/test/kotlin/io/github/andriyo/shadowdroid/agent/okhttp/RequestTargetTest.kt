package io.github.andriyo.shadowdroid.agent.okhttp

import okhttp3.HttpUrl.Companion.toHttpUrl
import org.junit.Assert.assertEquals
import org.junit.Test

class RequestTargetTest {
    @Test
    fun captureIncludesEncodedQueryForRedactionAndReplay() {
        val url =
            "https://api.example.com/v1/users?access_token=e2e-secret&email=person%40example.com&safe=visible"
                .toHttpUrl()

        assertEquals(
            "/v1/users?access_token=e2e-secret&email=person%40example.com&safe=visible",
            capturedRequestTarget(url),
        )
    }

    @Test
    fun captureOmitsQuestionMarkWhenThereIsNoQuery() {
        assertEquals("/health", capturedRequestTarget("https://api.example.com/health".toHttpUrl()))
    }
}
