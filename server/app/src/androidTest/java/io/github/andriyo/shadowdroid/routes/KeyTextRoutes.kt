package io.github.andriyo.shadowdroid.routes

import android.accessibilityservice.AccessibilityService
import android.app.Instrumentation
import android.os.Bundle
import android.os.SystemClock
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.accessibility.AccessibilityNodeInfo
import androidx.test.uiautomator.By
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.application.ApplicationCall
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
            handleKey(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/key") {
            handleKey(call, uiDevice, instr, guarded = true)
        }
        route.post("/text") {
            handleText(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/text") {
            handleText(call, uiDevice, instr, guarded = true)
        }
    }
}

private suspend fun handleKey(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val request: KeyReq = call.receive()
    val injected =
        withUiActionGuard(call, uiDevice, instr, guarded) {
            when {
                guarded -> pressGuardedKey(instr, request)
                request.code != null -> uiDevice.pressKeyCode(request.code)
                request.name != null -> pressNamed(uiDevice, request.name)
                else -> throw BadRequest("missing_key", "either 'name' or 'code' required")
            }
        }
    // UiDevice.pressBack/pressHome/pressKeyCode return false on Android 14+
    // even when the key event was delivered. Report the raw result rather than
    // re-pressing and potentially navigating twice.
    call.respond(OkResponse(ok = injected))
}

/** Inject immediately after validation; UiDevice's key helpers wait for idle before injection. */
private fun pressGuardedKey(
    instr: Instrumentation,
    request: KeyReq,
): Boolean {
    val code =
        request.code ?: when (val name = request.name?.lowercase()) {
            null -> throw BadRequest("missing_key", "either 'name' or 'code' required")
            "back" -> KeyEvent.KEYCODE_BACK
            "home" -> KeyEvent.KEYCODE_HOME
            "menu" -> KeyEvent.KEYCODE_MENU
            "enter" -> KeyEvent.KEYCODE_ENTER
            "search" -> KeyEvent.KEYCODE_SEARCH
            "delete" -> KeyEvent.KEYCODE_DEL
            "recent" -> return instr.uiAutomation.performGlobalAction(AccessibilityService.GLOBAL_ACTION_RECENTS)
            "dpad_up" -> KeyEvent.KEYCODE_DPAD_UP
            "dpad_down" -> KeyEvent.KEYCODE_DPAD_DOWN
            "dpad_left" -> KeyEvent.KEYCODE_DPAD_LEFT
            "dpad_right" -> KeyEvent.KEYCODE_DPAD_RIGHT
            "dpad_center" -> KeyEvent.KEYCODE_DPAD_CENTER
            "wakeup" -> KeyEvent.KEYCODE_WAKEUP
            "power" -> KeyEvent.KEYCODE_POWER
            "volume_up" -> KeyEvent.KEYCODE_VOLUME_UP
            "volume_down" -> KeyEvent.KEYCODE_VOLUME_DOWN
            "volume_mute" -> KeyEvent.KEYCODE_VOLUME_MUTE
            "camera" -> KeyEvent.KEYCODE_CAMERA
            "call" -> KeyEvent.KEYCODE_CALL
            "endcall" -> KeyEvent.KEYCODE_ENDCALL
            else -> throw BadRequest("unknown_key", "no mapping for '$name'; pass a numeric KeyEvent code as 'code' instead")
        }
    return injectKeyCodeWithoutIdleWait(code) { event -> instr.uiAutomation.injectInputEvent(event, true) }
}

/** Match UiAutomator's short-key event metadata and failure semantics without its pre-injection idle wait. */
internal fun injectKeyCodeWithoutIdleWait(
    keyCode: Int,
    inject: (KeyEvent) -> Boolean,
): Boolean {
    val eventTime = SystemClock.uptimeMillis()
    // Match InteractionController.sendKeys in UiAutomator 2.4.0, including
    // standalone modifier keys supplied through the numeric-code API.
    val metaState =
        when (keyCode) {
            KeyEvent.KEYCODE_SHIFT_LEFT -> KeyEvent.META_SHIFT_ON or KeyEvent.META_SHIFT_LEFT_ON
            KeyEvent.KEYCODE_SHIFT_RIGHT -> KeyEvent.META_SHIFT_ON or KeyEvent.META_SHIFT_RIGHT_ON
            KeyEvent.KEYCODE_ALT_LEFT -> KeyEvent.META_ALT_ON or KeyEvent.META_ALT_LEFT_ON
            KeyEvent.KEYCODE_ALT_RIGHT -> KeyEvent.META_ALT_ON or KeyEvent.META_ALT_RIGHT_ON
            KeyEvent.KEYCODE_SYM -> KeyEvent.META_SYM_ON
            KeyEvent.KEYCODE_FUNCTION -> KeyEvent.META_FUNCTION_ON
            KeyEvent.KEYCODE_CTRL_LEFT -> KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_LEFT_ON
            KeyEvent.KEYCODE_CTRL_RIGHT -> KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_RIGHT_ON
            KeyEvent.KEYCODE_META_LEFT -> KeyEvent.META_META_LEFT_ON
            KeyEvent.KEYCODE_META_RIGHT -> KeyEvent.META_META_RIGHT_ON
            KeyEvent.KEYCODE_CAPS_LOCK -> KeyEvent.META_CAPS_LOCK_ON
            KeyEvent.KEYCODE_NUM_LOCK -> KeyEvent.META_NUM_LOCK_ON
            KeyEvent.KEYCODE_SCROLL_LOCK -> KeyEvent.META_SCROLL_LOCK_ON
            else -> 0
        }

    fun event(action: Int) =
        KeyEvent(
            eventTime,
            eventTime,
            action,
            keyCode,
            0,
            metaState,
            KeyCharacterMap.VIRTUAL_KEYBOARD,
            0,
            0,
            InputDevice.SOURCE_KEYBOARD,
        )
    return inject(event(KeyEvent.ACTION_DOWN)) && inject(event(KeyEvent.ACTION_UP))
}

private suspend fun handleText(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val request: TextReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) { guard ->
        val selector = request.selector()
        val match =
            guard?.resolved
                ?: selector?.let {
                    chooseUnique(
                        guard?.let { snapshot ->
                            findElementMatches(it.copy(all = true), snapshot.captured.walked)
                        } ?: findElementMatches(it.copy(all = true), uiDevice, instr),
                        it,
                    )
                }
        if (match != null) {
            if (!setAccessibilityText(match.node, request.value)) {
                throw BadRequest("text_failed", "matched element rejected ACTION_SET_TEXT")
            }
        } else if (!setFocusedAccessibilityText(instr, request.value)) {
            val focused =
                uiDevice.findObject(By.focused(true))
                    ?: throw NotFound(
                        "no_focused_field",
                        "no element has input focus. Tap a text field first, or pass --id/--text/--rid/--desc/--xpath.",
                    )
            if (request.clear) focused.clear()
            focused.text = request.value
        }
    }
    call.respond(OkResponse())
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
