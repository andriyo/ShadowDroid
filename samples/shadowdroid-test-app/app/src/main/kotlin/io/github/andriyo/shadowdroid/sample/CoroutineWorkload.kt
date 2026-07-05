package io.github.andriyo.shadowdroid.sample

import android.util.Log
import kotlinx.coroutines.CoroutineName
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * A small zoo of deliberately-misbehaving coroutines, so `shadowdroid aar
 * coroutines` has something recognisable to dump:
 *
 * - **leaked-heartbeat** — a `GlobalScope`-style loop nobody cancels (a leak).
 * - **idle-worker-N** — parked forever on an empty channel (stuck receivers).
 * - **slow-collector** / **blocked-emitter** — a `SharedFlow` with no buffer and
 *   a slow collector, so the producer suspends inside `emit` (a clogged flow).
 *
 * Each coroutine carries a [CoroutineName] so it is identifiable in the dump.
 */
object CoroutineWorkload {
    private const val TAG = "ShadowDroidSample"

    // A long-lived, never-cancelled scope — that is the leak, on purpose.
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val started = AtomicBoolean(false)
    private val extraWorkers = AtomicInteger(0)

    // Nothing is ever sent → receivers suspend indefinitely.
    private val idleChannel = Channel<Unit>()

    // No buffer + SUSPEND overflow → a producer suspends once a subscriber lags.
    private val sharedFlow = MutableSharedFlow<Int>(extraBufferCapacity = 0)

    /** Idempotent: spins up the baseline zoo the first time only. */
    fun startOnce(): String {
        if (!started.compareAndSet(false, true)) return summary("already running")

        scope.launch(CoroutineName("leaked-heartbeat")) {
            var beats = 0L
            while (isActive) {
                delay(1_000)
                beats++
            }
        }

        repeat(BASE_WORKERS) { i ->
            scope.launch(CoroutineName("idle-worker-$i")) {
                idleChannel.receive() // never resumes — nothing is sent
            }
        }

        scope.launch(CoroutineName("slow-collector")) {
            sharedFlow.collect {
                delay(60_000) // one item takes a minute → producer backs up
            }
        }
        scope.launch(CoroutineName("blocked-emitter")) {
            var n = 0
            while (isActive) {
                sharedFlow.emit(n++) // suspends: no buffer + slow collector
            }
        }

        Log.i(TAG, "coroutine workload started")
        return summary("started")
    }

    /** Add one more parked worker on demand (to watch the count grow). */
    fun spawnWorker(): String {
        val id = extraWorkers.incrementAndGet()
        scope.launch(CoroutineName("extra-worker-$id")) {
            idleChannel.receive()
        }
        return summary("spawned extra-worker-$id")
    }

    private fun summary(note: String): String =
        "$note — baseline ${BASE_WORKERS + 3} coroutines + $extraWorkers extra"

    private const val BASE_WORKERS = 8
}
