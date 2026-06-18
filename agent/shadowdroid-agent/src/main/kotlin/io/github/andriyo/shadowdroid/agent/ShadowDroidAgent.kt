package io.github.andriyo.shadowdroid.agent

import android.content.Context
import android.os.Build
import android.os.Process
import android.util.Log
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The in-process debug agent. Bootstrapped by [ShadowDroidAgentProvider] in
 * debug builds and reached by ShadowDroid's CLI over a loopback control channel
 * (`adb forward`).
 *
 * It is intentionally a small, extensible host: capabilities register a command
 * verb in [handle]. Today it answers `info`/`ping`; network capture, state
 * inspection, and the net-mock fixture broker plug in here later.
 */
object ShadowDroidAgent {
    const val TAG = "ShadowDroidAgent"

    /** Logged once on startup; the CLI greps logcat for this to confirm the
     *  agent is live and to discover the control port. */
    const val MARKER = "shadowdroid-agent-ready"

    private val started = AtomicBoolean(false)

    @Volatile
    private var server: AgentServer? = null

    @Volatile
    var packageName: String = "?"
        private set

    /** Loopback control port, or -1 if the channel could not bind. */
    @Volatile
    var port: Int = -1
        private set

    /** Idempotent: safe to call more than once (only the first call binds). */
    fun start(context: Context) {
        if (!started.compareAndSet(false, true)) return
        packageName = context.packageName
        port = AgentServer(::handle).also { server = it }.startBestEffort()
        Log.i(
            TAG,
            "$MARKER version=${BuildInfo.VERSION} package=$packageName " +
                "sdk=${Build.VERSION.SDK_INT} pid=${Process.myPid()} port=$port",
        )
    }

    /** Dispatch one control-channel command line to a single JSON response. */
    private fun handle(line: String): String {
        val cmd = line.trim().substringBefore(' ').ifEmpty { "info" }
        return when (cmd) {
            "info", "ping" -> info()
            else -> jsonError("unknown command: $cmd")
        }
    }

    fun info(): String =
        """{"ok":true,"agent":"shadowdroid","version":"${BuildInfo.VERSION}",""" +
            """"package":"$packageName","sdk":${Build.VERSION.SDK_INT},""" +
            """"pid":${Process.myPid()},"port":$port}"""

    private fun jsonError(message: String): String =
        """{"ok":false,"error":"${message.replace("\"", "'")}"}"""
}
