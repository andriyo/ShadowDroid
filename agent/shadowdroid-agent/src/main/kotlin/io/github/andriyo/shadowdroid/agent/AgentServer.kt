package io.github.andriyo.shadowdroid.agent

import android.util.Log
import java.io.BufferedReader
import java.io.InputStreamReader
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket

/**
 * Minimal loopback control channel: one request line in → one JSON line out,
 * newline-framed (mirrors the host net daemon's line-JSON protocol). Bound to
 * 127.0.0.1 so it is only reachable on-device and via `adb forward`.
 *
 * Entirely best-effort — every failure is swallowed and logged so the debug
 * agent can never crash or hang the host app.
 */
internal class AgentServer(private val handler: (String) -> String) {

    @Volatile
    private var socket: ServerSocket? = null

    /** Bind to the first free port in [PORT_RANGE]; returns the port, or -1. */
    fun startBestEffort(): Int {
        val loopback = InetAddress.getByName("127.0.0.1")
        for (candidate in PORT_RANGE) {
            try {
                val server = ServerSocket(candidate, BACKLOG, loopback)
                socket = server
                Thread({ acceptLoop(server) }, "shadowdroid-agent").apply {
                    isDaemon = true
                    start()
                }
                return candidate
            } catch (_: Throwable) {
                // Port taken or unavailable — try the next one.
            }
        }
        return -1
    }

    private fun acceptLoop(server: ServerSocket) {
        while (!server.isClosed) {
            try {
                server.accept().use(::handleClient)
            } catch (t: Throwable) {
                if (server.isClosed) break
                Log.w(ShadowDroidAgent.TAG, "accept failed", t)
            }
        }
    }

    private fun handleClient(client: Socket) {
        try {
            val request = BufferedReader(
                InputStreamReader(client.getInputStream(), Charsets.UTF_8),
            ).readLine() ?: "info"
            val response = handler(request)
            client.getOutputStream().apply {
                write((response + "\n").toByteArray(Charsets.UTF_8))
                flush()
            }
        } catch (t: Throwable) {
            Log.w(ShadowDroidAgent.TAG, "request failed", t)
        }
    }

    companion object {
        private const val BACKLOG = 4
        private val PORT_RANGE = 8099..8108
    }
}
