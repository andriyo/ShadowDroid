package io.github.andriyo.shadowdroid.agent.okhttp

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class BodyCaptureTest {
    @Test
    fun `non-text response with zero length is exact empty`() {
        val captured = captureNonTextualResponse(0)

        assertNull(captured.text)
        assertEquals(0, captured.originalLength)
        assertFalse(captured.truncated)
        assertFalse(captured.streamed)
    }

    @Test
    fun `non-text response with declared bytes is incomplete`() {
        val captured = captureNonTextualResponse(128)

        assertNull(captured.text)
        assertEquals(128, captured.originalLength)
        assertFalse(captured.truncated)
        assertTrue(captured.streamed)
    }

    @Test
    fun `non-text response with unknown length is incomplete`() {
        val captured = captureNonTextualResponse(-1)

        assertNull(captured.text)
        assertEquals(0, captured.originalLength)
        assertFalse(captured.truncated)
        assertTrue(captured.streamed)
    }
}
