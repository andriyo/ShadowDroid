package io.github.andriyo.shadowdroid.studio

internal data class Response(
    val status: Int,
    val body: String,
) {
    fun status(): Int = status
    fun body(): String = body
}
