package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.os.Build
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BuildInfo
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.ServerState
import io.github.andriyo.shadowdroid.proto.Viewport
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import kotlinx.serialization.Serializable

object StateRoutes {
    /** GET /v1/state — cheap version + viewport probe. GET /v1/device — detail. */
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
        // `server_version` is read once from the installed main-app APK's
        // versionName (set from -Pversion at release time in build.gradle.kts).
        // This is the *same* value the CLI's install-time gate checks via
        // `dumpsys package … versionName`, so the version the server reports and
        // the version on the APK can never disagree. The compiled-in constant is
        // only a fallback for the unlikely case PackageManager can't see us.
        val serverVersion = resolveServerVersion(instr)
        val enrichmentCache = ScreenEnrichmentCache.shared(uiDevice, instr)

        route.get("/device") {
            val cfg = instr.targetContext.resources.configuration
            val metrics = instr.targetContext.resources.displayMetrics
            val locale =
                if (!cfg.locales.isEmpty) cfg.locales[0].toLanguageTag() else ""
            call.respond(
                DeviceInfo(
                    manufacturer = Build.MANUFACTURER ?: "",
                    model = Build.MODEL ?: "",
                    brand = Build.BRAND ?: "",
                    device = Build.DEVICE ?: "",
                    product = Build.PRODUCT ?: "",
                    fingerprint = Build.FINGERPRINT ?: "",
                    android_release = Build.VERSION.RELEASE ?: "",
                    android_sdk = Build.VERSION.SDK_INT,
                    locale = locale,
                    density_dpi = metrics.densityDpi,
                ),
            )
        }

        route.get("/state") {
            val pkg = uiDevice.currentPackageName
            val enrichment = enrichmentCache.snapshot(pkg)

            val state =
                ServerState(
                    server_version = serverVersion,
                    api_version = BuildInfo.API_VERSION,
                    ui_automator_version = BuildInfo.UI_AUTOMATOR_VERSION,
                    android_sdk = Build.VERSION.SDK_INT,
                    android_release = Build.VERSION.RELEASE ?: "",
                    viewport = Viewport(uiDevice.displayWidth, uiDevice.displayHeight),
                    current_app = AppRef(`package` = pkg, activity = enrichment.activity, pid = enrichment.pid),
                    is_television = isTelevision(instr),
                )
            call.respond(state)
        }
    }
}

/**
 * The on-device server's version: the installed main-app APK's versionName,
 * which build.gradle.kts sets from `-Pversion` at release time. Read from
 * PackageManager (via the instrumentation target context) so it always equals
 * the version stamped on the APK. Falls back to the compiled-in constant only if
 * the package somehow isn't visible.
 */
private fun resolveServerVersion(instr: Instrumentation): String =
    runCatching {
        val ctx = instr.targetContext
        ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName
    }.getOrNull()?.takeIf { it.isNotBlank() } ?: BuildInfo.SERVER_VERSION

/**
 * True on Android TV / leanback devices, where the UI is focus + D-pad driven
 * rather than touch driven. Checks the uiMode television bit first, then falls
 * back to the leanback system feature (covers devices reporting the feature
 * without the uiMode flag).
 */
private fun isTelevision(instr: Instrumentation): Boolean {
    val ctx = instr.targetContext
    val uiModeType = ctx.resources.configuration.uiMode and Configuration.UI_MODE_TYPE_MASK
    if (uiModeType == Configuration.UI_MODE_TYPE_TELEVISION) return true
    return ctx.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK)
}

@Serializable
private data class DeviceInfo(
    val manufacturer: String,
    val model: String,
    val brand: String,
    val device: String,
    val product: String,
    val fingerprint: String,
    val android_release: String,
    val android_sdk: Int,
    val locale: String,
    val density_dpi: Int,
)
