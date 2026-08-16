package io.github.andriyo.shadowdroid.agent

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

class InterceptTest {
    @Test
    fun `release at the absolute deadline fails open and reports deadline details`() {
        val clock = FakeClock(nanos = 0L, wallMs = 1_000L)
        val registry = clock.registry()
        registry.arm(JSONObject().put("host", "old.test").put("holdMs", 100L))
        val pending = registry.startHold("f1", host = "old.test")
        awaitHeld(registry, "f1")

        // Move the injected monotonic clock to the exact absolute deadline.
        // The waiter is intentionally still asleep: resolve itself must reject
        // the late action instead of depending on timer scheduling order.
        clock.nanos.set(TimeUnit.MILLISECONDS.toNanos(100L))
        clock.wallMs.set(1_100L)
        val result = registry.resolve("f1", JSONObject().put("drop", true))

        assertEquals(Intercept.ResultCode.DEADLINE_EXPIRED, result.code)
        assertFalse(result.released)
        assertEquals(1_000L, result.terminal?.heldAtMs)
        assertEquals(1_100L, result.terminal?.expiresAtMs)
        assertEquals(1_100L, result.terminal?.terminalAtMs)
        pending.join()
        assertSame(Intercept.Action.PassThrough, pending.action.get())
        assertEquals(
            Intercept.ResultCode.DEADLINE_EXPIRED,
            registry.resolve("f1", JSONObject()).code,
        )
    }

    @Test
    fun `concurrent resolvers claim a held flow exactly once`() {
        val clock = FakeClock()
        val registry = clock.registry()
        registry.arm(JSONObject().put("holdMs", 1_000L))
        val pending = registry.startHold("f-race")
        awaitHeld(registry, "f-race")

        val racers = 24
        val ready = CountDownLatch(racers)
        val start = CountDownLatch(1)
        val executor = Executors.newFixedThreadPool(racers)
        try {
            val futures = List(racers) {
                executor.submit<Intercept.ResolveResult> {
                    ready.countDown()
                    start.await()
                    registry.resolve("f-race", JSONObject().put("drop", true))
                }
            }
            assertTrue(ready.await(2, TimeUnit.SECONDS))
            start.countDown()
            val results = futures.map { it.get(2, TimeUnit.SECONDS) }

            assertEquals(1, results.count { it.code == Intercept.ResultCode.RELEASED })
            assertEquals(
                racers - 1,
                results.count { it.code == Intercept.ResultCode.ALREADY_RELEASED },
            )
            pending.join()
            assertSame(Intercept.Action.Drop, pending.action.get())
        } finally {
            executor.shutdownNow()
        }
    }

    @Test
    fun `release claimed before deadline survives delayed wake and later clock advance`() {
        val clock = FakeClock(nanos = 10L, wallMs = 3_000L)
        val registry = clock.registry()
        registry.arm(JSONObject().put("holdMs", 100L))
        val pending = registry.startHold("f-release-first")
        awaitHeld(registry, "f-release-first")

        val released = registry.resolve("f-release-first", JSONObject().put("drop", true))
        clock.nanos.addAndGet(TimeUnit.SECONDS.toNanos(1L))
        clock.wallMs.addAndGet(1_000L)

        assertEquals(Intercept.ResultCode.RELEASED, released.code)
        pending.thread.interrupt()
        pending.join()
        assertSame(Intercept.Action.Drop, pending.action.get())
        assertEquals(
            Intercept.ResultCode.ALREADY_RELEASED,
            registry.resolve("f-release-first", JSONObject()).code,
        )
        assertEquals(
            "released",
            registry.status().getJSONArray("terminal").getJSONObject(0).getString("state"),
        )
    }

    @Test
    fun `monotonic deadline remains correct across nanoTime wrap`() {
        val start = Long.MAX_VALUE - TimeUnit.MILLISECONDS.toNanos(50L)
        val clock = FakeClock(nanos = start, wallMs = 4_000L)
        val registry = clock.registry()
        registry.arm(JSONObject().put("holdMs", 100L))
        val pending = registry.startHold("f-wrap")
        awaitHeld(registry, "f-wrap")

        // AtomicLong deliberately wraps with normal two's-complement arithmetic,
        // matching System.nanoTime's permitted rollover behavior.
        clock.nanos.addAndGet(TimeUnit.MILLISECONDS.toNanos(99L))
        assertTrue(registry.resolve("f-wrap", JSONObject().put("drop", true)).released)
        pending.join()
        assertSame(Intercept.Action.Drop, pending.action.get())
    }

