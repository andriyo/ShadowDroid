package io.github.andriyo.shadowdroid.studio

/**
 * The paused thread reported no frames. Right after a pause Android Studio is
 * still settling it (its own views evaluate against the thread), and for a
 * moment its frame list reads as empty although the frame exists.
 */
internal class FramesNotReadyException(message: String) : IllegalStateException(message)

internal object FrameSettle {
    /**
     * How long a request keeps retrying a pause that is still settling. The
     * first pause after attaching was seen to settle for over 2 s.
     */
    const val BUDGET_MS = 5_000L

    const val INTERVAL_MS = 100L

    /**
     * Run [attempt] until it stops throwing [FramesNotReadyException] or
     * [budgetMs] has passed, then rethrow the last one. Call it off the
     * debugger manager thread, which Studio needs to finish settling.
     */
    fun <T> retry(
        budgetMs: Long = BUDGET_MS,
        intervalMs: Long = INTERVAL_MS,
        nowMs: () -> Long = System::currentTimeMillis,
        sleep: (Long) -> Unit = Thread::sleep,
        attempt: () -> T,
    ): T {
        val deadline = nowMs() + budgetMs
        while (true) {
            try {
                return attempt()
            } catch (e: FramesNotReadyException) {
                if (nowMs() >= deadline) throw e
                sleep(intervalMs)
            }
        }
    }
}
