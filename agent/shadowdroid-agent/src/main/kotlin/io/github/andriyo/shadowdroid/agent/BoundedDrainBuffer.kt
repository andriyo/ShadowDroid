package io.github.andriyo.shadowdroid.agent

import java.util.ArrayDeque

/** A bounded FIFO whose clear-drain is atomic with respect to producers. */
internal class BoundedDrainBuffer<T>(private val capacity: Int) {
    private val lock = Any()
    private val values = ArrayDeque<T>()

    init {
        require(capacity > 0) { "capacity must be positive" }
    }

    fun record(value: T) = synchronized(lock) {
        values.addLast(value)
        while (values.size > capacity) values.removeFirst()
    }

    fun snapshot(clear: Boolean): List<T> = synchronized(lock) {
        val snapshot = values.toList()
        if (clear) values.clear()
        snapshot
    }

    fun size(): Int = synchronized(lock) { values.size }

    fun clear() = synchronized(lock) { values.clear() }
}
