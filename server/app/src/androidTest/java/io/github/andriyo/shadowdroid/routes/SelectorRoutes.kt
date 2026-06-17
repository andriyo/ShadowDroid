package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.uiautomator.By
import androidx.test.uiautomator.Direction
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.github.andriyo.shadowdroid.dump.TreeWalker
import io.github.andriyo.shadowdroid.proto.Element
import io.ktor.server.request.receive
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.post
import kotlinx.serialization.Serializable

object SelectorRoutes {
    /** POST /v1/{find,find_tap,xpath}. */
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
        route.post("/find") {
            val request: SelectorReq = call.receive()
            val matches = findElementMatches(request, uiDevice, instr).map { it.element }
            call.respond(FindResp(matched = matches.firstOrNull(), elements = matches))
        }

        route.post("/find_tap") {
            val request: SelectorReq = call.receive()
            val match =
                findElementMatches(request.copy(all = false), uiDevice, instr).firstOrNull()
                    ?: throw NotFound("element_not_found", "no element matched selector")
            val tap = tapMatch(match, uiDevice)
            call.respond(FindTapResp(matched = match.element, x = tap.x, y = tap.y, action = tap.action))
        }

        route.post("/xpath") {
            val request: XpathReq = call.receive()
            val selector = SelectorReq(xpath = request.query, all = !request.tap)
            val matches = findElementMatches(selector, uiDevice, instr)
            if (request.tap) {
                val match =
                    matches.firstOrNull()
                        ?: throw NotFound("element_not_found", "no element matched xpath")
                val tap = tapMatch(match, uiDevice)
                call.respond(FindTapResp(matched = match.element, x = tap.x, y = tap.y, action = tap.action))
            } else {
                val elements = matches.map { it.element }
                call.respond(FindResp(matched = elements.firstOrNull(), elements = elements))
            }
        }

        // Fast on-device scroll-to: drive a scrollable UiObject2 directly rather
        // than the host loop's dump→swipe→dump per step. The CLI falls back to
        // the host loop if this route is absent (404) or there's no scrollable.
        route.post("/scroll") {
            val r: ScrollReq = call.receive()
            val target =
                bySelector(r.text, r.rid, r.desc)
                    ?: throw BadRequest("empty_selector", "scroll needs one of text|rid|desc")
            val scrollable =
                (
                    if (r.container_rid != null) {
                        uiDevice.findObject(By.res(r.container_rid))
                    } else {
                        uiDevice.findObject(By.scrollable(true))
                    }
                )
                    ?: throw NotFound("no_scrollable", "no scrollable container found")
            val dir =
                when (r.direction.lowercase()) {
                    "up" -> Direction.UP
                    "left" -> Direction.LEFT
                    "right" -> Direction.RIGHT
                    else -> Direction.DOWN
                }
            var found = uiDevice.findObject(target)
            var swipes = 0
            while (found == null && swipes < r.max_swipes) {
                val more = scrollable.scroll(dir, 0.8f)
                swipes++
                found = uiDevice.findObject(target)
                if (!more) break
            }
            if (found == null) {
                call.respond(ScrollResp(matched = false, x = -1, y = -1, swipes = swipes))
            } else {
                val center = found.visibleCenter
                if (r.tap) found.click()
                call.respond(ScrollResp(matched = true, x = center.x, y = center.y, swipes = swipes))
            }
        }
    }
}

private fun bySelector(
    text: String?,
    rid: String?,
    desc: String?,
) = when {
    rid != null -> By.res(rid)
    text != null -> By.textContains(text)
    desc != null -> By.descContains(desc)
    else -> null
}

internal data class ElementMatch(
    val element: Element,
    val node: AccessibilityNodeInfo,
)

internal fun findElementMatches(
    request: SelectorReq,
    uiDevice: UiDevice,
    instr: Instrumentation,
): List<ElementMatch> {
    request.validate()
    val root = instr.uiAutomation.rootInActiveWindow
    val elements = TreeWalker.walkWithNodes(root, uiDevice.displayWidth, uiDevice.displayHeight)
    val matches =
        elements
            .filter { request.matches(it.element) }
            .map { ElementMatch(it.element, it.node) }
    return if (request.all) matches else matches.take(1)
}

