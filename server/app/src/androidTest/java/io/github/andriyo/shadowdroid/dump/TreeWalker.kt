package io.github.andriyo.shadowdroid.dump

import android.graphics.Rect
import android.view.accessibility.AccessibilityNodeInfo
import io.github.andriyo.shadowdroid.proto.Element
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

    fun walk(root: AccessibilityNodeInfo?, viewportW: Int, viewportH: Int): List<Element> {
        val elements = mutableListOf<Element>()
        if (root == null) return elements
        visit(root, elements, viewportW, viewportH)
        return elements
    }

    private fun visit(
        node: AccessibilityNodeInfo,
        out: MutableList<Element>,
        vw: Int,
        vh: Int,
    ) {
        val text = node.text?.toString()?.trim().orEmpty()
        val desc = node.contentDescription?.toString()?.trim().orEmpty()
        val rid = node.viewIdResourceName.orEmpty()
        val cls = node.className?.toString().orEmpty()

        val isInput = cls.contains("EditText")
        val isInteractive =
            node.isClickable || node.isLongClickable ||
            node.isScrollable || node.isCheckable
        val hasContent = text.isNotEmpty() || desc.isNotEmpty()

        if (isInteractive || hasContent || isInput) {
            val bounds = Rect().also { node.getBoundsInScreen(it) }
            // Skip elements completely outside the viewport (off-screen pages in
            // a ViewPager etc.) — their tap points would be nonsense.
            val onScreen = bounds.right > 0 && bounds.bottom > 0 &&
                           bounds.left < vw && bounds.top < vh &&
                           bounds.width() > 0 && bounds.height() > 0
            if (onScreen) {
                out += Element(
                    id = out.size,
                    text = text.ifEmpty { null },
                    desc = desc.ifEmpty { null },
                    klass = cls.substringAfterLast('.').ifEmpty { null },
                    rid = rid.ifEmpty { null },
                    bounds = listOf(bounds.left, bounds.top, bounds.right, bounds.bottom),
                    tap = listOf((bounds.left + bounds.right) / 2, (bounds.top + bounds.bottom) / 2),
                    clickable = node.isClickable,
                    long_clickable = node.isLongClickable,
                    scrollable = node.isScrollable,
                    checkable = node.isCheckable,
                    focusable = node.isFocusable,
                    enabled = node.isEnabled,
                    selected = node.isSelected,
                    checked = node.isChecked,
                    focused = node.isFocused,
                    password = node.isPassword,
                    input = isInput,
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
     * Stable hash of the rendered tree. Computed from the same set of fields
     * we emit, so semantically-identical screens produce the same hash even if
     * accessibility-event sequencing differs. Used by the watch loop's
     * change-detection.
     */
    fun hashOf(elements: List<Element>): String {
        val md = MessageDigest.getInstance("SHA-256")
        for (e in elements) {
            md.update(e.text.orEmpty().toByteArray())
            md.update(e.desc.orEmpty().toByteArray())
            md.update(e.rid.orEmpty().toByteArray())
            md.update(e.klass.orEmpty().toByteArray())
            md.update(e.bounds.joinToString(",").toByteArray())
            md.update(byteArrayOf(
                if (e.clickable) 1 else 0,
                if (e.scrollable) 1 else 0,
                if (e.checked) 1 else 0,
                if (e.focused) 1 else 0,
                if (e.enabled) 1 else 0,
            ))
        }
        // First 8 bytes hex — matches movi's blake2b digest length for wire compat.
        return md.digest().take(8).joinToString("") { "%02x".format(it) }
    }
}
