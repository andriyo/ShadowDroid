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
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.add
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

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
            val match = chooseUnique(findElementMatches(request.copy(all = true), uiDevice, instr), request)
            val tap = tapMatch(match, uiDevice, request.coordinate_fallback)
            call.respond(
                FindTapResp(
                    matched = match.element,
                    activated_element = tap.activatedElement,
                    actionable_resolved = tap.actionableResolved,
                    input_delivered = tap.inputDelivered,
                    x = tap.x,
                    y = tap.y,
                    action = tap.action,
                ),
            )
        }

        route.post("/xpath") {
            val request: XpathReq = call.receive()
            // Always collect the complete match set. Taking the first match on
            // a tap silently bypassed ShadowDroid's strict ambiguity contract
            // and made identical screens behave differently depending on tree
            // order.
            val selector = SelectorReq(xpath = request.query, all = true)
            val matches = findElementMatches(selector, uiDevice, instr)
            if (request.tap) {
                val match = chooseUnique(matches, selector)
                val tap = tapMatch(match, uiDevice, request.coordinate_fallback)
                call.respond(
                    FindTapResp(
                        matched = match.element,
                        activated_element = tap.activatedElement,
                        actionable_resolved = tap.actionableResolved,
                        input_delivered = tap.inputDelivered,
                        x = tap.x,
                        y = tap.y,
                        action = tap.action,
                    ),
                )
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
            if (r.text == null && r.rid == null && r.desc == null) {
                throw BadRequest("empty_selector", "scroll needs one of text|rid|desc")
            }
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
            // Detect the target with the canonical normalized matcher (the same
            // one `/find` uses) — not `By.textContains`, which is case-sensitive
            // and unnormalized. A match with a usable tap point is on-screen.
            val selector = SelectorReq(text = r.text, rid = r.rid, desc = r.desc)

            fun visibleHit(): ElementMatch? =
                findElementMatches(selector, uiDevice, instr)
                    .firstOrNull { it.element.tap != null }

            var found = visibleHit()
            var swipes = 0
            while (found == null && swipes < r.max_swipes) {
                val more = scrollable.scroll(dir, 0.8f)
                swipes++
                found = visibleHit()
                if (!more) break
            }
            val hit = found
            val tap = hit?.element?.tap
            if (tap == null) {
                call.respond(ScrollResp(matched = false, x = -1, y = -1, swipes = swipes))
            } else {
                if (r.tap) tapMatch(hit, uiDevice, coordinateFallback = false)
                call.respond(ScrollResp(matched = true, x = tap[0], y = tap[1], swipes = swipes))
            }
        }
    }
}

/**
 * Resolve a selector to the single element to act on. A sole match wins; if
 * several match, a unique *exact* match disambiguates (so `--text Allow` taps the
 * bare "Allow" over "Allow all the time"); otherwise the selector is ambiguous
 * and the agent must narrow it with --exact/--rid/--clickable. Mirrors the host's
 * strict-ambiguity behavior for `ui focus`.
 */
internal fun chooseUnique(
    matches: List<ElementMatch>,
    request: SelectorReq,
): ElementMatch =
    when (matches.size) {
        0 -> throw NotFound("element_not_found", "no element matched selector")
        1 -> matches[0]
        else -> {
            val exact = matches.filter { request.copy(exact = true).matches(it.element) }
            if (exact.size == 1) {
                exact[0]
            } else {
                val message =
                    if (request.xpath != null) {
                        "xpath matched ${matches.size} elements; add a unique @resource-id or @content-desc clause"
                    } else {
                        "selector matched ${matches.size} elements; narrow with --exact, --rid, or --clickable"
                    }
                throw BadRequest(
                    "ambiguous_match",
                    message,
                    detail = mapOf("count" to matches.size, "candidates" to candidatesDetail(matches)),
                )
            }
        }
    }

private fun candidatesDetail(matches: List<ElementMatch>): JsonElement =
    buildJsonArray {
        matches.take(10).forEach { m ->
            add(
                buildJsonObject {
                    m.element.text?.let { put("text", it) }
                    m.element.rid?.let { put("rid", it) }
                    m.element.desc?.let { put("desc", it) }
                },
            )
        }
    }

