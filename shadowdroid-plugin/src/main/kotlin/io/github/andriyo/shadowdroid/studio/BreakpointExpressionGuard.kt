package io.github.andriyo.shadowdroid.studio

import com.intellij.debugger.JavaDebuggerBundle
import com.intellij.openapi.diagnostic.Logger
import com.intellij.openapi.project.Project
import com.intellij.xdebugger.BreakpointErrorData
import com.intellij.xdebugger.XBreakpointBehaviorPolicy.BreakpointErrorAction
import com.intellij.xdebugger.XDebugSession
import com.intellij.xdebugger.breakpoints.SuspendPolicy
import com.intellij.xdebugger.breakpoints.XBreakpoint
import java.awt.Toolkit
import java.awt.AWTEvent
import java.awt.Window
import java.awt.event.WindowEvent
import java.util.Collections
import java.util.WeakHashMap
import java.util.concurrent.atomic.AtomicBoolean
import javax.swing.JDialog

/**
 * Keeps breakpoint expressions set through the bridge from blocking the IDE.
 *
 * When a condition an agent set through the bridge fails to evaluate at hit
 * time, the platform would normally show a modal "Breakpoint Condition Error"
 * dialog on the EDT and park the debugger manager thread on it — the debuggee
 * stays frozen and every bridge request times out until a human answers.
 * [onEvaluationError] (via the breakpointBehaviorPolicy extension) answers
 * without any dialog for expressions the bridge owns: suspending breakpoints
 * pause, non-suspending logpoints resume, and both record the failure so
 * `debug breakpoints`/`status` can explain what happened.
 *
 * Expressions the bridge never touched keep the stock IDE dialog; the guard
 * only records them (and tracks the open dialog) so bridge timeouts can point
 * at the real blocker.
 */
internal object BreakpointExpressionGuard {
    const val KIND_CONDITION = "condition"
    const val KIND_LOG_EXPRESSION = "log_expression"

    private const val ERROR_HISTORY = 32
    private const val STATUS_ERROR_LIMIT = 8

    private val LOG = Logger.getInstance(BreakpointExpressionGuard::class.java)

    // Weak keys: entries die with the breakpoint. Values are the expression
    // texts the bridge applied; a breakpoint is only "managed" while its
    // current expression still matches, so a user edit in the IDE hands the
    // stock dialog behavior back.
    private val managed: MutableMap<XBreakpoint<*>, MutableMap<String, String>> =
        Collections.synchronizedMap(WeakHashMap())

    private val errors = EvaluationErrorLog(ERROR_HISTORY)

    private val modalWatcherInstalled = AtomicBoolean(false)
    private val openDialogs: MutableMap<Window, String> = Collections.synchronizedMap(WeakHashMap())

    // Resolved lazily from the same bundle the platform uses, so matching
    // survives localization and title rewording within a release.
    private val conditionDialogTitle: String? by lazy {
        runCatching { JavaDebuggerBundle.message("title.error.evaluating.breakpoint.condition") }.getOrNull()
    }
    private val actionDialogTitle: String? by lazy {
        runCatching { JavaDebuggerBundle.message("title.error.evaluating.breakpoint.action") }.getOrNull()
    }

    fun markManaged(breakpoint: XBreakpoint<*>, kind: String, expression: String?) {
        if (expression.isNullOrBlank()) {
            clearManaged(breakpoint, kind)
            return
        }
        synchronized(managed) {
            managed.getOrPut(breakpoint) { mutableMapOf() }[kind] = expression
        }
    }

    fun clearManaged(breakpoint: XBreakpoint<*>, kind: String) {
        synchronized(managed) {
            val kinds = managed[breakpoint] ?: return
            kinds.remove(kind)
            if (kinds.isEmpty()) managed.remove(breakpoint)
        }
    }

    fun forget(breakpoint: XBreakpoint<*>) {
        managed.remove(breakpoint)
    }

    /**
     * Policy callback: decide what a failed breakpoint expression does to the
     * session. Runs on the debugger manager thread — must never touch the EDT.
     */
    fun onEvaluationError(
        session: XDebugSession,
        breakpoint: XBreakpoint<*>,
        data: BreakpointErrorData,
    ): BreakpointErrorAction =
        try {
            val kind = errorKind(data.title)
            val isManaged = managedKindsFor(
                snapshotFor(breakpoint),
                currentCondition(breakpoint),
                currentLogExpression(breakpoint),
            ).let { kinds ->
                when (kind) {
                    KIND_CONDITION -> KIND_CONDITION in kinds
                    KIND_LOG_EXPRESSION -> KIND_LOG_EXPRESSION in kinds
                    else -> kinds.isNotEmpty()
                }
            }
            val nonSuspending = breakpoint.suspendPolicy == SuspendPolicy.NONE
            val action = errorActionFor(
                managed = isManaged,
                nonSuspending = nonSuspending,
            )
            recordError(session, breakpoint, data, action)
            if (isManaged && nonSuspending) {
                runCatching {
                    BreakpointBridge.recordLogpointEvaluationError(
                        project = session.project,
                        breakpoint = breakpoint,
                        session = ShadowDroidDebuggerBridge.logpointSessionSnapshotFor(session),
                        message = data.message,
                        kind = kind,
                        title = data.title,
                        action = recordedAction(action),
                    )
                }.onFailure { LOG.debug("unable to record structured logpoint evaluation error", it) }
            }
            action
        } catch (t: Throwable) {
            LOG.warn("breakpoint error policy failed; falling back to the IDE dialog", t)
            BreakpointErrorAction.UNHANDLED
        }

