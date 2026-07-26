package io.github.andriyo.shadowdroid

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ShadowDroidServerModeTest {
    @Test
    fun serverModeRequiresExplicitTrueArgument() {
        assertFalse(serverModeEnabled(null))
        assertFalse(serverModeEnabled(""))
        assertFalse(serverModeEnabled("false"))
        assertFalse(serverModeEnabled("TRUE"))
        assertTrue(serverModeEnabled("true"))
    }
}
