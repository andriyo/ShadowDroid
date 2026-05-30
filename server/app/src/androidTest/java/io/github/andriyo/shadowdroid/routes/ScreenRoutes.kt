package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.ErrorBody
import io.github.andriyo.shadowdroid.ErrorEnvelope
import io.github.andriyo.shadowdroid.dump.TreeWalker
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.ScreenResponse
import io.github.andriyo.shadowdroid.proto.Viewport
import io.ktor.http.ContentType
import io.ktor.http.HttpStatusCode
import io.ktor.server.response.respond
import io.ktor.server.response.respondBytes
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import java.io.ByteArrayOutputStream
import java.io.File

object ScreenRoutes {
    /** GET /v1/screen?format=elements|xml and GET /v1/screenshot.png. */
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
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
        // Optional ?format=png|jpeg, ?scale=0..1, ?quality=1..100 (jpeg).
        route.get("/screenshot.png") {
            val format = (call.request.queryParameters["format"] ?: "png").lowercase()
            val scale = call.request.queryParameters["scale"]?.toFloatOrNull() ?: 1.0f
            val quality = (call.request.queryParameters["quality"]?.toIntOrNull() ?: 90).coerceIn(1, 100)
            val cacheDir = instr.targetContext.cacheDir
            val tmp = File.createTempFile("sd-screenshot-", ".png", cacheDir)
            try {
                val ok = uiDevice.takeScreenshot(tmp)
                if (!ok) {
                    call.respond(
                        HttpStatusCode.InternalServerError,
                        ErrorEnvelope(
                            ErrorBody(
                                "screenshot_failed",
                                "UiDevice.takeScreenshot returned false",
                            ),
                        ),
                    )
                    return@get
                }
                // Fast path: untouched PNG when no transform is requested.
                if (format == "png" && scale >= 0.999f) {
                    call.respondBytes(tmp.readBytes(), ContentType.parse("image/png"))
                    return@get
                }
                var bitmap = BitmapFactory.decodeFile(tmp.path)
                if (scale in 0.05f..0.999f) {
                    val w = (bitmap.width * scale).toInt().coerceAtLeast(1)
                    val h = (bitmap.height * scale).toInt().coerceAtLeast(1)
                    bitmap = Bitmap.createScaledBitmap(bitmap, w, h, true)
                }
                val baos = ByteArrayOutputStream()
                val (compress, ctype) =
                    if (format == "jpeg" || format == "jpg") {
                        Bitmap.CompressFormat.JPEG to "image/jpeg"
                    } else {
                        Bitmap.CompressFormat.PNG to "image/png"
                    }
                bitmap.compress(compress, quality, baos)
                call.respondBytes(baos.toByteArray(), ContentType.parse(ctype))
            } finally {
                tmp.delete()
            }
        }
    }
}
