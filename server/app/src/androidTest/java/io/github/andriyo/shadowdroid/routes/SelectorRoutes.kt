package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
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
            val matches = findElements(request, uiDevice, instr)
            call.respond(FindResp(matched = matches.firstOrNull(), elements = matches))
        }

        route.post("/find_tap") {
            val request: SelectorReq = call.receive()
            val match =
                findElements(request.copy(all = false), uiDevice, instr).firstOrNull()
                    ?: throw NotFound("element_not_found", "no element matched selector")
            val x = match.tap[0]
            val y = match.tap[1]
            if (!uiDevice.click(x, y)) throw BadRequest("tap_failed", "UiDevice.click returned false")
            call.respond(FindTapResp(matched = match, x = x, y = y))
        }

        route.post("/xpath") {
            val request: XpathReq = call.receive()
            val selector = SelectorReq(xpath = request.query, all = !request.tap)
            val matches = findElements(selector, uiDevice, instr)
            if (request.tap) {
                val match =
                    matches.firstOrNull()
                        ?: throw NotFound("element_not_found", "no element matched xpath")
                val x = match.tap[0]
                val y = match.tap[1]
                if (!uiDevice.click(x, y)) throw BadRequest("tap_failed", "UiDevice.click returned false")
                call.respond(FindTapResp(matched = match, x = x, y = y))
            } else {
                call.respond(FindResp(matched = matches.firstOrNull(), elements = matches))
            }
        }
    }
}

private fun findElements(
    request: SelectorReq,
    uiDevice: UiDevice,
    instr: Instrumentation,
): List<Element> {
    request.validate()
    val root = instr.uiAutomation.rootInActiveWindow
    val elements = TreeWalker.walk(root, uiDevice.displayWidth, uiDevice.displayHeight)
    val matches = elements.filter { request.matches(it) }
    return if (request.all) matches else matches.take(1)
}

@Serializable
data class SelectorReq(
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
private data class FindResp(
    val matched: Element? = null,
    val elements: List<Element> = emptyList(),
)

@Serializable
private data class FindTapResp(
    val matched: Element,
    val x: Int,
    val y: Int,
)

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
