package io.github.andriyo.shadowdroid.dump

import android.graphics.Rect
import android.view.accessibility.AccessibilityNodeInfo
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.Element
import io.github.andriyo.shadowdroid.proto.ImeState
import io.github.andriyo.shadowdroid.proto.RangeSemantics
import io.github.andriyo.shadowdroid.proto.Viewport
import kotlinx.serialization.json.floatOrNull
import kotlinx.serialization.json.jsonPrimitive
import java.nio.charset.StandardCharsets
import java.security.MessageDigest

/**
 * Walks the active-window accessibility tree and produces the flat element list
 * we ship on the wire.
 *
 * Filter rule: include a node iff it's interactive (clickable / long-clickable /
 * scrollable / checkable / EditText-class) OR has user-visible content
 * (non-empty text or contentDescription). Pure layout containers (LinearLayout
 * etc with no text and no interaction) are dropped — they'd bloat the list
 * without helping an agent.
 *
 * Element ids are assigned in DFS order, stable for the duration of a single
 * dump. After any UI change the agent must re-dump.
 */
object TreeWalker {
    data class WalkedElement(
        val element: Element,
        val node: AccessibilityNodeInfo,
    )

    fun walk(
        root: AccessibilityNodeInfo?,
        viewportW: Int,
        viewportH: Int,
    ): List<Element> = walkWithNodes(root, viewportW, viewportH).map { it.element }

    fun walkWithNodes(
        root: AccessibilityNodeInfo?,
        viewportW: Int,
        viewportH: Int,
    ): List<WalkedElement> {
        val elements = mutableListOf<WalkedElement>()
        if (root == null) return elements
        visit(root, elements, viewportW, viewportH)
        return elements
    }

    private fun visit(
        node: AccessibilityNodeInfo,
        out: MutableList<WalkedElement>,
        vw: Int,
        vh: Int,
    ) {
        val text =
            node.text
                ?.toString()
                ?.trim()
                .orEmpty()
        val desc =
            node.contentDescription
                ?.toString()
                ?.trim()
                .orEmpty()
        val rid = node.viewIdResourceName.orEmpty()
        val cls = node.className?.toString().orEmpty()

        val isInput = cls.contains("EditText")
        var range = rangeSemantics(node.rangeInfo)
        if (range != null) {
            // Compose exposes sliders as virtual accessibility nodes. Their
            // cached RangeInfo can lag behind a successful set-progress action
            // even after the window sends a content-change event; refresh the
            // node before treating current as authoritative readback.
            runCatching { node.refresh() }
            range = rangeSemantics(node.rangeInfo)
        }
        val actions = accessibilityActions(node)
        val isInteractive =
            node.isClickable ||
                node.isLongClickable ||
                node.isScrollable ||
                node.isCheckable ||
                range != null ||
                actions.contains("set_progress")
        val hasContent = text.isNotEmpty() || desc.isNotEmpty()

        if (isInteractive || hasContent || isInput) {
            val bounds = Rect().also { node.getBoundsInScreen(it) }
            // Skip elements completely outside the viewport (off-screen pages in
            // a ViewPager etc.) — their tap points would be nonsense.
            val hasUsableBounds =
                bounds.width() > 0 &&
                    bounds.height() > 0 &&
                    bounds.right > 0 &&
                    bounds.bottom > 0 &&
                    bounds.left < vw &&
                    bounds.top < vh
            val isPositiveButOffscreen =
                bounds.width() > 0 &&
                    bounds.height() > 0 &&
                    !hasUsableBounds
            if (!isPositiveButOffscreen) {
                val boundsList =
                    if (hasUsableBounds) {
                        listOf(bounds.left, bounds.top, bounds.right, bounds.bottom)
                    } else {
                        null
                    }
                val tapList =
                    boundsList?.let { listOf((it[0] + it[2]) / 2, (it[1] + it[3]) / 2) }
                out +=
                    WalkedElement(
                        element =
                            Element(
                                id = out.size,
                                text = text.ifEmpty { null },
                                desc = desc.ifEmpty { null },
                                klass = cls.substringAfterLast('.').ifEmpty { null },
                                rid = rid.ifEmpty { null },
                                bounds = boundsList,
                                tap = tapList,
                                range = range,
                                actions = actions,
                                clickable = node.isClickable,
                                long_clickable = node.isLongClickable,
                                scrollable = node.isScrollable,
                                checkable = node.isCheckable,
                                focusable = node.isFocusable,
                                enabled = node.isEnabled,
                                selected = node.isSelected,
                                checked = node.isChecked,
                                focused = node.isFocused || node.isAccessibilityFocused,
                                password = node.isPassword,
                                input = isInput,
                            ),
                        node = node,
                    )
            }
        }

        // Recurse. Use getChildCount() for size; AccessibilityNodeInfo doesn't
        // expose children as an Iterable but each get is cheap.
        for (i in 0 until node.childCount) {
            val child = node.getChild(i) ?: continue
            try {
                visit(child, out, vw, vh)
            } finally {
                // recycle was deprecated in API 33; let GC handle it
            }
        }
    }

