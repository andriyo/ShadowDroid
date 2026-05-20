package io.github.andriyo.shadowdroid

import android.app.Instrumentation
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.routes.*
import io.ktor.http.*
import io.ktor.serialization.kotlinx.json.*
import io.ktor.server.application.*
import io.ktor.server.cio.*
import io.ktor.server.engine.*
import io.ktor.server.plugins.calllogging.*
import io.ktor.server.plugins.contentnegotiation.*
import io.ktor.server.plugins.statuspages.*
import io.ktor.server.response.*
import io.ktor.server.routing.*
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import org.slf4j.event.Level

/**
 * Ktor 3 / CIO-backed v1 HTTP API for ShadowDroid. Implements the protocol
 * documented in `docs/protocol.md`.
 *
 * Design choices (see architecture.md §9):
 *   • Ktor 3 (CIO engine) for active maintenance + the type-safe routing DSL.
 *   • No gzip, no WebSocket — single-request/single-response keeps the wire
 *     `curl`-able and the CLI in charge of the watch loop's cadence.
 *
 * UiDevice is acquired by the JUnit test runner (see ShadowDroidServerTest)
 * and passed in here, so every route has a working UI Automator handle.
 */
class HttpServer(
    private val instrumentation: Instrumentation,
    private val uiDevice: UiDevice,
    private val port: Int,
) {
    private var engine: EmbeddedServer<*, *>? = null

    fun start() {
        engine = embeddedServer(CIO, port = port, host = "127.0.0.1") {
            installPlugins()
            routing {
                route("/v1") {
                    StateRoutes.register(this, uiDevice, instrumentation)
                    ScreenRoutes.register(this, uiDevice, instrumentation)
                    GestureRoutes.register(this, uiDevice)
                    KeyTextRoutes.register(this, uiDevice)
                    AppRoutes.register(this, uiDevice, instrumentation)
                    SystemRoutes.register(this, uiDevice, instrumentation)
                    // M4:
                    SelectorRoutes.register(this, uiDevice)
                    ToastRoutes.register(this, uiDevice)
                    FileRoutes.register(this)
                }
            }
        }.also { it.start(wait = false) }
    }

    fun stop() {
        // 250ms grace for in-flight responses; hard cut at 1s.
        engine?.stop(gracePeriodMillis = 250, timeoutMillis = 1_000)
    }
}

/**
 * Cross-cutting Ktor plugins. Kept in one place so route handlers stay
 * focused on their endpoint logic.
 */
private fun Application.installPlugins() {
    install(ContentNegotiation) {
        json(Json {
            ignoreUnknownKeys = true
            encodeDefaults = true
            explicitNulls = false
        })
    }
    install(CallLogging) {
        level = Level.DEBUG  // captured in `adb logcat` under the test process tag
    }
    install(StatusPages) {
        // Map our domain exceptions to the wire-error envelope documented in
        // protocol.md §11. Anything we don't recognise turns into a 500 with
        // the exception type name in `detail.type`.
        exception<BadRequest> { call, e ->
            call.respond(HttpStatusCode.BadRequest, ErrorEnvelope(ErrorBody(e.code, e.message ?: "bad request", e.detail)))
        }
        exception<NotFound> { call, e ->
            call.respond(HttpStatusCode.NotFound, ErrorEnvelope(ErrorBody(e.code, e.message ?: "not found")))
        }
        exception<Timeout> { call, e ->
            call.respond(HttpStatusCode.RequestTimeout, ErrorEnvelope(ErrorBody(e.code, e.message ?: "timed out")))
        }
        exception<Throwable> { call, e ->
            call.respond(
                HttpStatusCode.InternalServerError,
                ErrorEnvelope(ErrorBody("internal", e.message ?: e::class.java.simpleName,
                    detail = mapOf("type" to e::class.java.simpleName))),
            )
        }
    }
}

// ── Wire-error types ───────────────────────────────────────────────────
// Routes throw these; StatusPages maps them to the JSON envelope.

class BadRequest(val code: String, message: String, val detail: Map<String, Any?>? = null) : RuntimeException(message)
class NotFound(val code: String, message: String) : RuntimeException(message)
class Timeout(val code: String, message: String) : RuntimeException(message)

@Serializable
data class ErrorEnvelope(val error: ErrorBody)

@Serializable
data class ErrorBody(
    val code: String,
    val message: String,
    val detail: Map<String, kotlinx.serialization.json.JsonElement>? = null,
) {
    constructor(code: String, message: String, detail: Map<String, Any?>? = null, @Suppress("UNUSED_PARAMETER") unused: Unit = Unit) :
        this(code, message, detail?.mapValues {
            when (val v = it.value) {
                null -> kotlinx.serialization.json.JsonNull
                is Number -> kotlinx.serialization.json.JsonPrimitive(v)
                is Boolean -> kotlinx.serialization.json.JsonPrimitive(v)
                else -> kotlinx.serialization.json.JsonPrimitive(v.toString())
            }
        })
}
