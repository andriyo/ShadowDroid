package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import androidx.test.uiautomator.By
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.application.ApplicationCall
import io.ktor.server.request.receive
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.post
import kotlinx.serialization.Serializable

object GestureRoutes {
    /** POST /v1/{tap,double_tap,long_tap,swipe,drag,swipe_ext}. */
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
        route.post("/tap") {
            handleTap(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/tap") {
            handleTap(call, uiDevice, instr, guarded = true)
        }
        route.post("/double_tap") {
            handleDoubleTap(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/double_tap") {
            handleDoubleTap(call, uiDevice, instr, guarded = true)
        }
        route.post("/long_tap") {
            handleLongTap(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/long_tap") {
            handleLongTap(call, uiDevice, instr, guarded = true)
        }
        route.post("/swipe") {
            handleSwipe(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/swipe") {
            handleSwipe(call, uiDevice, instr, guarded = true)
        }
        route.post("/drag") {
            handleDrag(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/drag") {
            handleDrag(call, uiDevice, instr, guarded = true)
        }
        route.post("/swipe_ext") {
            handleSwipeExt(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/swipe_ext") {
            handleSwipeExt(call, uiDevice, instr, guarded = true)
        }
        route.post("/pinch") {
            handlePinch(call, uiDevice, instr, guarded = false)
        }
        route.post("/guarded/pinch") {
            handlePinch(call, uiDevice, instr, guarded = true)
        }
    }
}

private suspend fun handleTap(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val r: XyReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) {
        if (!uiDevice.click(r.x, r.y)) throw BadRequest("tap_failed", "UiDevice.click returned false")
    }
    call.respond(OkResponse())
}

private suspend fun handleDoubleTap(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val r: XyReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) {
        uiDevice.click(r.x, r.y)
        Thread.sleep(50)
        uiDevice.click(r.x, r.y)
    }
    call.respond(OkResponse())
}

private suspend fun handleLongTap(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val r: LongTapReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) {
        // A zero-distance swipe provides long-press semantics.
        val steps = (r.duration_ms / 5).coerceAtLeast(10)
        uiDevice.swipe(r.x, r.y, r.x, r.y, steps)
    }
    call.respond(OkResponse())
}

private suspend fun handleSwipe(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val r: SwipeReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) {
        val steps = (r.duration_ms / 5).coerceAtLeast(1)
        if (!uiDevice.swipe(r.from[0], r.from[1], r.to[0], r.to[1], steps)) {
            throw BadRequest("swipe_failed", "UiDevice.swipe returned false")
        }
    }
    call.respond(OkResponse())
}

private suspend fun handleDrag(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val r: SwipeReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) {
        val initialDwell = 200
        uiDevice.swipe(r.from[0], r.from[1], r.from[0], r.from[1], initialDwell / 5)
        val moveSteps = (r.duration_ms / 5).coerceAtLeast(1)
        uiDevice.swipe(r.from[0], r.from[1], r.to[0], r.to[1], moveSteps)
    }
    call.respond(OkResponse())
}

private suspend fun handleSwipeExt(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val r: SwipeExtReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) {
        val (x1, y1, x2, y2) =
            swipeExtCoords(uiDevice.displayWidth, uiDevice.displayHeight, r.direction, r.scale)
        val steps = (r.duration_ms / 5).coerceAtLeast(1)
        uiDevice.swipe(x1, y1, x2, y2, steps)
    }
    call.respond(OkResponse())
}

private suspend fun handlePinch(
    call: ApplicationCall,
    uiDevice: UiDevice,
    instr: Instrumentation,
    guarded: Boolean,
) {
    val r: PinchReq = call.receive()
    withUiActionGuard(call, uiDevice, instr, guarded) {
        val by =
            when {
                r.rid != null -> By.res(r.rid)
                r.text != null -> By.textContains(r.text)
                r.desc != null -> By.descContains(r.desc)
                else -> throw BadRequest("empty_selector", "pinch needs one of rid|text|desc")
            }
        val obj =
            uiDevice.findObject(by)
                ?: throw NotFound("element_not_found", "no element matched the pinch selector")
        val pct = (r.percent.coerceIn(1, 100)) / 100f
        when (r.direction.lowercase()) {
            "in", "close" -> obj.pinchClose(pct)
            "out", "open" -> obj.pinchOpen(pct)
            else -> throw BadRequest("bad_direction", "direction must be in|out, got '${r.direction}'")
        }
    }
    call.respond(OkResponse())
}

/**
 * Compute swipe endpoints for a direction + scale (fraction of viewport).
 * Returns (x1, y1, x2, y2).
 */
private fun swipeExtCoords(
    w: Int,
    h: Int,
    dir: String,
    scale: Float,
): Quadruple<Int, Int, Int, Int> {
    val cx = w / 2
    val cy = h / 2
    val s = scale.coerceIn(0.05f, 0.95f)
    return when (dir.lowercase()) {
        "up" -> Quadruple(cx, (cy + h * s / 2).toInt(), cx, (cy - h * s / 2).toInt())
        "down" -> Quadruple(cx, (cy - h * s / 2).toInt(), cx, (cy + h * s / 2).toInt())
        "left" -> Quadruple((cx + w * s / 2).toInt(), cy, (cx - w * s / 2).toInt(), cy)
        "right" -> Quadruple((cx - w * s / 2).toInt(), cy, (cx + w * s / 2).toInt(), cy)
        else -> throw BadRequest(
            "bad_direction",
            "direction must be one of up|down|left|right, got '$dir'",
        )
    }
}

// 4-tuple: data classes auto-generate componentN() for destructuring.
private data class Quadruple<A, B, C, D>(
    val a: A,
    val b: B,
    val c: C,
    val d: D,
)

// ── request bodies ────────────────────────────────────────────────

@Serializable
private data class XyReq(
    val x: Int,
    val y: Int,
)

@Serializable
private data class LongTapReq(
    val x: Int,
    val y: Int,
    val duration_ms: Int = 600,
)

@Serializable
private data class SwipeReq(
    val from: List<Int>,
    val to: List<Int>,
    val duration_ms: Int = 200,
)

@Serializable
private data class SwipeExtReq(
    val direction: String,
    val scale: Float = 0.9f,
    val duration_ms: Int = 200,
)

@Serializable
private data class PinchReq(
    val rid: String? = null,
    val text: String? = null,
    val desc: String? = null,
    val direction: String,
    val percent: Int = 50,
)