@Serializable
data class SelectorReq(
    val id: Int? = null,
    val text: String? = null,
    val rid: String? = null,
    val desc: String? = null,
    val klass: String? = null,
    val xpath: String? = null,
    val all: Boolean = false,
    val exact: Boolean = false,
    val clickable: Boolean? = null,
    val enabled: Boolean? = null,
) {
    fun validate() {
        if (
            id == null &&
            text == null &&
            rid == null &&
            desc == null &&
            klass == null &&
            xpath == null &&
            clickable == null &&
            enabled == null
        ) {
            throw BadRequest("empty_selector", "at least one selector field is required")
        }
    }

    fun matches(element: Element): Boolean {
        if (id != null && element.id != id) return false
        if (!matchString(element.text, text, exact)) return false
        if (!matchString(element.rid, rid, exact)) return false
        if (!matchString(element.desc, desc, exact)) return false
        if (!matchString(element.klass, klass, exact)) return false
        if (clickable != null && element.clickable != clickable) return false
        if (enabled != null && element.enabled != enabled) return false
        if (xpath != null && !xpathMatcher(xpath).matches(element)) return false
        return true
    }
}

@Serializable
private data class XpathReq(
    val query: String,
    val tap: Boolean = false,
)

@Serializable
private data class ScrollReq(
    val text: String? = null,
    val rid: String? = null,
    val desc: String? = null,
    val direction: String = "down",
    val container_rid: String? = null,
    val max_swipes: Int = 12,
    val tap: Boolean = false,
)

@Serializable
private data class ScrollResp(
    val matched: Boolean,
    val x: Int,
    val y: Int,
    val swipes: Int,
)

@Serializable
private data class FindResp(
    val matched: Element? = null,
    val elements: List<Element> = emptyList(),
)

@Serializable
private data class FindTapResp(
    val matched: Element,
    val x: Int? = null,
    val y: Int? = null,
    val action: String,
)

private data class TapResult(
    val x: Int? = null,
    val y: Int? = null,
    val action: String,
)

private fun tapMatch(
    match: ElementMatch,
    uiDevice: UiDevice,
): TapResult {
    val tap = match.element.tap
    if (tap != null && tap.size >= 2) {
        val x = tap[0]
        val y = tap[1]
        if (!uiDevice.click(x, y)) throw BadRequest("tap_failed", "UiDevice.click returned false")
        return TapResult(x = x, y = y, action = "coordinate")
    }

    if (performAccessibilityClick(match.node)) {
        return TapResult(action = "accessibility_click")
    }
    throw BadRequest(
        "tap_failed",
        "matched element has no usable bounds and ACTION_CLICK failed",
    )
}

private fun performAccessibilityClick(node: AccessibilityNodeInfo): Boolean {
    var current: AccessibilityNodeInfo? = node
    var depth = 0
    while (current != null && depth < 5) {
        if (current.performAction(AccessibilityNodeInfo.ACTION_CLICK)) return true
        current = current.parent
        depth++
    }
    return false
}

private fun matchString(
    actual: String?,
    expected: String?,
    exact: Boolean,
): Boolean {
    if (expected == null) return true
    val value = actual ?: return false
    return if (exact) {
        value.equals(expected, ignoreCase = true)
    } else {
        value.contains(expected, ignoreCase = true)
    }
}

private data class XpathMatcher(
    val clauses: List<XpathClause>,
) {
    fun matches(element: Element): Boolean = clauses.all { it.matches(element) }
}

private data class XpathClause(
    val attr: String,
    val value: String,
    val contains: Boolean,
) {
    fun matches(element: Element): Boolean {
        val actual =
            when (attr.lowercase()) {
                "text" -> element.text
                "resource-id", "resource_id", "rid" -> element.rid
                "content-desc", "content_description", "desc" -> element.desc
                "class", "class-name", "class_name", "klass" -> element.klass
                "clickable" -> element.clickable.toString()
                "enabled" -> element.enabled.toString()
                else -> throw BadRequest("xpath_invalid", "unsupported xpath attribute '@$attr'")
            } ?: return false
        return if (contains) {
            actual.contains(value, ignoreCase = true)
        } else {
            actual.equals(value, ignoreCase = true)
        }
    }
}

private fun xpathMatcher(query: String): XpathMatcher {
    val trimmed = query.trim()
    if (!trimmed.startsWith("//")) {
        throw BadRequest("xpath_invalid", "only descendant xpath queries beginning with '//' are supported")
    }

    val clauses = mutableListOf<XpathClause>()
    val quoted = """['"]([^'"]*)['"]"""
    Regex("""contains\(\s*@([A-Za-z0-9_:-]+)\s*,\s*$quoted\s*\)""")
        .findAll(trimmed)
        .forEach { clauses += XpathClause(it.groupValues[1], it.groupValues[2], contains = true) }
    Regex("""@([A-Za-z0-9_:-]+)\s*=\s*$quoted""")
        .findAll(trimmed)
        .forEach { clauses += XpathClause(it.groupValues[1], it.groupValues[2], contains = false) }

    if (clauses.isEmpty()) {
        throw BadRequest(
            "xpath_invalid",
            "supported xpath forms are //*[@text='...'] and //*[contains(@text,'...')]",
        )
    }
    return XpathMatcher(clauses)
}
