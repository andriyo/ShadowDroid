package io.github.andriyo.shadowdroid.routes

import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.UiAutomationHealthTracker
import io.github.andriyo.shadowdroid.dump.TreeWalker
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.Element
import io.github.andriyo.shadowdroid.proto.ImeState
import io.github.andriyo.shadowdroid.proto.RangeSemantics
import io.github.andriyo.shadowdroid.proto.Viewport
import kotlinx.serialization.json.JsonPrimitive
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
    fun canonicalV3IdentityHasGoldenEncoding() {
        assertEquals("8c58a9031017bacb", hash(listOf(element())))
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
                base.copy(range = RangeSemantics("float", 0f, 1f, 0.5f)),
                base.copy(actions = listOf("set_progress")),
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

    @Test
    fun interactionIdentityIgnoresTelemetryVideoAndRangeCurrentValue() {
        val control =
            element(id = 8, text = null, desc = "Follow target").copy(
                klass = "SeekBar",
                clickable = true,
                range = RangeSemantics("float", 0f, 100f, 20f),
                interaction_path = listOf(0, 2),
            )
        val telemetryA =
            element(id = 1, text = "Confidence 71%", desc = null).copy(
                rid = "com.example:id/telemetry",
                interaction_path = listOf(0, 0),
            )
        val telemetryB = telemetryA.copy(text = "Confidence 99%")
        val video =
            element(id = 2, text = null, desc = null).copy(
                klass = "TextureView",
                rid = null,
                bounds = listOf(0, 0, 1080, 1200),
                interaction_path = listOf(0, 1),
            )

        assertNotEquals(hash(listOf(telemetryA, video, control)), hash(listOf(telemetryB, video, control.copy(range = control.range?.copy(current = 80f)))))
        assertEquals(
            interactionHash(listOf(telemetryA, video, control)),
            interactionHash(
                listOf(
                    telemetryB,
                    video.copy(bounds = listOf(0, 0, 1080, 1600)),
                    control.copy(range = control.range?.copy(current = 80f)),
                ),
            ),
        )
        // A display-only sibling may be inserted or removed before the control;
        // raw accessibility indexes shift, but the actionable hierarchy does not.
        assertEquals(
            interactionHash(listOf(control.copy(interaction_path = listOf(0, 1)))),
            interactionHash(
                listOf(
                    telemetryA.copy(interaction_path = listOf(0, 0)),
                    control.copy(interaction_path = listOf(0, 2)),
                ),
            ),
        )
    }

    @Test
    fun interactionIdentityTracksActionableHierarchyGeometryAndState() {
        val control =
            element(text = "Run", desc = "Run mission").copy(
                klass = "Button",
                clickable = true,
                interaction_path = listOf(0, 1),
            )
        val base = interactionHash(listOf(control))
        for (variant in listOf(
            control.copy(enabled = false),
            control.copy(bounds = listOf(10, 20, 100, 120)),
            control.copy(clickable = false, focusable = true),
            control.copy(actions = listOf("long_click")),
        )) {
            assertNotEquals(base, interactionHash(listOf(variant)))
        }
        assertNotEquals(base, interactionHash(listOf(control, control.copy(id = 2, interaction_path = listOf(0, 2)))))
        val child =
            control.copy(
                rid = "com.example:id/child",
                desc = "Child action",
                interaction_path = listOf(0, 1, 0),
            )
        assertNotEquals(
            interactionHash(listOf(control, child)),
            interactionHash(listOf(control, child.copy(interaction_path = listOf(1)))),
        )
        // A stable rid/description outranks mutable displayed button copy.
        assertEquals(base, interactionHash(listOf(control.copy(text = "Running 42%"))))
        // Text-only actions must retain their label in the interaction identity.
        val textOnly = control.copy(rid = null, desc = null)
        assertNotEquals(
            interactionHash(listOf(textOnly)),
            interactionHash(listOf(textOnly.copy(text = "Stop"))),
        )
    }

    @Test
    fun interactionHandlesUseActionableOrdinalNotVolatileDumpId() {
        val telemetry = element(id = 1, text = "Tick 1", desc = null)
        val control =
            element(id = 2, text = null, desc = "Run mission").copy(
                klass = "Button",
                clickable = true,
                interaction_path = listOf(0, 1),
            )
        val hash = interactionHash(listOf(telemetry, control))
        val first = TreeWalker.bindInteractionHandles(listOf(telemetry, control), hash)
        val shifted =
            TreeWalker.bindInteractionHandles(
                listOf(telemetry.copy(id = 4, text = "Tick 2"), control.copy(id = 9)),
                hash,
            )
        assertEquals("$hash/e:0", first.last().handle)
        assertEquals(first.last().handle, shifted.last().handle)
        assertNotEquals(first.last().id, shifted.last().id)
        assertEquals(null, first.first().handle)
    }

    @Test
    fun navigationWithReusedNumericIdInvalidatesOldHandle() {
        val source =
            element(id = 8, text = null, desc = "Open mission").copy(
                clickable = true,
                interaction_path = listOf(0, 1),
            )
        val destination =
            source.copy(
                desc = "Delete mission",
                rid = "com.example:id/delete",
            )
        val sourceHash = interactionHash(listOf(source))
        val destinationHash = interactionHash(listOf(destination))
        val oldHandle = TreeWalker.bindInteractionHandles(listOf(source), sourceHash).single().handle
        val newHandles =
            TreeWalker.bindInteractionHandles(listOf(destination), destinationHash).mapNotNull(Element::handle)
        assertNotEquals(sourceHash, destinationHash)
        assertFalse(oldHandle in newHandles)
    }

    @Test
    fun focusedActivityParserAcceptsModernAndLegacyDumpsysShapes() {
        val expected = FocusedApp("com.example", "com.example.MainActivity")
        assertEquals(
            expected,
            parseFocusedApp(
                "topResumedActivity=ActivityRecord{123 u0 com.example/.MainActivity t8}",
            ),
        )
        assertEquals(
            expected,
            parseFocusedApp(
                "mResumedActivity: ActivityRecord{123 u0 com.example/.MainActivity t8}",
            ),
        )
        assertEquals(
            expected,
            parseFocusedApp(
                "ResumedActivity: ActivityRecord{123 u0 com.example/.MainActivity t8}",
            ),
        )
        assertEquals(
            expected,
            parseFocusedApp(
                """
                mResumedActivity: null
                topResumedActivity=ActivityRecord{123 u0 com.example/.MainActivity t8}
                """.trimIndent(),
            ),
        )
    }

    @Test
    fun populatedTreeRequiresMatchingCompleteForegroundMetadata() {
        val pending =
            ScreenEnrichment(
                `package` = "com.example",
                activity = null,
                pid = null,
                keyboardVisible = null,
                keyboardDetectionAvailable = false,
                keyboardReason = null,
                windowId = null,
                sampledAtMs = 0,
                refreshedAtElapsedMs = 0,
            )
        assertEquals(
            "transitioning",
            assessSnapshot("com.example", 7, true, 3, "com.example", pending).state,
        )
        assertEquals(
            "transitioning",
            assessSnapshot("com.previous", 7, true, 3, "com.example", pending).state,
        )

        val complete =
            pending.copy(
                activity = "com.example.MainActivity",
                pid = 42,
                windowId = 7,
                sampledAtMs = 1,
                refreshedAtElapsedMs = 1,
            )
        assertEquals(
            "consistent",
            assessSnapshot("com.example", 7, true, 3, "com.example", complete).state,
        )
        assertEquals(
            "transitioning",
            assessSnapshot("com.example", 8, true, 3, "com.example", complete).state,
        )
    }

    @Test
    fun slowFirstDrawWithoutAccessibleContentIsTransitioning() {
        val enrichment =
            ScreenEnrichment(
                `package` = "com.example",
                activity = "com.example.MainActivity",
                pid = 42,
                keyboardVisible = null,
                keyboardDetectionAvailable = false,
                keyboardReason = null,
                windowId = 7,
                sampledAtMs = 1,
                refreshedAtElapsedMs = 1,
            )
        val assessment =
            assessSnapshot(
                treePackage = "com.example",
                treeWindowId = 7,
                treeReady = false,
                elementCount = 0,
                foregroundPackage = "com.example",
                enrichment = enrichment,
            )
        assertEquals("transitioning", assessment.state)
        assertTrue(assessment.warning?.contains("accessible content") == true)
    }

    @Test
    fun tapResolutionChoosesNearestEnabledClickableAncestor() {
        val states =
            listOf(
                TapCandidateState(enabled = true, clickable = false),
                TapCandidateState(enabled = true, clickable = true),
                TapCandidateState(enabled = true, clickable = true),
            )
        assertEquals(1, chooseActionableIndex(states))
        assertEquals(
            null,
            chooseActionableIndex(
                listOf(
                    TapCandidateState(enabled = true, clickable = false),
                    TapCandidateState(enabled = true, clickable = false),
                ),
            ),
        )
    }

    @Test
    fun tapResolutionRejectsDisabledTargetOrAncestor() {
        val disabledTarget =
            assertThrows(BadRequest::class.java) {
                chooseActionableIndex(
                    listOf(TapCandidateState(enabled = false, clickable = true)),
                )
            }
        assertEquals("element_disabled", disabledTarget.code)

        val disabledCard =
            assertThrows(BadRequest::class.java) {
                chooseActionableIndex(
                    listOf(
                        TapCandidateState(enabled = true, clickable = false),
                        TapCandidateState(enabled = false, clickable = true),
                        TapCandidateState(enabled = true, clickable = true),
                    ),
                )
            }
        assertEquals("element_disabled", disabledCard.code)
    }

    @Test
    fun progressTargetResolvesAbsolutePercentClampAndDeclaredStep() {
        val continuous = RangeSemantics("float", 10f, 20f, 12f)
        assertEquals(15f, resolveProgressTarget(SetProgressReq(percent = 50.0), continuous))
        assertEquals(18f, resolveProgressTarget(SetProgressReq(value = 18.0), continuous))
        assertEquals(20f, resolveProgressTarget(SetProgressReq(value = 99.0, clamp = true), continuous))

        val discrete = continuous.copy(step = JsonPrimitive(2f))
        assertEquals(16f, resolveProgressTarget(SetProgressReq(value = 15.2), discrete))
    }

    @Test
    fun progressTargetRejectsInvalidOrOutOfRangeRequests() {
        val range = RangeSemantics("float", 0f, 1f, 0.5f)
        assertEquals(
            "progress_target_required",
            assertThrows(BadRequest::class.java) {
                resolveProgressTarget(SetProgressReq(), range)
            }.code,
        )
        assertEquals(
            "progress_target_conflict",
            assertThrows(BadRequest::class.java) {
                resolveProgressTarget(SetProgressReq(value = 0.2, percent = 20.0), range)
            }.code,
        )
        assertEquals(
            "progress_value_out_of_range",
            assertThrows(BadRequest::class.java) {
                resolveProgressTarget(SetProgressReq(value = 2.0), range)
            }.code,
        )
        assertEquals(
            "progress_value_invalid",
            assertThrows(BadRequest::class.java) {
                resolveProgressTarget(SetProgressReq(value = Double.NaN), range)
            }.code,
        )
    }

    @Test
    fun progressReadbackUsesRangeScaledTolerance() {
        val range = RangeSemantics("float", 0f, 100f, 50.05f)
        assertTrue(progressMatches(range, 50f))
        assertFalse(progressMatches(range.copy(current = 51f), 50f))
        assertTrue(progressMatches(range.copy(current = 50.9f, step = JsonPrimitive(2f)), 50f))
        assertFalse(progressChanged(range, range.copy(current = 50.1f)))
        assertTrue(progressChanged(range, range.copy(current = 51f)))
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

    private fun interactionHash(
        elements: List<Element>,
        viewport: Viewport = Viewport(1080, 1920),
        app: AppRef = AppRef(`package` = "com.example", activity = "com.example.MainActivity", pid = 42),
    ): String = TreeWalker.interactionHashOf(elements, viewport, app)

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
