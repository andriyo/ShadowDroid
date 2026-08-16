package io.github.andriyo.shadowdroid.routes

import androidx.test.ext.junit.runners.AndroidJUnit4
import io.github.andriyo.shadowdroid.PreconditionFailed
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.Element
import io.github.andriyo.shadowdroid.proto.RangeSemantics
import io.github.andriyo.shadowdroid.proto.ScreenResponse
import io.github.andriyo.shadowdroid.proto.SnapshotState
import io.github.andriyo.shadowdroid.proto.Viewport
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ActionGuardTest {
    @Test
    fun matchingHashesAndHandleResolveOneCapturedElement() {
        val screen = screen()
        val element =
            validateActionGuard(
                ActionGuardSpec(
                    ifScreen = screen.screen_hash,
                    ifInteraction = screen.interaction_hash,
                    elementHandle = HANDLE,
                ),
                screen,
            )

        assertSame(screen.elements.single(), element)
    }

    @Test
    fun staleScreenFailsBeforeInputWithFreshScreenDetail() {
        val error =
            assertThrows(PreconditionFailed::class.java) {
                validateActionGuard(ActionGuardSpec(ifScreen = "old-screen"), screen())
            }

        assertEquals("screen_changed", error.code)
        assertEquals("old-screen", error.detail?.get("expected"))
        assertEquals("screen-now", error.detail?.get("actual"))
        assertEquals(true, error.detail?.containsKey("screen"))
    }

    @Test
    fun transitioningSnapshotFailsEvenWhenHashesMatch() {
        val screen = screen().copy(snapshot_state = SnapshotState.TRANSITIONING)
        val error =
            assertThrows(PreconditionFailed::class.java) {
                validateActionGuard(
                    ActionGuardSpec(
                        ifScreen = screen.screen_hash,
                        ifInteraction = screen.interaction_hash,
                    ),
                    screen,
                )
            }

        assertEquals("snapshot_not_consistent", error.code)
    }

    @Test
    fun handleCannotCrossInteractionSnapshotsOrDisappear() {
        val interactionError =
            assertThrows(PreconditionFailed::class.java) {
                validateActionGuard(
                    ActionGuardSpec(
                        ifInteraction = "i:ffffffffffffffff",
                        elementHandle = HANDLE,
                    ),
                    screen(),
                )
            }
        assertEquals("stale_element", interactionError.code)

        val missingError =
            assertThrows(PreconditionFailed::class.java) {
                validateActionGuard(
                    ActionGuardSpec(
                        ifInteraction = INTERACTION,
                        elementHandle = "$INTERACTION/e:9",
                    ),
                    screen(),
                )
            }
        assertEquals("stale_element", missingError.code)
    }

    @Test
    fun progressReadbackUsesHandleWhenIdsAndDescriptionsShift() {
        val range = RangeSemantics("float", 0f, 100f, 75f)
        val selector = SelectorReq(rid = "io.example:id/open_lab", exact = true)
        val shifted =
            screen().copy(
                elements =
                    listOf(
                        Element(id = 4, text = "new display-only node"),
                        screen().elements.single().copy(
                            id = 9,
                            handle = "i:ffffffffffffffff/e:0",
                            desc = "75 percent",
                            range = range,
                        ),
                    ),
                element_count = 2,
            )

        assertSame(range, rangeForHandleOrSelector(shifted, HANDLE, selector))
        assertEquals(
            null,
            rangeForHandleOrSelector(
                shifted.copy(snapshot_state = SnapshotState.TRANSITIONING),
                HANDLE,
                selector,
            ),
        )
    }

    private fun screen(): ScreenResponse {
        val element =
            Element(
                id = 4,
                handle = HANDLE,
                text = "Open lab",
                rid = "io.example:id/open_lab",
                clickable = true,
            )
        return ScreenResponse(
            screen_hash = "screen-now",
            content_hash = "c:screen-now",
            interaction_hash = INTERACTION,
            snapshot_state = SnapshotState.CONSISTENT,
            viewport = Viewport(1080, 1920),
            current_app = AppRef(`package` = "io.example"),
            element_count = 1,
            elements = listOf(element),
        )
    }

    private companion object {
        const val INTERACTION = "i:0123456789abcdef"
        const val HANDLE = "$INTERACTION/e:4"
    }
}
