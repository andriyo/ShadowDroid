package io.github.andriyo.shadowdroid.sample

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.AlertDialog
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Typeface
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Gravity
import android.view.View
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.PopupWindow
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import kotlin.math.roundToInt

class MainActivity : Activity() {
    private val mainHandler = Handler(Looper.getMainLooper())
    private lateinit var statusText: TextView
    private lateinit var counterValue: TextView
    private lateinit var nameInput: EditText
    private lateinit var urlInput: EditText
    private lateinit var bodyInput: EditText
    private lateinit var progress: ProgressBar
    private lateinit var webViewContainer: FrameLayout
    private var counter = 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        createNotificationChannel()
        startService(Intent(this, RemoteEchoService::class.java))
        render()
        setStatus("Ready: ${intentSummary(intent)}")
        Log.i(TAG, "MainActivity created")
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        setStatus("New intent: ${intentSummary(intent)}")
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        val granted = grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED
        setStatus("Permission request $requestCode result: granted=$granted")
    }

    private fun render() {
        val scroll = ScrollView(this).apply {
            isFillViewport = false
            contentDescription = "ShadowDroid sample scroll container"
        }
        val root = LinearLayout(this).apply {
            id = R.id.sample_root
            orientation = LinearLayout.VERTICAL
            setPadding(18.dp, 18.dp, 18.dp, 28.dp)
        }
        scroll.addView(root)

        root.addView(title("ShadowDroid Test App"))
        statusText = TextView(this).apply {
            id = R.id.status_text
            textSize = 15f
            contentDescription = "Current sample status"
            setPadding(0, 8.dp, 0, 8.dp)
        }
        root.addView(statusText, fullWidth())

        addSection(root, "Inputs")
        nameInput = editText(R.id.name_input, "agent name", "Name input")
        root.addView(nameInput, fullWidth())
        urlInput = editText(R.id.url_input, DEFAULT_HTTPS_URL, "Network URL input")
        root.addView(urlInput, fullWidth())
        bodyInput = editText(R.id.body_input, DEFAULT_GRAPHQL_BODY, "Request body input").apply {
            minLines = 3
            gravity = Gravity.TOP
        }
        root.addView(bodyInput, fullWidth())

        addSection(root, "Selectors")
        counterValue = TextView(this).apply {
            id = R.id.counter_value
            text = "Counter: 0"
            contentDescription = "Counter value"
        }
        root.addView(counterValue, fullWidth())
        root.addView(button(R.id.counter_button, "Increment counter", "Increment counter button") {
            counter += 1
            counterValue.text = "Counter: $counter"
            setStatus("Counter incremented to $counter")
        })
        root.addView(button(R.id.duplicate_one_button, "Duplicate action", "Duplicate action first") {
            setStatus("First duplicate action tapped")
        })
        root.addView(button(R.id.duplicate_two_button, "Duplicate action", "Duplicate action second") {
            setStatus("Second duplicate action tapped")
        })
        root.addView(button(R.id.disabled_button, "Disabled target", "Disabled target button") {
            setStatus("This should not run")
        }.apply {
            isEnabled = false
        })

        addSection(root, "Popups And Permissions")
        root.addView(button(R.id.dialog_button, "Show dialog", "Show alert dialog button") { showDialog() })
        root.addView(button(R.id.popup_button, "Show popup", "Show popup window button") { showPopup(root) })
        root.addView(button(R.id.toast_button, "Show toast", "Show toast button") {
            Toast.makeText(this, "ShadowDroid sample toast", Toast.LENGTH_LONG).show()
            setStatus("Toast shown")
        })
        root.addView(button(R.id.camera_permission_button, "Request camera permission", "Camera permission button") {
            requestPermissions(arrayOf(Manifest.permission.CAMERA), REQ_CAMERA)
        })
        root.addView(button(R.id.notification_button, "Post notification", "Post notification button") {
            postNotification()
        })

        addSection(root, "Lifecycle And Device")
        root.addView(button(R.id.detail_button, "Open detail activity", "Open detail activity button") {
            startActivity(Intent(this, DetailActivity::class.java).putExtra("source", "main-button"))
        })
        root.addView(button(R.id.deep_link_button, "Open deep link", "Open deep link button") {
            startActivity(Intent(Intent.ACTION_VIEW, Uri.parse("shadowdroid://sample/deeplink/from-main?value=42")))
        })
        root.addView(button(R.id.clipboard_button, "Copy clipboard value", "Copy clipboard button") { copyClipboard() })
        root.addView(button(R.id.file_button, "Write sample files", "Write sample files button") { writeSampleFiles() })
        root.addView(button(R.id.coroutines_button, "Open coroutine workload", "Open coroutine workload button") {
            startActivity(Intent(this, CoroutinesActivity::class.java))
        })

        addSection(root, "Logs And Failure Modes")
        root.addView(button(R.id.log_button, "Emit log messages", "Emit log messages button") { emitLogs() })
        root.addView(button(R.id.crash_button, "Crash now", "Crash now button") {
            throw RuntimeException("Deliberate ShadowDroid sample crash")
        })
        root.addView(button(R.id.anr_button, "Block main thread 12s", "Block main thread button") {
            setStatus("Blocking main thread for 12 seconds")
            Thread.sleep(12_000)
            setStatus("Main thread block finished")
        })

        addSection(root, "Network")
        progress = ProgressBar(this).apply {
            id = R.id.network_progress
            visibility = View.GONE
            isIndeterminate = true
            contentDescription = "Network request progress"
        }
        root.addView(progress, fullWidth())
        root.addView(button(R.id.https_get_button, "HTTPS GET", "HTTPS GET button") {
            runRequest("https-get", "GET", primaryUrl())
        })
        root.addView(button(R.id.http_get_button, "HTTP GET", "HTTP GET button") {
            runRequest("http-get", "GET", DEFAULT_HTTP_URL)
        })
        root.addView(button(R.id.json_post_button, "JSON POST", "JSON POST button") {
            runRequest("json-post", "POST", primaryUrl(), bodyInput.text.toString())
        })
        root.addView(button(R.id.graphql_post_button, "GraphQL POST", "GraphQL POST button") {
            runRequest(
                "graphql-post",
                "POST",
                urlWithPath("/anything/graphql"),
                bodyInput.text.toString(),
                mapOf("X-GraphQL-Operation" to "ShadowDroidSampleQuery"),
            )
        })
        root.addView(button(R.id.status_418_button, "HTTP 418 status", "HTTP 418 status button") {
            runRequest("status-418", "GET", urlWithPath("/status/418"))
        })
        root.addView(button(R.id.slow_request_button, "Slow request", "Slow request button") {
            runRequest("slow-request", "GET", urlWithPath("/delay/2"))
        })
        root.addView(button(R.id.large_body_button, "Large response", "Large response button") {
            runRequest("large-response", "GET", urlWithPath("/bytes/4096"))
        })

        webViewContainer = FrameLayout(this).apply {
            id = R.id.webview_container
            contentDescription = "WebView container"
        }
        root.addView(button(R.id.webview_button, "Load WebView", "Load WebView button") { loadWebView() })
        root.addView(webViewContainer, fullWidth().apply { height = 320.dp })

        setContentView(scroll)
    }

    private fun addSection(root: LinearLayout, label: String) {
        root.addView(TextView(this).apply {
            text = label
            textSize = 18f
            typeface = Typeface.DEFAULT_BOLD
            setPadding(0, 18.dp, 0, 4.dp)
        }, fullWidth())
    }

    private fun title(text: String): TextView =
        TextView(this).apply {
            this.text = text
            textSize = 24f
            typeface = Typeface.DEFAULT_BOLD
        }

    private fun editText(idValue: Int, value: String, description: String): EditText =
        EditText(this).apply {
            id = idValue
            setText(value)
            hint = description
            contentDescription = description
            setSingleLine(false)
            setPadding(12.dp, 8.dp, 12.dp, 8.dp)
        }

    private fun button(idValue: Int, text: String, description: String, onClick: () -> Unit): Button =
        Button(this).apply {
            id = idValue
            this.text = text
            contentDescription = description
            isAllCaps = false
            setOnClickListener { onClick() }
            layoutParams = fullWidth()
        }

    private fun fullWidth(): LinearLayout.LayoutParams =
        LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT,
            LinearLayout.LayoutParams.WRAP_CONTENT,
        ).apply {
            topMargin = 6.dp
        }

    private fun showDialog() {
        AlertDialog.Builder(this)
            .setTitle("ShadowDroid dialog")
            .setMessage("Dialog body for watcher and selector tests.")
            .setPositiveButton("Accept") { _, _ -> setStatus("Dialog accepted") }
            .setNegativeButton("Cancel") { _, _ -> setStatus("Dialog cancelled") }
            .setNeutralButton("Later") { _, _ -> setStatus("Dialog deferred") }
            .show()
    }

    private fun showPopup(parent: View) {
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(18.dp, 18.dp, 18.dp, 18.dp)
            setBackgroundColor(0xFFFFFFFF.toInt())
            addView(TextView(context).apply {
                text = "Popup window"
                textSize = 18f
                typeface = Typeface.DEFAULT_BOLD
            })
        }
        val popup = PopupWindow(content, 280.dp, LinearLayout.LayoutParams.WRAP_CONTENT, true)
        content.addView(Button(this).apply {
            text = "Dismiss popup"
            isAllCaps = false
            setOnClickListener {
                popup.dismiss()
                setStatus("Popup dismissed")
            }
        })
        popup.showAtLocation(parent, Gravity.CENTER, 0, 0)
        setStatus("Popup shown")
    }

    private fun postNotification() {
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQ_NOTIFICATIONS)
            return
        }
        val intent = Intent(this, MainActivity::class.java).putExtra("source", "notification")
        val pendingIntent = PendingIntent.getActivity(
            this,
            100,
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = if (Build.VERSION.SDK_INT >= 26) {
            Notification.Builder(this, CHANNEL_ID)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
        }
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle("ShadowDroid sample")
            .setContentText("Notification posted from the sample app")
            .setContentIntent(pendingIntent)
            .setAutoCancel(true)
            .build()
        (getSystemService(NOTIFICATION_SERVICE) as NotificationManager).notify(1001, notification)
        setStatus("Notification posted")
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT < 26) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notification_channel_name),
            NotificationManager.IMPORTANCE_DEFAULT,
        )
        (getSystemService(NOTIFICATION_SERVICE) as NotificationManager).createNotificationChannel(channel)
    }

    private fun copyClipboard() {
        val value = "ShadowDroid sample clip ${System.currentTimeMillis()}"
        val clipboard = getSystemService(CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("shadowdroid-sample", value))
        setStatus("Copied clipboard value: $value")
    }

    private fun writeSampleFiles() {
        val dir = File(filesDir, "shadowdroid-sample").apply { mkdirs() }
        val file = File(dir, "state.json")
        val cacheFile = File(cacheDir, "shadowdroid-sample-cache.txt")
        file.writeText(
            """
            {"counter":$counter,"name":"${nameInput.text}","timestamp":${System.currentTimeMillis()}}
            """.trimIndent(),
        )
        cacheFile.writeText("cache sample ${System.currentTimeMillis()}\n")
        setStatus("Wrote ${file.absolutePath} and ${cacheFile.absolutePath}")
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
        body: String? = null,
        headers: Map<String, String> = emptyMap(),
    ) {
        progress.visibility = View.VISIBLE
        setStatus("$label running: $url")
        Thread {
            val result = try {
                performRequest(label, method, url, body, headers)
            } catch (t: Throwable) {
                "$label failed: ${t.javaClass.simpleName}: ${t.message}"
            }
            mainHandler.post {
                progress.visibility = View.GONE
                setStatus(result)
            }
        }.start()
    }

    private fun performRequest(
        label: String,
        method: String,
        url: String,
        body: String?,
        headers: Map<String, String>,
    ): String {
        val started = System.currentTimeMillis()
        val connection = (URL(url).openConnection() as HttpURLConnection).apply {
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

    @SuppressLint("SetJavaScriptEnabled")
    private fun loadWebView() {
        val url = primaryUrl()
        val webView = WebView(this).apply {
            id = R.id.web_view
            contentDescription = "Sample WebView"
            settings.javaScriptEnabled = true
            webViewClient = object : WebViewClient() {
                override fun onPageFinished(view: WebView?, url: String?) {
                    setStatus("WebView loaded: $url")
                }

                override fun onReceivedError(
                    view: WebView?,
                    request: WebResourceRequest?,
                    error: WebResourceError?,
                ) {
                    if (request?.isForMainFrame != false) {
                        setStatus("WebView error: ${error?.description}")
                    }
                }
            }
        }
        webViewContainer.removeAllViews()
        webViewContainer.addView(
            webView,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT,
            ),
        )
        webView.loadUrl(url)
        setStatus("WebView loading: $url")
    }

    private fun primaryUrl(): String =
        urlInput.text.toString().trim().ifEmpty { DEFAULT_HTTPS_URL }

    private fun urlWithPath(path: String): String =
        try {
            val base = URL(primaryUrl())
            URL(base.protocol, base.host, base.port, path).toString()
        } catch (_: Throwable) {
            "https://httpbin.org$path"
        }

    private fun setStatus(message: String) {
        statusText.text = message
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
        private const val DEFAULT_HTTPS_URL = "https://httpbin.org/anything/shadowdroid"
        private const val DEFAULT_HTTP_URL = "http://httpbin.org/anything/shadowdroid-cleartext"
        private const val DEFAULT_GRAPHQL_BODY =
            """{"operationName":"ShadowDroidSampleQuery","query":"query ShadowDroidSampleQuery { sample: __typename }","variables":{"source":"shadowdroid-test-app"}}"""
    }
}
