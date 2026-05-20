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
     * Loops forever so the process stays alive. Polls UiAutomation every 500ms
     * — if it disappears (e.g. system_server killed it), we exit gracefully
     * and the next `am instrument` invocation will reconnect.
     */
    @Test
    @LargeTest
    fun runServerForever() {
        Log.i(TAG, "ShadowDroid server entering main loop")
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        while (true) {
            try {
                val nodeInfo = instrumentation.uiAutomation.rootInActiveWindow
                if (nodeInfo == null) {
                    Log.w(TAG, "UiAutomation lost its root window — exiting so we can be restarted")
                    return
                }
            } catch (t: Throwable) {
                Log.e(TAG, "UiAutomation health-check threw — exiting", t)
                return
            }
            Thread.sleep(500)
        }
    }

    @After
    fun tearDown() {
        Log.i(TAG, "ShadowDroid server stopping")
        try { server.stop() } catch (_: Throwable) {}
    }

    companion object {
        const val DEFAULT_PORT = 7912
        private const val TAG = "ShadowDroid"
    }
}
