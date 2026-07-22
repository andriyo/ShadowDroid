package io.github.andriyo.shadowdroid.sample

import android.graphics.Typeface
import android.os.Bundle
import android.text.method.ScrollingMovementMethod
import android.util.Log
import android.view.Gravity
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import androidx.activity.ComponentActivity
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.encodeUtf8
import java.util.concurrent.TimeUnit
import kotlin.math.roundToInt

class WebSocketChatActivity : ComponentActivity() {
    private val client = OkHttpClient.Builder()
        .pingInterval(5, TimeUnit.SECONDS)
        .build()

    private lateinit var urlInput: EditText
    private lateinit var messageInput: EditText
    private lateinit var statusText: TextView
    private lateinit var transcriptText: TextView

    @Volatile
    private var activeSocket: WebSocket? = null
    private var connectionGeneration = 0
    private var connected = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        render()
    }

    override fun onDestroy() {
        connectionGeneration += 1
        activeSocket?.cancel()
        activeSocket = null
        client.dispatcher.executorService.shutdown()
        super.onDestroy()
    }

    private fun render() {
        val root = LinearLayout(this).apply {
            id = R.id.websocket_chat_root
            orientation = LinearLayout.VERTICAL
            setPadding(18.dp, 18.dp, 18.dp, 28.dp)
        }

        root.addView(TextView(this).apply {
            text = "ShadowDroid WebSocket Chat"
            textSize = 24f
            typeface = Typeface.DEFAULT_BOLD
        }, fullWidth())
        root.addView(TextView(this).apply {
            text = "Connect through the ShadowDroid proxy, then send a message to the local Ktor room."
        }, fullWidth())

        urlInput = EditText(this).apply {
            id = R.id.websocket_url_input
            setText(DEFAULT_WSS_URL)
            hint = "WebSocket URL"
            contentDescription = "WebSocket URL input"
            setSingleLine(true)
        }
        root.addView(urlInput, fullWidth())

        root.addView(horizontalRow().apply {
            addView(button(R.id.websocket_use_ws_button, "Use WS", "Use cleartext WebSocket URL") {
                urlInput.setText(DEFAULT_WS_URL)
                setStatus("Selected local WS endpoint")
            })
            addView(button(R.id.websocket_use_wss_button, "Use WSS", "Use TLS WebSocket URL") {
                urlInput.setText(DEFAULT_WSS_URL)
                setStatus("Selected local WSS endpoint")
            })
        }, fullWidth())

        statusText = TextView(this).apply {
            id = R.id.websocket_status
            text = "Disconnected"
            textSize = 16f
            typeface = Typeface.DEFAULT_BOLD
            contentDescription = "WebSocket connection status"
            setPadding(0, 10.dp, 0, 10.dp)
        }
        root.addView(statusText, fullWidth())

        root.addView(button(R.id.websocket_connect_button, "Connect", "Connect WebSocket button") {
            connect()
        }, fullWidth())

        messageInput = EditText(this).apply {
            id = R.id.websocket_message_input
            setText(DEFAULT_MESSAGE)
            hint = "Chat message"
            contentDescription = "WebSocket message input"
            setSingleLine(true)
        }
        root.addView(messageInput, fullWidth())

        root.addView(button(R.id.websocket_send_button, "Send message", "Send WebSocket message button") {
            sendMessage()
        }, fullWidth())
        root.addView(button(R.id.websocket_send_binary_button, "Send binary", "Send WebSocket binary message button") {
            sendBinary()
        }, fullWidth())
        root.addView(button(R.id.websocket_send_large_button, "Send large text", "Send large WebSocket text button") {
            sendLargeText()
        }, fullWidth())
        root.addView(button(R.id.websocket_disconnect_button, "Disconnect", "Disconnect WebSocket button") {
            disconnect()
        }, fullWidth())
        root.addView(button(R.id.websocket_clear_button, "Clear transcript", "Clear WebSocket transcript button") {
            transcriptText.text = ""
        }, fullWidth())

        transcriptText = TextView(this).apply {
            id = R.id.websocket_transcript
            text = ""
            textSize = 15f
            contentDescription = "WebSocket chat transcript"
            setPadding(12.dp, 12.dp, 12.dp, 12.dp)
            setBackgroundColor(0xFFECEFF1.toInt())
            gravity = Gravity.TOP
            movementMethod = ScrollingMovementMethod()
            minLines = 8
        }
        root.addView(transcriptText, fullWidth())

        setContentView(ScrollView(this).apply { addView(root) })
    }

    private fun connect() {
        val url = urlInput.text.toString().trim()
        if (!url.startsWith("ws://") && !url.startsWith("wss://")) {
            setStatus("Invalid WebSocket URL")
            return
        }

        connectionGeneration += 1
        val generation = connectionGeneration
        connected = false
        activeSocket?.cancel()
        setStatus("Connecting: $url")
        appendTranscript("system → connecting to $url")

        val request = Request.Builder()
            .url(url)
            .header("X-ShadowDroid-Sample", "websocket-chat")
            .build()
        activeSocket = client.newWebSocket(request, listener(generation))
    }

    private fun listener(generation: Int): WebSocketListener =
        object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                postIfCurrent(generation) {
                    connected = true
                    setStatus("Connected (${response.protocol})")
                    appendTranscript("system → connected")
                }
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                postIfCurrent(generation) {
                    appendTranscript("server → ${text.transcriptPreview()}")
                }
            }

            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                postIfCurrent(generation) {
                    appendTranscript("server → binary ${bytes.size} bytes")
                }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(code, reason)
                postIfCurrent(generation) {
                    setStatus("Closing: code=$code reason=$reason")
                }
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                postIfCurrent(generation) {
                    connected = false
                    activeSocket = null
                    setStatus("Disconnected: code=$code reason=$reason")
                    appendTranscript("system → disconnected code=$code")
                }
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                postIfCurrent(generation) {
                    connected = false
                    activeSocket = null
                    val detail = response?.code?.let { "HTTP $it" } ?: t.javaClass.simpleName
                    setStatus("Connection failed: $detail: ${t.message.orEmpty()}")
                    appendTranscript("system → failure $detail: ${t.message.orEmpty()}")
                }
            }
        }

    private fun sendMessage() {
        val message = messageInput.text.toString()
        val socket = activeSocket
        if (!connected || socket == null) {
            setStatus("Connect before sending")
            return
        }
        if (message.isBlank()) {
            setStatus("Enter a message")
            return
        }
        if (socket.send(message)) {
            appendTranscript("client → $message")
            setStatus("Message sent (${message.toByteArray().size} bytes)")
        } else {
            setStatus("Message queue is closed")
        }
    }

    private fun sendBinary() {
        val socket = connectedSocket() ?: return
        val payload = "binary-through-shadowdroid".encodeUtf8()
        if (socket.send(payload)) {
            appendTranscript("client → binary ${payload.size} bytes")
            setStatus("Binary message sent (${payload.size} bytes)")
        } else {
            setStatus("Message queue is closed")
        }
    }

    private fun sendLargeText() {
        val socket = connectedSocket() ?: return
        val payload = buildString {
            while (length < 4_096) append("compressible-shadowdroid-chat-")
        }.take(4_096)
        if (socket.send(payload)) {
            appendTranscript("client → large text ${payload.toByteArray().size} bytes")
            setStatus("Large text sent (${payload.toByteArray().size} bytes)")
        } else {
            setStatus("Message queue is closed")
        }
    }

    private fun connectedSocket(): WebSocket? {
        val socket = activeSocket
        if (!connected || socket == null) {
            setStatus("Connect before sending")
            return null
        }
        return socket
    }

    private fun disconnect() {
        val socket = activeSocket
        if (socket == null) {
            setStatus("Already disconnected")
            return
        }
        setStatus("Disconnecting")
        socket.close(1_000, "sample complete")
    }

    private fun postIfCurrent(generation: Int, block: () -> Unit) {
        runOnUiThread {
            if (generation == connectionGeneration) block()
        }
    }

    private fun setStatus(message: String) {
        statusText.text = message
        Log.i(TAG, message)
    }

    private fun appendTranscript(message: String) {
        val current = transcriptText.text.toString()
        transcriptText.text = if (current.isEmpty()) message else "$current\n$message"
        Log.i(TAG, message)
    }

    private fun String.transcriptPreview(): String =
        if (length <= 240) this else "${take(120)}… (${toByteArray().size} bytes)"

    private fun button(idValue: Int, label: String, description: String, action: () -> Unit): Button =
        Button(this).apply {
            id = idValue
            text = label
            contentDescription = description
            isAllCaps = false
            setOnClickListener { action() }
        }

    private fun horizontalRow(): LinearLayout =
        LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_HORIZONTAL
        }

    private fun fullWidth(): LinearLayout.LayoutParams =
        LinearLayout.LayoutParams(
            LinearLayout.LayoutParams.MATCH_PARENT,
            LinearLayout.LayoutParams.WRAP_CONTENT,
        ).apply { topMargin = 6.dp }

    private val Int.dp: Int
        get() = (this * resources.displayMetrics.density).roundToInt()

    companion object {
        private const val TAG = "ShadowDroidWsChat"
        private const val DEFAULT_WS_URL = "ws://shadowdroid.localhost:18080/chat?name=android"
        private const val DEFAULT_WSS_URL = "wss://shadowdroid.localhost:18443/chat?name=android"
        private const val DEFAULT_MESSAGE = "hello-through-shadowdroid"
    }
}
