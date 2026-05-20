package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.content.Intent
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.github.andriyo.shadowdroid.proto.AppRef
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.request.receive
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import io.ktor.server.routing.post
import kotlinx.serialization.Serializable

object AppRoutes {
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
        route.post("/app/start") {
            val r: PkgReq = call.receive()
            // First try PackageManager (gives us a clean ActivityNotFoundException
            // if the app really is missing). Fall back to `monkey` via shell,
            // which doesn't need package visibility — useful for apps in other
            // user profiles or when the manifest's <queries> is too restrictive.
            val ctx = instr.context // test-package context, has QUERY_ALL_PACKAGES
            val intent =
                try {
                    ctx.packageManager.getLaunchIntentForPackage(r.`package`)
                } catch (_: Throwable) {
                    null
                }
            if (intent != null) {
                intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
                ctx.startActivity(intent)
            } else {
                // `monkey -p PKG -c LAUNCHER 1` sends one MAIN+LAUNCHER intent.
                // Errors visibly in the shell output if the package isn't found.
                val out =
                    uiDevice.executeShellCommand(
                        "monkey -p ${r.`package`} -c android.intent.category.LAUNCHER 1",
                    )
                if (out.contains("No activities found")) {
                    throw NotFound(
                        "no_launch_intent",
                        "no launcher activity for '${r.`package`}' — is it installed?",
                    )
                }
            }
            call.respond(OkResponse())
        }
        route.post("/app/stop") {
            val r: PkgReq = call.receive()
            uiDevice.executeShellCommand("am force-stop ${r.`package`}")
            call.respond(OkResponse())
        }
        route.post("/app/clear") {
            val r: PkgReq = call.receive()
            uiDevice.executeShellCommand("pm clear ${r.`package`}")
            call.respond(OkResponse())
        }
        route.post("/app/wait") {
            val r: AppWaitReq = call.receive()
            val deadline = System.currentTimeMillis() + r.timeout_ms
            var lastPkg: String? = null
            while (System.currentTimeMillis() < deadline) {
                val cur = uiDevice.currentPackageName
                lastPkg = cur
                val match =
                    if (r.front) {
                        cur == r.`package`
                    } else {
                        (cur == r.`package` || pidForPackage(uiDevice, r.`package`) != null)
                    }
                if (match) {
                    call.respond(AppWaitResp(matched = true, current = cur))
                    return@post
                }
                Thread.sleep(100)
            }
            call.respond(AppWaitResp(matched = false, current = lastPkg))
        }
        route.get("/app/info") {
            val pkg =
                call.request.queryParameters["package"]
                    ?: throw BadRequest("missing_package", "?package= is required")
            // Try PackageManager first (clean structured info). Fall back to
            // `dumpsys package` parsing if the package isn't visible to us
            // (only possible if QUERY_ALL_PACKAGES isn't granted — shouldn't
            // happen on our APK, but keep the fallback for robustness).
            val pm = instr.context.packageManager
            val info =
                try {
                    pm.getPackageInfo(pkg, 0)
                } catch (_: Throwable) {
                    null
                }
            if (info != null) {
                call.respond(
                    AppInfoResp(
                        version_name = info.versionName,
                        version_code = (info.longVersionCode and 0xFFFFFFFFL).toInt(),
                        label = info.applicationInfo?.let { pm.getApplicationLabel(it).toString() } ?: pkg,
                    ),
                )
                return@get
            }
            // Fallback: parse dumpsys
            val out = uiDevice.executeShellCommand("dumpsys package $pkg")
            val versionName =
                Regex("""versionName=(.+)""")
                    .find(out)
                    ?.groupValues
                    ?.get(1)
                    ?.trim()
            val versionCode =
                Regex("""versionCode=(\d+)""")
                    .find(out)
                    ?.groupValues
                    ?.get(1)
                    ?.toIntOrNull()
            if (versionName == null && versionCode == null) {
                throw NotFound("package_not_found", "no installed package '$pkg'")
            }
            call.respond(
                AppInfoResp(
                    version_name = versionName,
                    version_code = versionCode ?: 0,
                    label = pkg,
                ),
            )
        }
        route.get("/app/current") {
            val pkg = uiDevice.currentPackageName
            call.respond(
                AppRef(
                    `package` = pkg,
                    activity = currentFocusedActivity(uiDevice),
                    pid = pidForPackage(uiDevice, pkg),
                ),
            )
        }
    }
}

@Serializable
private data class PkgReq(
    val `package`: String,
)

@Serializable
private data class AppWaitReq(
    val `package`: String,
    val timeout_ms: Int = 20000,
    val front: Boolean = false,
)

@Serializable
private data class AppWaitResp(
    val matched: Boolean,
    val current: String? = null,
)

@Serializable
private data class AppInfoResp(
    val version_name: String?,
    val version_code: Int,
    val label: String,
)
