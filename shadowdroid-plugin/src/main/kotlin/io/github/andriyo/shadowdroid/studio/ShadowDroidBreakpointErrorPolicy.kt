package io.github.andriyo.shadowdroid.studio

import com.intellij.xdebugger.BreakpointErrorData
import com.intellij.xdebugger.XBreakpointBehaviorPolicy
import com.intellij.xdebugger.XBreakpointBehaviorPolicy.BreakpointErrorAction
import com.intellij.xdebugger.XDebugSession
import com.intellij.xdebugger.breakpoints.XBreakpoint

/**
 * Answers the platform's "breakpoint expression failed to evaluate — what
 * now?" question for bridge-managed expressions, instead of the modal dialog
 * that would otherwise freeze the debugger until a human clicks it.
 */
internal class ShadowDroidBreakpointErrorPolicy : XBreakpointBehaviorPolicy {
    override fun chooseBreakpointErrorAction(
        session: XDebugSession,
        breakpoint: XBreakpoint<*>,
        error: BreakpointErrorData,
    ): BreakpointErrorAction = BreakpointExpressionGuard.onEvaluationError(session, breakpoint, error)
}
