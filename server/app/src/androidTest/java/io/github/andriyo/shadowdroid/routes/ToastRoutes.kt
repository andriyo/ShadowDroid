package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.view.accessibility.AccessibilityEvent
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.request.receive
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import io.ktor.server.routing.post
import kotlinx.serialization.Serializable

object ToastRoutes {
    private val lock = Any()
    private var bufferSize = 50
    private val toasts = ArrayDeque<ToastEvent>()

    /**
     * POST /v1/toast/{start,stop}, GET /v1/toast/recent.
     *
     * Backed by the UiAutomation accessibility-event listener; keeps a small
     * ring buffer of notification/toast text.
     */
    fun register(
        route: Route,
        instr: Instrumentation,
    ) {
        route.post("/toast/start") {
            val request = runCatching { call.receive<ToastStartReq>() }.getOrDefault(ToastStartReq())
            synchronized(lock) {
                bufferSize = request.buffer_size.coerceIn(1, 500)
                toasts.clear()
            }
            instr.uiAutomation.setOnAccessibilityEventListener { event ->
                if (event.eventType == AccessibilityEvent.TYPE_NOTIFICATION_STATE_CHANGED) {
                    recordToast(event)
                }
            }
            call.respond(OkResponse())
        }

        route.post("/toast/stop") {
            instr.uiAutomation.setOnAccessibilityEventListener(null)
            call.respond(OkResponse())
        }

        route.get("/toast/recent") {
            val since =
                call.request.queryParameters["since_ts"]?.toLongOrNull()
                    ?: (System.currentTimeMillis() - 5_000)
            val recent = synchronized(lock) { toasts.filter { it.ts >= since } }
            call.respond(ToastRecentResp(recent))
        }
    }

    private fun recordToast(event: AccessibilityEvent) {
        val text = event.text.joinToString(" ").trim()
        if (text.isEmpty()) return
        val toast =
            ToastEvent(
                `package` = event.packageName?.toString(),
                text = text,
                ts = System.currentTimeMillis(),
            )
        synchronized(lock) {
            toasts += toast
            while (toasts.size > bufferSize) {
                toasts.removeFirst()
            }
        }
    }
}

@Serializable
private data class ToastStartReq(
    val buffer_size: Int = 50,
)

@Serializable
data class ToastRecentResp(
    val toasts: List<ToastEvent>,
)

@Serializable
data class ToastEvent(
    val `package`: String? = null,
    val text: String,
    val ts: Long,
)