internal data class ElementMatch(
    val element: Element,
    val node: AccessibilityNodeInfo,
    val tree: List<TreeWalker.WalkedElement> = emptyList(),
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
            .map { ElementMatch(it.element, it.node, elements) }
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
    val coordinate_fallback: Boolean = false,
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
    val coordinate_fallback: Boolean = false,
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
    val activated_element: Element? = null,
    val actionable_resolved: Boolean,
    val input_delivered: Boolean,
    val x: Int? = null,
    val y: Int? = null,
    val action: String,
)

private data class TapResult(
    val activatedElement: Element? = null,
    val actionableResolved: Boolean,
    val inputDelivered: Boolean,
    val x: Int? = null,
    val y: Int? = null,
    val action: String,
)

private fun tapMatch(
    match: ElementMatch,
    uiDevice: UiDevice,
    coordinateFallback: Boolean,
): TapResult {
    val candidates = actionableCandidates(match)
    val actionableIndex =
        try {
            chooseActionableIndex(candidates.map { it.state })
        } catch (error: BadRequest) {
            if (error.code != "element_disabled") throw error
            val depth = (error.detail?.get("ancestor_depth") as? Number)?.toInt() ?: 0
            val disabled = candidates.getOrNull(depth)?.element
            throw BadRequest(
                "element_disabled",
                error.message ?: "tap target is disabled; no input was injected",
                detail =
                    buildMap {
                        put("ancestor_depth", depth)
                        put("selector_matched", true)
                        put("actionable_resolved", false)
                        put("input_delivered", false)
                        put("postcondition_satisfied", null)
                        put("matched_element", elementDetail(match.element))
                        disabled?.let { put("disabled_element", elementDetail(it)) }
                    },
            )
        }
    if (actionableIndex != null) {
        val activated = candidates[actionableIndex]
        val activatedElement =
            activated.element
                ?: throw BadRequest(
                    "tap_failed",
                    "clickable ancestor was absent from the actionable UI tree",
                )
        if (activated.node.performAction(AccessibilityNodeInfo.ACTION_CLICK)) {
            return TapResult(
                activatedElement = activatedElement,
                actionableResolved = true,
                inputDelivered = true,
                action = "accessibility_click",
            )
        }
        if (!coordinateFallback) {
            throw BadRequest(
                "tap_failed",
                "ACTION_CLICK failed for the resolved clickable element; pass coordinate_fallback=true for explicit center injection",
                detail =
                    mapOf(
                        "selector_matched" to true,
                        "actionable_resolved" to true,
                        "input_delivered" to false,
                        "postcondition_satisfied" to null,
                        "activated_element" to elementDetail(activatedElement),
                    ),
            )
        }
        return coordinateTap(activatedElement, uiDevice, activatedElement, actionableResolved = true)
    }

    if (!coordinateFallback) {
        throw BadRequest(
            "element_not_clickable",
            "matched element has no enabled clickable ancestor; pass coordinate_fallback=true for explicit center injection",
            detail =
                mapOf(
                    "selector_matched" to true,
                    "actionable_resolved" to false,
                    "input_delivered" to false,
                    "postcondition_satisfied" to null,
                    "matched_element" to elementDetail(match.element),
                ),
        )
    }
    return coordinateTap(match.element, uiDevice, activatedElement = null, actionableResolved = false)
}

internal data class TapCandidateState(
    val enabled: Boolean,
    val clickable: Boolean,
)

private data class TapCandidate(
    val node: AccessibilityNodeInfo,
    val element: Element?,
    val state: TapCandidateState,
)

private fun actionableCandidates(match: ElementMatch): List<TapCandidate> {
    val candidates = mutableListOf(
        TapCandidate(
            node = match.node,
            element = match.element,
            state = TapCandidateState(match.element.enabled, match.element.clickable),
        ),
    )
    var current = match.node.parent
    var depth = 0
    while (current != null && depth < MAX_ANCESTOR_DEPTH) {
        val walked = match.tree.firstOrNull { it.node == current }
        candidates +=
            TapCandidate(
                node = current,
                element = walked?.element,
                state = TapCandidateState(current.isEnabled, current.isClickable),
            )
        current = current.parent
        depth++
    }
    return candidates
}

