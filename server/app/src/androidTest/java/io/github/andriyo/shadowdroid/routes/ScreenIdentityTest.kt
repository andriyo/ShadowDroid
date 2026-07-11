package io.github.andriyo.shadowdroid.routes

import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.UiAutomationHealthTracker
import io.github.andriyo.shadowdroid.dump.TreeWalker
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.Element
import io.github.andriyo.shadowdroid.proto.ImeState
import io.github.andriyo.shadowdroid.proto.Viewport
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ScreenIdentityTest {
    @Test
    fun canonicalV2IdentityHasGoldenEncoding() {
        assertEquals("8571983d6cc34025", hash(listOf(element())))
    }

    @Test
    fun lengthDelimitedIdentityRejectsFormerStringConcatenationCollision() {
        val left = listOf(element(text = "ab", desc = "c"))
        val right = listOf(element(text = "a", desc = "bc"))

        assertNotEquals(hash(left), hash(right))
    }

    @Test
    fun identityIncludesEveryActionableElementField() {
        val base = element()
        val variants =
            listOf(
                base.copy(id = 2),
                base.copy(text = "other"),
                base.copy(desc = "other"),
                base.copy(klass = "Button"),
                base.copy(rid = "other"),
                base.copy(bounds = listOf(1, 2, 30, 40)),
                base.copy(tap = listOf(16, 21)),
                base.copy(clickable = true),
                base.copy(long_clickable = true),
                base.copy(scrollable = true),
                base.copy(checkable = true),
                base.copy(focusable = true),
                base.copy(enabled = false),
                base.copy(selected = true),
                base.copy(checked = true),
                base.copy(focused = true),
                base.copy(password = true),
                base.copy(input = true),
            )

        val baseHash = hash(listOf(base))
        assertEquals(variants.size, variants.map { hash(listOf(it)) }.toSet().size)
        variants.forEach { assertNotEquals(baseHash, hash(listOf(it))) }
    }

    @Test
    fun identityIncludesViewportCurrentAppAndImeState() {
        val elements = listOf(element())
        val base = hash(elements)

        assertNotEquals(base, hash(elements, viewport = Viewport(1920, 1080)))
        assertNotEquals(base, hash(elements, app = AppRef(`package` = "other", activity = "A", pid = 7)))
        assertNotEquals(base, hash(elements, ime = ImeState(keyboard_visible = true)))
    }

    @Suppress("DEPRECATION")
    @Test
    fun xpathActionRejectsAmbiguousMatches() {
        val first = AccessibilityNodeInfo.obtain()
        val second = AccessibilityNodeInfo.obtain()
        try {
            val matches =
                listOf(
                    ElementMatch(element(text = "Allow"), first),
                    ElementMatch(element(id = 2, text = "Allow"), second),
                )
            val error =
                assertThrows(BadRequest::class.java) {
                    chooseUnique(matches, SelectorReq(xpath = "//*[@text='Allow']", all = true))
                }
            assertEquals("ambiguous_match", error.code)
        } finally {
            first.recycle()
            second.recycle()
        }
    }

    @Test
    fun healthTrackerToleratesTransientRootLossAndResets() {
        val health = UiAutomationHealthTracker(failureLimit = 3)

        assertFalse(health.recordUnavailable())
        assertEquals(1, health.consecutiveFailures)
        health.recordHealthy()
        assertEquals(0, health.consecutiveFailures)
        assertFalse(health.recordUnavailable())
        assertFalse(health.recordUnavailable())
        assertTrue(health.recordUnavailable())
    }

    private fun hash(
        elements: List<Element>,
        viewport: Viewport = Viewport(1080, 1920),
        app: AppRef = AppRef(`package` = "com.example", activity = "com.example.MainActivity", pid = 42),
        ime: ImeState = ImeState(),
    ): String = TreeWalker.hashOf(elements, viewport, app, ime)

    private fun element(
        id: Int = 1,
        text: String? = "text",
        desc: String? = "desc",
    ): Element =
        Element(
            id = id,
            text = text,
            desc = desc,
            klass = "TextView",
            rid = "com.example:id/text",
            bounds = listOf(1, 2, 29, 40),
            tap = listOf(15, 21),
        )
}
