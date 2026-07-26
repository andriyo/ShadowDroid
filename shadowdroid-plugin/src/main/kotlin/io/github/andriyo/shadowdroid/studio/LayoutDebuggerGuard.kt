package io.github.andriyo.shadowdroid.studio

/**
 * Runtime identity requested by a Layout Inspector bridge operation.
 *
 * This model intentionally has no Android Studio dependencies so matching stays
 * deterministic and can be covered by ordinary unit tests.
 */
internal data class LayoutDebuggerRequestTarget(
    val projectKey: String,
    val device: String?,
    val packageName: String?,
    val pid: Int?,
)

/**
 * Best-effort runtime identity resolved for an active Android Studio debugger
 * session.
 */
internal data class LayoutDebuggerSessionTarget(
    val sessionId: String,
    val projectKey: String,
    val deviceSerial: String?,
    val deviceAvd: String?,
    val packageName: String?,
    val processName: String?,
    val pid: Int?,
)

internal object LayoutDebuggerGuard {
    fun matchingSession(
        request: LayoutDebuggerRequestTarget,
        sessions: List<LayoutDebuggerSessionTarget>,
    ): LayoutDebuggerSessionTarget? = sessions.firstOrNull { matches(request, it) }

    internal fun matches(
        request: LayoutDebuggerRequestTarget,
        session: LayoutDebuggerSessionTarget,
    ): Boolean {
        if (request.projectKey != session.projectKey) return false

        val requestedDevice = request.device.nonBlank()
        val sessionDevices = listOfNotNull(
            session.deviceSerial.nonBlank(),
            session.deviceAvd.nonBlank(),
        )
        val deviceMatches = requestedDevice != null && requestedDevice in sessionDevices
        if (requestedDevice != null && sessionDevices.isNotEmpty() && !deviceMatches) return false

        val requestedPackage = request.packageName.nonBlank()
        val sessionPackages = listOfNotNull(
            session.packageName.nonBlank(),
            session.processName.nonBlank(),
        )
        val packageMatches = requestedPackage != null &&
            sessionPackages.any { candidate -> packageMatches(requestedPackage, candidate) }
        if (requestedPackage != null && sessionPackages.isNotEmpty() && !packageMatches) return false

        val pidMatches = request.pid != null && request.pid == session.pid
        if (request.pid != null && session.pid != null && !pidMatches) return false

        // Package/process or PID is the app-process identity. A device match
        // alone must not block Layout Inspector when the debugger client's app
        // metadata is temporarily unavailable.
        if (requestedPackage != null || request.pid != null) {
            return packageMatches || pidMatches
        }

        // Target-less requests can only be scoped by device/project. This is
        // still enough to protect a request against a debugger on that exact
        // target; with no device selector, the selected project is the target.
        return requestedDevice == null || deviceMatches
    }

    private fun packageMatches(requested: String, candidate: String): Boolean =
        requested == candidate || candidate.startsWith("$requested:")

    private fun String?.nonBlank(): String? = this?.takeIf { it.isNotBlank() }
}