internal fun chooseActionableIndex(candidates: List<TapCandidateState>): Int? {
    candidates.forEachIndexed { index, candidate ->
        if (!candidate.enabled) {
            val subject = if (index == 0) "matched element" else "ancestor at depth $index"
            throw BadRequest(
                "element_disabled",
                "$subject is disabled; no input was injected",
                detail = mapOf("ancestor_depth" to index),
            )
        }
        if (candidate.clickable) return index
    }
    return null
}

private fun coordinateTap(
    target: Element,
    uiDevice: UiDevice,
    activatedElement: Element?,
    actionableResolved: Boolean,
): TapResult {
    val tap = target.tap
    if (tap == null || tap.size < 2) {
        throw BadRequest(
            "tap_failed",
            "explicit coordinate fallback requested but the target has no usable center point",
        )
    }
    val x = tap[0]
    val y = tap[1]
    if (!uiDevice.click(x, y)) throw BadRequest("tap_failed", "UiDevice.click returned false")
    return TapResult(
        activatedElement = activatedElement,
        actionableResolved = actionableResolved,
        inputDelivered = true,
        x = x,
        y = y,
        action = "coordinate_fallback",
    )
}

private fun elementDetail(element: Element): JsonElement =
    buildJsonObject {
        put("id", element.id)
        element.text?.let { put("text", it) }
        element.rid?.let { put("rid", it) }
        element.desc?.let { put("desc", it) }
        put("clickable", element.clickable)
        put("enabled", element.enabled)
    }

private const val MAX_ANCESTOR_DEPTH = 32

private fun matchString(
    actual: String?,
    expected: String?,
    exact: Boolean,
): Boolean {
    if (expected == null) return true
    val value = normalizeForMatch(actual ?: return false)
    val want = normalizeForMatch(expected)
    return if (exact) {
        value.equals(want, ignoreCase = true)
    } else {
        value.contains(want, ignoreCase = true)
    }
}

/**
 * Canonical text-selector normalization. MUST stay in lockstep with the host's
 * Rust `selector::normalize` (cli/src/selector.rs) so a selector behaves the same
 * whether matched on the device (find/tap/text/scroll) or on the host
 * (wait/focus/watchers). Steps:
 *   1. drop zero-width / bidirectional control characters,
 *   2. fold typographic punctuation to ASCII — curly quotes/apostrophes/primes
 *      and the ellipsis — but NOT dashes (an en/em dash is not a hyphen),
 *   3. collapse every run of whitespace (NBSP, tabs, newlines, …) to one space
 *      and trim the ends.
 * Case is handled by the `ignoreCase` compares in [SelectorReq.matches], so this
 * intentionally does not change case.
 */
internal fun normalizeForMatch(s: String): String {
    val out = StringBuilder(s.length)
    var pendingSpace = false
    for (c in s) {
        if (isZeroWidthOrBidi(c)) continue
        if (c.isWhitespace()) {
            // Defer: emit one space only before the next real char, so leading,
            // trailing, and repeated whitespace all collapse away.
            if (out.isNotEmpty()) pendingSpace = true
            continue
        }
        if (pendingSpace) {
            out.append(' ')
            pendingSpace = false
        }
        when (c) {
            '‘', '’', 'ʼ', '′', '‛' -> out.append('\'')
            '“', '”', '″', '‟' -> out.append('"')
            '…' -> out.append("...")
            else -> out.append(c)
        }
    }
    return out.toString()
}

/** Zero-width and bidi marks: visually absent, but break a naive comparison. */
private fun isZeroWidthOrBidi(c: Char): Boolean = c in '\u200B'..'\u200F' || c in '\u202A'..'\u202E' || c == '\u2060' || c == '\uFEFF'

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
        val haystack = normalizeForMatch(actual)
        val needle = normalizeForMatch(value)
        return if (contains) {
            haystack.contains(needle, ignoreCase = true)
        } else {
            haystack.equals(needle, ignoreCase = true)
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
