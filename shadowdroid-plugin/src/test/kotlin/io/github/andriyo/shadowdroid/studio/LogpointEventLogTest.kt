package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

class LogpointEventLogTest {
    private fun candidate(
        id: String = "bp_1",
        owner: String? = "agent-a",
        project: String = "demo",
        session: String = "session_1",
        device: String? = "emulator-5554",
        message: String = "value=1",
        maxChars: Int = DEFAULT_LOGPOINT_MAX_MESSAGE_CHARS,
        maxRate: Int = DEFAULT_LOGPOINT_MAX_EVENTS_PER_SECOND,
    ): LogpointEventCandidate = LogpointEventCandidate(
        breakpointId = id,
        owner = owner,
        projectName = project,
        projectPath = "/work/$project",
        session = LogpointSessionSnapshot(
            session,
            "Debug $project",
            deviceSerial = device,
            packageName = "com.example.demo",
            processName = "com.example.demo:worker",
            pid = 1234,
        ),
        file = "/work/$project/Foo.kt",
        url = "file:///work/$project/Foo.kt",
        line = 42,
        type = "kotlin-line",
        condition = "ready",
        logExpression = "value",
        logMessage = false,
        logStack = false,
        message = message,
        maxMessageChars = maxChars,
        maxEventsPerSecond = maxRate,
    )

    @Test
    fun tailReadReturnsNewestEventsInSequenceOrder() {
        var now = 1_000L
        val log = LogpointEventLog(capacity = 8, streamId = "stream-test") { now++ }
        log.append(candidate(message = "one"))
        log.append(candidate(message = "two"))
        log.append(candidate(message = "three"))

        val page = log.read(after = null, limit = 2)

        assertEquals("stream-test", page.streamId)
        assertEquals(listOf(2L, 3L), page.events.map { it["seq"] })
        assertEquals(3L, page.nextCursor)
        assertEquals(3L, page.latestCursor)
        assertEquals(1L, page.oldestCursor)
        assertFalse(page.overflowed)
    }

    @Test
    fun evictionReportsCursorOverflow() {
        var now = 1_000L
        val log = LogpointEventLog(capacity = 2, streamId = "stream-test") {
            now += 1_000
            now
        }
        log.append(candidate(message = "one"))
        log.append(candidate(message = "two"))
        log.append(candidate(message = "three"))

        val page = log.read(after = 0, limit = 10)

        assertEquals(listOf(2L, 3L), page.events.map { it["seq"] })
        assertEquals(2L, page.oldestCursor)
        assertEquals(1L, page.evictedTotal)
        assertTrue(page.overflowed)
    }

    @Test
    fun longPollReturnsKnownFilteredCursorGapImmediately() {
        val log = LogpointEventLog(capacity = 2, streamId = "stream-test")
        log.append(candidate(id = "bp_target", message = "evicted match"))
        log.append(candidate(id = "bp_other", message = "retained one"))
        log.append(candidate(id = "bp_other", message = "retained two"))

        val page = log.read(
            after = 0,
            limit = 10,
            filter = LogpointEventFilter(breakpointId = "bp_target"),
            timeoutMs = 250,
        )

        assertTrue(page.overflowed)
        assertTrue(page.events.isEmpty())
        assertFalse(page.timedOut)
        assertEquals(3L, page.nextCursor)
    }

    @Test
    fun filtersAdvancePastExaminedUnrelatedEventsWithoutSkippingLimitedMatches() {
        var now = 1_000L
        val log = LogpointEventLog(capacity = 8, streamId = "stream-test") {
            now += 1_000
            now
        }
        log.append(candidate(id = "bp_a", owner = "alpha"))
        log.append(candidate(id = "bp_b", owner = "beta"))
        log.append(candidate(id = "bp_a", owner = "alpha"))

        val beta = log.read(after = 0, limit = 10, filter = LogpointEventFilter(owner = "beta"))
        assertEquals(listOf(2L), beta.events.map { it["seq"] })
        assertEquals(3L, beta.nextCursor)

        val limitedAlpha = log.read(after = 0, limit = 1, filter = LogpointEventFilter(owner = "alpha"))
        assertEquals(listOf(1L), limitedAlpha.events.map { it["seq"] })
        assertEquals(1L, limitedAlpha.nextCursor)

        val byProjectSessionAndDevice = log.read(
            after = 0,
            limit = 10,
            filter = LogpointEventFilter(
                project = "/work/demo",
                session = "session_1",
                device = "emulator-5554",
            ),
        )
        assertEquals(3, byProjectSessionAndDevice.events.size)
    }

    @Test
    fun rateLimitCountsDropsWithoutCreatingCursorHoles() {
        var now = 1_000L
        val log = LogpointEventLog(capacity = 8, streamId = "stream-test") { now }
        assertEquals(1L, log.append(candidate(maxRate = 2)))
        assertEquals(2L, log.append(candidate(maxRate = 2)))
        assertNull(log.append(candidate(maxRate = 2)))

        var page = log.read(after = 0, limit = 10)
        assertEquals(listOf(1L, 2L), page.events.map { it["seq"] })
        assertEquals(1L, page.rateLimitedTotal)

        now += 1_000
        assertEquals(3L, log.append(candidate(maxRate = 2)))
        page = log.read(after = 2, limit = 10)
        assertEquals(listOf(3L), page.events.map { it["seq"] })
        assertEquals(3L, page.latestCursor)
    }

