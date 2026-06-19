package io.github.andriyo.shadowdroid.agent

import android.content.Context
import android.os.Build
import android.os.Process
import android.util.Log
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The in-process debug agent. Bootstrapped by [ShadowDroidAgentProvider] in
 * debug builds and reached by ShadowDroid's CLI over a loopback control channel
 * (`adb forward`).
 *
 * It is intentionally a small, extensible host: capabilities register a command
 * verb in [handle]. It answers `info`/`ping`, and — when the OkHttp companion
 * interceptor is wired into the host app — the network verbs `capture`,
 * `intercept`, `resume`, `drop`, and `status` (above-TLS, in-process capture +
 * agent-in-the-loop interception; see [Capture] / [Intercept]).
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

    /**
     * Dispatch one control-channel command line to a single JSON response.
     *
     * Grammar (newline-framed, one line in → one JSON line out):
     * - `info` / `ping`
     * - `capture [--clear]`            → buffered flows (FlowRecord shape)
     * - `capture-clear`                → drop the capture buffer
     * - `intercept {json-matcher}`     → arm interception
     * - `intercept-clear`              → disarm
     * - `status`                       → armed matcher + held flows + counts
     * - `resume <id> [{status,body,contentType}]`
     * - `drop <id>`
     */
    private fun handle(line: String): String {
        val trimmed = line.trim()
        val cmd = trimmed.substringBefore(' ').ifEmpty { "info" }
        val rest = trimmed.substringAfter(' ', "").trim()
        return try {
            when (cmd) {
                "info", "ping" -> info()
                "capture" -> capture(clear = rest.contains("--clear"))
                "capture-clear" -> { Capture.clear(); ok() }
                "intercept" -> intercept(rest)
                "intercept-clear", "disarm" -> { Intercept.disarm(); statusJson() }
                "status" -> statusJson()
                "resume" -> resolve(rest, drop = false)
                "drop" -> resolve(rest, drop = true)
                else -> jsonError("unknown command: $cmd")
            }
        } catch (t: Throwable) {
            jsonError("command '$cmd' failed: ${t.message}")
        }
    }

    private fun capture(clear: Boolean): String =
        JSONObject().apply {
            put("ok", true)
            val flows = Capture.drain(clear)
            put("flows", flows)
            put("count", flows.length())
        }.toString()

    private fun intercept(rest: String): String {
        val spec = if (rest.isEmpty()) JSONObject() else JSONObject(rest)
        Intercept.arm(spec)
        return statusJson()
    }

    private fun statusJson(): String =
        JSONObject().apply {
            put("ok", true)
            put("package", packageName)
            put("captured", Capture.size())
            put("intercept", Intercept.status())
        }.toString()

    private fun resolve(rest: String, drop: Boolean): String {
        val id = rest.substringBefore(' ').ifEmpty { return jsonError("missing flow id") }
        val payload = rest.substringAfter(' ', "").trim()
        val action = when {
            drop -> JSONObject().put("drop", true)
            payload.isEmpty() -> JSONObject()
            else -> JSONObject(payload)
        }
        val resolved = Intercept.resolve(id, action)
        return JSONObject().apply {
            put("ok", resolved)
            put("id", id)
            if (!resolved) put("error", "no held flow with id '$id'")
        }.toString()
    }

    private fun ok(): String = """{"ok":true}"""

    fun info(): String =
        """{"ok":true,"agent":"shadowdroid","version":"${BuildInfo.VERSION}",""" +
            """"package":"$packageName","sdk":${Build.VERSION.SDK_INT},""" +
            """"pid":${Process.myPid()},"port":$port}"""

    private fun jsonError(message: String): String =
        """{"ok":false,"error":"${message.replace("\"", "'")}"}"""
}