    /**
     * Stable identity of the actionable screen state.
     *
     * Every field that can affect a subsequent action is encoded with an
     * explicit null marker and byte length. This prevents concatenation
     * collisions such as `(text="ab", desc="c")` and
     * `(text="a", desc="bc")`, which produced the same input in v1. The
     * version/domain prefix lets us evolve the canonical representation
     * without accidentally comparing hashes produced by different schemas.
     */
    fun hashOf(
        elements: List<Element>,
        viewport: Viewport,
        currentApp: AppRef,
        ime: ImeState,
    ): String {
        val digest = CanonicalDigest(MessageDigest.getInstance("SHA-256"))
        digest.putString("shadowdroid.screen.v3")
        digest.putInt(viewport.w)
        digest.putInt(viewport.h)
        digest.putNullableString(currentApp.`package`)
        digest.putNullableString(currentApp.activity)
        digest.putNullableInt(currentApp.pid)
        digest.putBoolean(ime.keyboard_visible)
        digest.putNullableInt(ime.focused_element?.id)
        digest.putNullableInt(ime.focused_input?.id)
        digest.putInt(elements.size)
        for (e in elements) {
            digest.putInt(e.id)
            digest.putNullableString(e.text)
            digest.putNullableString(e.desc)
            digest.putNullableString(e.klass)
            digest.putNullableString(e.rid)
            digest.putNullableIntList(e.bounds)
            digest.putNullableIntList(e.tap)
            digest.putBoolean(e.range != null)
            e.range?.let { range ->
                digest.putString(range.type)
                digest.putFloat(range.min)
                digest.putFloat(range.max)
                digest.putFloat(range.current)
                digest.putNullableFloat(range.step.jsonPrimitive.floatOrNull)
            }
            digest.putInt(e.actions.size)
            e.actions.forEach(digest::putString)
            digest.putBoolean(e.clickable)
            digest.putBoolean(e.long_clickable)
            digest.putBoolean(e.scrollable)
            digest.putBoolean(e.checkable)
            digest.putBoolean(e.focusable)
            digest.putBoolean(e.enabled)
            digest.putBoolean(e.selected)
            digest.putBoolean(e.checked)
            digest.putBoolean(e.focused)
            digest.putBoolean(e.password)
            digest.putBoolean(e.input)
        }
        // First 8 bytes hex to match the public screen_hash length.
        return digest.finish().take(8).joinToString("") { "%02x".format(it) }
    }
}

private fun rangeSemantics(info: AccessibilityNodeInfo.RangeInfo?): RangeSemantics? {
    if (info == null) return null
    val type =
        when (info.type) {
            AccessibilityNodeInfo.RangeInfo.RANGE_TYPE_INT -> "int"
            AccessibilityNodeInfo.RangeInfo.RANGE_TYPE_FLOAT -> "float"
            AccessibilityNodeInfo.RangeInfo.RANGE_TYPE_PERCENT -> "percent"
            AccessibilityNodeInfo.RangeInfo.RANGE_TYPE_INDETERMINATE -> "indeterminate"
            else -> "unknown"
        }
    return RangeSemantics(
        type = type,
        min = info.min,
        max = info.max,
        current = info.current,
    )
}

/**
 * Stable, non-localized semantic actions that are not already represented by
 * an Element boolean. ACTION_CLICK/scroll/focus/set-text stay compact through
 * clickable/scrollable/focusable/input; set-progress is the new capability an
 * agent otherwise cannot discover.
 */
private fun accessibilityActions(node: AccessibilityNodeInfo): List<String> =
    node.actionList
        .mapNotNull { action ->
            when (action.id) {
                AccessibilityNodeInfo.AccessibilityAction.ACTION_SET_PROGRESS.id -> "set_progress"
                else -> null
            }
        }.distinct()
        .sorted()

/** Small binary encoder for the versioned screen identity above. */
private class CanonicalDigest(
    private val digest: MessageDigest,
) {
    private val intBytes = ByteArray(Int.SIZE_BYTES)

    fun putBoolean(value: Boolean) {
        digest.update((if (value) 1 else 0).toByte())
    }

    fun putInt(value: Int) {
        intBytes[0] = (value ushr 24).toByte()
        intBytes[1] = (value ushr 16).toByte()
        intBytes[2] = (value ushr 8).toByte()
        intBytes[3] = value.toByte()
        digest.update(intBytes)
    }

    fun putNullableInt(value: Int?) {
        putBoolean(value != null)
        value?.let(::putInt)
    }

    fun putFloat(value: Float) = putInt(value.toBits())

    fun putNullableFloat(value: Float?) {
        putBoolean(value != null)
        value?.let(::putFloat)
    }

    fun putString(value: String) {
        val bytes = value.toByteArray(StandardCharsets.UTF_8)
        putInt(bytes.size)
        digest.update(bytes)
    }

    fun putNullableString(value: String?) {
        putBoolean(value != null)
        value?.let(::putString)
    }

    fun putNullableIntList(values: List<Int>?) {
        putBoolean(values != null)
        if (values != null) {
            putInt(values.size)
            values.forEach(::putInt)
        }
    }

    fun finish(): ByteArray = digest.digest()
}
