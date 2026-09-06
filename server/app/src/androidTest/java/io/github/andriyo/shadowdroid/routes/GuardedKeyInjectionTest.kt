package io.github.andriyo.shadowdroid.routes

import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class GuardedKeyInjectionTest {
    @Test
    fun deliversOneShortKeyPressWithUiAutomatorMetadata() {
        val events = mutableListOf<KeyEvent>()

        val delivered =
            injectKeyCodeWithoutIdleWait(KeyEvent.KEYCODE_BACK) { event ->
                events.add(event)
                true
            }

        assertTrue(delivered)
        assertEquals(listOf(KeyEvent.ACTION_DOWN, KeyEvent.ACTION_UP), events.map { it.action })
        events.forEach { event ->
            assertEquals(KeyEvent.KEYCODE_BACK, event.keyCode)
            assertEquals(KeyCharacterMap.VIRTUAL_KEYBOARD, event.deviceId)
            assertEquals(InputDevice.SOURCE_KEYBOARD, event.source)
            assertEquals(0, event.repeatCount)
            assertEquals(0, event.metaState)
            assertEquals(0, event.flags)
            assertEquals(0, event.scanCode)
            assertEquals(events.first().downTime, event.downTime)
            assertEquals(events.first().eventTime, event.eventTime)
        }
    }

    @Test
    fun preservesUiAutomatorModifierBitsForNumericKeyCodes() {
        // Expected masks from UiAutomator 2.4.0 InteractionController.KEY_MODIFIER.
        val modifiers =
            listOf(
                KeyEvent.KEYCODE_SHIFT_LEFT to 0x41,
                KeyEvent.KEYCODE_SHIFT_RIGHT to 0x81,
                KeyEvent.KEYCODE_ALT_LEFT to 0x12,
                KeyEvent.KEYCODE_ALT_RIGHT to 0x22,
                KeyEvent.KEYCODE_SYM to 0x4,
                KeyEvent.KEYCODE_FUNCTION to 0x8,
                KeyEvent.KEYCODE_CTRL_LEFT to 0x3000,
                KeyEvent.KEYCODE_CTRL_RIGHT to 0x5000,
                KeyEvent.KEYCODE_META_LEFT to 0x20000,
                KeyEvent.KEYCODE_META_RIGHT to 0x40000,
                KeyEvent.KEYCODE_CAPS_LOCK to 0x100000,
                KeyEvent.KEYCODE_NUM_LOCK to 0x200000,
                KeyEvent.KEYCODE_SCROLL_LOCK to 0x400000,
            )

        modifiers.forEach { (keyCode, expectedMetaState) ->
            val events = mutableListOf<KeyEvent>()
            val delivered =
                injectKeyCodeWithoutIdleWait(keyCode) { event ->
                    events.add(event)
                    true
                }

            assertTrue(delivered)
            assertEquals(listOf(KeyEvent.ACTION_DOWN, KeyEvent.ACTION_UP), events.map { it.action })
            events.forEach { event ->
                assertEquals(keyCode, event.keyCode)
                assertEquals("keyCode=$keyCode action=${event.action}", expectedMetaState, event.metaState)
            }
        }
    }

    @Test
    fun rejectedKeyDownStopsWithoutReportingDelivery() {
        val events = mutableListOf<KeyEvent>()

        val delivered =
            injectKeyCodeWithoutIdleWait(KeyEvent.KEYCODE_DPAD_CENTER) { event ->
                events.add(event)
                false
            }

        assertFalse(delivered)
        assertEquals(listOf(KeyEvent.ACTION_DOWN), events.map { it.action })
    }

    @Test
    fun rejectedKeyUpDoesNotReportSuccessfulDelivery() {
        val events = mutableListOf<KeyEvent>()

        val delivered =
            injectKeyCodeWithoutIdleWait(KeyEvent.KEYCODE_BACK) { event ->
                events.add(event)
                event.action == KeyEvent.ACTION_DOWN
            }

        assertFalse(delivered)
        assertEquals(listOf(KeyEvent.ACTION_DOWN, KeyEvent.ACTION_UP), events.map { it.action })
    }
}