    @Test
    fun `status prunes an expired hold and wakes waiter fail open`() {
        val clock = FakeClock(nanos = 5L, wallMs = 10_000L)
        val registry = clock.registry()
        registry.arm(JSONObject().put("holdMs", 100L))
        val pending = registry.startHold("f-status")
        awaitHeld(registry, "f-status")

        clock.nanos.addAndGet(TimeUnit.MILLISECONDS.toNanos(101L))
        clock.wallMs.set(10_101L)
        val status = registry.status()

        assertEquals(0, status.getJSONArray("held").length())
        val terminal = status.getJSONArray("terminal").getJSONObject(0)
        assertEquals("f-status", terminal.getString("id"))
        assertEquals("deadline_expired", terminal.getString("state"))
        assertEquals(10_000L, terminal.getLong("heldAtMs"))
        assertEquals(10_100L, terminal.getLong("expiresAtMs"))
        assertEquals(10_101L, terminal.getLong("terminalAtMs"))
        pending.join()
        assertSame(Intercept.Action.PassThrough, pending.action.get())
    }

    @Test
    fun `rearm publishes matcher and hold duration together without changing live holds`() {
        val clock = FakeClock(nanos = 0L, wallMs = 2_000L)
        val registry = clock.registry()
        registry.arm(JSONObject().put("host", "old.test").put("holdMs", 100L))
        val oldPending = registry.startHold("f-old", host = "old.test")
        awaitHeld(registry, "f-old")

        clock.wallMs.set(2_010L)
        registry.arm(JSONObject().put("host", "new.test").put("holdMs", 900L))
        val afterRearm = registry.status()
        assertEquals("new.test", afterRearm.getJSONObject("matcher").getString("host"))
        assertEquals(900L, afterRearm.getJSONObject("matcher").getLong("holdMs"))
        val oldHeld = afterRearm.getJSONArray("held").getJSONObject(0)
        assertEquals("f-old", oldHeld.getString("id"))
        assertEquals(100L, oldHeld.getLong("holdMs"))
        assertEquals(2_100L, oldHeld.getLong("expiresAtMs"))

        // The old matcher no longer holds, while the new matcher uses the new
        // duration. This would expose a split matcher/holdMs publication.
        assertSame(
            Intercept.Action.PassThrough,
            registry.maybeHold(
                "f-old-miss",
                "GET",
                "old.test",
                "/",
                null,
                summary("f-old-miss"),
            ),
        )
        val newPending = registry.startHold("f-new", host = "new.test")
        val both = awaitHeld(registry, "f-new")
        val newHeld = (0 until both.length())
            .map { both.getJSONObject(it) }
            .single { it.getString("id") == "f-new" }
        assertEquals(900L, newHeld.getLong("holdMs"))
        assertEquals(2_910L, newHeld.getLong("expiresAtMs"))

        assertTrue(registry.resolve("f-old", JSONObject().put("drop", true)).released)
        assertTrue(registry.resolve("f-new", JSONObject()).released)
        oldPending.join()
        newPending.join()
        assertSame(Intercept.Action.Drop, oldPending.action.get())
        assertSame(Intercept.Action.PassThrough, newPending.action.get())
    }

    @Test
    fun `interrupted waiter becomes terminal and later action cannot succeed`() {
        val clock = FakeClock()
        val registry = clock.registry()
        registry.arm(JSONObject().put("holdMs", 1_000L))
        val pending = registry.startHold("f-interrupted")
        awaitHeld(registry, "f-interrupted")

        pending.thread.interrupt()
        pending.join()

        assertSame(Intercept.Action.PassThrough, pending.action.get())
        assertTrue(pending.interruptedAfter.get())
        val status = registry.status()
        assertEquals(0, status.getJSONArray("held").length())
        assertEquals(
            "client_interrupted",
            status.getJSONArray("terminal").getJSONObject(0).getString("state"),
        )
        assertEquals(
            Intercept.ResultCode.CLIENT_INTERRUPTED,
            registry.resolve("f-interrupted", JSONObject().put("drop", true)).code,
        )
    }

