package io.github.andriyo.shadowdroid.routes

import androidx.test.uiautomator.By
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.request.*
import io.ktor.server.response.*
import io.ktor.server.routing.*
import kotlinx.serialization.Serializable

object KeyTextRoutes {
    /** POST /v1/key and POST /v1/text. */
    fun register(route: Route, uiDevice: UiDevice) {
        route.post("/key") {
            val r: KeyReq = call.receive()
            val ok = when {
                r.code != null -> uiDevice.pressKeyCode(r.code)
                r.name != null -> pressNamed(uiDevice, r.name)
                else -> throw BadRequest("missing_key", "either 'name' or 'code' required")
            }
            if (!ok) throw BadRequest("key_failed", "UiDevice.pressKey returned false")
            call.respond(OkResponse())
        }
        route.post("/text") {
            val r: TextReq = call.receive()
            // Find the focused field. UI Automator finds the focused-input
            // node via By.focused(true). If clear=true, clear it first.
            val focused = uiDevice.findObject(By.focused(true))
                ?: throw NotFound("no_focused_field",
                    "no element has input focus. Tap a text field first.")
            if (r.clear) focused.clear()
            focused.text = r.value
            call.respond(OkResponse())
        }
    }
}

private fun pressNamed(ui: UiDevice, name: String): Boolean = when (name.lowercase()) {
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
            "camera, call, endcall"
    )
}

@Serializable
private data class KeyReq(val name: String? = null, val code: Int? = null)

@Serializable
private data class TextReq(val value: String, val clear: Boolean = false)
