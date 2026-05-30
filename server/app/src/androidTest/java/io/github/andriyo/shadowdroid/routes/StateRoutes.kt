package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
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
            val activity = currentFocusedActivity(uiDevice)
            val pid = pidForPackage(uiDevice, pkg)

            val state =
                ServerState(
                    server_version = BuildInfo.SERVER_VERSION,
                    api_version = BuildInfo.API_VERSION,
                    ui_automator_version = BuildInfo.UI_AUTOMATOR_VERSION,
                    android_sdk = Build.VERSION.SDK_INT,
                    android_release = Build.VERSION.RELEASE ?: "",
                    viewport = Viewport(uiDevice.displayWidth, uiDevice.displayHeight),
                    current_app = AppRef(`package` = pkg, activity = activity, pid = pid),
                )
            call.respond(state)
        }
    }
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
