package io.github.andriyo.shadowdroid.proto

import kotlinx.serialization.Serializable
import kotlinx.serialization.Transient
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull

/*
 * Wire types for the v1 HTTP API.
 *
 * Kept in their own package + file so the wire schema is greppable as one unit
 * and so adding/changing a field has a single point of audit.
 */

// ── /v1/state ────────────────────────────────────────────────────────

@Serializable
data class ServerState(
    val server_version: String,
    val api_version: String,
    val ui_automator_version: String,
    val android_sdk: Int,
    val android_release: String,
    val viewport: Viewport,
    val current_app: AppRef,
    // True on leanback / Android TV devices, where the UI is focus + D-pad driven
    // rather than touch driven. Agents should navigate with `ui focus` / `ui key
    // dpad_*` instead of coordinate/selector taps. Defaults false for phones/tablets.
    val is_television: Boolean = false,
)

@Serializable
data class Viewport(
    val w: Int,
    val h: Int,
)

@Serializable
data class AppRef(
    val `package`: String? = null,
    val activity: String? = null,
    val pid: Int? = null,
    val sampled_at_ms: Long? = null,
)

// ── /v1/screen ──────────────────────────────────────────────────────

@Serializable
data class ScreenResponse(
    val screen_hash: String,
    val screen_hash_version: Int = 3,
    val content_hash: String,
    val interaction_hash: String,
    val interaction_hash_version: Int = 1,
    val snapshot_state: String = "consistent",
    val captured_at_ms: Long? = null,
    val viewport: Viewport,
    val current_app: AppRef,
    val ui_tree: UiTreeSnapshot? = null,
    val warning: String? = null,
    val element_count: Int,
    val ime: ImeState = ImeState(),
    val elements: List<Element>,
)

@Serializable
data class StableScreenResponse(
    val stable: Boolean,
    val settle_ms: Long,
    val quiet_period_ms: Long,
    val screen: ScreenResponse,
)

@Serializable
data class UiTreeSnapshot(
    val sampled_at_ms: Long,
    val age_ms: Long,
    val `package`: String? = null,
    val window_id: Int? = null,
)

@Serializable
data class ImeState(
    val keyboard_visible: Boolean = false,
    val focused_element: Element? = null,
    val focused_input: Element? = null,
    val detection: String? = null,
    val reason: String? = null,
    val suggested_actions: List<String> = emptyList(),
)

@Serializable
data class Element(
    val id: Int,
    val handle: String? = null,
    val text: String? = null,
    val desc: String? = null,
    val klass: String? = null,
    val rid: String? = null,
    val bounds: List<Int>? = null, // [x1, y1, x2, y2] when UIA exposes usable bounds
    val tap: List<Int>? = null, // [cx, cy] when coordinate tapping is possible
    val range: RangeSemantics? = null,
    val actions: List<String> = emptyList(),
    val clickable: Boolean = false,
    val long_clickable: Boolean = false,
    val scrollable: Boolean = false,
    val checkable: Boolean = false,
    val focusable: Boolean = false,
    val enabled: Boolean = true,
    val selected: Boolean = false,
    val checked: Boolean = false,
    val focused: Boolean = false,
    val password: Boolean = false,
    val input: Boolean = false,
    // Internal DFS hierarchy identity used only to calculate interaction_hash.
    // It is intentionally absent from the public wire payload.
    @Transient val interaction_path: List<Int> = emptyList(),
)

@Serializable
data class RangeSemantics(
    val type: String,
    val min: Float,
    val max: Float,
    val current: Float,
    // AccessibilityNodeInfo.RangeInfo does not expose a declared interval.
    // Keep this nullable so we never invent precision for discrete controls.
    val step: JsonElement = JsonNull,
)

// ── shared 'ok' response for state-changing endpoints ────────────────

@Serializable
data class OkResponse(
    val ok: Boolean = true,
)
