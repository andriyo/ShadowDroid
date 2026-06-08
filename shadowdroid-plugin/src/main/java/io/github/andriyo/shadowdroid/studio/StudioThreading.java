package io.github.andriyo.shadowdroid.studio;

import com.intellij.debugger.engine.DebuggerManagerThreadImpl;
import com.intellij.debugger.engine.JavaDebugProcess;
import com.intellij.debugger.engine.events.DebuggerCommandImpl;
import com.intellij.openapi.application.Application;
import com.intellij.openapi.application.ApplicationManager;
import com.intellij.xdebugger.XDebugSession;

import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicReference;

final class StudioThreading {
    private static final ExecutorService DEBUGGER_REQUESTS = Executors.newCachedThreadPool(runnable -> {
        Thread thread = new Thread(runnable, "ShadowDroid debugger request");
        thread.setDaemon(true);
        return thread;
    });

    private StudioThreading() {
    }

    static <T> T onIdeaThread(ThrowingSupplier<T> supplier) throws Exception {
        Application app = ApplicationManager.getApplication();
        if (app.isDispatchThread()) return supplier.get();
        AtomicReference<T> value = new AtomicReference<>();
        AtomicReference<Exception> error = new AtomicReference<>();
        app.invokeAndWait(() -> {
            try {
                value.set(supplier.get());
            } catch (Exception e) {
                error.set(e);
            }
        });
        if (error.get() != null) throw error.get();
        return value.get();
    }

    static <T> T onDebuggerThread(XDebugSession session, ThrowingSupplier<T> supplier) throws Exception {
        return onDebuggerThread(session, BridgeProtocol.DEFAULT_DEBUGGER_TIMEOUT_MS, supplier);
    }

    static <T> T onDebuggerThread(XDebugSession session, int timeoutMs, ThrowingSupplier<T> supplier) throws Exception {
        if (DebuggerManagerThreadImpl.isManagerThread()) return supplier.get();
        if (!(session.getDebugProcess() instanceof JavaDebugProcess javaProcess)) return supplier.get();

        Future<T> future = DEBUGGER_REQUESTS.submit(() -> {
            DebuggerManagerThreadImpl managerThread = javaProcess.getDebuggerSession().getProcess().getManagerThread();
            AtomicReference<T> value = new AtomicReference<>();
            AtomicReference<Throwable> error = new AtomicReference<>();
            managerThread.invokeAndWait(new DebuggerCommandImpl() {
                @Override
                protected void action() {
                    try {
                        value.set(supplier.get());
                    } catch (Throwable t) {
                        error.set(t);
                    }
                }
            });
            if (error.get() instanceof Exception e) throw e;
            if (error.get() instanceof Error e) throw e;
            if (error.get() != null) throw new RuntimeException(error.get());
            return value.get();
        });
        try {
            return future.get(Math.max(50, timeoutMs), TimeUnit.MILLISECONDS);
        } catch (TimeoutException e) {
            future.cancel(true);
            throw new IllegalStateException("debugger manager did not answer within " + Math.max(50, timeoutMs) + "ms");
        } catch (ExecutionException e) {
            Throwable cause = e.getCause();
            if (cause instanceof Exception exception) throw exception;
            if (cause instanceof Error error) throw error;
            throw new RuntimeException(cause);
        }
    }
}
