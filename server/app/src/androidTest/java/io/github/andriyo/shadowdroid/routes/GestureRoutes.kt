package io.github.andriyo.shadowdroid.routes

import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.proto.OkResponse
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
    ) {
        route.post("/tap") {
            val r: XyReq = call.receive()
            if (!uiDevice.click(r.x, r.y)) throw BadRequest("tap_failed", "UiDevice.click returned false")
            call.respond(OkResponse())
        }
        route.post("/double_tap") {
            val r: XyReq = call.receive()
            uiDevice.click(r.x, r.y)
            Thread.sleep(50)
            uiDevice.click(r.x, r.y)
            call.respond(OkResponse())
        }
        route.post("/long_tap") {
            val r: LongTapReq = call.receive()
            // Implemented as a zero-distance swipe — UiDevice has no
            // long_click(x,y) primitive, but a "swipe" that doesn't move
            // for N ms is a long-press.
            val steps = (r.duration_ms / 5).coerceAtLeast(10) // ~5ms per step
            uiDevice.swipe(r.x, r.y, r.x, r.y, steps)
            call.respond(OkResponse())
        }
        route.post("/swipe") {
            val r: SwipeReq = call.receive()
            // UiDevice.swipe takes a `steps` count (1 step ≈ 5ms).
            val steps = (r.duration_ms / 5).coerceAtLeast(1)
            if (!uiDevice.swipe(r.from[0], r.from[1], r.to[0], r.to[1], steps)) {
                throw BadRequest("swipe_failed", "UiDevice.swipe returned false")
            }
            call.respond(OkResponse())
        }
        route.post("/drag") {
            val r: SwipeReq = call.receive()
            // UiDevice.drag has a separate "long-press then move" semantics
            // distinct from swipe; we synthesize via a long initial dwell
            // then the swipe. UI Automator 2.x doesn't expose drag(x,y,x,y).
            val initialDwell = 200 // ms
            uiDevice.swipe(r.from[0], r.from[1], r.from[0], r.from[1], initialDwell / 5)
            val moveSteps = (r.duration_ms / 5).coerceAtLeast(1)
            uiDevice.swipe(r.from[0], r.from[1], r.to[0], r.to[1], moveSteps)
            call.respond(OkResponse())
        }
        route.post("/swipe_ext") {
            val r: SwipeExtReq = call.receive()
            val (x1, y1, x2, y2) =
                swipeExtCoords(
                    uiDevice.displayWidth,
                    uiDevice.displayHeight,
                    r.direction,
                    r.scale,
                )
            val steps = (r.duration_ms / 5).coerceAtLeast(1)
            uiDevice.swipe(x1, y1, x2, y2, steps)
            call.respond(OkResponse())
        }
    }
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
