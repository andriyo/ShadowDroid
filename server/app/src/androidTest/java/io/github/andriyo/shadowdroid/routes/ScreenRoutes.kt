package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.os.SystemClock
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.ErrorBody
import io.github.andriyo.shadowdroid.ErrorEnvelope
import io.github.andriyo.shadowdroid.dump.TreeWalker
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.Element
import io.github.andriyo.shadowdroid.proto.ImeState
import io.github.andriyo.shadowdroid.proto.ScreenResponse
import io.github.andriyo.shadowdroid.proto.Viewport
import io.ktor.http.ContentType
import io.ktor.http.HttpStatusCode
import io.ktor.server.response.respond
import io.ktor.server.response.respondBytes
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import java.io.ByteArrayOutputStream
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

object ScreenRoutes {
    /** GET /v1/screen?format=elements|xml and GET /v1/screenshot.png. */
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
        // Activity, PID, and IME visibility previously ran three shell commands
        // in the request path. Keep those compatibility fields, but refresh
        // them asynchronously and serve the latest snapshot so the primary
        // observe loop is bounded by the accessibility-tree walk, not dumpsys.
        val enrichmentCache = ScreenEnrichmentCache.shared(uiDevice, instr)

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
            val viewport = Viewport(uiDevice.displayWidth, uiDevice.displayHeight)
            val elements = TreeWalker.walk(root, viewport.w, viewport.h)
            val pkg = uiDevice.currentPackageName
            val enrichment = enrichmentCache.snapshot(pkg)
            val ime = detectImeState(elements, enrichment)
            val currentApp = AppRef(`package` = pkg, activity = enrichment.activity, pid = enrichment.pid)
            call.respond(
                ScreenResponse(
                    screen_hash = TreeWalker.hashOf(elements, viewport, currentApp, ime),
                    viewport = viewport,
                    current_app = currentApp,
                    element_count = elements.size,
                    ime = ime,
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

private fun detectImeState(
    elements: List<Element>,
    enrichment: ScreenEnrichment,
): ImeState {
    val focusedElement = elements.firstOrNull { it.focused }
    val focusedInput = elements.firstOrNull { it.focused && it.input }
    val keyboardVisible = enrichment.keyboardVisible
    val suggestedActions =
        if (keyboardVisible == true) {
            listOf("shadowdroid ui key back", "shadowdroid ui hide-keyboard")
        } else {
            emptyList()
        }
    return ImeState(
        keyboard_visible = keyboardVisible ?: false,
        focused_element = focusedElement,
        focused_input = focusedInput,
        detection =
            when {
                enrichment.keyboardDetectionAvailable -> "dumpsys input_method (cached)"
                else -> "unavailable"
            },
        reason =
            when {
                enrichment.keyboardDetectionAvailable -> null
                else -> enrichment.keyboardReason
            },
        suggested_actions = suggestedActions,
    )
}

internal data class ScreenEnrichment(
    val `package`: String?,
    val activity: String?,
    val pid: Int?,
    val keyboardVisible: Boolean?,
    val keyboardDetectionAvailable: Boolean,
    val keyboardReason: String?,
    val refreshedAtMs: Long,
)

/**
 * Best-effort, stale-while-refresh cache for fields that require device shell
 * commands. A package transition never reuses the previous package's activity
 * or PID; the first response for the new package reports those fields as null
 * while a refresh runs in the background.
 */
internal class ScreenEnrichmentCache private constructor(
    private val uiDevice: UiDevice,
    private val instr: Instrumentation,
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val refreshInFlight = AtomicBoolean(false)
    private val value =
        AtomicReference(
            ScreenEnrichment(
                `package` = null,
                activity = null,
                pid = null,
                keyboardVisible = null,
                keyboardDetectionAvailable = false,
                keyboardReason = "background refresh pending",
                refreshedAtMs = 0L,
            ),
        )

    init {
        requestRefresh(uiDevice.currentPackageName)
    }

    fun snapshot(currentPackage: String?): ScreenEnrichment {
        val cached = value.get()
        val now = SystemClock.elapsedRealtime()
        val packageMatches = cached.`package` == currentPackage
        if (!packageMatches || now - cached.refreshedAtMs >= ENRICHMENT_TTL_MS) {
            requestRefresh(currentPackage)
        }
        return if (packageMatches) {
            cached
        } else {
            // Keyboard visibility is device-global and remains useful while the
            // package-specific fields refresh.
            cached.copy(`package` = currentPackage, activity = null, pid = null)
        }
    }

    private fun requestRefresh(currentPackage: String?) {
        if (!refreshInFlight.compareAndSet(false, true)) return
        scope.launch {
            try {
                val keyboardDump = runCatching { uiDevice.executeShellCommand("dumpsys input_method") }
                val keyboardVisible = keyboardDump.getOrNull()?.let(::parseKeyboardVisible)
                val keyboardReason =
                    when {
                        keyboardDump.isFailure ->
                            keyboardDump.exceptionOrNull()?.message ?: "dumpsys input_method failed"
                        keyboardVisible == null ->
                            "dumpsys input_method did not expose a recognized keyboard visibility field"
                        else -> null
                    }
                value.set(
                    ScreenEnrichment(
                        `package` = currentPackage,
                        activity = currentFocusedActivity(uiDevice),
                        pid = pidForPackage(instr, uiDevice, currentPackage),
                        keyboardVisible = keyboardVisible,
                        keyboardDetectionAvailable = keyboardVisible != null,
                        keyboardReason = keyboardReason,
                        refreshedAtMs = SystemClock.elapsedRealtime(),
                    ),
                )
            } finally {
                refreshInFlight.set(false)
                // If the foreground package changed while the old package was
                // being enriched, immediately schedule the new snapshot. Do
                // not require a second client request to notice the transition.
                val latestPackage = uiDevice.currentPackageName
                if (latestPackage != currentPackage) {
                    requestRefresh(latestPackage)
                }
            }
        }
    }

    companion object {
        private const val ENRICHMENT_TTL_MS = 1_000L

        @Volatile
        private var instance: ScreenEnrichmentCache? = null

        fun shared(
            uiDevice: UiDevice,
            instr: Instrumentation,
        ): ScreenEnrichmentCache {
            instance?.takeIf { it.uiDevice === uiDevice }?.let { return it }
            return synchronized(this) {
                instance?.takeIf { it.uiDevice === uiDevice }
                    ?: ScreenEnrichmentCache(uiDevice, instr).also { instance = it }
            }
        }
    }
}

private fun parseKeyboardVisible(dumpsys: String): Boolean? {
    Regex("""\bmInputShown=(true|false)\b""")
        .find(dumpsys)
        ?.groupValues
        ?.get(1)
        ?.let { return it == "true" }
    Regex("""\bmImeWindowVis=([0-9A-Fa-fx]+)\b""")
        .find(dumpsys)
        ?.groupValues
        ?.get(1)
        ?.let { raw ->
            parseMaybeHex(raw)?.let { return it != 0 }
        }
    return null
}

private fun parseMaybeHex(raw: String): Int? {
    val value = raw.trim()
    return if (value.startsWith("0x", ignoreCase = true)) {
        value.removePrefix("0x").removePrefix("0X").toIntOrNull(16)
    } else {
        value.toIntOrNull()
    }
}
