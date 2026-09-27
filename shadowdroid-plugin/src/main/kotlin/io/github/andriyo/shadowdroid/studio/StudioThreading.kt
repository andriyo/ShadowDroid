package io.github.andriyo.shadowdroid.studio

import com.intellij.debugger.engine.DebuggerManagerThreadImpl
import com.intellij.debugger.engine.JavaDebugProcess
import com.intellij.debugger.engine.events.DebuggerCommandImpl
import com.intellij.openapi.application.ApplicationManager
import com.intellij.xdebugger.XDebugSession
import java.awt.Dialog
import java.awt.Window
import java.util.concurrent.ExecutionException
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import java.util.concurrent.atomic.AtomicReference
import kotlin.math.max

internal object StudioThreading {
    private val debuggerRequests = Executors.newCachedThreadPool { runnable ->
        Thread(runnable, "ShadowDroid debugger request").apply { isDaemon = true }
    }

    /** How long a bridge request waits for the UI thread to start it. */
    private const val IDEA_THREAD_START_TIMEOUT_MS = 5_000L

    /** How long started UI-thread work is awaited before its outcome is unknown. */
    private const val IDEA_THREAD_RUN_TIMEOUT_MS = 30_000L

    /**
     * Run [supplier] on the UI thread. A modal dialog blocks the UI queue, and
     * `invokeAndWait` used to wait for it indefinitely, then run the request —
     * a step or resume — after the CLI had timed out and possibly retried. Now
     * the request is cancelled if it has not started within
     * [IDEA_THREAD_START_TIMEOUT_MS] and fails with [StudioUiBusyException].
     */
    @JvmStatic
    @Throws(Exception::class)
    fun <T> onIdeaThread(supplier: ThrowingSupplier<T>): T {
        val app = ApplicationManager.getApplication()
        if (app.isDispatchThread) return supplier.get()
        val handoff = CancellableHandoff { supplier.get() }
        app.invokeLater { handoff.run() }
        return handoff.await(IDEA_THREAD_START_TIMEOUT_MS, IDEA_THREAD_RUN_TIMEOUT_MS) {
            val dialogs = (modalDialogs() + BreakpointExpressionGuard.blockedDialogs()).distinct()
            if (dialogs.isEmpty()) "" else " (${dialogs.joinToString()})"
        }
    }

    /**
     * Titles of the modal dialogs open in the IDE. While one shows, requests
     * that need the UI thread cannot start. Safe to read from any thread.
     */
    @JvmStatic
    fun modalDialogs(): List<String> =
        Window.getWindows()
            .filterIsInstance<Dialog>()
            .filter { it.isModal && it.isShowing }
            .map { dialog -> dialog.title?.takeIf { it.isNotBlank() } ?: dialog.javaClass.simpleName }

    @JvmStatic
    @Throws(Exception::class)
    fun <T> onDebuggerThread(session: XDebugSession, supplier: ThrowingSupplier<T>): T =
        onDebuggerThread(session, BridgeProtocol.DEFAULT_DEBUGGER_TIMEOUT_MS, supplier)

    @JvmStatic
    @Throws(Exception::class)
    fun <T> onDebuggerThread(session: XDebugSession, timeoutMs: Int, supplier: ThrowingSupplier<T>): T {
        if (DebuggerManagerThreadImpl.isManagerThread()) return supplier.get()
        val javaProcess = session.debugProcess as? JavaDebugProcess ?: return supplier.get()

        val future = debuggerRequests.submit<T> {
            val managerThread = javaProcess.debuggerSession.process.managerThread
            val value = AtomicReference<T>()
            val error = AtomicReference<Throwable>()
            managerThread.invokeAndWait(object : DebuggerCommandImpl() {
                override fun action() {
                    try {
                        value.set(supplier.get())
                    } catch (t: Throwable) {
                        error.set(t)
                    }
                }
            })
            when (val throwable = error.get()) {
                null -> value.get()
                is Exception -> throw throwable
                is Error -> throw throwable
                else -> throw RuntimeException(throwable)
            }
        }
        val boundedTimeoutMs = max(50, timeoutMs)
        try {
            return future.get(boundedTimeoutMs.toLong(), TimeUnit.MILLISECONDS)
        } catch (e: TimeoutException) {
            future.cancel(true)
            // A blocking evaluation-error dialog parks the debugger manager
            // thread; name it so the caller isn't left with a bare timeout.
            val dialogs = BreakpointExpressionGuard.blockedDialogs()
            val hint = if (dialogs.isEmpty()) {
                ""
            } else {
                "; Android Studio is showing a blocking dialog (${dialogs.joinToString()}) — " +
                    "dismiss it in the IDE or clear the offending breakpoint expression"
            }
            throw IllegalStateException("debugger manager did not answer within ${boundedTimeoutMs}ms$hint")
        } catch (e: ExecutionException) {
            val cause = e.cause
            when (cause) {
                is Exception -> throw cause
                is Error -> throw cause
                else -> throw RuntimeException(cause)
            }
        }
    }
}
