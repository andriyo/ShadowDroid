package io.github.andriyo.shadowdroid.sample

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.PopupWindow
import android.widget.TextView
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import kotlin.math.roundToInt

class MainActivity : ComponentActivity() {
    private val mainHandler = Handler(Looper.getMainLooper())
    private val events = mutableStateListOf<String>()
    private var statusMessage by mutableStateOf("Booting the Field Lab…")
    private var networkBusy by mutableStateOf(false)

    // Intentionally simple and breakpoint-friendly. This exact mutation is a
    // long-lived debugger fixture used for conditional breakpoint validation.
    private var counter by mutableIntStateOf(0)

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        createNotificationChannel()
        startService(Intent(this, RemoteEchoService::class.java))
        setStatus("Field Lab ready · ${intentSummary(intent)}")

        setContent {
            ShadowLabTheme {
                ShadowLabApp(
                    status = statusMessage,
                    events = events,
                    networkBusy = networkBusy,
                    counter = counter,
                    onIncrementCounter = ::incrementCounter,
                    actions =
                        LabActions(
                            setStatus = ::setStatus,
                            startUnstableUpdates = ::startUnstableUpdates,
                            showPopup = ::showPopup,
                            showToast = ::showToast,
                            requestCameraPermission = ::requestCameraPermission,
                            postNotification = ::postNotification,
                            openDetail = ::openDetail,
                            openDelayedDetail = ::openDelayedDetail,
                            openDeepLink = ::openDeepLink,
                            copyClipboard = ::copyClipboard,
                            writeSampleFiles = ::writeSampleFiles,
                            openCoroutines = ::openCoroutines,
                            emitLogs = ::emitLogs,
                            crashNow = ::crashNow,
                            blockMainThread = ::blockMainThread,
                            openWebSocketChat = ::openWebSocketChat,
                            runRequest = ::runRequest,
                        ),
                )
            }
        }

        Log.i(TAG, "MainActivity created")
        runShadowDroidProbe(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        setStatus("New intent · ${intentSummary(intent)}")
        runShadowDroidProbe(intent)
    }

