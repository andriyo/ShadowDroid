package io.github.andriyo.shadowdroid.routes

import androidx.test.uiautomator.UiDevice
import io.ktor.server.routing.*

/**
 * Stubs for route groups landing in M4. Kept as no-op `register` functions so
 * HttpServer's installation call site stays uniform.
 */

object SelectorRoutes {
    /** POST /v1/{find,find_tap,xpath}. */
    fun register(route: Route, uiDevice: UiDevice) { /* M4 */
        @Suppress("UNUSED_PARAMETER") val _ignored = Pair(route, uiDevice)
    }
}

object ToastRoutes {
    /**
     * POST /v1/toast/{start,stop}, GET /v1/toast/recent.
     *
     * Backed by an accessibility-event listener; keeps a small ring buffer.
     */
    fun register(route: Route, uiDevice: UiDevice) { /* M4 */
        @Suppress("UNUSED_PARAMETER") val _ignored = Pair(route, uiDevice)
    }
}

object FileRoutes {
    /** PUT/GET /v1/files{path}. Limited to the server's accessible storage. */
    fun register(route: Route) { /* M4 */
        @Suppress("UNUSED_PARAMETER") val _ignored = route
    }
}
