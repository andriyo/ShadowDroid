package io.github.andriyo.shadowdroid.studio

import java.util.concurrent.CompletableFuture
import java.util.concurrent.ExecutionException
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import java.util.concurrent.atomic.AtomicInteger

/**
 * Android Studio's UI thread did not start a bridge request in time — usually
 * because a modal dialog is open. The request was cancelled and will never run,
 * so the caller may safely retry once the IDE is free.
 */
internal class StudioUiBusyException(message: String) : IllegalStateException(message)

/**
 * Work handed from a bridge thread to another thread (the IDE's UI thread) that
 * either starts before [await]'s deadline or is cancelled and never runs. A
 * plain `invokeAndWait` had no deadline, and a timed wait on it would still
 * let the queued work — a debugger step, resume or breakpoint change — run
 * later, after the caller had given up and possibly retried.
 */
internal class CancellableHandoff<T>(private val work: () -> T) {
    private val state = AtomicInteger(PENDING)
    private val result = CompletableFuture<T>()

    /** Run the work unless [await] already cancelled it. Call on the target thread. */
    fun run() {
        if (!state.compareAndSet(PENDING, RUNNING)) return
        try {
            result.complete(work())
        } catch (t: Throwable) {
            result.completeExceptionally(t)
        }
    }

    /**
     * Wait for the result. If the work has not started within [startTimeoutMs]
     * it is cancelled and [StudioUiBusyException] is thrown. Work that already
     * started is awaited for up to [runTimeoutMs], because its effect is underway.
     */
    @Throws(Exception::class)
    fun await(startTimeoutMs: Long, runTimeoutMs: Long, busyHint: () -> String = { "" }): T {
        try {
            return unwrap { result.get(startTimeoutMs, TimeUnit.MILLISECONDS) }
        } catch (_: TimeoutException) {
            if (state.compareAndSet(PENDING, CANCELLED)) {
                throw StudioUiBusyException(
                    "Android Studio's UI thread did not start the request within ${startTimeoutMs}ms; " +
                        "it was cancelled and will not run. A modal dialog may be open${busyHint()}",
                )
            }
        }
        try {
            return unwrap { result.get(runTimeoutMs, TimeUnit.MILLISECONDS) }
        } catch (_: TimeoutException) {
            throw IllegalStateException(
                "the request started on Android Studio's UI thread but did not finish within " +
                    "${startTimeoutMs + runTimeoutMs}ms; its outcome is unknown",
            )
        }
    }

    private inline fun unwrap(block: () -> T): T =
        try {
            block()
        } catch (e: ExecutionException) {
            throw e.cause ?: e
        }

    private companion object {
        const val PENDING = 0
        const val RUNNING = 1
        const val CANCELLED = 2
    }
}
