package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest

/*
 * Cross-route helpers. Kept here so we don't duplicate the dumpsys-parsing
 * regex in every place that needs current_app info.
 */

internal data class FocusedApp(
    val `package`: String,
    val activity: String,
)

/** Foreground package/activity parsed across current and legacy dumpsys shapes. */
internal fun currentFocusedApp(ui: UiDevice): FocusedApp? {
    val out =
        runCatching { ui.executeShellCommand("dumpsys activity activities") }.getOrNull()
            ?: return null
    return parseFocusedApp(out)
}

internal fun parseFocusedApp(dumpsys: String): FocusedApp? {
    val regex = Regex("""([A-Za-z0-9_.]+)/([A-Za-z0-9_.${'$'}]+)""")
    val m =
        listOf("topResumedActivity=", "mResumedActivity:", "ResumedActivity:")
            .firstNotNullOfOrNull { marker ->
                dumpsys.lineSequence()
                    .filter { it.contains(marker) }
                    .mapNotNull(regex::find)
                    .firstOrNull()
            } ?: return null
    val pkg = m.groupValues[1]
    val activity = m.groupValues[2]
    return FocusedApp(
        `package` = pkg,
        activity = if (activity.startsWith(".")) pkg + activity else activity,
    )
}

/** Activity FQN of whatever has window focus, via `dumpsys activity activities`. */
internal fun currentFocusedActivity(ui: UiDevice): String? = currentFocusedApp(ui)?.activity

/** PID of the given package's foreground process, or null. */
internal fun pidForPackage(
    instr: Instrumentation,
    ui: UiDevice,
    pkg: String?,
): Int? {
    if (pkg.isNullOrEmpty()) return null
    val packageName = requireAndroidPackage(pkg)
    val (out, exitCode) =
        runCatching {
            runDeviceShell(
                instr,
                ui,
                "pidof ${quoteDeviceShellArg(packageName)}",
                timeoutMs = 5_000,
            )
        }.getOrNull()
            ?: return null
    if (exitCode?.let { it != 0 } == true) return null
    return out
        .trim()
        .split(Regex("\\s+"))
        .firstOrNull()
        ?.toIntOrNull()
}

/**
 * Validate an Android package before it reaches PackageManager or a shell
 * command. User application ids contain at least two dot-separated Java-like
 * identifiers. `android` is the platform package's intentional exception.
 */
internal fun requireAndroidPackage(value: String): String {
    val segments = value.split('.')
    val valid =
        value == "android" ||
            (
                value.isNotEmpty() &&
                    value.trim() == value &&
                    segments.size >= 2 &&
                    segments.all { segment ->
                        segment.isNotEmpty() &&
                            segment.first().isAsciiLetter() &&
                            segment.drop(1).all { it.isAsciiLetterOrDigit() || it == '_' }
                    }
            )
    if (!valid) {
        throw BadRequest(
            "invalid_package",
            "invalid Android package '$value'; expected dot-separated ASCII identifiers",
        )
    }
    return value
}

/** Resolve and validate the activity forms accepted by `app start --activity`. */
internal fun normalizeAndroidActivity(
    pkg: String,
    raw: String,
): String {
    requireAndroidPackage(pkg)
    var activity = raw.trim()
    if (activity.contains('/')) {
        val parts = activity.split('/', limit = 2)
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
    val resolved =
        when {
            activity.startsWith('.') -> pkg + activity
            !activity.contains('.') -> "$pkg.$activity"
            else -> activity
        }
    val valid =
        resolved.split('.').all { segment ->
            segment.isNotEmpty() &&
                (segment.first().isAsciiLetter() || segment.first() == '_' || segment.first() == '$') &&
                segment.drop(1).all {
                    it.isAsciiLetterOrDigit() || it == '_' || it == '$'
                }
        }
    if (!valid) {
        throw BadRequest(
            "invalid_activity",
            "invalid Android activity '$raw'; expected a Java class name",
        )
    }
    return resolved
}

/** Quote one argument for Android's POSIX-like device shell. */
internal fun quoteDeviceShellArg(value: String): String = "'${value.replace("'", "'\"'\"'")}'"

private fun Char.isAsciiLetter(): Boolean = this in 'a'..'z' || this in 'A'..'Z'

private fun Char.isAsciiLetterOrDigit(): Boolean = isAsciiLetter() || this in '0'..'9'
