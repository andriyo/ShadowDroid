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
            val pkg = requireAndroidPackage(r.`package`)
            val ctx = instr.context // test-package context, has QUERY_ALL_PACKAGES
            val launchers = launcherActivities(ctx.packageManager, pkg)
            val explicit = r.activity?.trim()?.takeIf { it.isNotEmpty() }
            val launchedActivity =
                if (explicit != null) {
                    val activity = normalizeAndroidActivity(pkg, explicit)
                    val intent =
                        Intent()
                            .setClassName(pkg, activity)
                            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
                    try {
                        ctx.startActivity(intent)
                    } catch (t: Throwable) {
                        throw BadRequest(
                            "activity_start_failed",
                            "failed to start $pkg/$activity",
                            detail = mapOf("error" to (t.message ?: t::class.java.name)),
                        )
                    }
                    waitForForegroundActivity(uiDevice, pkg, activity)
                        ?: throw BadRequest(
                            "activity_start_not_foreground",
                            "started $pkg/$activity, but it did not reach the foreground",
                            detail =
                                mapOf(
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
                            ctx.packageManager.getLaunchIntentForPackage(pkg)
                        } catch (_: Throwable) {
                            null
                        }
                    if (intent != null) {
                        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
                        val resolved = intent.resolveActivity(ctx.packageManager)
                        ctx.startActivity(intent)
                        waitForForegroundActivity(uiDevice, pkg, resolved?.className ?: intent.component?.className)
                            ?: resolved?.className
                            ?: intent.component?.className
                    } else {
                        // `monkey -p PKG -c LAUNCHER 1` sends one MAIN+LAUNCHER intent.
                        // Errors visibly in the shell output if the package isn't found.
                        val out =
                            uiDevice.executeShellCommand(
                                "monkey -p ${quoteDeviceShellArg(pkg)} -c android.intent.category.LAUNCHER 1",
                            )
                        if (out.contains("No activities found")) {
                            throw NotFound(
                                "no_launch_intent",
                                "no launcher activity for '$pkg' — is it installed?",
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
            ScreenEnrichmentCache.shared(uiDevice, instr).invalidate()
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
            val pkg = requireAndroidPackage(r.`package`)
            requireInstalledPackage(instr, uiDevice, pkg)
            val (output, exitCode) =
                runDeviceShell(
                    instr,
                    uiDevice,
                    "am force-stop ${quoteDeviceShellArg(pkg)}",
                    timeoutMs = 20_000,
                )
            val deadline = System.currentTimeMillis() + 2_000
            var remainingPid = pidForPackage(instr, uiDevice, pkg)
            while (remainingPid != null && System.currentTimeMillis() < deadline) {
                Thread.sleep(50)
                remainingPid = pidForPackage(instr, uiDevice, pkg)
            }
            if (exitCode?.let { it != 0 } == true || remainingPid != null) {
                throw BadRequest(
                    "app_stop_failed",
                    "Android did not fully stop '$pkg'",
                    detail =
                        mapOf(
                            "package" to pkg,
                            "output" to output.trim(),
                            "exit_code" to exitCode,
                            "remaining_pid" to remainingPid,
                        ),
                )
            }
            ScreenEnrichmentCache.shared(uiDevice, instr).invalidate()
            call.respond(OkResponse())
        }
        route.post("/app/clear") {
            val r: PkgReq = call.receive()
            val pkg = requireAndroidPackage(r.`package`)
            requireInstalledPackage(instr, uiDevice, pkg)
            val (output, exitCode) =
                runDeviceShell(
                    instr,
                    uiDevice,
                    "pm clear ${quoteDeviceShellArg(pkg)}",
                    timeoutMs = 20_000,
                )
            if (exitCode?.let { it != 0 } == true || !pmClearSucceeded(output)) {
                throw BadRequest(
                    "app_clear_failed",
                    "Android did not clear app data for '$pkg'",
                    detail =
                        mapOf(
                            "package" to pkg,
                            "output" to output.trim(),
                            "exit_code" to exitCode,
                        ),
                )
            }
            ScreenEnrichmentCache.shared(uiDevice, instr).invalidate()
            call.respond(OkResponse())
        }
        route.post("/app/wait") {
            val r: AppWaitReq = call.receive()
            val pkg = requireAndroidPackage(r.`package`)
            val deadline = System.currentTimeMillis() + r.timeout_ms
            var lastPkg: String? = null
            while (System.currentTimeMillis() < deadline) {
                val cur = uiDevice.currentPackageName
                lastPkg = cur
                val match =
                    if (r.front) {
                        cur == pkg
                    } else {
                        (cur == pkg || pidForPackage(instr, uiDevice, pkg) != null)
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
            requireAndroidPackage(pkg)
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
            val out = uiDevice.executeShellCommand("dumpsys package ${quoteDeviceShellArg(pkg)}")
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
                    pid = pidForPackage(instr, uiDevice, pkg),
                ),
            )
        }
    }
}

private fun requireInstalledPackage(
    instr: Instrumentation,
    uiDevice: UiDevice,
    pkg: String,
) {
    val visibleToPackageManager =
        runCatching { instr.context.packageManager.getPackageInfo(pkg, 0) }
            .isSuccess
    if (visibleToPackageManager) return

    val output = uiDevice.executeShellCommand("pm path ${quoteDeviceShellArg(pkg)}")
    if (!packagePathExists(output)) {
        throw NotFound(
            "package_not_found",
            "no installed package '$pkg'",
            detail = mapOf("package" to pkg, "pm_path_output" to output.trim()),
        )
    }
}

internal fun packagePathExists(output: String): Boolean = output.lineSequence().any { line -> line.trim().startsWith("package:") }

internal fun pmClearSucceeded(output: String): Boolean = output.trim() == "Success"

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
        pm
            .queryIntentActivities(intent, 0)
            .mapNotNull { it.activityInfo?.name }
            .distinct()
            .sorted()
    } catch (_: Throwable) {
        emptyList()
    }
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
