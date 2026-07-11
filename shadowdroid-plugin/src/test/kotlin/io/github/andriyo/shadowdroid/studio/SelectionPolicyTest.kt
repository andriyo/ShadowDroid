package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class SelectionPolicyTest {
    private val sessions = listOf("session_7", "session_9")

    @Test
    fun sessionSelectorAcceptsStableIdOrCurrentIndex() {
        assertEquals(1, SelectionPolicy.sessionIndex("session_9", sessions))
        assertEquals(0, SelectionPolicy.sessionIndex("0", sessions))
    }

    @Test
    fun invalidExplicitSessionNeverFallsBack() {
        val error = assertThrows(IllegalArgumentException::class.java) {
            SelectionPolicy.sessionIndex("session_missing", sessions)
        }
        assertEquals("debugger session not found: session_missing", error.message)

        val staleIndex = assertThrows(IllegalArgumentException::class.java) {
            SelectionPolicy.sessionIndex("99", sessions)
        }
        assertEquals("debugger session not found: 99", staleIndex.message)
    }

    @Test
    fun mutationRequiresTargetWhenMoreThanOneSessionIsActive() {
        assertThrows(IllegalArgumentException::class.java) {
            SelectionPolicy.requireExplicitSessionTarget(2, null, null)
        }
        SelectionPolicy.requireExplicitSessionTarget(2, "session_7", null)
        SelectionPolicy.requireExplicitSessionTarget(2, null, "emulator-5554")
    }

    @Test
    fun projectSelectorRejectsMissingAndAmbiguousNames() {
        val projects = listOf(
            ProjectSelectorValue("app", "/workspace/one"),
            ProjectSelectorValue("app", "/workspace/two"),
        )

        assertEquals(1, SelectionPolicy.projectIndex("/workspace/two", projects))
        assertThrows(IllegalArgumentException::class.java) {
            SelectionPolicy.projectIndex("app", projects)
        }
        assertThrows(IllegalArgumentException::class.java) {
            SelectionPolicy.projectIndex("missing", projects)
        }

        assertThrows(IllegalArgumentException::class.java) {
            SelectionPolicy.requireUnambiguousProjectFallback(2, strict = true)
        }
        SelectionPolicy.requireUnambiguousProjectFallback(1, strict = true)
        SelectionPolicy.requireUnambiguousProjectFallback(2, strict = false)
    }

    @Test
    fun watchCacheKeyIncludesSessionIdentity() {
        val first = WatchCacheKey("watch_a", "session_7")
        val second = WatchCacheKey("watch_a", "session_9")

        assertNotEquals(first, second)
    }
}
