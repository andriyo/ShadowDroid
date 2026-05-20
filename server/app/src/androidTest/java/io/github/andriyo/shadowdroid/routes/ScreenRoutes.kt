package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.ErrorBody
import io.github.andriyo.shadowdroid.ErrorEnvelope
import io.github.andriyo.shadowdroid.dump.TreeWalker
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.ScreenResponse
import io.github.andriyo.shadowdroid.proto.Viewport
import io.ktor.http.*
import io.ktor.server.response.*
import io.ktor.server.routing.*
import java.io.ByteArrayOutputStream
import java.io.File

object ScreenRoutes {
    /** GET /v1/screen?format=elements|xml and GET /v1/screenshot.png. */
    fun register(route: Route, uiDevice: UiDevice, instr: Instrumentation) {
        route.get("/screen") {
            val format = call.request.queryParameters["format"] ?: "elements"
            if (format == "xml") {
                // Raw UI Automator XML for callers who want it.
                val baos = ByteArrayOutputStream()
                uiDevice.dumpWindowHierarchy(baos)
                call.respondBytes(baos.toByteArray(), ContentType.parse("application/xml"))
                return@get
            }
            val root = instr.uiAutomation.rootInActiveWindow
            val elements = TreeWalker.walk(root, uiDevice.displayWidth, uiDevice.displayHeight)
            val pkg = uiDevice.currentPackageName
            val activity = currentFocusedActivity(uiDevice)
            val pid = pidForPackage(uiDevice, pkg)
            call.respond(
                ScreenResponse(
                    screen_hash = TreeWalker.hashOf(elements),
                    viewport = Viewport(uiDevice.displayWidth, uiDevice.displayHeight),
                    current_app = AppRef(`package` = pkg, activity = activity, pid = pid),
                    element_count = elements.size,
                    elements = elements,
                ),
            )
        }

        // GET /v1/screenshot.png — PNG of the current display.
        route.get("/screenshot.png") {
            val cacheDir = instr.targetContext.cacheDir
            val tmp = File.createTempFile("sd-screenshot-", ".png", cacheDir)
            try {
                val ok = uiDevice.takeScreenshot(tmp)
                if (!ok) {
                    call.respond(
                        HttpStatusCode.InternalServerError,
                        ErrorEnvelope(ErrorBody("screenshot_failed",
                            "UiDevice.takeScreenshot returned false"))
                    )
                    return@get
                }
                call.respondBytes(tmp.readBytes(), ContentType.parse("image/png"))
            } finally {
                tmp.delete()
            }
        }
    }
}
