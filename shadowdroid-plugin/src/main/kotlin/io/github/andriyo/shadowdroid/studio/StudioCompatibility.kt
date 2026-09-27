package io.github.andriyo.shadowdroid.studio

import com.intellij.lang.Language
import com.intellij.openapi.application.ApplicationInfo

/**
 * Studio internals the plugin reaches by reflection or by extension id. A
 * Studio update can remove any of them; each check says which feature then
 * runs degraded, so `studio status` can tell the agent instead of the feature
 * failing quietly.
 */
internal object StudioCompatibility {
    private class Check(val feature: String, val impact: String, val probe: () -> Boolean)

    private val checks = listOf(
        Check(
            "breakpoint_apply_barrier",
            "breakpoint add/update can return before the debugger applies the change, so a trigger " +
                "right after it may not stop",
        ) { BreakpointBridge.hasReloadBarrier() },
        Check(
            "running_devices_tabs",
            "layout commands cannot turn Layout Inspector on; enable it in Running Devices by hand",
        ) { LayoutInspectorBridge.canReadRunningDevicesTabs() },
        Check(
            "running_devices_mirroring",
            "layout commands cannot open a device in Running Devices; open it there by hand",
        ) { LayoutInspectorBridge.canShowDevicesInRunningDevices() },
        Check(
            "kotlin_line_breakpoints",
            "breakpoints in Kotlin files fall back to Java line breakpoints, which can miss lambdas and inlined code",
        ) { BreakpointBridge.hasKotlinLineType() },
        Check(
            "kotlin_field_watchpoints",
            "field watchpoints on Kotlin properties are unavailable",
        ) { BreakpointBridge.hasKotlinFieldType() },
        Check(
            "kotlin_condition_validation",
            "Kotlin breakpoint conditions are not syntax-checked before they are set",
        ) { Language.findLanguageByID("kotlin") != null },
    )

    /** Checks are cheap lookups, but Studio can't change under a running plugin: probe once. */
    private val results: Map<String, Boolean> by lazy {
        checks.associate { it.feature to runCatching(it.probe).getOrDefault(false) }
    }

    fun payload(): Map<String, Any?> {
        val degraded = checks
            .filter { results[it.feature] != true }
            .map { mapOf("feature" to it.feature, "impact" to it.impact) }
        val info = runCatching { ApplicationInfo.getInstance() }.getOrNull()
        return mapOf(
            "ok" to degraded.isEmpty(),
            "studio_version" to info?.fullVersion,
            "studio_build" to info?.build?.asString(),
            "checks" to results,
            "degraded" to degraded,
        )
    }
}
