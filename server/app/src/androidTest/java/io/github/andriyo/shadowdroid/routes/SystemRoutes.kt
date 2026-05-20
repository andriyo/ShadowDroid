package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.request.*
import io.ktor.server.response.*
import io.ktor.server.routing.*
import kotlinx.serialization.Serializable

object SystemRoutes {
    /** Power, orientation, clipboard, notifications, quick_settings, url, shell. */
    fun register(route: Route, uiDevice: UiDevice, instr: Instrumentation) {
        route.post("/screen/on") {
            uiDevice.wakeUp(); call.respond(OkResponse())
        }
        route.post("/screen/off") {
            uiDevice.sleep(); call.respond(OkResponse())
        }
        route.post("/wakeup") {
            uiDevice.wakeUp(); call.respond(OkResponse())
        }
        route.post("/unlock") {
            // Wake + swipe up to dismiss the lock screen (works for swipe-to-unlock).
            uiDevice.wakeUp()
            val w = uiDevice.displayWidth
            val h = uiDevice.displayHeight
            uiDevice.swipe(w / 2, (h * 0.9).toInt(), w / 2, (h * 0.1).toInt(), 20)
            call.respond(OkResponse())
        }

        route.get("/orientation") {
            val v = when {
                uiDevice.isNaturalOrientation -> "natural"
                else -> when (instr.targetContext.resources.configuration.orientation) {
                    android.content.res.Configuration.ORIENTATION_LANDSCAPE -> "landscape"
                    else -> "other"
                }
            }
            call.respond(OrientationResp(v))
        }
        route.post("/orientation") {
            val r: OrientationReq = call.receive()
            when (r.value.lowercase()) {
                "natural", "n" -> uiDevice.setOrientationNatural()
                "left", "l" -> uiDevice.setOrientationLeft()
                "right", "r" -> uiDevice.setOrientationRight()
                "upsidedown", "upside_down", "u" -> uiDevice.setOrientationLeft().also {
                    // No direct setOrientationUpsideDown; left twice ≈ upside-down on
                    // most devices. For strict 180° rotation, callers should use
                    // settings put system user_rotation 2 via /v1/shell.
                }
                else -> throw BadRequest("bad_orientation",
                    "value must be natural|left|right|upsidedown, got '${r.value}'")
            }
            call.respond(OkResponse())
        }

        route.get("/clipboard") {
            val cm = instr.targetContext.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            val text = cm.primaryClip?.getItemAt(0)?.coerceToText(instr.targetContext)?.toString()
            call.respond(ClipResp(text))
        }
        route.post("/clipboard") {
            val r: ClipReq = call.receive()
            val cm = instr.targetContext.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            cm.setPrimaryClip(ClipData.newPlainText("shadowdroid", r.value))
            call.respond(OkResponse())
        }

        route.post("/notifications/open") {
            uiDevice.openNotification(); call.respond(OkResponse())
        }
        route.post("/quick_settings/open") {
            uiDevice.openQuickSettings(); call.respond(OkResponse())
        }

        route.post("/url/open") {
            val r: UrlReq = call.receive()
            val intent = Intent(Intent.ACTION_VIEW, Uri.parse(r.url))
            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            instr.targetContext.startActivity(intent)
            call.respond(OkResponse())
        }

        route.post("/shell") {
            val r: ShellReq = call.receive()
            // UiDevice.executeShellCommand runs as shell uid (2000) — same as `adb shell`.
            // Doesn't return exit code; runs the cmd via su-bin pipe and returns stdout.
            val output = try {
                uiDevice.executeShellCommand(r.cmd)
            } catch (t: Throwable) {
                throw BadRequest("shell_failed", t.message ?: "shell exec threw")
            }
            call.respond(ShellResp(input = r.cmd, output = output, exit_code = null))
        }
    }
}

@Serializable
private data class OrientationReq(val value: String)

@Serializable
private data class OrientationResp(val value: String)

@Serializable
private data class ClipReq(val value: String)

@Serializable
private data class ClipResp(val value: String?)

@Serializable
private data class UrlReq(val url: String)

@Serializable
private data class ShellReq(val cmd: String, val timeout_ms: Int = 30000)

@Serializable
private data class ShellResp(val input: String, val output: String, val exit_code: Int? = null)
