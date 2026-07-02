package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.app.UiAutomation
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.ParcelFileDescriptor
import android.view.Surface
import androidx.test.uiautomator.UiDevice
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.proto.OkResponse
import io.ktor.server.request.receive
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import io.ktor.server.routing.post
import kotlinx.serialization.Serializable

object SystemRoutes {
    /** Power, orientation, clipboard, notifications, quick_settings, url, shell. */
    fun register(
        route: Route,
        uiDevice: UiDevice,
        instr: Instrumentation,
    ) {
        route.post("/screen/on") {
            uiDevice.wakeUp()
            call.respond(OkResponse())
        }
        route.post("/screen/off") {
            uiDevice.sleep()
            call.respond(OkResponse())
        }
        route.post("/wakeup") {
            uiDevice.wakeUp()
            call.respond(OkResponse())
        }
        route.post("/unlock") {
            // Wake + swipe up to dismiss the lock screen (works for swipe-to-unlock).
            uiDevice.wakeUp()
            val w = uiDevice.displayWidth
            val h = uiDevice.displayHeight
            uiDevice.swipe(w / 2, (h * 0.9).toInt(), w / 2, (h * 0.1).toInt(), 20)
            call.respond(OkResponse())
        }

        route.get("/orientation") {
            // Report the display rotation in the SAME vocabulary `set` accepts, so
            // a get→set round-trip works. (Previously get returned
            // natural|landscape|other, which `set` rejects.)
            val v =
                when (uiDevice.displayRotation) {
                    Surface.ROTATION_0 -> "natural"
                    Surface.ROTATION_90 -> "left"
                    Surface.ROTATION_180 -> "upsidedown"
                    Surface.ROTATION_270 -> "right"
                    else -> "natural"
                }
            call.respond(OrientationResp(v))
        }
        route.post("/orientation") {
            val r: OrientationReq = call.receive()
            when (r.value.lowercase()) {
                "natural", "n" -> uiDevice.setOrientationNatural()
                "left", "l" -> uiDevice.setOrientationLeft()
                "right", "r" -> uiDevice.setOrientationRight()
                "upsidedown", "upside_down", "u" -> {
                    // UiDevice has no setOrientationUpsideDown; freeze at 180°
                    // directly (the same UiAutomation call the left/right helpers
                    // use for 90°/270°), then wait for the rotation to settle.
                    instr.uiAutomation.setRotation(UiAutomation.ROTATION_FREEZE_180)
                    uiDevice.waitForIdle()
                }
                else -> throw BadRequest(
                    "bad_orientation",
                    "value must be natural|left|right|upsidedown, got '${r.value}'",
                )
            }
            call.respond(OkResponse())
        }

        route.get("/clipboard") {
            val cm = instr.targetContext.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            // Android 10+ (API 29) denies clipboard reads to apps that are neither
            // the foreground app nor the default IME. The instrumentation server is
            // neither, so a plain getPrimaryClip() returns null regardless of the
            // real contents. Briefly adopt the shell UID's permission identity (shell
            // holds READ_CLIPBOARD_IN_BACKGROUND) so the read returns the actual clip;
            // after this a null genuinely means "empty". (Adopt affects the whole
            // instrumentation process, but clipboard reads are rare and quick.)
            val automation = instr.uiAutomation
            val adopt = Build.VERSION.SDK_INT >= 29
            if (adopt) automation.adoptShellPermissionIdentity()
            val text =
                try {
                    cm.primaryClip
                        ?.getItemAt(0)
                        ?.coerceToText(instr.targetContext)
                        ?.toString()
                } finally {
                    if (adopt) automation.dropShellPermissionIdentity()
                }
            call.respond(ClipResp(text))
        }
        route.post("/clipboard") {
            val r: ClipReq = call.receive()
            val cm = instr.targetContext.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            cm.setPrimaryClip(ClipData.newPlainText("shadowdroid", r.value))
            call.respond(OkResponse())
        }

        route.post("/notifications/open") {
            uiDevice.openNotification()
            call.respond(OkResponse())
        }
        route.post("/quick_settings/open") {
            uiDevice.openQuickSettings()
            call.respond(OkResponse())
        }

        route.post("/url/open") {
            val r: UrlReq = call.receive()
            val intent = Intent(Intent.ACTION_VIEW, Uri.parse(r.url))
            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            instr.targetContext.startActivity(intent)
            call.respond(OkResponse())
        }

        route.post("/shell") {
            val r: ShellReq = call.receive()
            val (output, exitCode) =
                try {
                    runShell(instr, uiDevice, r.cmd, r.timeout_ms)
                } catch (t: ShellTimeout) {
                    throw BadRequest("shell_timeout", t.message ?: "shell command timed out")
                } catch (t: Throwable) {
                    throw BadRequest("shell_failed", t.message ?: "shell exec threw")
                }
            call.respond(ShellResp(input = r.cmd, output = output, exit_code = exitCode))
        }
    }
}

