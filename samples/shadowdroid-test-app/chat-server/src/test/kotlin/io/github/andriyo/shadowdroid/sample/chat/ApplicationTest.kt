package io.github.andriyo.shadowdroid.sample.chat

import io.ktor.client.plugins.websocket.DefaultClientWebSocketSession
import io.ktor.client.plugins.websocket.WebSockets
import io.ktor.client.plugins.websocket.webSocket
import io.ktor.server.testing.testApplication
import io.ktor.websocket.Frame
import io.ktor.websocket.readText
import io.ktor.websocket.send
import kotlinx.coroutines.withTimeout
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertTrue

class ApplicationTest {
    @Test
    fun `chat sends server greeting and returns client message`() = testApplication {
        application { chatModule() }
        val websocketClient = createClient { install(WebSockets) }

        websocketClient.webSocket("/chat?name=test-client") {
            assertEquals("server: connected as test-client", receiveText())

            send("hello-through-shadowdroid")
            val replies = buildList {
                repeat(2) { add(receiveText()) }
            }
            assertTrue(
                replies.contains("test-client: hello-through-shadowdroid"),
                "chat reply missing from $replies",
            )
        }
    }

    private suspend fun DefaultClientWebSocketSession.receiveText(): String =
        withTimeout(2_000) { (incoming.receive() as Frame.Text).readText() }
}
