package io.github.andriyo.shadowdroid.sample.chat

import io.ktor.http.ContentType
import io.ktor.network.tls.certificates.buildKeyStore
import io.ktor.network.tls.certificates.saveToFile
import io.ktor.network.tls.extensions.HashAlgorithm
import io.ktor.network.tls.extensions.SignatureAlgorithm
import io.ktor.server.application.Application
import io.ktor.server.application.install
import io.ktor.server.engine.applicationEnvironment
import io.ktor.server.engine.connector
import io.ktor.server.engine.embeddedServer
import io.ktor.server.engine.sslConnector
import io.ktor.server.netty.Netty
import io.ktor.server.response.respondText
import io.ktor.server.routing.get
import io.ktor.server.routing.routing
import io.ktor.server.websocket.WebSockets
import io.ktor.server.websocket.pingPeriod
import io.ktor.server.websocket.timeout
import io.ktor.server.websocket.webSocket
import io.ktor.websocket.Frame
import io.ktor.websocket.readBytes
import io.ktor.websocket.readText
import io.ktor.websocket.send
import io.ktor.websocket.WebSocketDeflateExtension
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import org.slf4j.LoggerFactory
import java.nio.file.Files
import java.util.zip.Deflater
import kotlin.time.Duration.Companion.seconds

private const val DEFAULT_WS_PORT = 18_080
private const val DEFAULT_WSS_PORT = 18_443
private const val KEY_ALIAS = "shadowdroid-chat"
private const val KEY_PASSWORD = "shadowdroid-chat-key"
private const val STORE_PASSWORD = "shadowdroid-chat-store"

fun main() {
    val wsPort = envPort("SHADOWDROID_CHAT_WS_PORT", DEFAULT_WS_PORT)
    val wssPort = envPort("SHADOWDROID_CHAT_WSS_PORT", DEFAULT_WSS_PORT)
    val keyStoreFile = Files.createTempFile("shadowdroid-chat-", ".jks").toFile().apply {
        deleteOnExit()
    }
    val keyStore = buildKeyStore {
        certificate(KEY_ALIAS) {
            password = KEY_PASSWORD
            hash = HashAlgorithm.SHA256
            sign = SignatureAlgorithm.RSA
            keySizeInBits = 2_048
            domains = listOf("127.0.0.1", "localhost", "shadowdroid.localhost")
        }
    }
    keyStore.saveToFile(keyStoreFile, STORE_PASSWORD)

    embeddedServer(
        Netty,
        applicationEnvironment {
            log = LoggerFactory.getLogger("shadowdroid.chat")
        },
        configure = {
            connector {
                host = "127.0.0.1"
                port = wsPort
            }
            sslConnector(
                keyStore = keyStore,
                keyAlias = KEY_ALIAS,
                keyStorePassword = { STORE_PASSWORD.toCharArray() },
                privateKeyPassword = { KEY_PASSWORD.toCharArray() },
            ) {
                host = "127.0.0.1"
                port = wssPort
                keyStorePath = keyStoreFile
            }
            shutdownGracePeriod = 500
            shutdownTimeout = 1_500
        },
        module = Application::chatModule,
    ).start(wait = true)
}

fun Application.chatModule() {
    val room = ChatRoom()
    val chatLog = environment.log

    install(WebSockets) {
        pingPeriod = 5.seconds
        timeout = 15.seconds
        maxFrameSize = 64 * 1024
        masking = false
        extensions {
            install(WebSocketDeflateExtension) {
                compressionLevel = Deflater.DEFAULT_COMPRESSION
                clientNoContextTakeOver = true
                serverNoContextTakeOver = true
                compressIfBiggerThan(bytes = 1)
            }
        }
    }

    routing {
        get("/health") {
            call.respondText(
                """{"status":"ok","service":"shadowdroid-websocket-chat"}""",
                ContentType.Application.Json,
            )
        }

        webSocket("/chat") {
            val clientName = call.request.queryParameters["name"].safeClientName()
            chatLog.info("WebSocket connected: {}", clientName)
            send("server: connected as $clientName")

            val outbound = launch(start = CoroutineStart.UNDISPATCHED) {
                room.messages.collect { message -> send(Frame.Text(message)) }
            }
            room.publish("server: $clientName joined")

            try {
                for (frame in incoming) {
                    when (frame) {
                        is Frame.Text -> {
                            val message = frame.readText().take(4_096)
                            chatLog.info(
                                "WebSocket text message from {}: {} bytes",
                                clientName,
                                message.toByteArray().size,
                            )
                            room.publish("$clientName: $message")
                        }

                        is Frame.Binary -> {
                            val bytes = frame.readBytes()
                            chatLog.info("WebSocket binary message from {}: {} bytes", clientName, bytes.size)
                            send(Frame.Binary(fin = true, data = bytes))
                        }

                        else -> Unit
                    }
                }
            } finally {
                outbound.cancelAndJoin()
                room.publish("server: $clientName left")
                chatLog.info("WebSocket disconnected: {}", clientName)
            }
        }
    }
}

private class ChatRoom {
    val messages = MutableSharedFlow<String>(extraBufferCapacity = 64)

    suspend fun publish(message: String) {
        messages.emit(message)
    }
}

private fun String?.safeClientName(): String =
    this
        ?.trim()
        ?.filter { it.isLetterOrDigit() || it == '-' || it == '_' }
        ?.take(32)
        ?.ifEmpty { null }
        ?: "android"

private fun envPort(name: String, default: Int): Int =
    System.getenv(name)
        ?.toIntOrNull()
        ?.takeIf { it in 1..65_535 }
        ?: default
