package io.github.andriyo.shadowdroid.studio

import java.util.UUID

internal const val DEFAULT_LOGPOINT_EVENT_CAPACITY = 512
internal const val DEFAULT_LOGPOINT_MAX_MESSAGE_CHARS = 16_384
internal const val MAX_LOGPOINT_MESSAGE_CHARS = 65_536
internal const val DEFAULT_LOGPOINT_MAX_EVENTS_PER_SECOND = 100

/**
 * Immutable debugger-session metadata captured away from the logpoint callback.
 *
 * Android Studio invokes the callback synchronously while it is processing the
 * breakpoint action. Keeping device and ClientData discovery out of that path
 * avoids extending the debuggee's otherwise non-suspending stop.
 */
internal data class LogpointSessionSnapshot(
    val id: String,
    val name: String,
    val deviceSerial: String? = null,
    val deviceAvd: String? = null,
    val packageName: String? = null,
    val processName: String? = null,
    val pid: Int? = null,
)

internal data class LogpointEventCandidate(
    val breakpointId: String,
    val owner: String?,
    val projectName: String,
    val projectPath: String?,
    val session: LogpointSessionSnapshot,
    val file: String?,
    val url: String?,
    val line: Int?,
    val type: String,
    val condition: String?,
    val logExpression: String?,
    val logMessage: Boolean,
    val logStack: Boolean,
    val message: String,
    val evaluationError: LogpointEvaluationError? = null,
    val maxMessageChars: Int = DEFAULT_LOGPOINT_MAX_MESSAGE_CHARS,
    val maxEventsPerSecond: Int = DEFAULT_LOGPOINT_MAX_EVENTS_PER_SECOND,
)

internal data class LogpointEvaluationError(
    val kind: String,
    val title: String?,
    val action: String,
)

/**
 * Android Studio 2026.1 reports a failed log-expression evaluation through
 * [com.intellij.xdebugger.breakpoints.XBreakpointListener.breakpointLogMessage]
 * as rendered text instead of invoking the breakpoint error policy. Recognize
 * only the exact platform prefix for the configured expression so a user value
 * that merely resembles an error is not reclassified.
 */
internal fun renderedLogpointEvaluationError(
    message: String,
    expression: String?,
): LogpointEvaluationError? {
    if (expression.isNullOrBlank()) return null
    val expectedPrefix = "Unable to evaluate the expression \"$expression\" :"
    if (!message.trimStart().startsWith(expectedPrefix)) return null
    return LogpointEvaluationError(
        kind = BreakpointExpressionGuard.KIND_LOG_EXPRESSION,
        title = null,
        action = "resumed_without_dialog",
    )
}

internal data class LogpointEventFilter(
    val breakpointId: String? = null,
    val owner: String? = null,
    val project: String? = null,
    val session: String? = null,
    val device: String? = null,
)

internal data class LogpointEventRead(
    val streamId: String,
    val events: List<Map<String, Any?>>,
    val nextCursor: Long,
    val latestCursor: Long,
    val oldestCursor: Long,
    val overflowed: Boolean,
    val evictedTotal: Long,
    val rateLimitedTotal: Long,
    val timedOut: Boolean,
)

/**
 * Process-local, bounded logpoint event stream.
 *
 * Sequence numbers are assigned only to accepted events. Rate-limited callbacks
 * are counted separately, so a cursor never contains unexplained holes. All
 * state is guarded by this object's monitor. Append work and retained payload
 * size are bounded by the configured message limit; truncation never scans the
 * unbounded remainder of Android Studio's composite message.
 */
