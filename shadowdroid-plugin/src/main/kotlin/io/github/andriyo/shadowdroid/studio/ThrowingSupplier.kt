package io.github.andriyo.shadowdroid.studio

internal fun interface ThrowingSupplier<T> {
    @Throws(Exception::class)
    fun get(): T
}
