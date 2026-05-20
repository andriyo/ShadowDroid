package io.github.andriyo.shadowdroid.routes

import androidx.test.uiautomator.UiDevice

/**
 * Cross-route helpers. Kept here so we don't duplicate the dumpsys-parsing
 * regex in every place that needs current_app info.
 */

/** Activity FQN of whatever has window focus, via `dumpsys activity activities`. */
internal fun currentFocusedActivity(ui: UiDevice): String? {
    val out = runCatching { ui.executeShellCommand("dumpsys activity activities") }.getOrNull()
        ?: return null
    val resumed = out.lineSequence().firstOrNull { it.contains("ResumedActivity:") } ?: return null
    val regex = Regex("""([A-Za-z0-9_.]+)/([A-Za-z0-9_.${'$'}]+)""")
    val m = regex.find(resumed) ?: return null
    val activity = m.groupValues[2]
    return if (activity.startsWith(".")) m.groupValues[1] + activity else activity
}

/** PID of the given package's foreground process, or null. */
internal fun pidForPackage(ui: UiDevice, pkg: String?): Int? {
    if (pkg.isNullOrEmpty()) return null
    val out = runCatching { ui.executeShellCommand("pidof $pkg") }.getOrNull() ?: return null
    return out.trim().split(Regex("\\s+")).firstOrNull()?.toIntOrNull()
}
