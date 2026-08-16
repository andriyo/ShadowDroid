package io.github.andriyo.shadowdroid.studio

import java.util.Collections
import java.util.WeakHashMap

/**
 * Object-lifetime claim set for callbacks that must accept only a first hit.
 *
 * Weak keys avoid retaining IDE breakpoint objects if a removal notification
 * is missed. Methods are synchronized because breakpoint callbacks for
 * concurrent debugger sessions can race on different debugger threads.
 */
internal class FirstHitClaimSet<T : Any> {
    private val claimed = Collections.newSetFromMap(WeakHashMap<T, Boolean>())

    @Synchronized
    fun claim(value: T): Boolean = claimed.add(value)

    @Synchronized
    fun forget(value: T) {
        claimed.remove(value)
    }
}
