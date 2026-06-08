package io.github.andriyo.shadowdroid.studio

import com.intellij.debugger.engine.DebuggerManagerThreadImpl
import com.intellij.debugger.engine.JavaDebugProcess
import com.intellij.debugger.engine.events.DebuggerCommandImpl
import com.intellij.openapi.application.ApplicationManager
import com.intellij.xdebugger.XDebugSession
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

    @JvmStatic
    @Throws(Exception::class)
    fun <T> onIdeaThread(supplier: ThrowingSupplier<T>): T {
        val app = ApplicationManager.getApplication()
        if (app.isDispatchThread) return supplier.get()
        val value = AtomicReference<T>()
        val error = AtomicReference<Exception>()
        app.invokeAndWait {
            try {
                value.set(supplier.get())
            } catch (e: Exception) {
                error.set(e)
            }
        }
        error.get()?.let { throw it }
        return value.get()
    }

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
            throw IllegalStateException("debugger manager did not answer within ${boundedTimeoutMs}ms")
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
