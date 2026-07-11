package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import java.util.concurrent.CountDownLatch
import kotlin.concurrent.thread

class SessionWatchCacheTest {
    private val key = WatchCacheKey("watch_a", "session_7")

    @Test
    fun stoppedSessionRejectsLateRefresh() {
        val cache = SessionWatchCache<String>()
        assertEquals(WatchCachePutResult.STORED, cache.putIfActive(key, "before") { true })

        cache.stopSession(key.sessionId)

        assertNull(cache[key])
        assertEquals(
            WatchCachePutResult.SESSION_STOPPED,
            cache.putIfActive(key, "late") { true },
        )
        assertNull(cache[key])
    }

    @Test
    fun stopRacingInFlightRefreshLeavesNoValue() {
        val cache = SessionWatchCache<String>()
        val refreshEntered = CountDownLatch(1)
        val releaseRefresh = CountDownLatch(1)
        val writer = thread {
            cache.putIfActive(key, "late") {
                refreshEntered.countDown()
                releaseRefresh.await()
                true
            }
        }
        refreshEntered.await()
        val stopper = thread { cache.stopSession(key.sessionId) }

        releaseRefresh.countDown()
        writer.join()
        stopper.join()

        assertNull(cache[key])
    }
}
