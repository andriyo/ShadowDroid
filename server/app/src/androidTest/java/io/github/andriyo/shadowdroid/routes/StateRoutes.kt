package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.os.Build
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BuildInfo
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.ServerState
import io.github.andriyo.shadowdroid.proto.Viewport
import io.ktor.server.response.*
import io.ktor.server.routing.*

object StateRoutes {
    /** GET /v1/state — cheap version + viewport probe. */
    fun register(route: Route, uiDevice: UiDevice, @Suppress("UNUSED_PARAMETER") instr: Instrumentation) {
        route.get("/state") {
            val pkg = uiDevice.currentPackageName
            val activity = currentFocusedActivity(uiDevice)
            val pid = pidForPackage(uiDevice, pkg)

            val state = ServerState(
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
