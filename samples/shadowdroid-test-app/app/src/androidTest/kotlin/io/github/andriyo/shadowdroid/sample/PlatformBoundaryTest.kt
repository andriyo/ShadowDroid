package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.appwidget.AppWidgetHost
import android.appwidget.AppWidgetHostView
import android.appwidget.AppWidgetManager
import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import android.media.session.MediaController
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.widget.Button
import android.widget.TextView
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.UiDevice
import org.junit.After
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/** Independent assertions over Android-delivered observations, with randomized payloads. */
@RunWith(AndroidJUnit4::class)
class PlatformBoundaryTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext
    private val broken = InstrumentationRegistry.getArguments().getString("fixtureVariant") == "broken"
    private var activity: PlatformVerificationActivity? = null
    private val others = mutableListOf<Activity>()

    private fun <T> onMain(block: () -> T): T {
        val result = AtomicReference<T>()
        instrumentation.runOnMainSync { result.set(block()) }
        return result.get()
    }

    @Before fun launch() {
        assumeTrue("fixture requires API 29+", Build.VERSION.SDK_INT >= 29)
        activity = instrumentation.startActivitySync(Intent(context, PlatformVerificationActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK).putExtra("broken", broken)) as PlatformVerificationActivity
        instrumentation.waitForIdleSync()
    }

    @After fun cleanup() {
        onMain { others.forEach { it.finish() }; activity?.finish() }
        instrumentation.waitForIdleSync()
        instrumentation.uiAutomation.dropShellPermissionIdentity()
    }

    private fun eventually(description: String, condition: () -> Boolean) {
        val deadline = SystemClock.uptimeMillis() + 5000
        while (SystemClock.uptimeMillis() < deadline) {
            if (onMain(condition)) return
            SystemClock.sleep(50)
        }
        fail(description)
    }

    @Test fun outboundIntentExtras() {
        val payload = "intent-" + UUID.randomUUID()
        val monitor = instrumentation.addMonitor(VerificationIntentReceiver::class.java.name, null, false)
        try {
            onMain {
                activity!!.value.setText(payload)
                activity!!.findViewById<Button>(R.id.platform_send).performClick()
            }
            val receiver = monitor.waitForActivityWithTimeout(5000)
            assertNotNull("real intent receiver was never launched", receiver)
            others.add(receiver)
            assertEquals(payload, receiver.intent.getStringExtra("payload"))
        } finally { instrumentation.removeMonitor(monitor) }
    }

    @Test fun mediaSessionReleasedOnStop() {
        val destroyed = CountDownLatch(1)
        val controller = onMain {
            activity!!.findViewById<Button>(R.id.platform_media).performClick()
            val session = activity!!.media!!
            assertTrue("fixture did not create an active media session", session.isActive)
            MediaController(context, session.sessionToken)
        }
        val callback = object : MediaController.Callback() {
            override fun onSessionDestroyed() { destroyed.countDown() }
        }
        controller.registerCallback(callback, Handler(Looper.getMainLooper()))
        try {
            UiDevice.getInstance(instrumentation).pressHome()
            assertTrue("media session was not actually released after backgrounding", destroyed.await(5, TimeUnit.SECONDS))
        } finally { controller.unregisterCallback(callback) }
    }

    @Test fun pictureInPictureEntryObserved() {
        assumeTrue("device does not support PiP", context.packageManager.hasSystemFeature(PackageManager.FEATURE_PICTURE_IN_PICTURE))
        onMain { activity!!.findViewById<Button>(R.id.platform_pip).performClick() }
        eventually("activity never entered system Picture-in-Picture") { activity!!.isInPictureInPictureMode }
    }

    @Test fun widgetUpdateReachesHost() {
        assumeTrue("device does not support app widgets", context.packageManager.hasSystemFeature(PackageManager.FEATURE_APP_WIDGETS))
        instrumentation.uiAutomation.adoptShellPermissionIdentity("android.permission.BIND_APPWIDGET")
        val manager = AppWidgetManager.getInstance(context)
        val host = onMain { AppWidgetHost(context, 0x5344) }
        val id = host.allocateAppWidgetId()
        val preferences = context.getSharedPreferences("verification-platform", 0)
        preferences.edit().clear().putBoolean("broken", broken).putString("value", "initial").commit()
        try {
            assertTrue("binding permission/capability unavailable", manager.bindAppWidgetIdIfAllowed(id, ComponentName(context, VerificationWidgetProvider::class.java)))
            val view: AppWidgetHostView = onMain {
                host.startListening()
                host.createView(activity, id, manager.getAppWidgetInfo(id)).also {
                    activity!!.root.addView(it, android.widget.LinearLayout.LayoutParams(-1, 180))
                }
            }
            fun update() = context.sendBroadcast(Intent(context, VerificationWidgetProvider::class.java)
                .setAction(AppWidgetManager.ACTION_APPWIDGET_UPDATE)
                .putExtra(AppWidgetManager.EXTRA_APPWIDGET_IDS, intArrayOf(id)))
            update()
            eventually("initial widget never rendered in an actual host") {
                view.findViewById<TextView>(R.id.verification_widget_text)?.text?.toString() == "initial"
            }
            val payload = "widget-" + UUID.randomUUID()
            preferences.edit().putString("value", payload).commit()
            update()
            eventually("post-render widget update did not reach the host") {
                view.findViewById<TextView>(R.id.verification_widget_text)?.text?.toString() == payload
            }
        } finally {
            onMain { host.stopListening(); host.deleteAppWidgetId(id); host.deleteHost() }
            preferences.edit().clear().commit()
        }
    }
}