    @Test
    fun truncationUsesUnicodeCodePointsAndNeverSplitsSurrogatePairs() {
        val log = LogpointEventLog(capacity = 8, streamId = "stream-test") { 1_000L }
        log.append(candidate(message = "a😀b", maxChars = 2))

        val event = log.read(after = 0, limit = 1).events.single()
        assertEquals("a😀", event["message"])
        assertEquals(true, event["message_truncated"])
        assertEquals(4, event["original_message_chars"])
        assertEquals(1_000L, event["timestamp_ms"])
    }

    @Test
    fun eventCarriesCachedAppIdentityAndEnforcesTheAbsoluteMessageCeiling() {
        val log = LogpointEventLog(capacity = 8, streamId = "stream-test") { 1_000L }
        log.append(
            candidate(
                message = "x".repeat(MAX_LOGPOINT_MESSAGE_CHARS + 100),
                maxChars = Int.MAX_VALUE,
            ),
        )

        val event = log.read(after = 0, limit = 1).events.single()
        assertEquals("com.example.demo", event["package"])
        assertEquals("com.example.demo:worker", event["process_name"])
        assertEquals(1234, event["pid"])
        assertEquals(MAX_LOGPOINT_MESSAGE_CHARS, (event["message"] as String).length)
        assertEquals(true, event["message_truncated"])
        assertEquals(MAX_LOGPOINT_MESSAGE_CHARS + 100, event["original_message_chars"])
    }

    @Test
    fun evaluationFailureIsARegularStructuredLogpointEvent() {
        val log = LogpointEventLog(capacity = 8, streamId = "stream-test") { 1_000L }
        log.append(
            candidate(message = "Unresolved reference: missing").copy(
                evaluationError = LogpointEvaluationError(
                    kind = BreakpointExpressionGuard.KIND_LOG_EXPRESSION,
                    title = "Error evaluating breakpoint action",
                    action = "resumed_without_dialog",
                ),
            ),
        )

        val event = log.read(after = 0, limit = 1).events.single()
        assertEquals("logpoint", event["type"])
        assertEquals(1, event["schema_version"])
        assertEquals("kotlin-line", event["breakpoint_type"])
        assertEquals("evaluation_error", event["event_kind"])
        @Suppress("UNCHECKED_CAST")
        val error = event["evaluation_error"] as Map<String, Any?>
        assertEquals(BreakpointExpressionGuard.KIND_LOG_EXPRESSION, error["kind"])
        assertEquals("resumed_without_dialog", error["action"])
        assertFalse(event.containsKey("redacted"))
    }

    @Test
    fun renderedEvaluationFailureIsClassifiedOnlyForTheConfiguredExpression() {
        val rendered =
            "Unable to evaluate the expression \"missingRuntimeValue\" : " +
                "Unresolved reference 'missingRuntimeValue'.\n"
        val error = renderedLogpointEvaluationError(rendered, "missingRuntimeValue")

        assertEquals(BreakpointExpressionGuard.KIND_LOG_EXPRESSION, error?.kind)
        assertEquals("resumed_without_dialog", error?.action)
        assertNull(
            renderedLogpointEvaluationError(
                "Unable to evaluate the expression \"other\" : failed",
                "missingRuntimeValue",
            ),
        )
        assertNull(renderedLogpointEvaluationError("ordinary rendered value", "missingRuntimeValue"))
    }

    @Test
    fun longPollWakesOnlyWhenMatchingEventArrives() {
        val log = LogpointEventLog(capacity = 8, streamId = "stream-test")
        val executor = Executors.newSingleThreadExecutor()
        try {
            val waiting = executor.submit<LogpointEventRead> {
                log.read(
                    after = 0,
                    limit = 10,
                    filter = LogpointEventFilter(breakpointId = "bp_target"),
                    timeoutMs = 2_000,
                )
            }
            log.append(candidate(id = "bp_other"))
            assertFalse(waiting.isDone)
            log.append(candidate(id = "bp_target"))

            val page = waiting.get(2, TimeUnit.SECONDS)
            assertEquals("bp_target", page.events.single()["breakpoint_id"])
            assertFalse(page.timedOut)
        } finally {
            executor.shutdownNow()
        }
    }

    @Test
    fun concurrentAppendsProduceUniqueOrderedSequences() {
        val workers = 8
        val perWorker = 100
        val log = LogpointEventLog(capacity = workers * perWorker, streamId = "stream-test")
        val executor = Executors.newFixedThreadPool(workers)
        val start = CountDownLatch(1)
        val done = CountDownLatch(workers)
        try {
            repeat(workers) { worker ->
                executor.execute {
                    start.await()
                    repeat(perWorker) { index ->
                        log.append(
                            candidate(
                                id = "bp_$worker",
                                message = "$worker:$index",
                                maxRate = 10_000,
                            ),
                        )
                    }
                    done.countDown()
                }
            }
            start.countDown()
            assertTrue(done.await(5, TimeUnit.SECONDS))

            val page = log.read(after = 0, limit = workers * perWorker)
            val sequences = page.events.map { it["seq"] as Long }
            assertEquals(workers * perWorker, sequences.size)
            assertEquals(sequences.sorted(), sequences)
            assertEquals(sequences.size, sequences.toSet().size)
        } finally {
            executor.shutdownNow()
        }
    }
}
