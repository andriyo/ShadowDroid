package io.github.andriyo.shadowdroid.sample

import io.github.andriyo.shadowdroid.agent.okhttp.ShadowDroidCaptureInterceptor
import okhttp3.OkHttpClient

/** Adds ShadowDroid's optional plaintext capture/intercept hook to debug clients. */
internal fun fieldLabOkHttpClientBuilder(): OkHttpClient.Builder =
    OkHttpClient.Builder()
        .addInterceptor(ShadowDroidCaptureInterceptor())