    @Suppress("DEPRECATION", "OVERRIDE_DEPRECATION")
    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        val granted = grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED
        setStatus("Permission request $requestCode result · granted=$granted")
    }

    private fun incrementCounter() {
        counter += 1
        setStatus("Counter incremented to $counter")
    }

    private fun startUnstableUpdates() {
        var update = 0
        val updater =
            object : Runnable {
                override fun run() {
                    update += 1
                    setStatus("Unstable accessibility update $update")
                    if (update < UNSTABLE_UPDATE_COUNT) {
                        mainHandler.postDelayed(this, UNSTABLE_UPDATE_INTERVAL_MS)
                    }
                }
            }
        mainHandler.post(updater)
    }

    private fun showToast() {
        Toast.makeText(this, "ShadowDroid Field Lab signal received", Toast.LENGTH_LONG).show()
        setStatus("Toast shown")
    }

    private fun requestCameraPermission() {
        requestPermissions(arrayOf(Manifest.permission.CAMERA), REQ_CAMERA)
    }

    private fun showPopup(parent: View) {
        val content =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(22.dp, 20.dp, 22.dp, 18.dp)
                background =
                    GradientDrawable().apply {
                        setColor(0xFF1A2032.toInt())
                        cornerRadius = 22.dp.toFloat()
                        setStroke(1.dp, 0xFF55D6E8.toInt())
                    }
                addView(
                    TextView(context).apply {
                        text = "Relay overlay"
                        textSize = 20f
                        typeface = Typeface.DEFAULT_BOLD
                        setTextColor(0xFFF3F4FF.toInt())
                    },
                )
                addView(
                    TextView(context).apply {
                        text = "A native PopupWindow above a Compose destination."
                        textSize = 14f
                        setTextColor(0xFFA9AFC3.toInt())
                        setPadding(0, 8.dp, 0, 12.dp)
                    },
                )
            }
        val popup =
            PopupWindow(
                content,
                320.dp,
                LinearLayout.LayoutParams.WRAP_CONTENT,
                true,
            ).apply {
                elevation = 18.dp.toFloat()
                isOutsideTouchable = true
            }
        content.addView(
            Button(this).apply {
                text = "Dismiss overlay"
                isAllCaps = false
                contentDescription = "Dismiss popup"
                setOnClickListener {
                    popup.dismiss()
                    setStatus("Popup dismissed")
                }
            },
        )
        popup.setOnDismissListener {
            if (statusMessage == "Popup shown") setStatus("Popup dismissed")
        }
        popup.showAtLocation(parent, Gravity.CENTER, 0, 0)
        setStatus("Popup shown")
    }

    private fun openDetail() {
        startActivity(
            Intent(this, DetailActivity::class.java)
                .putExtra("source", "main-button"),
        )
    }

    private fun openDelayedDetail() {
        setStatus("Delayed detail navigation scheduled")
        mainHandler.postDelayed(
            {
                startActivity(
                    Intent(this, DetailActivity::class.java)
                        .putExtra("source", "delayed-button"),
                )
            },
            DELAYED_NAVIGATION_MS,
        )
    }

    private fun openDeepLink() {
        startActivity(
            Intent(
                Intent.ACTION_VIEW,
                Uri.parse("shadowdroid://sample/deeplink/from-main?value=42"),
            ),
        )
    }

    private fun openCoroutines() {
        startActivity(Intent(this, CoroutinesActivity::class.java))
    }

    private fun openWebSocketChat() {
        startActivity(Intent(this, WebSocketChatActivity::class.java))
    }

    private fun crashNow() {
        throw RuntimeException("Deliberate ShadowDroid sample crash")
    }

    private fun blockMainThread() {
        setStatus("Blocking main thread for 12 seconds")
        Thread.sleep(12_000)
        setStatus("Main thread block finished")
    }

    private fun postNotification() {
        if (
            Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(
                arrayOf(Manifest.permission.POST_NOTIFICATIONS),
                REQ_NOTIFICATIONS,
            )
            return
        }
        val intent = Intent(this, MainActivity::class.java).putExtra("source", "notification")
        val pendingIntent =
            PendingIntent.getActivity(
                this,
                100,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        val notification =
            (
                if (Build.VERSION.SDK_INT >= 26) {
                    Notification.Builder(this, CHANNEL_ID)
                } else {
                    @Suppress("DEPRECATION")
                    Notification.Builder(this)
                }
            )
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentTitle("Field Lab recovery")
                .setContentText("The ShadowDroid relay is ready for inspection")
                .setContentIntent(pendingIntent)
                .setAutoCancel(true)
                .build()
        (getSystemService(NOTIFICATION_SERVICE) as NotificationManager).notify(1001, notification)
        setStatus("Notification posted")
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT < 26) return
        val channel =
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.notification_channel_name),
                NotificationManager.IMPORTANCE_DEFAULT,
            )
        (getSystemService(NOTIFICATION_SERVICE) as NotificationManager)
            .createNotificationChannel(channel)
    }

    private fun copyClipboard() {
        val value = "ShadowDroid sample clip ${System.currentTimeMillis()}"
        val clipboard = getSystemService(CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("shadowdroid-sample", value))
        setStatus("Copied clipboard value: $value")
    }

    private fun writeSampleFiles(
        operatorName: String,
        currentCounter: Int,
    ) {
        val dir = File(filesDir, "shadowdroid-sample").apply { mkdirs() }
        val file = File(dir, "state.json")
        val cacheFile = File(cacheDir, "shadowdroid-sample-cache.txt")
        val timestamp = System.currentTimeMillis()
        file.writeText(
            """
            {"counter":$currentCounter,"name":"$operatorName","timestamp":$timestamp}
            """.trimIndent(),
        )
        cacheFile.writeText("cache sample $timestamp\n")
        getSharedPreferences("shadowdroid-state", MODE_PRIVATE)
            .edit()
            .putString("session", "sample-session-$timestamp")
            .putInt("counter", currentCounter)
            .commit()
        val database = openOrCreateDatabase("shadowdroid-state.db", MODE_PRIVATE, null)
        database.enableWriteAheadLogging()
        database.execSQL(
            "CREATE TABLE IF NOT EXISTS state (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        database.insertWithOnConflict(
            "state",
            null,
            ContentValues().apply {
                put("id", 1)
                put("value", "sample-db-$timestamp")
            },
            android.database.sqlite.SQLiteDatabase.CONFLICT_REPLACE,
        )
        database.close()
        setStatus("Wrote private files, SharedPreferences, and SQLite state")
    }

    private fun emitLogs() {
        Log.v(TAG, "verbose sample log")
        Log.d(TAG, "debug sample log")
        Log.i(TAG, "info sample log")
        Log.w(TAG, "warn sample log")
        Log.e(TAG, "error sample log")
        setStatus("Log messages emitted")
    }

    private fun runRequest(
        label: String,
        method: String,
        url: String,
        body: String?,
        headers: Map<String, String>,
    ) {
        networkBusy = true
        setStatus("$label running: $url")
        Thread {
            val result =
                try {
                    performRequest(label, method, url, body, headers)
                } catch (t: Throwable) {
                    "$label failed: ${t.javaClass.simpleName}: ${t.message}"
                }
            mainHandler.post {
                networkBusy = false
                setStatus(result)
            }
        }.start()
    }

    private fun runShadowDroidProbe(intent: Intent) {
        val uri = intent.data ?: return
        if (
            intent.action == Intent.ACTION_VIEW &&
            uri.scheme == "https" &&
            uri.host == "example.com" &&
            uri.path?.startsWith("/.well-known/shadowdroid-canary/") == true
        ) {
            runRequest(
                "ShadowDroid canary",
                "GET",
                uri.toString(),
                null,
                emptyMap(),
            )
        }
    }

    private fun performRequest(
        label: String,
        method: String,
        url: String,
        body: String?,
        headers: Map<String, String>,
    ): String {
        val started = System.currentTimeMillis()
        val connection =
            (URL(url).openConnection() as HttpURLConnection).apply {
                requestMethod = method
                connectTimeout = 8_000
                readTimeout = 8_000
                setRequestProperty("User-Agent", "ShadowDroidTestApp/0.1")
                setRequestProperty("X-ShadowDroid-Sample", label)
                headers.forEach { (name, value) -> setRequestProperty(name, value) }
            }
        if (body != null) {
            connection.doOutput = true
            connection.setRequestProperty("Content-Type", "application/json")
            connection.outputStream.use { out ->
                out.write(body.toByteArray(Charsets.UTF_8))
            }
        }
        val code = connection.responseCode
        val stream = if (code >= 400) connection.errorStream else connection.inputStream
        val preview = stream?.bufferedReader()?.use { it.readText().take(240) }.orEmpty()
        val elapsed = System.currentTimeMillis() - started
        connection.disconnect()
        return "$label completed status=$code bytes=${preview.length} elapsed=${elapsed}ms preview=${preview.squash()}"
    }

    private fun setStatus(message: String) {
        statusMessage = message
        if (events.firstOrNull() != message) {
            events.add(0, message)
            while (events.size > MAX_EVENTS) events.removeAt(events.lastIndex)
        }
        Log.i(TAG, message)
    }

    private fun intentSummary(intent: Intent?): String {
        if (intent == null) return "none"
        val data = intent.data?.toString() ?: "none"
        val source = intent.getStringExtra("source") ?: "none"
        return "action=${intent.action ?: "none"} data=$data source=$source"
    }

    private val Int.dp: Int
        get() = (this * resources.displayMetrics.density).roundToInt()

    private fun String.squash(): String = replace(Regex("\\s+"), " ").take(180)

    companion object {
        private const val TAG = "ShadowDroidSample"
        private const val CHANNEL_ID = "shadowdroid-sample-events"
        private const val REQ_CAMERA = 2001
        private const val REQ_NOTIFICATIONS = 2002
        private const val DELAYED_NAVIGATION_MS = 350L
        private const val UNSTABLE_UPDATE_COUNT = 40
        private const val UNSTABLE_UPDATE_INTERVAL_MS = 50L
        private const val MAX_EVENTS = 20
    }
}
