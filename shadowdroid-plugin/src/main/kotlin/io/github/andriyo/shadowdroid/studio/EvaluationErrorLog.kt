package io.github.andriyo.shadowdroid.studio

/**
 * Bounded history of breakpoint-expression evaluation failures. Entries are
 * plain maps so the bridge can embed them into JSON payloads verbatim.
 */
internal class EvaluationErrorLog(private val capacity: Int) {
    private val entries = ArrayDeque<Map<String, Any?>>()

    @Synchronized
    fun add(entry: Map<String, Any?>) {
        entries.addLast(entry)
        while (entries.size > capacity) {
            entries.removeFirst()
        }
    }

    @Synchronized
    fun recent(limit: Int): List<Map<String, Any?>> = entries.takeLast(limit.coerceAtLeast(0))

    @Synchronized
    fun lastFor(breakpointId: String): Map<String, Any?>? =
        entries.lastOrNull { it["breakpoint_id"] == breakpointId }

    @Synchronized
    fun clearFor(breakpointId: String) {
        entries.removeIf { it["breakpoint_id"] == breakpointId }
    }
}
