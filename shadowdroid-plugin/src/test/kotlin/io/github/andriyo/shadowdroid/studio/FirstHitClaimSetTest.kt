package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

class FirstHitClaimSetTest {
    @Test
    fun concurrentCallbacksAcceptExactlyOneFirstHitAndForgetResetsIt() {
        val claims = FirstHitClaimSet<Any>()
        val breakpoint = Any()
        val workers = 16
        val start = CountDownLatch(1)
        val done = CountDownLatch(workers)
        val accepted = AtomicInteger()
        val executor = Executors.newFixedThreadPool(workers)
        try {
            repeat(workers) {
                executor.execute {
                    start.await()
                    if (claims.claim(breakpoint)) accepted.incrementAndGet()
                    done.countDown()
                }
            }
            start.countDown()
            assertTrue(done.await(5, TimeUnit.SECONDS))
            assertEquals(1, accepted.get())
            assertFalse(claims.claim(breakpoint))

            claims.forget(breakpoint)
            assertTrue(claims.claim(breakpoint))
        } finally {
            executor.shutdownNow()
        }
    }
}
