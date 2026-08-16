package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.PreconditionFailed
import io.github.andriyo.shadowdroid.proto.Element
import io.github.andriyo.shadowdroid.proto.ScreenResponse
import io.github.andriyo.shadowdroid.proto.SnapshotState
import io.ktor.server.application.ApplicationCall
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.encodeToJsonElement

internal const val IF_SCREEN_HEADER = "X-ShadowDroid-If-Screen"
internal const val IF_INTERACTION_HEADER = "X-ShadowDroid-If-Interaction"
internal const val ELEMENT_HANDLE_HEADER = "X-ShadowDroid-Element-Handle"

internal data class ActionGuardSpec(
    val ifScreen: String? = null,
    val ifInteraction: String? = null,
    val elementHandle: String? = null,
)

internal data class GuardedUiSnapshot(
    val captured: CapturedScreen,
    val response: ScreenResponse,
    val resolved: ElementMatch? = null,
)

private val guardJson =
    Json {
        encodeDefaults = true
        explicitNulls = false
    }

// Serialize guard-capable input routes (plus /scroll) so another such request
// cannot slip between a guarded capture and its injection. App/system routes
// and external Android transitions remain observable through the hashes.
private val uiActionMutex = Mutex()

internal suspend fun <T> withUiActionGuard(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
    action: suspend (GuardedUiSnapshot?) -> T,
): T =
    uiActionMutex.withLock {
        val snapshot = if (guarded) captureAndValidateActionGuard(call, uiDevice, instr) else null
        action(snapshot)
    }

internal suspend fun captureAndValidateActionGuard(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
): GuardedUiSnapshot {
    val spec =
        ActionGuardSpec(
            ifScreen = call.request.headers[IF_SCREEN_HEADER],
            ifInteraction = call.request.headers[IF_INTERACTION_HEADER],
            elementHandle = call.request.headers[ELEMENT_HANDLE_HEADER],
        )
    if (spec.ifScreen == null && spec.ifInteraction == null && spec.elementHandle == null) {
        throw BadRequest("missing_action_guard", "guarded action requires a screen or interaction precondition")
    }
    val captured =
        captureScreen(
            uiDevice,
            instr,
            ScreenEnrichmentCache.shared(uiDevice, instr),
        )
    val response = captured.toResponse()
    val resolvedElement = validateActionGuard(spec, response)
    val resolved =
        resolvedElement?.let { element ->
            val walked =
                captured.walked.firstOrNull { it.element.id == element.id }
                    ?: throw PreconditionFailed(
                        "stale_element",
                        "element handle resolved outside the captured accessibility tree",
                        guardDetail(response, "handle" to (spec.elementHandle ?: "")),
                    )
            ElementMatch(element, walked.node, captured.walked)
        }
    return GuardedUiSnapshot(captured, response, resolved)
}

internal fun validateActionGuard(
    spec: ActionGuardSpec,
    screen: ScreenResponse,
): Element? {
    if (screen.snapshot_state != SnapshotState.CONSISTENT) {
        throw PreconditionFailed(
            "snapshot_not_consistent",
            "current UI snapshot is not consistent; no input was injected",
            guardDetail(screen),
        )
    }
    spec.ifScreen?.let { expected ->
        if (!screen.screen_hash.equals(expected, ignoreCase = true)) {
            throw PreconditionFailed(
                "screen_changed",
                "screen changed since the guarded read; no input was injected",
                guardDetail(screen, "expected" to expected, "actual" to screen.screen_hash),
            )
        }
    }
    spec.ifInteraction?.let { expected ->
        if (!screen.interaction_hash.equals(expected, ignoreCase = true)) {
            val code = if (spec.elementHandle == null) "interaction_changed" else "stale_element"
            throw PreconditionFailed(
                code,
                "interaction structure changed since the guarded read; no input was injected",
                guardDetail(
                    screen,
                    "expected" to expected,
                    "actual" to screen.interaction_hash,
                    "handle" to spec.elementHandle,
                ),
            )
        }
    }
    return spec.elementHandle?.let { handle ->
        screen.elements.firstOrNull { it.handle == handle }
            ?: throw PreconditionFailed(
                "stale_element",
                "element handle is not present in the guarded interaction snapshot; no input was injected",
                guardDetail(
                    screen,
                    "handle" to handle,
                    "expected" to spec.ifInteraction,
                    "actual" to screen.interaction_hash,
                ),
            )
    }
}

private fun guardDetail(
    screen: ScreenResponse,
    vararg values: Pair<String, Any?>,
): Map<String, Any?> =
    buildMap {
        put("screen", guardJson.encodeToJsonElement(screen))
        for ((key, value) in values) {
            if (value != null) put(key, value)
        }
    }
