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
import io.github.andriyo.shadowdroid.proto.StableScreenResponse
import io.github.andriyo.shadowdroid.proto.UiTreeSnapshot
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
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.io.ByteArrayOutputStream
import java.io.File
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import java.util.concurrent.TimeoutException

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
            val snapshot = captureScreen(uiDevice, instr, enrichmentCache)
            call.respond(snapshot.toResponse())
        }

        // Accessibility-event-backed post-action observation. waitForIdle
        // requires a real quiet period, so delayed Compose/navigation events
        // reset the clock instead of a fixed host sleep capturing the source
        // tree as the destination.
        route.get("/screen/stable") {
            val quietMs =
                (call.request.queryParameters["quiet_ms"]?.toLongOrNull() ?: DEFAULT_OBSERVE_QUIET_MS)
                    .coerceIn(MIN_OBSERVE_QUIET_MS, MAX_OBSERVE_QUIET_MS)
            val timeoutMs =
                (call.request.queryParameters["timeout_ms"]?.toLongOrNull() ?: DEFAULT_OBSERVE_TIMEOUT_MS)
                    .coerceIn(MIN_OBSERVE_TIMEOUT_MS, MAX_OBSERVE_TIMEOUT_MS)
            val started = SystemClock.elapsedRealtime()
            val idle =
                try {
                    instr.uiAutomation.waitForIdle(quietMs, timeoutMs)
                    true
                } catch (_: TimeoutException) {
                    false
                }
            val snapshot = captureScreen(uiDevice, instr, enrichmentCache)
            call.respond(
                StableScreenResponse(
                    stable = idle && snapshot.assessment.state == "consistent",
                    settle_ms = (SystemClock.elapsedRealtime() - started).coerceAtLeast(0L),
                    quiet_period_ms = quietMs,
                    screen = snapshot.toResponse(),
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

private fun CapturedScreen.toResponse(): ScreenResponse {
    val ime = detectImeState(elements, enrichment)
    val currentApp =
        AppRef(
            `package` = foregroundPackage,
            activity = enrichment.activity,
            pid = enrichment.pid,
            sampled_at_ms = enrichment.sampledAtMs.takeIf { it > 0L },
        )
    return ScreenResponse(
        screen_hash = TreeWalker.hashOf(elements, viewport, currentApp, ime),
        snapshot_state = assessment.state,
        captured_at_ms = capturedAtMs,
        viewport = viewport,
        current_app = currentApp,
        ui_tree =
            UiTreeSnapshot(
                sampled_at_ms = treeSampledAtMs,
                age_ms = (capturedAtMs - treeSampledAtMs).coerceAtLeast(0L),
                `package` = treePackage,
                window_id = treeWindowId,
            ),
        warning = assessment.warning,
        element_count = elements.size,
        ime = ime,
        elements = elements,
    )
}

private data class CapturedScreen(
    val viewport: Viewport,
    val elements: List<Element>,
    val treePackage: String?,
    val treeWindowId: Int?,
    val treeSampledAtMs: Long,
    val foregroundPackage: String?,
    val enrichment: ScreenEnrichment,
    val assessment: SnapshotAssessment,
    val capturedAtMs: Long,
)

internal data class SnapshotAssessment(
    val state: String,
    val warning: String? = null,
)

/**
 * Capture UI tree and foreground metadata inside one bounded convergence loop.
 * Stable screens take the first-pass/cache fast path. During lifecycle changes,
 * retry until the tree package and complete foreground metadata agree, or make
 * the transition explicit instead of returning a contradictory authoritative
 * snapshot.
 */
private suspend fun captureScreen(
    uiDevice: UiDevice,
    instr: Instrumentation,
    enrichmentCache: ScreenEnrichmentCache,
): CapturedScreen {
    val deadline = SystemClock.elapsedRealtime() + SNAPSHOT_CONVERGENCE_MS
    var latest: CapturedScreen? = null
    do {
        val viewport = Viewport(uiDevice.displayWidth, uiDevice.displayHeight)
        val root = instr.uiAutomation.rootInActiveWindow
        val elements = TreeWalker.walk(root, viewport.w, viewport.h)
        val treePackage = root?.packageName?.toString()?.takeIf { it.isNotBlank() }
        val treeWindowId = root?.windowId
        val treeReady = elements.isNotEmpty() || (root?.childCount ?: 0) > 0
        val treeSampledAtMs = System.currentTimeMillis()
        val foregroundPackage = uiDevice.currentPackageName
        val remaining = (deadline - SystemClock.elapsedRealtime()).coerceAtLeast(0L)
        val enrichment =
            enrichmentCache.snapshot(
                currentPackage = foregroundPackage,
                treeWindowId = treeWindowId,
                requireComplete = elements.isNotEmpty(),
                waitMs = remaining.coerceAtMost(ENRICHMENT_WAIT_SLICE_MS),
            )
        val assessment =
            assessSnapshot(
                treePackage = treePackage,
                treeWindowId = treeWindowId,
                treeReady = treeReady,
                elementCount = elements.size,
                foregroundPackage = foregroundPackage,
                enrichment = enrichment,
            )
        latest =
            CapturedScreen(
                viewport = viewport,
                elements = elements,
                treePackage = treePackage,
                treeWindowId = treeWindowId,
                treeSampledAtMs = treeSampledAtMs,
                foregroundPackage = foregroundPackage,
                enrichment = enrichment,
                assessment = assessment,
                capturedAtMs = System.currentTimeMillis(),
            )
        if (assessment.state == "consistent" || SystemClock.elapsedRealtime() >= deadline) {
            return latest
        }
        delay(SNAPSHOT_RETRY_MS)
    } while (SystemClock.elapsedRealtime() < deadline)
    return checkNotNull(latest)
}

internal fun assessSnapshot(
    treePackage: String?,
    treeWindowId: Int?,
    treeReady: Boolean,
    elementCount: Int,
    foregroundPackage: String?,
    enrichment: ScreenEnrichment,
): SnapshotAssessment {
    if (elementCount > 0 && treePackage == null) {
        return SnapshotAssessment(
            "transitioning",
            "UI tree package was unavailable while visible elements were present",
        )
    }
    if (treePackage != null && treePackage != foregroundPackage) {
        return SnapshotAssessment(
            "transitioning",
            "UI tree package '$treePackage' did not match foreground package '$foregroundPackage'",
        )
    }
    if (!treeReady && foregroundPackage != null) {
        return SnapshotAssessment(
            "transitioning",
            "foreground UI tree had not produced accessible content after the bounded consistency check",
        )
    }
    if (elementCount > 0 && enrichment.windowId != treeWindowId) {
        return SnapshotAssessment(
            "transitioning",
            "UI tree window '$treeWindowId' did not match foreground metadata window '${enrichment.windowId}'",
        )
    }
    if (foregroundPackage != enrichment.`package`) {
        return SnapshotAssessment(
            "transitioning",
            "foreground metadata had not converged for package '$foregroundPackage'",
        )
    }
    if (elementCount > 0 && foregroundPackage != null &&
        (enrichment.activity == null || enrichment.pid == null)
    ) {
        return SnapshotAssessment(
            "transitioning",
            "foreground activity/PID remained incomplete after the bounded consistency check",
        )
    }
    return SnapshotAssessment("consistent")
}

private const val SNAPSHOT_CONVERGENCE_MS = 800L
private const val ENRICHMENT_WAIT_SLICE_MS = 250L
private const val SNAPSHOT_RETRY_MS = 40L
private const val MIN_OBSERVE_QUIET_MS = 50L
private const val MAX_OBSERVE_QUIET_MS = 2_000L
private const val DEFAULT_OBSERVE_QUIET_MS = 500L
private const val DEFAULT_OBSERVE_TIMEOUT_MS = 3_000L
private const val MIN_OBSERVE_TIMEOUT_MS = 1L
private const val MAX_OBSERVE_TIMEOUT_MS = 10_000L

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
    val windowId: Int?,
    val sampledAtMs: Long,
    val refreshedAtElapsedMs: Long,
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
                windowId = null,
                sampledAtMs = 0L,
                refreshedAtElapsedMs = 0L,
            ),
        )

    init {
        requestRefresh(uiDevice.currentPackageName)
    }

    suspend fun snapshot(
        currentPackage: String?,
        treeWindowId: Int?,
        requireComplete: Boolean,
        waitMs: Long,
    ): ScreenEnrichment {
        val deadline = SystemClock.elapsedRealtime() + waitMs
        while (true) {
            val cached = value.get()
            val now = SystemClock.elapsedRealtime()
            val packageMatches = cached.`package` == currentPackage
            val complete = !requireComplete || currentPackage == null ||
                (
                    cached.activity != null &&
                        cached.pid != null &&
                        cached.windowId == treeWindowId
                )
            val fresh = now - cached.refreshedAtElapsedMs < ENRICHMENT_TTL_MS
            if (packageMatches && complete && fresh) {
                return cached
            }
            requestRefresh(currentPackage)
            if (now >= deadline) {
                return if (packageMatches) {
                    cached
                } else {
                    // Keyboard visibility is device-global and remains useful
                    // while package-specific fields are still converging.
                    cached.copy(
                        `package` = currentPackage,
                        activity = null,
                        pid = null,
                        windowId = null,
                        sampledAtMs = 0L,
                    )
                }
            }
            delay(ENRICHMENT_POLL_MS)
        }
    }

    fun invalidate() {
        value.updateAndGet { it.copy(refreshedAtElapsedMs = 0L) }
        requestRefresh(uiDevice.currentPackageName)
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
                val windowBefore = instr.uiAutomation.rootInActiveWindow?.windowId
                val focused = currentFocusedApp(uiDevice)
                val sampledPackage = focused?.`package` ?: currentPackage
                val pid = pidForPackage(instr, uiDevice, sampledPackage)
                val windowAfter = instr.uiAutomation.rootInActiveWindow?.windowId
                val sampledWindowId = windowBefore.takeIf { it != null && it == windowAfter }
                value.set(
                    ScreenEnrichment(
                        `package` = sampledPackage,
                        activity = focused?.activity,
                        pid = pid,
                        keyboardVisible = keyboardVisible,
                        keyboardDetectionAvailable = keyboardVisible != null,
                        keyboardReason = keyboardReason,
                        windowId = sampledWindowId,
                        sampledAtMs = System.currentTimeMillis(),
                        refreshedAtElapsedMs = SystemClock.elapsedRealtime(),
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
        private const val ENRICHMENT_POLL_MS = 25L

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