/** Signals that a shell command outlived its timeout budget. */
private class ShellTimeout(
    message: String,
) : RuntimeException(message)

private const val SHELL_RC_MARKER = "__SD_RC__"

/**
 * Run a device shell command and return (output, exit_code).
 *
 * On Android 12+ (API 31) this feeds a real `sh` script to
 * `UiAutomation.executeShellCommandRwe` over stdin, so the command runs as the
 * shell uid (like `adb shell`) AND gets full shell semantics (pipes, `;`,
 * `$()`, redirects), stderr (folded into stdout via `exec 2>&1`), and an exit
 * code — none of which the plain `executeShellCommand` API can provide. Older
 * devices, or any failure of the Rwe path, fall back to the legacy stdout-only
 * executor (exit_code null).
 */
private fun runShell(
    instr: Instrumentation,
    uiDevice: UiDevice,
    cmd: String,
    timeoutMs: Int,
): Pair<String, Int?> {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        try {
            return runShellViaSh(instr, cmd, timeoutMs)
        } catch (t: ShellTimeout) {
            throw t
        } catch (_: Throwable) {
            // executeShellCommandRwe unavailable on this build — degrade below.
        }
    }
    return uiDevice.executeShellCommand(cmd) to null
}

private fun runShellViaSh(
    instr: Instrumentation,
    cmd: String,
    timeoutMs: Int,
): Pair<String, Int?> {
    // [stdout(read), stdin(write), stderr(read)]
    val fds = instr.uiAutomation.executeShellCommandRwe("sh")
    val stdoutFd = fds[0]
    val stdinFd = fds[1]
    val stderrFd = fds[2]
    // Capture the command's own exit status before anything else runs, then
    // emit it behind a marker on its own line. `exec 2>&1` folds stderr into the
    // single stdout stream we drain (so there's no second pipe to deadlock on).
    val script =
        buildString {
            append("exec 2>&1\n")
            append(cmd)
            append("\n__sd_rc=\$?\n")
            append("echo \"\"\n")
            append("echo \"$SHELL_RC_MARKER\${__sd_rc}__\"\n")
        }

    val out = arrayOfNulls<ByteArray>(1)
    val failure = arrayOfNulls<Throwable>(1)
    val worker =
        Thread {
            try {
                ParcelFileDescriptor.AutoCloseOutputStream(stdinFd).use {
                    it.write(script.toByteArray())
                    it.flush()
                }
                out[0] = ParcelFileDescriptor.AutoCloseInputStream(stdoutFd).use { it.readBytes() }
                // stderr was redirected into stdout; just drain + close fd2.
                ParcelFileDescriptor.AutoCloseInputStream(stderrFd).use { it.readBytes() }
            } catch (t: Throwable) {
                failure[0] = t
            }
        }
    worker.start()
    worker.join(if (timeoutMs > 0) timeoutMs.toLong() else 0L)
    if (worker.isAlive) {
        // Unblock the pending read by closing the fds, then report the timeout.
        runCatching { stdoutFd.close() }
        runCatching { stdinFd.close() }
        runCatching { stderrFd.close() }
        worker.join(500)
        throw ShellTimeout("command exceeded ${timeoutMs}ms")
    }
    failure[0]?.let { throw it }
    return parseShellOutput(out[0]?.toString(Charsets.UTF_8) ?: "")
}

/** Split the trailing `\n__SD_RC__<code>__` marker off the captured output. */
private fun parseShellOutput(raw: String): Pair<String, Int?> {
    val marker = "\n$SHELL_RC_MARKER"
    val idx = raw.lastIndexOf(marker)
    if (idx < 0) return raw to null
    val code = raw.substring(idx + marker.length).trim().removeSuffix("__").toIntOrNull()
    return raw.substring(0, idx) to code
}

@Serializable
private data class OrientationReq(
    val value: String,
)

@Serializable
private data class OrientationResp(
    val value: String,
)

@Serializable
private data class ClipReq(
    val value: String,
)

@Serializable
private data class ClipResp(
    val value: String?,
)

@Serializable
private data class UrlReq(
    val url: String,
)

@Serializable
private data class ShellReq(
    val cmd: String,
    val timeout_ms: Int = 30000,
)

@Serializable
private data class ShellResp(
    val input: String,
    val output: String,
    val exit_code: Int? = null,
)