    /**
     * Pure managed-state matching: which bridge-set expression kinds are still
     * in force on a breakpoint whose current texts are [condition]/[logExpression].
     */
    fun managedKindsFor(
        snapshot: Map<String, String>,
        condition: String?,
        logExpression: String?,
    ): Set<String> {
        val kinds = mutableSetOf<String>()
        if (condition != null && snapshot[KIND_CONDITION] == condition) kinds += KIND_CONDITION
        if (logExpression != null && snapshot[KIND_LOG_EXPRESSION] == logExpression) kinds += KIND_LOG_EXPRESSION
        return kinds
    }

    /** Pure policy selector kept separate so non-suspending behavior is unit-testable. */
    fun errorActionFor(managed: Boolean, nonSuspending: Boolean): BreakpointErrorAction = when {
        !managed -> BreakpointErrorAction.UNHANDLED
        nonSuspending -> BreakpointErrorAction.RESUME
        else -> BreakpointErrorAction.PAUSE
    }

    fun recentErrors(): List<Map<String, Any?>> = errors.recent(STATUS_ERROR_LIMIT)

    fun lastErrorFor(breakpointId: String): Map<String, Any?>? = errors.lastFor(breakpointId)

    /**
     * Android Studio can render a failed log expression through the ordinary
     * log-message callback without invoking [onEvaluationError]. Keep that
     * high-confidence failure visible in the same bounded status history.
     */
    fun recordRenderedLogpointError(
        project: Project,
        session: LogpointSessionSnapshot,
        breakpointId: String,
        message: String,
        error: LogpointEvaluationError,
    ) {
        errors.add(
            BridgeProtocol.map(
                "at", BridgeProtocol.nowMs(),
                "breakpoint_id", breakpointId,
                "kind", error.kind,
                "message", message,
                "action", error.action,
                "session_name", session.name,
                "project", project.basePath ?: project.name,
            ),
        )
    }

    /** A fresh expression supersedes the failure history of the old one. */
    fun clearErrorsFor(breakpointId: String) = errors.clearFor(breakpointId)

    /**
     * Track the two evaluation-error dialogs so bridge timeouts can say what
     * is actually blocking the debugger instead of a bare "did not answer".
     */
    fun installModalWatcher() {
        if (!modalWatcherInstalled.compareAndSet(false, true)) return
        try {
            Toolkit.getDefaultToolkit().addAWTEventListener({ event ->
                val windowEvent = event as? WindowEvent ?: return@addAWTEventListener
                val dialog = windowEvent.window as? JDialog ?: return@addAWTEventListener
                when (windowEvent.id) {
                    WindowEvent.WINDOW_OPENED -> {
                        val title = dialog.title ?: return@addAWTEventListener
                        if (title == conditionDialogTitle || title == actionDialogTitle) {
                            openDialogs[dialog] = title
                        }
                    }
                    WindowEvent.WINDOW_CLOSED, WindowEvent.WINDOW_CLOSING -> openDialogs.remove(dialog)
                }
            }, AWTEvent.WINDOW_EVENT_MASK)
        } catch (t: Throwable) {
            LOG.warn("unable to watch for breakpoint error dialogs", t)
        }
    }

    fun blockedDialogs(): List<String> = synchronized(openDialogs) { openDialogs.values.toList() }

    private fun recordError(
        session: XDebugSession,
        breakpoint: XBreakpoint<*>,
        data: BreakpointErrorData,
        action: BreakpointErrorAction,
    ) {
        val breakpointId = try {
            // No ReadAction here: onEvaluationError runs on the debugger manager
            // thread and its return value gates the pause/resume decision. A
            // blocking read lock on that thread could park behind an EDT write
            // action (or deadlock against a manager-thread invokeAndWait) and
            // re-introduce the very freeze this feature removes. breakpointId is
            // file-url + line + type; BreakpointBridge.recordHit already computes
            // it lock-free on the hit path, so no read lock is required.
            BreakpointBridge.breakpointIdFor(session.project, breakpoint)
        } catch (t: Throwable) {
            LOG.debug("unable to compute breakpoint id for evaluation error", t)
            null
        }
        errors.add(
            BridgeProtocol.map(
                "at", BridgeProtocol.nowMs(),
                "breakpoint_id", breakpointId,
                "kind", errorKind(data.title),
                "message", data.message,
                "action", recordedAction(action),
                "session_name", session.sessionName,
                "project", session.project.basePath ?: session.project.name,
            ),
        )
    }

    private fun recordedAction(action: BreakpointErrorAction): String = when (action) {
        BreakpointErrorAction.PAUSE -> "paused_without_dialog"
        BreakpointErrorAction.RESUME -> "resumed_without_dialog"
        BreakpointErrorAction.UNHANDLED -> "left_to_ide_dialog"
    }

    private fun errorKind(title: String?): String = when (title) {
        null -> "unknown"
        conditionDialogTitle -> KIND_CONDITION
        actionDialogTitle -> KIND_LOG_EXPRESSION
        else -> "unknown"
    }

    private fun snapshotFor(breakpoint: XBreakpoint<*>): Map<String, String> =
        synchronized(managed) { managed[breakpoint]?.toMap() ?: emptyMap() }

    private fun currentCondition(breakpoint: XBreakpoint<*>): String? =
        runCatching { breakpoint.conditionExpression?.expression }.getOrNull()

    private fun currentLogExpression(breakpoint: XBreakpoint<*>): String? =
        runCatching { breakpoint.logExpressionObject?.expression }.getOrNull()
}
