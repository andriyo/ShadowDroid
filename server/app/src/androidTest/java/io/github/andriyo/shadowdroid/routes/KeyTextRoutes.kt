package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.os.Bundle
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.uiautomator.By
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.request.receive
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.post
import kotlinx.serialization.Serializable

object KeyTextRoutes {
    /** POST /v1/key and POST /v1/text. */
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
        route.post("/key") {
            val r: KeyReq = call.receive()
            val injected =
                when {
                    r.code != null -> uiDevice.pressKeyCode(r.code)
                    r.name != null -> pressNamed(uiDevice, r.name)
                    else -> throw BadRequest("missing_key", "either 'name' or 'code' required")
                }
            // UiDevice.pressBack/pressHome/pressKeyCode return false on Android
            // 14+ even when the key event is delivered and handled — the boolean
            // reflects an injection-reporting quirk, not whether the action
            // happened (verified: `back` navigates correctly while still
            // returning false). Re-pressing would double-navigate, so we report
            // the raw result via `ok` rather than treating false as a hard
            // failure. Genuinely bad input (unknown name, or neither name nor
            // code) still errors above with unknown_key / missing_key.
            call.respond(OkResponse(ok = injected))
        }
        route.post("/text") {
            val r: TextReq = call.receive()
            val selector = r.selector()
            if (selector != null) {
                // Strict ambiguity, like `find_tap`: a unique (or uniquely-exact)
                // match, else `ambiguous_match` — never type into the wrong field.
                val match = chooseUnique(findElementMatches(selector.copy(all = true), uiDevice, instr), selector)
                if (!setAccessibilityText(match.node, r.value)) {
                    throw BadRequest(
                        "text_failed",
                        "matched element rejected ACTION_SET_TEXT",
                    )
                }
            } else if (!setFocusedAccessibilityText(instr, r.value)) {
                val focused =
                    uiDevice.findObject(By.focused(true))
                        ?: throw NotFound(
                            "no_focused_field",
                            "no element has input focus. Tap a text field first, or pass --id/--text/--rid/--desc/--xpath.",
                        )
                if (r.clear) focused.clear()
                focused.text = r.value
            }
            call.respond(OkResponse())
        }
    }
}

private fun pressNamed(
    ui: UiDevice,
    name: String,
): Boolean =
    when (name.lowercase()) {
        "back" -> ui.pressBack()
        "home" -> ui.pressHome()
        "menu" -> ui.pressMenu()
        "enter" -> ui.pressEnter()
        "search" -> ui.pressSearch()
        "delete" -> ui.pressDelete()
        "recent" -> ui.pressRecentApps()
        "dpad_up" -> ui.pressDPadUp()
        "dpad_down" -> ui.pressDPadDown()
        "dpad_left" -> ui.pressDPadLeft()
        "dpad_right" -> ui.pressDPadRight()
        "dpad_center" -> ui.pressDPadCenter()
        // Common keycodes via name; full list in KeyEvent
        "wakeup" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_WAKEUP)
        "power" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_POWER)
        "volume_up" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_VOLUME_UP)
        "volume_down" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_VOLUME_DOWN)
        "volume_mute" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_VOLUME_MUTE)
        "camera" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_CAMERA)
        "call" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_CALL)
        "endcall" -> ui.pressKeyCode(android.view.KeyEvent.KEYCODE_ENDCALL)
        else -> throw BadRequest(
            "unknown_key",
            "no mapping for '$name'. Pass a numeric KeyEvent code as 'code' instead, " +
                "or use one of: back, home, menu, enter, search, delete, recent, " +
                "dpad_{up,down,left,right,center}, wakeup, power, volume_{up,down,mute}, " +
                "camera, call, endcall",
        )
    }

@Serializable
private data class KeyReq(
    val name: String? = null,
    val code: Int? = null,
)

@Serializable
private data class TextReq(
    val value: String,
    val clear: Boolean = false,
    val id: Int? = null,
    val text: String? = null,
    val rid: String? = null,
    val desc: String? = null,
    val klass: String? = null,
    val xpath: String? = null,
    val exact: Boolean = false,
) {
    fun selector(): SelectorReq? {
        if (id == null && text == null && rid == null && desc == null && klass == null && xpath == null) {
            return null
        }
        return SelectorReq(
            id = id,
            text = text,
            rid = rid,
            desc = desc,
            klass = klass,
            xpath = xpath,
            exact = exact,
        )
    }
}

private fun setFocusedAccessibilityText(
    instr: Instrumentation,
    value: String,
): Boolean {
    val root = instr.uiAutomation.rootInActiveWindow ?: return false
    val focused =
        root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
            ?: root.findFocus(AccessibilityNodeInfo.FOCUS_ACCESSIBILITY)
            ?: return false
    return setAccessibilityText(focused, value)
}

private fun setAccessibilityText(
    node: AccessibilityNodeInfo,
    value: String,
): Boolean {
    val args =
        Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, value)
        }
    return node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
}
