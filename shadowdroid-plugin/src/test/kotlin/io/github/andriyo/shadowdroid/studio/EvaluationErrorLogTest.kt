package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class EvaluationErrorLogTest {
    private fun entry(id: String?, message: String): Map<String, Any?> =
        mapOf("breakpoint_id" to id, "message" to message)

    @Test
    fun capacityEvictsOldestFirst() {
        val log = EvaluationErrorLog(2)
        log.add(entry("bp_1", "first"))
        log.add(entry("bp_2", "second"))
        log.add(entry("bp_3", "third"))

        val recent = log.recent(8)
        assertEquals(listOf("second", "third"), recent.map { it["message"] })
    }

    @Test
    fun recentReturnsNewestLastAndHonorsLimit() {
        val log = EvaluationErrorLog(8)
        log.add(entry("bp_1", "a"))
        log.add(entry("bp_1", "b"))
        log.add(entry("bp_2", "c"))

        assertEquals(listOf("b", "c"), log.recent(2).map { it["message"] })
        assertTrue(log.recent(0).isEmpty())
    }

    @Test
    fun lastForPicksTheNewestEntryOfThatBreakpoint() {
        val log = EvaluationErrorLog(8)
        log.add(entry("bp_1", "old"))
        log.add(entry("bp_2", "other"))
        log.add(entry("bp_1", "new"))

        assertEquals("new", log.lastFor("bp_1")?.get("message"))
        assertNull(log.lastFor("bp_404"))
    }

    @Test
    fun clearForRemovesOnlyThatBreakpointsHistory() {
        val log = EvaluationErrorLog(8)
        log.add(entry("bp_1", "a"))
        log.add(entry("bp_2", "b"))
        log.clearFor("bp_1")

        assertNull(log.lastFor("bp_1"))
        assertEquals("b", log.lastFor("bp_2")?.get("message"))
    }

    @Test
    fun managedKindMatchingRequiresExactCurrentText() {
        val snapshot = mapOf(
            BreakpointExpressionGuard.KIND_CONDITION to "count == 3",
            BreakpointExpressionGuard.KIND_LOG_EXPRESSION to "user.name",
        )

        // Both expressions still in force.
        assertEquals(
            setOf(
                BreakpointExpressionGuard.KIND_CONDITION,
                BreakpointExpressionGuard.KIND_LOG_EXPRESSION,
            ),
            BreakpointExpressionGuard.managedKindsFor(snapshot, "count == 3", "user.name"),
        )
        // A user edit in the IDE hands the stock dialog behavior back.
        assertTrue(
            BreakpointExpressionGuard.managedKindsFor(snapshot, "count == 4", null).isEmpty(),
        )
        // A cleared expression is no longer managed.
        assertTrue(
            BreakpointExpressionGuard.managedKindsFor(emptyMap(), "count == 3", null).isEmpty(),
        )
    }
}
