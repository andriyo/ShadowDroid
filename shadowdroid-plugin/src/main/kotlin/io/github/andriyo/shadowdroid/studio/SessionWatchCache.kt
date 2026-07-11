package io.github.andriyo.shadowdroid.studio

/**
 * Session-scoped watch cache with terminal stop semantics. Every mutation is
 * serialized so an evaluation that finishes during session shutdown cannot
 * recreate a value after cleanup.
 */
internal class SessionWatchCache<V> {
    private val lock = Any()
    private val values = hashMapOf<WatchCacheKey, V>()
    private val stoppedSessionIds = hashSetOf<String>()

    fun putIfActive(
        key: WatchCacheKey,
        value: V,
        watchIsActive: () -> Boolean,
    ): WatchCachePutResult = synchronized(lock) {
        if (key.sessionId in stoppedSessionIds) return@synchronized WatchCachePutResult.SESSION_STOPPED
        if (!watchIsActive()) return@synchronized WatchCachePutResult.WATCH_INACTIVE
        values[key] = value
        WatchCachePutResult.STORED
    }

    operator fun get(key: WatchCacheKey): V? = synchronized(lock) { values[key] }

    fun stopSession(sessionId: String) = synchronized(lock) {
        stoppedSessionIds += sessionId
        values.keys.removeIf { it.sessionId == sessionId }
    }

    fun removeWatch(watchId: String) = synchronized(lock) {
        values.keys.removeIf { it.watchId == watchId }
    }

    fun clear() = synchronized(lock) {
        values.clear()
    }
}

internal enum class WatchCachePutResult {
    STORED,
    SESSION_STOPPED,
    WATCH_INACTIVE,
}

internal data class WatchCacheKey(
    val watchId: String,
    val sessionId: String,
)