@Suppress("PLATFORM_CLASS_MAPPED_TO_KOTLIN")
internal class LogpointEventLog(
    private val capacity: Int = DEFAULT_LOGPOINT_EVENT_CAPACITY,
    private val streamId: String = "logpoints_${UUID.randomUUID()}",
    private val clockMs: () -> Long = System::currentTimeMillis,
) {
    init {
        require(capacity > 0) { "capacity must be positive" }
    }

    private data class Entry(
        val seq: Long,
        val breakpointId: String,
        val owner: String?,
        val projectName: String,
        val projectPath: String?,
        val sessionId: String,
        val sessionName: String,
        val deviceSerial: String?,
        val deviceAvd: String?,
        val payload: Map<String, Any?>,
    )

    private data class RateWindow(
        var second: Long,
        var accepted: Int,
    )

    private val entries = ArrayDeque<Entry>()
    private val rates = mutableMapOf<String, RateWindow>()
    private var nextSeq = 1L
    private var evictedTotal = 0L
    private var rateLimitedTotal = 0L

    /** Returns the accepted sequence, or null when the per-breakpoint rate limit dropped it. */
    @Synchronized
    fun append(candidate: LogpointEventCandidate): Long? {
        val now = clockMs()
        val second = now / 1_000L
        val maxPerSecond = candidate.maxEventsPerSecond.coerceAtLeast(1)
        val rate = rates.getOrPut(candidate.breakpointId) { RateWindow(second, 0) }
        if (rate.second != second) {
            rate.second = second
            rate.accepted = 0
        }
        if (rate.accepted >= maxPerSecond) {
            rateLimitedTotal++
            return null
        }
        rate.accepted++

        val truncated = truncateCodePoints(
            candidate.message,
            candidate.maxMessageChars.coerceIn(1, MAX_LOGPOINT_MESSAGE_CHARS),
        )
        val seq = nextSeq++
        val device = if (candidate.session.deviceSerial == null && candidate.session.deviceAvd == null) {
            null
        } else {
            BridgeProtocol.map(
                "serial", candidate.session.deviceSerial,
                "avd", candidate.session.deviceAvd,
            )
        }
        val source = BridgeProtocol.map(
            "file", candidate.file,
            "url", candidate.url,
            "line", candidate.line,
        )
        val evaluationError = candidate.evaluationError?.let { error ->
            BridgeProtocol.map(
                "kind", error.kind,
                "title", error.title,
                "action", error.action,
            )
        }
        val payload = BridgeProtocol.map(
            "seq", seq,
            "timestamp_ms", now,
            "type", "logpoint",
            "schema_version", 1,
            "event_kind", if (evaluationError == null) "message" else "evaluation_error",
            "breakpoint_id", candidate.breakpointId,
            "owner", candidate.owner,
            "managed", candidate.owner != null,
            "project", BridgeProtocol.map(
                "name", candidate.projectName,
                "base_path", candidate.projectPath,
            ),
            "session", BridgeProtocol.map(
                "id", candidate.session.id,
                "name", candidate.session.name,
            ),
            "device", device,
            // ClientData is resolved and snapshotted when the debugger session
            // listener is installed. Keeping these fields flat makes it cheap
            // for app-scoped JSONL consumers to reject unknown or mismatched
            // events without touching debugger frames on the hit callback.
            "package", candidate.session.packageName,
            "process_name", candidate.session.processName,
            "pid", candidate.session.pid,
            "source", source,
            // Flat source fields make simple JSONL consumers cheaper while the
            // nested object preserves room for future source metadata.
            "file", candidate.file,
            "url", candidate.url,
            "line", candidate.line,
            "breakpoint_type", candidate.type,
            "condition", candidate.condition,
            "log_expression", candidate.logExpression,
            "log_message", candidate.logMessage,
            "log_stack", candidate.logStack,
            // This is Android Studio's composite rendered callback payload. It
            // may contain the default hit text, stack, and expression result.
            "message", truncated.text,
            "evaluation_error", evaluationError,
            "message_truncated", truncated.truncated,
            // UTF-16 code units, matching java.lang.String.length. This is O(1)
            // even if Android Studio hands us a very large composite message.
            "original_message_chars", truncated.originalChars,
        )
        entries.addLast(
            Entry(
                seq = seq,
                breakpointId = candidate.breakpointId,
                owner = candidate.owner,
                projectName = candidate.projectName,
                projectPath = candidate.projectPath,
                sessionId = candidate.session.id,
                sessionName = candidate.session.name,
                deviceSerial = candidate.session.deviceSerial,
                deviceAvd = candidate.session.deviceAvd,
                payload = payload,
            ),
        )
        while (entries.size > capacity) {
            entries.removeFirst()
            evictedTotal++
        }
        (this as java.lang.Object).notifyAll()
        return seq
    }

    /**
     * Read after a cursor, optionally waiting for a matching future event.
     * A null cursor is a one-shot tail read: newest matching events are returned.
     */
    @Synchronized
    fun read(
        after: Long?,
        limit: Int,
        filter: LogpointEventFilter = LogpointEventFilter(),
        timeoutMs: Long = 0,
    ): LogpointEventRead {
        val boundedLimit = limit.coerceAtLeast(1)
        val deadline = clockMs() + timeoutMs.coerceAtLeast(0)
        var timedOut = false

        if (after != null && timeoutMs > 0) {
            // A known cursor gap is already a readable state. Returning it
            // immediately lets followers report loss instead of hiding the
            // overflow behind a long-poll timeout when retained entries do not
            // match this filter.
            while (!hasMatchingAfter(after, filter) && !isCursorOverflowed(after)) {
                val remaining = deadline - clockMs()
                if (remaining <= 0) {
                    timedOut = true
                    break
                }
                (this as java.lang.Object).wait(remaining)
            }
        }

        val latest = nextSeq - 1
        val oldest = entries.firstOrNull()?.seq ?: if (latest == 0L) 0L else latest + 1
        val overflowed = after != null && oldest > 0 && after < oldest - 1

        val matched: List<Entry>
        val nextCursor: Long
        if (after == null) {
            matched = entries.asSequence()
                .filter { it.matches(filter) }
                .toList()
                .takeLast(boundedLimit)
            nextCursor = latest
        } else {
            val selected = mutableListOf<Entry>()
            var examinedThrough = after.coerceAtMost(latest)
            for (entry in entries) {
                if (entry.seq <= after) continue
                examinedThrough = entry.seq
                if (entry.matches(filter)) {
                    selected += entry
                    if (selected.size >= boundedLimit) break
                }
            }
            matched = selected
            // When fewer than limit matched, the full current stream was
            // examined. Advancing to latest avoids rescanning unrelated events.
            nextCursor = if (selected.size >= boundedLimit) examinedThrough else latest
        }

        return LogpointEventRead(
            streamId = streamId,
            events = matched.map(Entry::payload),
            nextCursor = nextCursor,
            latestCursor = latest,
            oldestCursor = oldest,
            overflowed = overflowed,
            evictedTotal = evictedTotal,
            rateLimitedTotal = rateLimitedTotal,
            timedOut = timedOut,
        )
    }

    @Synchronized
    fun forgetBreakpoint(breakpointId: String) {
        rates.remove(breakpointId)
    }

    private fun hasMatchingAfter(after: Long, filter: LogpointEventFilter): Boolean =
        entries.any { it.seq > after && it.matches(filter) }

    private fun isCursorOverflowed(after: Long): Boolean {
        val latest = nextSeq - 1
        val oldest = entries.firstOrNull()?.seq ?: if (latest == 0L) 0L else latest + 1
        return oldest > 0 && after < oldest - 1
    }

    private fun Entry.matches(filter: LogpointEventFilter): Boolean {
        if (filter.breakpointId != null && filter.breakpointId != breakpointId) return false
        if (filter.owner != null && filter.owner != owner) return false
        if (filter.project != null && filter.project != projectName && filter.project != projectPath) return false
        if (filter.session != null && filter.session != sessionId && filter.session != sessionName) return false
        if (filter.device != null && filter.device != deviceSerial && filter.device != deviceAvd) return false
        return true
    }

    private data class TruncatedText(
        val text: String,
        val truncated: Boolean,
        val originalChars: Int,
    )

    private fun truncateCodePoints(value: String, maxCodePoints: Int): TruncatedText {
        // The common case is an already-small message and needs no scan/copy.
        if (value.length <= maxCodePoints) return TruncatedText(value, false, value.length)

        // Walk at most maxCodePoints code points. Unlike codePointCount over the
        // whole string, callback work stays bounded even for a huge stack trace.
        var end = 0
        var codePoints = 0
        while (end < value.length && codePoints < maxCodePoints) {
            val first = value[end]
            end += if (
                first.isHighSurrogate() && end + 1 < value.length && value[end + 1].isLowSurrogate()
            ) {
                2
            } else {
                1
            }
            codePoints++
        }
        if (end >= value.length) return TruncatedText(value, false, value.length)
        return TruncatedText(value.substring(0, end), true, value.length)
    }
}
