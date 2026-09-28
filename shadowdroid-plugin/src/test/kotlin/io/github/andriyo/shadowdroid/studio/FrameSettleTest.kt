package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class FrameSettleTest {
    private var now = 0L

    private fun <T> retry(attempt: () -> T): T =
        FrameSettle.retry(budgetMs = 1_000, intervalMs = 100, nowMs = { now }, sleep = { now += it }, attempt = attempt)

    @Test
    fun aPauseThatSettlesIsRetriedUntilItAnswers() {
        var calls = 0
        val value = retry {
            calls++
            if (calls < 3) throw FramesNotReadyException("not yet")
            "frames"
        }
        assertEquals("frames", value)
        assertEquals(3, calls)
    }

    @Test
    fun aPauseThatNeverSettlesFailsAfterTheBudget() {
        var calls = 0
        assertThrows(FramesNotReadyException::class.java) {
            retry<String> {
                calls++
                throw FramesNotReadyException("never")
            }
        }
        assertEquals(11, calls)
    }

    @Test
    fun otherFailuresAreNotRetried() {
        var calls = 0
        assertThrows(IllegalArgumentException::class.java) {
            retry<String> {
                calls++
                throw IllegalArgumentException("frame index out of bounds: 5")
            }
        }
        assertEquals(1, calls)
    }
}
