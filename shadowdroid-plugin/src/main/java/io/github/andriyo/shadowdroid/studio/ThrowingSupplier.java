package io.github.andriyo.shadowdroid.studio;

interface ThrowingSupplier<T> {
    T get() throws Exception;
}