    @Test
    fun `terminal history is bounded and evicted ids become unknown`() {
        val clock = FakeClock()
        val registry = clock.registry(terminalHistoryCap = 2)
        registry.arm(JSONObject().put("holdMs", 1_000L))

        for (index in 1..3) {
            val id = "f$index"
            val pending = registry.startHold(id)
            awaitHeld(registry, id)
            assertTrue(registry.resolve(id, JSONObject()).released)
            pending.join()
        }

        val terminal = registry.status().getJSONArray("terminal")
        assertEquals(2, terminal.length())
        assertEquals("f2", terminal.getJSONObject(0).getString("id"))
        assertEquals("f3", terminal.getJSONObject(1).getString("id"))
        assertEquals(Intercept.ResultCode.UNKNOWN_ID, registry.resolve("f1", JSONObject()).code)
        assertEquals(
            Intercept.ResultCode.ALREADY_RELEASED,
            registry.resolve("f3", JSONObject()).code,
        )
    }

    @Test
    fun `active hold budget fails open instead of retaining unbounded calls`() {
        val clock = FakeClock()
        val registry = clock.registry(maxHeldFlows = 1)
        registry.arm(JSONObject().put("holdMs", 1_000L))
        val first = registry.startHold("f-cap-1")
        awaitHeld(registry, "f-cap-1")

        val second = registry.maybeHold(
            id = "f-cap-2",
            method = "GET",
            host = "example.test",
            path = "/resource",
            operationName = null,
            summary = summary("f-cap-2"),
        )

        assertSame(Intercept.Action.PassThrough, second)
        val status = registry.status()
        assertEquals(1, status.getJSONArray("held").length())
        assertEquals(1L, status.getLong("rejectedHolds"))
        assertTrue(registry.resolve("f-cap-1", JSONObject()).released)
        first.join()
    }

    private class FakeClock(
        nanos: Long = 0L,
        wallMs: Long = 1_000L,
    ) {
        val nanos = AtomicLong(nanos)
        val wallMs = AtomicLong(wallMs)

        fun registry(
            terminalHistoryCap: Int = 256,
            maxHeldFlows: Int = 32,
        ): Intercept.Registry =
            Intercept.Registry(nanos::get, wallMs::get, terminalHistoryCap, maxHeldFlows)
    }

    private class PendingHold(
        val thread: Thread,
        val action: AtomicReference<Intercept.Action?>,
        val interruptedAfter: AtomicBoolean,
    ) {
        fun join() {
            thread.join(2_000L)
            assertFalse("held-flow waiter did not finish", thread.isAlive)
        }
    }

    private fun Intercept.Registry.startHold(
        id: String,
        host: String = "example.test",
    ): PendingHold {
        val action = AtomicReference<Intercept.Action?>()
        val interruptedAfter = AtomicBoolean(false)
        val thread = Thread {
            action.set(
                maybeHold(
                    id = id,
                    method = "GET",
                    host = host,
                    path = "/resource",
                    operationName = null,
                    summary = summary(id),
                ),
            )
            interruptedAfter.set(Thread.currentThread().isInterrupted)
        }
        thread.name = "InterceptTest-$id"
        thread.start()
        return PendingHold(thread, action, interruptedAfter)
    }

    private fun awaitHeld(registry: Intercept.Registry, id: String): org.json.JSONArray {
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2L)
        while (System.nanoTime() < deadline) {
            val held = registry.status().getJSONArray("held")
            if ((0 until held.length()).any { held.getJSONObject(it).getString("id") == id }) {
                return held
            }
            Thread.sleep(2L)
        }
        throw AssertionError("flow '$id' was not held before the test deadline")
    }

    private fun summary(id: String): JSONObject = JSONObject()
        .put("id", id)
        .put("method", "GET")
        .put("host", "example.test")
        .put("path", "/resource")
}
