package io.github.andriyo.shadowdroid.sample

import android.graphics.Color
import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.encodeUtf8
import java.util.concurrent.TimeUnit

class WebSocketChatActivity : ComponentActivity() {
    private val client =
        fieldLabOkHttpClientBuilder()
            .pingInterval(5, TimeUnit.SECONDS)
            .build()

    private var url by mutableStateOf(DEFAULT_WSS_URL)
    private var message by mutableStateOf(DEFAULT_MESSAGE)
    private var channelStatus by mutableStateOf("Disconnected")
    private val transcript = mutableStateListOf<ChatEntry>()

    @Volatile
    private var activeSocket: WebSocket? = null
    private var connectionGeneration = 0
    private var connected = false

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
        )
        super.onCreate(savedInstanceState)
        transcript += ChatEntry(ChatDirection.System, "Field channel is standing by.")
        setContent {
            ShadowLabTheme {
                WebSocketChatScreen(
                    url = url,
                    onUrlChanged = { url = it },
                    message = message,
                    onMessageChanged = { message = it },
                    status = channelStatus,
                    connected = connected,
                    transcript = transcript,
                    onUseWs = {
                        url = DEFAULT_WS_URL
                        setStatus("Selected local WS endpoint")
                    },
                    onUseWss = {
                        url = DEFAULT_WSS_URL
                        setStatus("Selected local WSS endpoint")
                    },
                    onConnect = ::connect,
                    onSendMessage = ::sendMessage,
                    onSendBinary = ::sendBinary,
                    onSendLarge = ::sendLargeText,
                    onDisconnect = ::disconnect,
                    onClear = {
                        transcript.clear()
                        setStatus("Transcript cleared")
                    },
                )
            }
        }
    }

    override fun onDestroy() {
        connectionGeneration += 1
        activeSocket?.cancel()
        activeSocket = null
        client.dispatcher.executorService.shutdown()
        super.onDestroy()
    }

    private fun connect() {
        val targetUrl = url.trim()
        if (!targetUrl.startsWith("ws://") && !targetUrl.startsWith("wss://")) {
            setStatus("Invalid WebSocket URL")
            return
        }

        connectionGeneration += 1
        val generation = connectionGeneration
        connected = false
        activeSocket?.cancel()
        setStatus("Connecting: $targetUrl")
        appendTranscript(ChatDirection.System, "Connecting to $targetUrl")

        val request =
            Request.Builder()
                .url(targetUrl)
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
                    appendTranscript(ChatDirection.System, "Secure channel connected")
                }
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                postIfCurrent(generation) {
                    appendTranscript(ChatDirection.Server, text.transcriptPreview())
                }
            }

            override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                postIfCurrent(generation) {
                    appendTranscript(ChatDirection.Server, "Binary frame · ${bytes.size} bytes")
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
                    appendTranscript(ChatDirection.System, "Channel closed · code $code")
                }
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                postIfCurrent(generation) {
                    connected = false
                    activeSocket = null
                    val detail = response?.code?.let { "HTTP $it" } ?: t.javaClass.simpleName
                    setStatus("Connection failed: $detail: ${t.message.orEmpty()}")
                    appendTranscript(
                        ChatDirection.System,
                        "Connection failure · $detail · ${t.message.orEmpty()}",
                    )
                }
            }
        }

    private fun sendMessage() {
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
            appendTranscript(ChatDirection.Client, message)
            setStatus("Message sent (${message.toByteArray().size} bytes)")
        } else {
            setStatus("Message queue is closed")
        }
    }

    private fun sendBinary() {
        val socket = connectedSocket() ?: return
        val payload = "binary-through-shadowdroid".encodeUtf8()
        if (socket.send(payload)) {
            appendTranscript(ChatDirection.Client, "Binary frame · ${payload.size} bytes")
            setStatus("Binary message sent (${payload.size} bytes)")
        } else {
            setStatus("Message queue is closed")
        }
    }

    private fun sendLargeText() {
        val socket = connectedSocket() ?: return
        val payload =
            buildString {
                while (length < 4_096) append("compressible-shadowdroid-chat-")
            }.take(4_096)
        if (socket.send(payload)) {
            appendTranscript(ChatDirection.Client, "Compressed challenge · 4096 bytes")
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

    private fun postIfCurrent(
        generation: Int,
        block: () -> Unit,
    ) {
        runOnUiThread {
            if (generation == connectionGeneration) block()
        }
    }

    private fun setStatus(message: String) {
        channelStatus = message
        Log.i(TAG, message)
    }

    private fun appendTranscript(
        direction: ChatDirection,
        message: String,
    ) {
        transcript += ChatEntry(direction, message)
        Log.i(TAG, "${direction.name.lowercase()} → $message")
    }

    private fun String.transcriptPreview(): String =
        if (length <= 240) this else "${take(120)}… (${toByteArray().size} bytes)"

    companion object {
        private const val TAG = "ShadowDroidWsChat"
        private const val DEFAULT_WS_URL = "ws://shadowdroid.localhost:18080/chat?name=android"
        private const val DEFAULT_WSS_URL = "wss://shadowdroid.localhost:18443/chat?name=android"
        private const val DEFAULT_MESSAGE = "hello-through-shadowdroid"
    }
}

private enum class ChatDirection {
    System,
    Client,
    Server,
}

private data class ChatEntry(
    val direction: ChatDirection,
    val text: String,
)

@Composable
@OptIn(ExperimentalMaterial3Api::class)
private fun WebSocketChatScreen(
    url: String,
    onUrlChanged: (String) -> Unit,
    message: String,
    onMessageChanged: (String) -> Unit,
    status: String,
    connected: Boolean,
    transcript: List<ChatEntry>,
    onUseWs: () -> Unit,
    onUseWss: () -> Unit,
    onConnect: () -> Unit,
    onSendMessage: () -> Unit,
    onSendBinary: () -> Unit,
    onSendLarge: () -> Unit,
    onDisconnect: () -> Unit,
    onClear: () -> Unit,
) {
    var advanced by rememberSaveable { mutableStateOf(false) }
    val connecting = status.startsWith("Connecting")

    Scaffold(
        containerColor = LabInk,
        contentColor = LabText,
        topBar = {
            ChannelHeader(status = status, connected = connected)
        },
    ) { padding ->
        Column(
            modifier =
                Modifier
                    .fillMaxSize()
                    .padding(padding)
                    .imePadding()
                    .semantics { testTagsAsResourceId = true }
                    .testTag("websocket_chat_root"),
        ) {
            LazyColumn(
                modifier = Modifier.weight(1f),
                contentPadding = PaddingValues(16.dp, 16.dp, 16.dp, 12.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                item {
                    EndpointCard(
                        url = url,
                        onUrlChanged = onUrlChanged,
                        connected = connected,
                        connecting = connecting,
                        onUseWs = onUseWs,
                        onUseWss = onUseWss,
                        onConnect = onConnect,
                    )
                }
                item {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            "CHANNEL TRANSCRIPT",
                            style = MaterialTheme.typography.labelSmall,
                            color = LabTextMuted,
                            modifier =
                                Modifier
                                    .weight(1f)
                                    .testTag("websocket_transcript")
                                    .semantics {
                                        contentDescription =
                                            "WebSocket chat transcript: ${transcript.joinToString(" | ") { it.text }}"
                                    },
                        )
                        TextButton(
                            onClick = onClear,
                            modifier =
                                Modifier
                                    .testTag("websocket_clear_button")
                                    .semantics { contentDescription = "Clear WebSocket transcript button" },
                        ) {
                            Text("CLEAR")
                        }
                    }
                }
                if (transcript.isEmpty()) {
                    item {
                        Text(
                            "No messages yet.",
                            color = LabTextMuted,
                            modifier = Modifier.padding(vertical = 20.dp),
                        )
                    }
                } else {
                    items(transcript) { entry ->
                        ChatBubble(entry)
                    }
                }
            }

            Surface(
                color = LabDeep,
                contentColor = LabText,
                shadowElevation = 16.dp,
            ) {
                Column(Modifier.padding(14.dp)) {
                    OutlinedTextField(
                        value = message,
                        onValueChange = onMessageChanged,
                        label = { Text("Challenge response") },
                        singleLine = true,
                        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                        modifier =
                            Modifier
                                .fillMaxWidth()
                                .testTag("websocket_message_input")
                                .semantics { contentDescription = "WebSocket message input" },
                    )
                    Spacer(Modifier.size(9.dp))
                    Button(
                        onClick = onSendMessage,
                        modifier =
                            Modifier
                                .fillMaxWidth()
                                .testTag("websocket_send_button")
                                .semantics { contentDescription = "Send WebSocket message button" },
                    ) {
                        Text("Transmit message")
                    }
                    TextButton(
                        onClick = { advanced = !advanced },
                        modifier =
                            Modifier
                                .fillMaxWidth()
                                .testTag("websocket_advanced_toggle"),
                    ) {
                        Text(if (advanced) "HIDE ADVANCED FRAMES" else "ADVANCED FRAMES")
                    }
                    AnimatedVisibility(visible = advanced) {
                        Column {
                            HorizontalDivider(color = LabOutline)
                            Spacer(Modifier.size(9.dp))
                            Row(
                                modifier = Modifier.fillMaxWidth(),
                                horizontalArrangement = Arrangement.spacedBy(8.dp),
                            ) {
                                OutlinedButton(
                                    onClick = onSendBinary,
                                    modifier =
                                        Modifier
                                            .weight(1f)
                                            .testTag("websocket_send_binary_button")
                                            .semantics {
                                                contentDescription = "Send WebSocket binary message button"
                                            },
                                ) {
                                    Text("BINARY", style = MaterialTheme.typography.labelSmall)
                                }
                                OutlinedButton(
                                    onClick = onSendLarge,
                                    modifier =
                                        Modifier
                                            .weight(1f)
                                            .testTag("websocket_send_large_button")
                                            .semantics {
                                                contentDescription = "Send large WebSocket text button"
                                            },
                                ) {
                                    Text("4KB TEXT", style = MaterialTheme.typography.labelSmall)
                                }
                                OutlinedButton(
                                    onClick = onDisconnect,
                                    modifier =
                                        Modifier
                                            .weight(1f)
                                            .testTag("websocket_disconnect_button")
                                            .semantics {
                                                contentDescription = "Disconnect WebSocket button"
                                            },
                                ) {
                                    Text("CLOSE", style = MaterialTheme.typography.labelSmall)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun ChannelHeader(
    status: String,
    connected: Boolean,
) {
    Surface(
        color = LabDeep,
        contentColor = LabText,
        shadowElevation = 9.dp,
        modifier =
            Modifier
                .statusBarsPadding()
                .semantics { testTagsAsResourceId = true },
    ) {
        Column(
            modifier =
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 18.dp, vertical = 12.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    modifier =
                        Modifier
                            .size(42.dp)
                            .background(
                                Brush.linearGradient(listOf(LabViolet, LabCyan)),
                                RoundedCornerShape(14.dp),
                            ),
                    contentAlignment = Alignment.Center,
                ) {
                    Text("WS", style = MaterialTheme.typography.labelSmall, color = LabInk)
                }
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text("Secure field channel", style = MaterialTheme.typography.titleLarge)
                    Text(
                        "OkHttp · bidirectional evidence",
                        style = MaterialTheme.typography.labelSmall,
                        color = LabTextMuted,
                    )
                }
                Box(
                    Modifier
                        .size(10.dp)
                        .background(if (connected) LabMint else LabTextMuted, CircleShape),
                )
            }
            Spacer(Modifier.size(9.dp))
            Text(
                text = status,
                style = MaterialTheme.typography.bodyMedium,
                color = if (connected) LabMint else LabTextMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .testTag("websocket_status")
                        .semantics { contentDescription = "WebSocket connection status: $status" },
            )
        }
    }
}

@Composable
private fun EndpointCard(
    url: String,
    onUrlChanged: (String) -> Unit,
    connected: Boolean,
    connecting: Boolean,
    onUseWs: () -> Unit,
    onUseWss: () -> Unit,
    onConnect: () -> Unit,
) {
    Card(
        colors = CardDefaults.cardColors(containerColor = LabPanel, contentColor = LabText),
        shape = RoundedCornerShape(22.dp),
        border = androidx.compose.foundation.BorderStroke(1.dp, LabOutline),
    ) {
        Column(Modifier.padding(16.dp)) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                OutlinedButton(
                    onClick = onUseWs,
                    modifier =
                        Modifier
                            .weight(1f)
                            .testTag("websocket_use_ws_button")
                            .semantics { contentDescription = "Use cleartext WebSocket URL" },
                ) {
                    Text("WS")
                }
                Button(
                    onClick = onUseWss,
                    colors =
                        ButtonDefaults.buttonColors(
                            containerColor = LabCyan.copy(alpha = 0.18f),
                            contentColor = LabCyan,
                        ),
                    modifier =
                        Modifier
                            .weight(1f)
                            .testTag("websocket_use_wss_button")
                            .semantics { contentDescription = "Use TLS WebSocket URL" },
                ) {
                    Text("WSS")
                }
            }
            Spacer(Modifier.size(10.dp))
            OutlinedTextField(
                value = url,
                onValueChange = onUrlChanged,
                label = { Text("Channel endpoint") },
                singleLine = true,
                textStyle =
                    MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace),
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .testTag("websocket_url_input")
                        .semantics { contentDescription = "WebSocket URL input" },
            )
            Spacer(Modifier.size(11.dp))
            if (connecting) {
                LinearProgressIndicator(Modifier.fillMaxWidth())
                Spacer(Modifier.size(10.dp))
            }
            Button(
                onClick = onConnect,
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .testTag("websocket_connect_button")
                        .semantics { contentDescription = "Connect WebSocket button" },
            ) {
                Text(if (connected) "Reconnect channel" else "Establish channel")
            }
        }
    }
}

@Composable
private fun ChatBubble(entry: ChatEntry) {
    val alignment =
        when (entry.direction) {
            ChatDirection.Client -> Alignment.CenterEnd
            ChatDirection.Server -> Alignment.CenterStart
            ChatDirection.System -> Alignment.Center
        }
    val container =
        when (entry.direction) {
            ChatDirection.Client -> LabViolet.copy(alpha = 0.25f)
            ChatDirection.Server -> LabCyan.copy(alpha = 0.16f)
            ChatDirection.System -> LabPanelRaised
        }
    val border =
        when (entry.direction) {
            ChatDirection.Client -> LabViolet.copy(alpha = 0.45f)
            ChatDirection.Server -> LabCyan.copy(alpha = 0.38f)
            ChatDirection.System -> LabOutline
        }

    Box(Modifier.fillMaxWidth(), contentAlignment = alignment) {
        Column(
            modifier =
                Modifier
                    .fillMaxWidth(if (entry.direction == ChatDirection.System) 1f else 0.84f)
                    .background(container, RoundedCornerShape(18.dp))
                    .border(1.dp, border, RoundedCornerShape(18.dp))
                    .padding(horizontal = 14.dp, vertical = 11.dp),
        ) {
            Text(
                entry.direction.name.uppercase(),
                style = MaterialTheme.typography.labelSmall,
                color =
                    when (entry.direction) {
                        ChatDirection.Client -> LabVioletBright
                        ChatDirection.Server -> LabCyan
                        ChatDirection.System -> LabTextMuted
                    },
            )
            Spacer(Modifier.size(3.dp))
            Text(entry.text, style = MaterialTheme.typography.bodyMedium)
        }
    }
}
