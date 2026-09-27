package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class BreakpointIdsTest {
    @Test
    fun idsAreShortStableAndDistinct() {
        val identity = "/work/app|kotlin-line|file:///work/app/src/Main.kt|/work/app/src/Main.kt|101|"
        val id = BreakpointIds.short(identity)
        assertTrue(id.matches(Regex("bp_[0-9a-f]{16}")))
        assertEquals(id, BreakpointIds.short(identity))
        assertNotEquals(id, BreakpointIds.short(identity.replace("|101|", "|102|")))
    }
}
