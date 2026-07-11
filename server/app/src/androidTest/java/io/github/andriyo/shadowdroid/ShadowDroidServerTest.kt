package io.github.andriyo.shadowdroid

import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.filters.LargeTest
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import org.junit.After
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Starts the ShadowDroid HTTP server inside a JUnit test, then loops forever
 * to keep the Instrumentation process alive. Same pattern as openatx's
 * `android-uiautomator-server` Stub class.
 *
 * Why a JUnit @Test and not a custom AndroidJUnitRunner subclass?
 *   AndroidJUnitRunner sets up UiAutomation as part of its normal test-discovery
 *   path. If we override `onStart()` to skip test discovery, we also skip the
 *   UiAutomation init — then `UiDevice.getInstance()` from our code races the
 *   framework's setup and throws "UiAutomationService already registered!".
 *   By running as a real @Test, we let the framework do its job and UiDevice
 *   just works.
 *
 * Lifecycle:
 *   • `@Before setUp()` — get UiDevice (framework guarantees it's ready),
 *     start Ktor HTTP server.
 *   • `@Test runServerForever()` — block on a sentinel loop. The HTTP server
 *     runs on Ktor's own threads; this method only exists to keep the
 *     Instrumentation process alive (when this method returns, JUnit tears
 *     everything down).
 *   • `@After tearDown()` — stop Ktor cleanly.
 *
 * Started with:
 *   adb shell am instrument -w -e debug false \
 *     -e class io.github.andriyo.shadowdroid.ShadowDroidServerTest \
 *     io.github.andriyo.shadowdroid.test/androidx.test.runner.AndroidJUnitRunner
 */
@RunWith(AndroidJUnit4::class)
class ShadowDroidServerTest {
    private lateinit var server: HttpServer
    private lateinit var uiDevice: UiDevice

    @Before
    fun setUp() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        // Standard call — works because AndroidJUnitRunner already initialised
        // UiAutomation. No flag dance, no Configurator setup. Matches openatx
        // exactly.
        uiDevice = UiDevice.getInstance(instrumentation)
        uiDevice.wakeUp()
        server = HttpServer(instrumentation, uiDevice, port = DEFAULT_PORT).also { it.start() }
        Log.i(TAG, "ShadowDroid server listening on 127.0.0.1:$DEFAULT_PORT")
    }

    /**
     * Loops forever so the process stays alive. Polls UiAutomation every 500ms.
     * A null root is normal during window transitions, so only a sustained run
     * of unavailable roots/errors ends the process and asks the next
     * `am instrument` invocation to reconnect.
     */
    @Test
    @LargeTest
    fun runServerForever() {
        Log.i(TAG, "ShadowDroid server entering main loop")
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val health = UiAutomationHealthTracker(MAX_CONSECUTIVE_HEALTH_FAILURES)
        while (true) {
            try {
                val nodeInfo = instrumentation.uiAutomation.rootInActiveWindow
                if (nodeInfo == null) {
                    if (health.recordUnavailable()) {
                        Log.w(TAG, "UiAutomation root unavailable for the full grace period — exiting for restart")
                        return
                    }
                    if (health.consecutiveFailures == 1) {
                        Log.w(TAG, "UiAutomation root temporarily unavailable — keeping server alive")
                    }
                } else {
                    if (health.consecutiveFailures > 0) {
                        Log.i(TAG, "UiAutomation root recovered after ${health.consecutiveFailures} failed checks")
                    }
                    health.recordHealthy()
                }
            } catch (t: Throwable) {
                if (health.recordUnavailable()) {
                    Log.e(TAG, "UiAutomation health-check failed for the full grace period — exiting", t)
                    return
                }
                if (health.consecutiveFailures == 1) {
                    Log.w(TAG, "UiAutomation health-check failed transiently — keeping server alive", t)
                }
            }
            Thread.sleep(500)
        }
    }

    @After
    fun tearDown() {
        Log.i(TAG, "ShadowDroid server stopping")
        try {
            server.stop()
        } catch (_: Throwable) {
        }
    }

    companion object {
        const val DEFAULT_PORT = 7912
        private const val MAX_CONSECUTIVE_HEALTH_FAILURES = 20 // 10 seconds
        private const val TAG = "ShadowDroid"
    }
}

internal class UiAutomationHealthTracker(
    private val failureLimit: Int,
) {
    var consecutiveFailures: Int = 0
        private set

    init {
        require(failureLimit > 0)
    }

    /** Returns true when the unavailable grace period has been exhausted. */
    fun recordUnavailable(): Boolean {
        consecutiveFailures++
        return consecutiveFailures >= failureLimit
    }

    fun recordHealthy() {
        consecutiveFailures = 0
    }
}
