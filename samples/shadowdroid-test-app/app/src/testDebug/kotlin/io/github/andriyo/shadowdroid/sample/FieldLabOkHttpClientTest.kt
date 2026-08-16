package io.github.andriyo.shadowdroid.sample

import io.github.andriyo.shadowdroid.agent.okhttp.ShadowDroidCaptureInterceptor
import org.junit.Assert.assertEquals
import org.junit.Test

class FieldLabOkHttpClientTest {
    @Test
    fun debugClientInstallsExactlyOneShadowDroidInterceptor() {
        val interceptors =
            fieldLabOkHttpClientBuilder()
                .interceptors()
                .filterIsInstance<ShadowDroidCaptureInterceptor>()

        assertEquals(1, interceptors.size)
    }
}
