package io.github.andriyo.shadowdroid.sample

import okhttp3.OkHttpClient

/** Release builds do not package ShadowDroid's debug-only agent or interceptor. */
internal fun fieldLabOkHttpClientBuilder(): OkHttpClient.Builder = OkHttpClient.Builder()
