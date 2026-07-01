package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.content.Intent
import android.content.pm.PackageManager
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
            val ctx = instr.context // test-package context, has QUERY_ALL_PACKAGES
            val launchers = launcherActivities(ctx.packageManager, r.`package`)
            val explicit = r.activity?.trim()?.takeIf { it.isNotEmpty() }
            val launchedActivity =
                if (explicit != null) {
                    val activity = normalizeActivity(r.`package`, explicit)
                    val intent =
                        Intent()
                            .setClassName(r.`package`, activity)
                            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
                    try {
                        ctx.startActivity(intent)
                    } catch (t: Throwable) {
                        throw BadRequest(
                            "activity_start_failed",
                            "failed to start ${r.`package`}/$activity",
                            detail = mapOf("error" to (t.message ?: t::class.java.name)),
                        )
                    }
                    waitForForegroundActivity(uiDevice, r.`package`, activity)
                        ?: throw BadRequest(
                            "activity_start_not_foreground",
                            "started ${r.`package`}/$activity, but it did not reach the foreground",
                            detail = mapOf(
                                "current_package" to uiDevice.currentPackageName,
                                "current_activity" to (currentFocusedActivity(uiDevice) ?: ""),
                            ),
                        )
                } else {
                    // First try PackageManager (gives us a clean ActivityNotFoundException
                    // if the app really is missing). Fall back to `monkey` via shell,
                    // which doesn't need package visibility — useful for apps in other
                    // user profiles or when the manifest's <queries> is too restrictive.
                    val intent =
                        try {
                            ctx.packageManager.getLaunchIntentForPackage(r.`package`)
                        } catch (_: Throwable) {
                            null
                        }
                    if (intent != null) {
                        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
                        val resolved = intent.resolveActivity(ctx.packageManager)
                        ctx.startActivity(intent)
                        waitForForegroundActivity(uiDevice, r.`package`, resolved?.className ?: intent.component?.className)
                            ?: resolved?.className
                            ?: intent.component?.className
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
                        null
                    }
                }
            val warning =
                if (explicit == null && launchers.size > 1) {
                    "package exposes multiple launcher activities; pass --activity with the intended activity to avoid Android choosing the wrong one"
                } else {
                    null
                }
            call.respond(
                AppStartResp(
                    activity = launchedActivity,
                    launcher_activities = launchers,
                    warning = warning,
                ),
            )
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
    val activity: String? = null,
)

@Serializable
private data class AppStartResp(
    val ok: Boolean = true,
    val activity: String? = null,
    val launcher_activities: List<String> = emptyList(),
    val warning: String? = null,
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

private fun launcherActivities(
    pm: PackageManager,
    pkg: String,
): List<String> {
    val intent =
        Intent(Intent.ACTION_MAIN)
            .addCategory(Intent.CATEGORY_LAUNCHER)
            .setPackage(pkg)
    return try {
        pm.queryIntentActivities(intent, 0)
            .mapNotNull { it.activityInfo?.name }
            .distinct()
            .sorted()
    } catch (_: Throwable) {
        emptyList()
    }
}

private fun normalizeActivity(
    pkg: String,
    raw: String,
): String {
    var activity = raw.trim()
    if (activity.contains("/")) {
        val parts = activity.split("/", limit = 2)
        if (parts.first() != pkg) {
            throw BadRequest(
                "activity_package_mismatch",
                "activity component package '${parts.first()}' does not match '$pkg'",
            )
        }
        activity = parts.getOrElse(1) { "" }
    }
    if (activity.isBlank()) {
        throw BadRequest("missing_activity", "--activity must not be empty")
    }
    return if (activity.startsWith(".")) pkg + activity else activity
}

private fun waitForForegroundActivity(
    uiDevice: UiDevice,
    pkg: String,
    expectedActivity: String?,
    timeoutMs: Long = 5000L,
): String? {
    val deadline = System.currentTimeMillis() + timeoutMs
    while (System.currentTimeMillis() < deadline) {
        val activity = currentFocusedActivity(uiDevice)
        if (uiDevice.currentPackageName == pkg && (expectedActivity == null || activity == expectedActivity)) {
            return activity
        }
        Thread.sleep(100)
    }
    return null
}
