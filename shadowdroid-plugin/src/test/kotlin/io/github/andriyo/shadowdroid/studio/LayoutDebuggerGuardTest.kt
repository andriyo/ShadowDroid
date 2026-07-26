package io.github.andriyo.shadowdroid.studio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class LayoutDebuggerGuardTest {
    private val request = LayoutDebuggerRequestTarget(
        projectKey = "/workspace/app",
        device = "emulator-5554",
        packageName = "com.example.app",
        pid = 4312,
    )

    private val session = LayoutDebuggerSessionTarget(
        sessionId = "session_7",
        projectKey = "/workspace/app",
        deviceSerial = "emulator-5554",
        deviceAvd = "Pixel_9_API_36",
        packageName = "com.example.app",
        processName = "com.example.app",
        pid = 4312,
    )

    @Test
    fun exactRuntimeTargetMatchesActiveDebuggerSession() {
        assertEquals(
            session,
            LayoutDebuggerGuard.matchingSession(request, listOf(session)),
        )
    }

    @Test
    fun packageTargetMatchesDebuggerInNamedSubprocess() {
        val subprocess = session.copy(
            packageName = "com.example.app",
            processName = "com.example.app:worker",
        )
        val packageOnly = request.copy(pid = null)

        assertTrue(LayoutDebuggerGuard.matches(packageOnly, subprocess))
    }

    @Test
    fun knownPidDeviceOrPackageMismatchRejectsSession() {
        assertFalse(LayoutDebuggerGuard.matches(request, session.copy(pid = 99)))
        assertFalse(
            LayoutDebuggerGuard.matches(
                request,
                session.copy(deviceSerial = "emulator-5556", deviceAvd = "Other_Device"),
            ),
        )
        assertFalse(
            LayoutDebuggerGuard.matches(
                request,
                session.copy(packageName = "com.example.other", processName = "com.example.other"),
            ),
        )
    }

    @Test
    fun projectMismatchRejectsOtherwiseExactSession() {
        assertFalse(
            LayoutDebuggerGuard.matches(
                request,
                session.copy(projectKey = "/workspace/other"),
            ),
        )
    }

    @Test
    fun deviceMatchAloneDoesNotClaimRequestedAppWhenClientIdentityIsUnknown() {
        val unresolvedClient = session.copy(
            packageName = null,
            processName = null,
            pid = null,
        )

        assertNull(LayoutDebuggerGuard.matchingSession(request, listOf(unresolvedClient)))
    }

    @Test
    fun exactPidCanMatchWhenPackageMetadataIsUnavailable() {
        val pidOnlySession = session.copy(packageName = null, processName = null)

        assertTrue(LayoutDebuggerGuard.matches(request, pidOnlySession))
    }

    @Test
    fun targetlessRequestUsesProjectAndOptionalDeviceScope() {
        val projectOnly = request.copy(device = null, packageName = null, pid = null)
        val deviceOnly = request.copy(packageName = null, pid = null)

        assertTrue(LayoutDebuggerGuard.matches(projectOnly, session))
        assertTrue(LayoutDebuggerGuard.matches(deviceOnly, session))
        assertFalse(
            LayoutDebuggerGuard.matches(
                deviceOnly,
                session.copy(deviceSerial = "emulator-5556", deviceAvd = "Other_Device"),
            ),
        )
    }
}
