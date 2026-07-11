package io.github.andriyo.shadowdroid.agent

import org.junit.Assert.assertEquals
import org.junit.Test
import java.util.Collections
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

class BoundedDrainBufferTest {
    @Test
    fun concurrentClearDrainsNeverDeleteAnUnreturnedRecord() {
        val total = 20_000
        val buffer = BoundedDrainBuffer<Int>(total)
        val producerDone = AtomicBoolean(false)
        val returned = Collections.synchronizedList(mutableListOf<Int>())

        val producer = thread {
            for (value in 0 until total) buffer.record(value)
            producerDone.set(true)
        }
        val drainer = thread {
            while (!producerDone.get() || buffer.size() > 0) {
                returned += buffer.snapshot(clear = true)
                Thread.yield()
            }
        }
        producer.join()
        drainer.join()
        returned += buffer.snapshot(clear = true)

        assertEquals((0 until total).toSet(), returned.toSet())
        assertEquals(total, returned.size)
    }

    @Test
    fun capacityDropsOnlyTheOldestValues() {
        val buffer = BoundedDrainBuffer<Int>(2)
        buffer.record(1)
        buffer.record(2)
        buffer.record(3)

        assertEquals(listOf(2, 3), buffer.snapshot(clear = true))
        assertEquals(0, buffer.size())
    }
}
