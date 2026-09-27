package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

class CancellableHandoffTest {
    @Test
    fun workThatNeverStartsIsCancelledAndCannotRunLater() {
        val runs = AtomicInteger()
        val handoff = CancellableHandoff { runs.incrementAndGet() }
        try {
            handoff.await(50, 1_000) { " (Breakpoint Condition Error)" }
            fail("expected StudioUiBusyException")
        } catch (e: StudioUiBusyException) {
            assertTrue(e.message!!.contains("will not run"))
            assertTrue(e.message!!.contains("Breakpoint Condition Error"))
        }
        // The UI thread frees up later (the dialog closes): the step must not run.
        handoff.run()
        assertEquals(0, runs.get())
    }

    @Test
    fun workThatStartedInTimeIsAwaitedEvenIfSlow() {
        val started = CountDownLatch(1)
        val handoff = CancellableHandoff {
            started.countDown()
            Thread.sleep(200)
            "done"
        }
        val runner = Thread { handoff.run() }
        runner.start()
        assertTrue(started.await(1, TimeUnit.SECONDS))
        // Started before the 50 ms start deadline expired, so it is not cancelled.
        assertEquals("done", handoff.await(50, 5_000))
        runner.join()
    }

    @Test
    fun failuresOfTheWorkPropagateUnwrapped() {
        val handoff = CancellableHandoff<String> { throw IllegalArgumentException("bad frame") }
        handoff.run()
        try {
            handoff.await(1_000, 1_000)
            fail("expected IllegalArgumentException")
        } catch (e: IllegalArgumentException) {
            assertEquals("bad frame", e.message)
        }
    }
}
