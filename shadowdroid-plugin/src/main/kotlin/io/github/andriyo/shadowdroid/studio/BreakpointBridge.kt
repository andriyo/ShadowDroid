package io.github.andriyo.shadowdroid.studio

import com.intellij.debugger.ui.breakpoints.JavaExceptionBreakpointType
import com.intellij.debugger.ui.breakpoints.JavaFieldBreakpointType
import com.intellij.debugger.ui.breakpoints.JavaWildcardMethodBreakpointType
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.diagnostic.Logger
import com.intellij.openapi.project.Project
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.xdebugger.XDebuggerManager
import com.intellij.xdebugger.XSourcePosition
import com.intellij.xdebugger.breakpoints.SuspendPolicy
import com.intellij.xdebugger.breakpoints.XBreakpoint
import com.intellij.xdebugger.breakpoints.XBreakpointProperties
import com.intellij.xdebugger.breakpoints.XBreakpointType
import com.intellij.xdebugger.breakpoints.XLineBreakpoint
import com.intellij.xdebugger.breakpoints.XLineBreakpointType
import org.jetbrains.java.debugger.breakpoints.properties.JavaBreakpointProperties
import org.jetbrains.java.debugger.breakpoints.properties.JavaExceptionBreakpointProperties
import org.jetbrains.java.debugger.breakpoints.properties.JavaFieldBreakpointProperties
import org.jetbrains.java.debugger.breakpoints.properties.JavaLineBreakpointProperties
import org.jetbrains.java.debugger.breakpoints.properties.JavaMethodBreakpointProperties
import java.io.File
import java.net.HttpURLConnection
import java.nio.charset.StandardCharsets
import java.util.Base64
import java.util.Collections
import java.util.WeakHashMap
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Semaphore

internal object BreakpointBridge {
    // One physical hit can surface through more than one listener (pause
    // position match, log-message callback — observed duplicated with
    // concurrent sessions); collapse records landing within this window.
    // hit_count is documented as observed/approximate, so a burst of real
    // hits inside the window under-counts rather than double-counting.
    private const val HIT_DEDUPE_WINDOW_MS = 250L
    internal const val MAX_LOGPOINT_FOLLOWERS = 8

    private const val JAVA_LINE_TYPE_ID = "java-line"
    private const val KOTLIN_LINE_TYPE_ID = "kotlin-line"

    private val LOG = Logger.getInstance(BreakpointBridge::class.java)

    // Plain line breakpoints only; field watchpoints are also XLineBreakpoints
    // at a file/line and must never be reused as a line breakpoint.
    private val LINE_TYPE_IDS = setOf(JAVA_LINE_TYPE_ID, KOTLIN_LINE_TYPE_ID)

    private val breakpointHits = ConcurrentHashMap<String, Long>()
    private val breakpointLastHit = ConcurrentHashMap<String, Long>()
    private val logpointEventLog = LogpointEventLog()
    private val logpointFollowers = Semaphore(MAX_LOGPOINT_FOLLOWERS)
    private val temporaryHitClaims = FirstHitClaimSet<XBreakpoint<*>>()

    // Ownership is deliberately process-local and weak. After an IDE restart a
    // persisted breakpoint becomes an unowned/manual logpoint, which is safer
    // than guessing and deleting it during a later owner cleanup.
    private val ownedLogpoints: MutableMap<XBreakpoint<*>, OwnedLogpoint> =
        Collections.synchronizedMap(WeakHashMap())
    private val bridgeMutations: MutableSet<XBreakpoint<*>> =
        Collections.synchronizedSet(Collections.newSetFromMap(WeakHashMap()))

    @JvmStatic
    fun recordHit(project: Project, breakpoint: XBreakpoint<*>) {
        val id = breakpointId(project, breakpoint)
        val now = System.currentTimeMillis()
        val last = breakpointLastHit.put(id, now)
        if (last != null && now - last < HIT_DEDUPE_WINDOW_MS) return
        breakpointHits.merge(id, 1L, Long::plus)
    }

    /**
     * Record Android Studio's already-rendered composite logpoint message.
     * This runs synchronously on the breakpoint action path: no frame lookup,
     * debugger-thread hop, IDE read lock, or I/O is allowed here.
     */
    @JvmStatic
    fun recordLogpointMessage(
        project: Project,
        breakpoint: XBreakpoint<*>,
        session: LogpointSessionSnapshot,
        message: String,
    ) {
        if (!isLogpoint(breakpoint)) return
        if (!claimLogpointEventCapture(breakpoint)) return
        val id = breakpointId(project, breakpoint)
        val now = System.currentTimeMillis()
        breakpointLastHit[id] = now
        breakpointHits.merge(id, 1L, Long::plus)
        val evaluationError = renderedLogpointEvaluationError(
            message = message,
            expression = breakpoint.logExpressionObject?.expression,
        )
        if (evaluationError != null) {
            BreakpointExpressionGuard.recordRenderedLogpointError(
                project = project,
                session = session,
                breakpointId = id,
                message = message,
                error = evaluationError,
            )
        }
        appendLogpointEvent(project, breakpoint, session, message, evaluationError, breakpointId = id)
        scheduleOwnedTemporaryRemoval(project, breakpoint)
    }

    @JvmStatic
    fun recordLogpointEvaluationError(
        project: Project,
        breakpoint: XBreakpoint<*>,
        session: LogpointSessionSnapshot,
        message: String,
        kind: String,
        title: String?,
        action: String,
    ) {
        if (!isLogpoint(breakpoint)) return
        if (!claimLogpointEventCapture(breakpoint)) return
        val id = breakpointId(project, breakpoint)
        val now = System.currentTimeMillis()
        breakpointLastHit[id] = now
        breakpointHits.merge(id, 1L, Long::plus)
        appendLogpointEvent(
            project,
            breakpoint,
            session,
            message,
            LogpointEvaluationError(kind = kind, title = title, action = action),
            id,
        )
        scheduleOwnedTemporaryRemoval(project, breakpoint)
    }

    /**
     * The platform does not remove a temporary breakpoint whose suspend policy
     * is NONE after its log action. Preserve one-shot semantics ourselves, but
     * only for a still-owned bridge logpoint. The event is buffered first, and
     * the IDE mutation is posted asynchronously so the debugger callback stays
     * bounded and lock-free.
     */
    private fun scheduleOwnedTemporaryRemoval(project: Project, breakpoint: XBreakpoint<*>) {
        val lineBreakpoint = breakpoint as? XLineBreakpoint<*> ?: return
        if (!lineBreakpoint.isTemporary || activeOwnedLogpoint(breakpoint) == null) return
        ApplicationManager.getApplication().invokeLater {
            if (project.isDisposed) return@invokeLater
            if (!lineBreakpoint.isTemporary || activeOwnedLogpoint(breakpoint) == null) return@invokeLater
            runCatching {
                XDebuggerManager.getInstance(project).breakpointManager.removeBreakpoint(breakpoint)
            }.onFailure { LOG.debug("unable to remove temporary managed logpoint", it) }
        }
    }

    /**
     * Atomically claim the sole event emitted by a managed temporary logpoint.
     * The claim happens before counters, error records, or the event buffer are
     * touched, closing the callback race before asynchronous IDE removal.
     */
    private fun claimLogpointEventCapture(breakpoint: XBreakpoint<*>): Boolean {
        val lineBreakpoint = breakpoint as? XLineBreakpoint<*> ?: return true
        if (!lineBreakpoint.isTemporary || activeOwnedLogpoint(breakpoint) == null) return true
        return temporaryHitClaims.claim(breakpoint)
    }

    private fun appendLogpointEvent(
        project: Project,
        breakpoint: XBreakpoint<*>,
        session: LogpointSessionSnapshot,
        message: String,
        evaluationError: LogpointEvaluationError?,
        breakpointId: String,
    ) {
        // Revalidate ownership on the hit path too. The breakpoint-change
        // notification is normally synchronous, but a delayed/missed IDE
        // notification must not label an externally edited logpoint as owned.
        val owned = activeOwnedLogpoint(breakpoint)
        val lineBreakpoint = breakpoint as? XLineBreakpoint<*>
        val url = lineBreakpoint?.fileUrl
        logpointEventLog.append(
            LogpointEventCandidate(
                breakpointId = breakpointId,
                owner = owned?.owner,
                projectName = project.name,
                projectPath = project.basePath,
                session = session,
                file = owned?.file ?: url?.removePrefix("file://"),
                url = url,
                line = lineBreakpoint?.let { it.line + 1 },
                type = breakpoint.type.id,
                condition = breakpoint.conditionExpression?.expression,
                logExpression = breakpoint.logExpressionObject?.expression,
                logMessage = breakpoint.isLogMessage,
                logStack = breakpoint.isLogStack,
                message = message,
                evaluationError = evaluationError,
                maxMessageChars = owned?.maxMessageChars ?: DEFAULT_LOGPOINT_MAX_MESSAGE_CHARS,
                maxEventsPerSecond = owned?.maxEventsPerSecond ?: DEFAULT_LOGPOINT_MAX_EVENTS_PER_SECOND,
            ),
        )
    }

    @JvmStatic
    fun forget(project: Project, breakpoint: XBreakpoint<*>) {
        BreakpointExpressionGuard.forget(breakpoint)
        ownedLogpoints.remove(breakpoint)
        bridgeMutations.remove(breakpoint)
        temporaryHitClaims.forget(breakpoint)
        val id = try {
            breakpointId(project, breakpoint)
        } catch (_: Throwable) {
            return
        }
        breakpointHits.remove(id)
        breakpointLastHit.remove(id)
        logpointEventLog.forgetBreakpoint(id)
    }

    /** Relinquish cleanup ownership when a user changes an owned logpoint in the IDE. */
    @JvmStatic
    fun breakpointChanged(breakpoint: XBreakpoint<*>) {
        if (bridgeMutations.contains(breakpoint)) return
        val owned = ownedLogpoint(breakpoint) ?: return
        val current = runCatching { logpointFingerprint(breakpoint) }.getOrNull()
        if (current != owned.fingerprint) {
            synchronized(ownedLogpoints) {
                if (ownedLogpoints[breakpoint] === owned) ownedLogpoints.remove(breakpoint)
            }
        }
    }

    /** Stable breakpoint id for other bridge components (e.g. error records). */
    @JvmStatic
    internal fun breakpointIdFor(project: Project, breakpoint: XBreakpoint<*>): String =
        breakpointId(project, breakpoint)

    @JvmStatic
    fun addLine(query: Map<String, String>, project: Project?): Response {
        val file = query[BridgeQuery.FILE]
        if (file.isNullOrBlank()) return BridgeProtocol.bad("missing file")
        val line = BridgeProtocol.intParam(query, BridgeQuery.LINE, -1, 1, Int.MAX_VALUE)
        if (line < 1) return BridgeProtocol.bad("missing or invalid line")
        val enabled = BridgeProtocol.booleanParam(query, BridgeQuery.ENABLED, true)
        val temporary = BridgeProtocol.booleanParam(query, BridgeQuery.TEMPORARY, false)
        val condition = query[BridgeQuery.CONDITION]
        val clearCondition = BridgeProtocol.booleanParam(query, BridgeQuery.CLEAR_CONDITION, false)
        val validate = BridgeProtocol.booleanParam(query, BridgeQuery.VALIDATE, true)
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            // Find-or-create only. A newly created breakpoint is left DISABLED
            // here so it cannot fire during validation; enabled/temporary and
            // the condition are all applied in one later hop, after validation
            // passes — so a rejected expression leaves no partially-configured
            // breakpoint (atomic, like update()).
            val prepared = StudioThreading.onIdeaThread {
                val virtualFile = LocalFileSystem.getInstance().refreshAndFindFileByIoFile(File(file))
                    ?: throw IllegalArgumentException("file not found in IDE VFS: $file")
                val chosen = lineBreakpointTypeFor(project, virtualFile, line - 1)
                val existing = findLineBreakpoint(project, virtualFile.url, line - 1)
                val target = existing ?: run {
                    @Suppress("UNCHECKED_CAST")
                    val lineType = chosen.type as XLineBreakpointType<XBreakpointProperties<*>>
                    val props = lineType.createBreakpointProperties(virtualFile, line - 1)
                    XDebuggerManager.getInstance(project).breakpointManager
                        .addLineBreakpoint(lineType, virtualFile.url, line - 1, props, temporary)
                        .also { it.setEnabled(false) }
                }
                PreparedLine(target, chosen.positionSupported, created = existing == null)
            }
            val newCondition = if (clearCondition) null else condition?.ifBlank { null }
            if (validate && newCondition != null) {
                val problems = ExpressionValidation.validate(
                    project,
                    prepared.breakpoint,
                    newCondition,
                    expectBoolean = true,
                )
                if (problems.isNotEmpty()) {
                    // Roll back a breakpoint we just created; leave a reused one
                    // untouched (its prior state, including any old condition).
                    if (prepared.created) {
                        StudioThreading.onIdeaThread {
                            XDebuggerManager.getInstance(project).breakpointManager
                                .removeBreakpoint(prepared.breakpoint)
                            null
                        }
                    }
                    return invalidExpression(
                        project,
                        prepared.breakpoint,
                        BreakpointExpressionGuard.KIND_CONDITION,
                        newCondition,
                        problems,
                        removed = prepared.created,
                    )
                }
            }
            StudioThreading.onIdeaThread {
                prepared.breakpoint.setEnabled(enabled)
                prepared.breakpoint.setTemporary(temporary)
                if (clearCondition || condition != null) {
                    applyCondition(project, prepared.breakpoint, newCondition)
                }
                null
            }
            BridgeProtocol.ok(
                "ok", true,
                "breakpoint", breakpointInfo(project, prepared.breakpoint),
                "warning", if (prepared.positionSupported) {
                    null
                } else {
                    "no line breakpoint type accepts this position (blank or comment line?); the breakpoint may never bind"
                },
            )
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @JvmStatic
    fun addLogpoint(query: Map<String, String>, project: Project?): Response {
        val file = query[BridgeQuery.FILE]
        if (file.isNullOrBlank()) return BridgeProtocol.bad("missing file")
        val line = BridgeProtocol.intParam(query, BridgeQuery.LINE, -1, 1, Int.MAX_VALUE)
        if (line < 1) return BridgeProtocol.bad("missing or invalid line")
        val owner = query[BridgeQuery.OWNER]?.trim()
        if (owner.isNullOrEmpty()) return BridgeProtocol.bad("missing owner")
        if (owner.length > 256) return BridgeProtocol.bad("owner is too long (maximum 256 characters)")
        val enabled = BridgeProtocol.booleanParam(query, BridgeQuery.ENABLED, true)
        val temporary = BridgeProtocol.booleanParam(query, BridgeQuery.TEMPORARY, false)
        val condition = query[BridgeQuery.CONDITION]?.ifBlank { null }
        val logExpression = query[BridgeQuery.LOG_EXPRESSION]?.ifBlank { null }
        val logMessage = BridgeProtocol.booleanParam(query, BridgeQuery.LOG_MESSAGE, false)
        val logStack = BridgeProtocol.booleanParam(query, BridgeQuery.LOG_STACK, false)
        if (logExpression == null && !logMessage && !logStack) {
            return BridgeProtocol.bad("logpoint requires log_expression, log_message=true, or log_stack=true")
        }
        val passCount = BridgeProtocol.intParam(query, BridgeQuery.PASS_COUNT, 0, 0, Int.MAX_VALUE)
        val validate = BridgeProtocol.booleanParam(query, BridgeQuery.VALIDATE, true)
        val maxMessageChars = BridgeProtocol.intParam(
            query,
            BridgeQuery.MAX_MESSAGE_CHARS,
            DEFAULT_LOGPOINT_MAX_MESSAGE_CHARS,
            256,
            MAX_LOGPOINT_MESSAGE_CHARS,
        )
        val maxEventsPerSecond = BridgeProtocol.intParam(
            query,
            BridgeQuery.MAX_EVENTS_PER_SECOND,
            DEFAULT_LOGPOINT_MAX_EVENTS_PER_SECOND,
            1,
            10_000,
        )
        if (project == null) return BridgeProtocol.bad("no project")

        var prepared: PreparedLine? = null
        var previous: BreakpointSnapshot? = null
        var rollbackDone = false
        return try {
            prepared = StudioThreading.onIdeaThread {
                val virtualFile = LocalFileSystem.getInstance().refreshAndFindFileByIoFile(File(file))
                    ?: throw IllegalArgumentException("file not found in IDE VFS: $file")
                val chosen = lineBreakpointTypeFor(project, virtualFile, line - 1)
                val existingAtLine = findLineBreakpoints(project, virtualFile.url, line - 1)
                if (existingAtLine.isNotEmpty()) {
                    // Check every plain line breakpoint at the position. Reusing
                    // only the first match could hide a second manual breakpoint
                    // that the bridge must never claim or later owner-clear.
                    val candidates = existingAtLine.map { it to activeOwnedLogpoint(it) }
                    val conflict = candidates.firstOrNull { (_, ownership) ->
                        ownership == null || ownership.owner != owner
                    }
                    if (conflict != null) {
                        val (existing, existingOwner) = conflict
                        throw LogpointConflictException(
                            if (existingOwner == null) {
                                "an unowned breakpoint already exists at $file:$line"
                            } else {
                                "a logpoint owned by '${existingOwner.owner}' already exists at $file:$line"
                            },
                            breakpointId(project, existing),
                            existingOwner?.owner,
                        )
                    }
                    val existing = candidates.first().first
                    PreparedLine(
                        breakpoint = existing,
                        positionSupported = chosen.positionSupported,
                        created = false,
                        preparedBreakpointId = breakpointId(project, existing),
                    )
                } else {
                    @Suppress("UNCHECKED_CAST")
                    val lineType = chosen.type as XLineBreakpointType<XBreakpointProperties<*>>
                    val props = lineType.createBreakpointProperties(virtualFile, line - 1)
                    val created = XDebuggerManager.getInstance(project).breakpointManager
                        .addLineBreakpoint(lineType, virtualFile.url, line - 1, props, temporary)
                    created.setEnabled(false)
                    PreparedLine(
                        breakpoint = created,
                        positionSupported = chosen.positionSupported,
                        created = true,
                        preparedBreakpointId = breakpointId(project, created),
                        preparedFingerprint = logpointFingerprint(created),
                    )
                }
            }

            val configured = checkNotNull(prepared)
            val target = configured.breakpoint
            if (validate && condition != null) {
                val problems = ExpressionValidation.validate(project, target, condition, expectBoolean = true)
                if (problems.isNotEmpty()) {
                    val removed = configured.created && rollbackPrepared(project, configured, null)
                    rollbackDone = configured.created
                    return invalidExpression(
                        project,
                        target,
                        BreakpointExpressionGuard.KIND_CONDITION,
                        condition,
                        problems,
                        removed = removed,
                    )
                }
            }
            if (validate && logExpression != null) {
                val problems = ExpressionValidation.validate(project, target, logExpression, expectBoolean = false)
                if (problems.isNotEmpty()) {
                    val removed = configured.created && rollbackPrepared(project, configured, null)
                    rollbackDone = configured.created
                    return invalidExpression(
                        project,
                        target,
                        BreakpointExpressionGuard.KIND_LOG_EXPRESSION,
                        logExpression,
                        problems,
                        removed = removed,
                    )
                }
            }

            if (!configured.created) {
                previous = StudioThreading.onIdeaThread {
                    val currentOwner = activeOwnedLogpoint(target)
                        ?: throw LogpointConflictException(
                            "the existing logpoint changed during validation and is no longer owned",
                            breakpointId(project, target),
                            null,
                        )
                    if (currentOwner.owner != owner) {
                        throw LogpointConflictException(
                            "the existing logpoint owner changed during validation",
                            breakpointId(project, target),
                            currentOwner.owner,
                        )
                    }
                    breakpointSnapshot(target)
                }
            }
            var mutationBegan = false
            try {
                StudioThreading.onIdeaThread {
                    if (configured.created) {
                        val registered = XDebuggerManager.getInstance(project).breakpointManager
                            .allBreakpoints.any { it === target }
                        val expected = checkNotNull(configured.preparedFingerprint)
                        if (!registered || logpointFingerprint(target) != expected) {
                            throw LogpointConflictException(
                                "the newly created logpoint changed or was removed during validation",
                                checkNotNull(configured.preparedBreakpointId),
                                activeOwnedLogpoint(target)?.owner,
                            )
                        }
                    } else {
                        val expected = checkNotNull(previous)
                        val currentOwner = activeOwnedLogpoint(target)
                        if (currentOwner != expected.ownership ||
                            logpointFingerprint(target) != expected.fingerprint
                        ) {
                            throw LogpointConflictException(
                                "the existing logpoint changed before configuration could be applied",
                                breakpointId(project, target),
                                currentOwner?.owner,
                            )
                        }
                    }
                    withBridgeMutation(target) {
                        mutationBegan = true
                        // Disable first and enable last. A reused logpoint may
                        // continue using its old valid configuration during
                        // validation, but can never fire half-configured here.
                        target.setEnabled(false)
                        target.setTemporary(temporary)
                        applyCondition(project, target, condition)
                        applyLogExpression(project, target, logExpression)
                        target.setLogMessage(logMessage)
                        target.setLogStack(logStack)
                        target.setSuspendPolicy(SuspendPolicy.NONE)
                        (target.properties as? JavaBreakpointProperties<*>)?.let { props ->
                            props.setCOUNT_FILTER_ENABLED(passCount > 0)
                            props.setCOUNT_FILTER(passCount)
                        }
                        // Install ownership before enabling. A logpoint at a hot
                        // line can fire immediately from setEnabled(true).
                        ownedLogpoints[target] = OwnedLogpoint(
                            owner = owner,
                            createdByBridge = true,
                            file = File(file).absolutePath,
                            maxMessageChars = maxMessageChars,
                            maxEventsPerSecond = maxEventsPerSecond,
                            fingerprint = logpointFingerprint(target),
                        )
                        target.setEnabled(enabled)
                        ownedLogpoints[target] = ownedLogpoints.getValue(target).copy(
                            fingerprint = logpointFingerprint(target),
                        )
                    }
                    null
                }
            } catch (t: Throwable) {
                // A failed recheck did not mutate a reused breakpoint, so
                // restoring the earlier snapshot would overwrite the IDE edit
                // that caused the conflict. A new breakpoint is removed only
                // if it is still registered and unchanged; once an IDE edit is
                // observed, that external state is preserved too.
                if (mutationBegan || configured.created) {
                    rollbackPrepared(
                        project,
                        configured,
                        previous,
                        forceCreatedRemoval = mutationBegan,
                    )
                }
                rollbackDone = true
                throw t
            }

            BridgeProtocol.ok(
                "ok", true,
                "created", configured.created,
                "breakpoint", breakpointInfo(project, target),
                "warning", if (configured.positionSupported) {
                    null
                } else {
                    "no line breakpoint type accepts this position (blank or comment line?); the logpoint may never bind"
                },
            )
        } catch (conflict: LogpointConflictException) {
            Response(
                HttpURLConnection.HTTP_CONFLICT,
                BridgeProtocol.obj(
                    "ok", false,
                    "error", conflict.message,
                    "error_code", "logpoint_conflict",
                    "existing_breakpoint_id", conflict.breakpointId,
                    "existing_owner", conflict.owner,
                ),
            )
        } catch (t: Throwable) {
            if (!rollbackDone) {
                prepared?.takeIf { it.created || previous != null }
                    ?.let { runCatching { rollbackPrepared(project, it, previous) } }
            }
            BridgeProtocol.bad(t)
        }
    }

    private fun rollbackPrepared(
        project: Project,
        prepared: PreparedLine,
        previous: BreakpointSnapshot?,
        forceCreatedRemoval: Boolean = false,
    ): Boolean = StudioThreading.onIdeaThread {
        if (prepared.created) {
            val manager = XDebuggerManager.getInstance(project).breakpointManager
            val registered = manager.allBreakpoints.any { it === prepared.breakpoint }
            val unchanged = prepared.preparedFingerprint?.let { expected ->
                runCatching { logpointFingerprint(prepared.breakpoint) == expected }.getOrDefault(false)
            } ?: false
            // If a user changed or removed the just-created disabled
            // breakpoint during validation, it is external state now. Never
            // delete or overwrite it while rolling back the bridge request.
            if (!registered || (!forceCreatedRemoval && !unchanged)) return@onIdeaThread false
        }
        withBridgeMutation(prepared.breakpoint) {
            ownedLogpoints.remove(prepared.breakpoint)
            if (prepared.created) {
                BreakpointExpressionGuard.forget(prepared.breakpoint)
                temporaryHitClaims.forget(prepared.breakpoint)
                XDebuggerManager.getInstance(project).breakpointManager.removeBreakpoint(prepared.breakpoint)
            } else if (previous != null) {
                restoreBreakpoint(project, prepared.breakpoint, previous)
            }
        }
        true
    }

    private fun applyCondition(project: Project, breakpoint: XBreakpoint<*>, condition: String?) {
        breakpoint.setCondition(condition)
        if (condition == null) {
            BreakpointExpressionGuard.clearManaged(breakpoint, BreakpointExpressionGuard.KIND_CONDITION)
        } else {
            BreakpointExpressionGuard.markManaged(breakpoint, BreakpointExpressionGuard.KIND_CONDITION, condition)
        }
        BreakpointExpressionGuard.clearErrorsFor(breakpointId(project, breakpoint))
    }

    private fun applyLogExpression(project: Project, breakpoint: XBreakpoint<*>, expression: String?) {
        breakpoint.setLogExpression(expression)
        if (expression == null) {
            BreakpointExpressionGuard.clearManaged(breakpoint, BreakpointExpressionGuard.KIND_LOG_EXPRESSION)
        } else {
            BreakpointExpressionGuard.markManaged(breakpoint, BreakpointExpressionGuard.KIND_LOG_EXPRESSION, expression)
        }
        BreakpointExpressionGuard.clearErrorsFor(breakpointId(project, breakpoint))
    }

    private fun invalidExpression(
        project: Project,
        breakpoint: XBreakpoint<*>,
        kind: String,
        expression: String,
        problems: List<ExpressionValidation.Problem>,
        removed: Boolean = false,
    ): Response {
        val typeId = breakpoint.type.id
        val javaTypeOnKotlinFile = typeId == JAVA_LINE_TYPE_ID &&
            (breakpoint as? XLineBreakpoint<*>)?.fileUrl?.endsWith(".kt") == true
        val hint = if (javaTypeOnKotlinFile) {
            "this existing breakpoint evaluates expressions as Java; remove it and re-add so the " +
                "Kotlin file gets a Kotlin breakpoint, or write the $kind in Java syntax"
        } else {
            "fix the expression, or pass validate=false (CLI --force) to set it anyway; a failing " +
                "expression no longer blocks Android Studio — suspending breakpoints pause without a dialog, " +
                "while non-suspending logpoints resume, and the error is reported on the breakpoint"
        }
        return Response(
            HttpURLConnection.HTTP_BAD_REQUEST,
            BridgeProtocol.obj(
                "ok", false,
                "error", "invalid_expression: ${problems.first().message}",
                "error_code", "invalid_expression",
                "expression", expression,
                "expression_kind", kind,
                "breakpoint_type", typeId,
                "problems", problems.map(ExpressionValidation.Problem::toMap),
                "applied", false,
                // A breakpoint we created just to validate is rolled back on
                // failure, so nothing was left installed; a reused breakpoint is
                // reported as-is (unchanged).
                "unconfigured_breakpoint_removed", removed,
                "hint", hint,
                "breakpoint", if (removed) null else breakpointInfo(project, breakpoint),
            ),
        )
    }

    @Suppress("UNCHECKED_CAST")
    @JvmStatic
    fun addException(query: Map<String, String>, project: Project?): Response {
        val exception = query[BridgeQuery.EXCEPTION]
        if (exception.isNullOrBlank()) return BridgeProtocol.bad("missing exception")
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            val breakpoint = StudioThreading.onIdeaThread {
                val type = breakpointType(JavaExceptionBreakpointType::class.java)
                    ?: throw IllegalStateException("Java exception breakpoint type is not available")
                // Idempotent: re-adding the same exception updates the existing
                // breakpoint instead of piling up duplicates on agent retries.
                val target = findExceptionBreakpoint(project, type.id, exception)
                    ?: XDebuggerManager.getInstance(project).breakpointManager
                        .addBreakpoint(
                            type as XBreakpointType<XBreakpoint<JavaExceptionBreakpointProperties>, JavaExceptionBreakpointProperties>,
                            JavaExceptionBreakpointProperties(exception),
                        )
                (target.properties as? JavaExceptionBreakpointProperties)?.let { props ->
                    props.NOTIFY_CAUGHT = BridgeProtocol.booleanParam(query, BridgeQuery.CAUGHT, true)
                    props.NOTIFY_UNCAUGHT = BridgeProtocol.booleanParam(query, BridgeQuery.UNCAUGHT, true)
                }
                target.setEnabled(BridgeProtocol.booleanParam(query, BridgeQuery.ENABLED, true))
                target
            }
            BridgeProtocol.ok("ok", true, "breakpoint", breakpointInfo(project, breakpoint))
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @Suppress("UNCHECKED_CAST")
    @JvmStatic
    fun addMethod(query: Map<String, String>, project: Project?): Response {
        val classPattern = query[BridgeQuery.CLASS]
        val method = query[BridgeQuery.METHOD]
        if (classPattern.isNullOrBlank()) return BridgeProtocol.bad("missing class")
        if (method.isNullOrBlank()) return BridgeProtocol.bad("missing method")
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            val breakpoint = StudioThreading.onIdeaThread {
                val type = breakpointType(JavaWildcardMethodBreakpointType::class.java)
                    ?: throw IllegalStateException("Java wildcard method breakpoint type is not available")
                // Idempotent: re-adding the same class#method updates the
                // existing breakpoint instead of duplicating it.
                val target = findMethodBreakpoint(project, type.id, classPattern, method)
                    ?: XDebuggerManager.getInstance(project).breakpointManager
                        .addBreakpoint(
                            type as XBreakpointType<XBreakpoint<JavaMethodBreakpointProperties>, JavaMethodBreakpointProperties>,
                            JavaMethodBreakpointProperties(classPattern, method),
                        )
                (target.properties as? JavaMethodBreakpointProperties)?.let { props ->
                    props.WATCH_ENTRY = BridgeProtocol.booleanParam(query, BridgeQuery.ENTRY, true)
                    props.WATCH_EXIT = BridgeProtocol.booleanParam(query, BridgeQuery.EXIT, false)
                }
                target.setEnabled(BridgeProtocol.booleanParam(query, BridgeQuery.ENABLED, true))
                target
            }
            BridgeProtocol.ok("ok", true, "breakpoint", breakpointInfo(project, breakpoint))
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @JvmStatic
    fun addField(query: Map<String, String>, project: Project?): Response {
        val file = query[BridgeQuery.FILE]
        val className = query[BridgeQuery.CLASS]
        val field = query[BridgeQuery.FIELD]
        if (file.isNullOrBlank()) return BridgeProtocol.bad("missing file")
        if (className.isNullOrBlank()) return BridgeProtocol.bad("missing class")
        if (field.isNullOrBlank()) return BridgeProtocol.bad("missing field")
        val line = BridgeProtocol.intParam(query, BridgeQuery.LINE, -1, 1, Int.MAX_VALUE)
        if (line < 1) return BridgeProtocol.bad("missing or invalid line")
        val temporary = BridgeProtocol.booleanParam(query, BridgeQuery.TEMPORARY, false)
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            val target = StudioThreading.onIdeaThread {
                val type = breakpointType(JavaFieldBreakpointType::class.java)
                    ?: throw IllegalStateException("Java field breakpoint type is not available")
                val virtualFile = LocalFileSystem.getInstance().refreshAndFindFileByIoFile(File(file))
                    ?: throw IllegalArgumentException("file not found in IDE VFS: $file")
                // Constructor order is (fieldName, className) — passing them
                // swapped stores the class in myFieldName and the watchpoint
                // never matches a real field.
                val props = JavaFieldBreakpointProperties(field, className)
                props.WATCH_ACCESS = BridgeProtocol.booleanParam(query, BridgeQuery.ACCESS, false)
                props.WATCH_MODIFICATION = BridgeProtocol.booleanParam(query, BridgeQuery.MODIFICATION, true)
                // Match on the field name too -- reusing any field breakpoint on
                // the same line would silently return one for a different field.
                var breakpoint = findFieldBreakpoint(project, virtualFile.url, line - 1, type.id, field)
                if (breakpoint == null) {
                    breakpoint = XDebuggerManager.getInstance(project).breakpointManager
                        .addLineBreakpoint(type, virtualFile.url, line - 1, props, temporary)
                } else {
                    (breakpoint.properties as? JavaFieldBreakpointProperties)?.let { existing ->
                        existing.WATCH_ACCESS = props.WATCH_ACCESS
                        existing.WATCH_MODIFICATION = props.WATCH_MODIFICATION
                    }
                }
                breakpoint.setEnabled(BridgeProtocol.booleanParam(query, BridgeQuery.ENABLED, true))
                breakpoint.setTemporary(temporary)
                breakpoint
            }
            BridgeProtocol.ok("ok", true, "breakpoint", breakpointInfo(project, target))
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @JvmStatic
    fun list(projects: List<Project>): Response {
        val payload = mutableListOf<Any>()
        for (project in projects) {
            for (breakpoint in XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints) {
                if (breakpoint is XLineBreakpoint<*>) {
                    payload += breakpointInfo(project, breakpoint)
                }
            }
        }
        return BridgeProtocol.ok("ok", true, "breakpoints", payload)
    }

    @JvmStatic
    fun listLogpoints(
        query: Map<String, String>,
        projects: List<Project>,
        requestedProject: Project?,
    ): Response {
        val payload = mutableListOf<Any>()
        for (project in projects) {
            if (requestedProject != null && requestedProject !== project) continue
            for (breakpoint in XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints) {
                if (breakpoint is XLineBreakpoint<*> && isLogpoint(breakpoint)) {
                    val id = breakpointId(project, breakpoint)
                    if (query[BridgeQuery.ID]?.let { it != id } == true) continue
                    val owner = activeOwnedLogpoint(breakpoint)?.owner
                    if (query[BridgeQuery.OWNER]?.let { it != owner } == true) continue
                    payload += breakpointInfo(project, breakpoint)
                }
            }
        }
        return BridgeProtocol.ok(
            "ok", true,
            "logpoints", payload,
            "defaults", BridgeProtocol.map(
                "event_capacity", DEFAULT_LOGPOINT_EVENT_CAPACITY,
                "max_message_chars", DEFAULT_LOGPOINT_MAX_MESSAGE_CHARS,
                "max_configurable_message_chars", MAX_LOGPOINT_MESSAGE_CHARS,
                "max_events_per_second", DEFAULT_LOGPOINT_MAX_EVENTS_PER_SECOND,
            ),
        )
    }

    @JvmStatic
    fun logpointEvents(query: Map<String, String>): Response {
        val after = query[BridgeQuery.AFTER]?.let { raw ->
            raw.toLongOrNull()?.takeIf { it >= 0 }
                ?: return BridgeProtocol.bad("invalid after cursor: $raw")
        }
        val limit = BridgeProtocol.intParam(query, BridgeQuery.LIMIT, 100, 1, 200)
        val timeoutMs = BridgeProtocol.intParam(query, BridgeQuery.TIMEOUT_MS, 0, 0, 30_000)
        val followerSlot = timeoutMs == 0 || logpointFollowers.tryAcquire()
        if (!followerSlot) {
            return Response(
                429,
                BridgeProtocol.obj(
                    "ok", false,
                    "error", "too many concurrent logpoint event followers",
                    "error_code", "logpoint_follow_limit",
                    "max_followers", MAX_LOGPOINT_FOLLOWERS,
                ),
            )
        }
        return try {
            val result = logpointEventLog.read(
                after = after,
                limit = limit,
                filter = LogpointEventFilter(
                    breakpointId = query[BridgeQuery.ID],
                    owner = query[BridgeQuery.OWNER],
                    project = query[BridgeQuery.PROJECT],
                    session = query[BridgeQuery.SESSION],
                    device = query[BridgeQuery.DEVICE],
                ),
                timeoutMs = timeoutMs.toLong(),
            )
            BridgeProtocol.ok(
                "ok", true,
                "stream_id", result.streamId,
                "events", result.events,
                "next_cursor", result.nextCursor,
                "latest_cursor", result.latestCursor,
                "oldest_cursor", result.oldestCursor,
                "overflowed", result.overflowed,
                "evicted_total", result.evictedTotal,
                "rate_limited_total", result.rateLimitedTotal,
                "timed_out", result.timedOut,
            )
        } catch (interrupted: InterruptedException) {
            Thread.currentThread().interrupt()
            BridgeProtocol.bad("logpoint event wait interrupted")
        } finally {
            if (timeoutMs > 0) logpointFollowers.release()
        }
    }

    @JvmStatic
    fun removeLogpoint(
        query: Map<String, String>,
        projects: List<Project>,
        requestedProject: Project?,
    ): Response {
        val owner = query[BridgeQuery.OWNER]?.trim()
        if (owner.isNullOrEmpty()) return BridgeProtocol.bad("missing owner")
        val selected = findBreakpoint(query, projects, requestedProject)
            ?: return BridgeProtocol.bad("logpoint not found")
        if (!isLogpoint(selected.breakpoint)) return BridgeProtocol.bad("logpoint not found")
        val ownership = activeOwnedLogpoint(selected.breakpoint)
            ?: return logpointOwnershipError(
                "logpoint is manual or no longer owned; refusing to remove it",
                "logpoint_not_owned",
            )
        if (!ownership.createdByBridge || ownership.owner != owner) {
            return logpointOwnershipError(
                "logpoint is owned by '${ownership.owner}', not '$owner'",
                "logpoint_owner_mismatch",
                ownership.owner,
            )
        }
        val id = breakpointId(selected.project, selected.breakpoint)
        return try {
            val removed = StudioThreading.onIdeaThread {
                val current = activeOwnedLogpoint(selected.breakpoint)
                if (current?.createdByBridge != true || current.owner != owner) {
                    false
                } else {
                    withBridgeMutation(selected.breakpoint) {
                        forget(selected.project, selected.breakpoint)
                        XDebuggerManager.getInstance(selected.project).breakpointManager
                            .removeBreakpoint(selected.breakpoint)
                    }
                    true
                }
            }
            if (!removed) {
                return logpointOwnershipError(
                    "logpoint ownership changed before removal; refusing to remove it",
                    "logpoint_ownership_changed",
                )
            }
            BridgeProtocol.ok("ok", true, "removed", true, "id", id, "owner", owner)
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @JvmStatic
    fun clearLogpoints(
        query: Map<String, String>,
        projects: List<Project>,
        requestedProject: Project?,
    ): Response {
        val owner = query[BridgeQuery.OWNER]?.trim()
        if (owner.isNullOrEmpty()) return BridgeProtocol.bad("missing owner")
        val selected = mutableListOf<ProjectBreakpoint>()
        for (project in projects) {
            if (requestedProject != null && requestedProject !== project) continue
            for (breakpoint in XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints) {
                val ownership = activeOwnedLogpoint(breakpoint) ?: continue
                if (ownership.createdByBridge && ownership.owner == owner && isLogpoint(breakpoint)) {
                    selected += ProjectBreakpoint(project, breakpoint)
                }
            }
        }
        return try {
            val removedIds = StudioThreading.onIdeaThread {
                val ids = mutableListOf<String>()
                for (target in selected) {
                    val current = activeOwnedLogpoint(target.breakpoint)
                    if (current?.createdByBridge != true || current.owner != owner) continue
                    val id = breakpointId(target.project, target.breakpoint)
                    withBridgeMutation(target.breakpoint) {
                        forget(target.project, target.breakpoint)
                        XDebuggerManager.getInstance(target.project).breakpointManager
                            .removeBreakpoint(target.breakpoint)
                    }
                    ids += id
                }
                ids
            }
            BridgeProtocol.ok(
                "ok", true,
                "owner", owner,
                "removed", removedIds.size,
                "ids", removedIds,
            )
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    private fun logpointOwnershipError(message: String, code: String, actualOwner: String? = null): Response =
        Response(
            HttpURLConnection.HTTP_CONFLICT,
            BridgeProtocol.obj(
                "ok", false,
                "error", message,
                "error_code", code,
                "actual_owner", actualOwner,
            ),
        )

    @JvmStatic
    fun update(query: Map<String, String>, projects: List<Project>, requestedProject: Project?): Response {
        val selected = findBreakpoint(query, projects, requestedProject) ?: return BridgeProtocol.bad("breakpoint not found")
        val breakpoint = selected.breakpoint
        val validate = BridgeProtocol.booleanParam(query, BridgeQuery.VALIDATE, true)
        val clearCondition = BridgeProtocol.booleanParam(query, BridgeQuery.CLEAR_CONDITION, false)
        val newCondition = if (!clearCondition && query.containsKey(BridgeQuery.CONDITION)) {
            query[BridgeQuery.CONDITION]?.ifBlank { null }
        } else {
            null
        }
        val clearLogExpression = BridgeProtocol.booleanParam(query, BridgeQuery.CLEAR_LOG_EXPRESSION, false)
        val newLogExpression = if (!clearLogExpression && query.containsKey(BridgeQuery.LOG_EXPRESSION)) {
            query[BridgeQuery.LOG_EXPRESSION]?.ifBlank { null }
        } else {
            null
        }
        // Reject invalid expressions before mutating anything: the whole
        // update stays unapplied, so the agent retries one atomic call.
        if (validate && newCondition != null) {
            val problems = ExpressionValidation.validate(selected.project, breakpoint, newCondition, expectBoolean = true)
            if (problems.isNotEmpty()) {
                return invalidExpression(
                    selected.project,
                    breakpoint,
                    BreakpointExpressionGuard.KIND_CONDITION,
                    newCondition,
                    problems,
                )
            }
        }
        if (validate && newLogExpression != null) {
            val problems = ExpressionValidation.validate(selected.project, breakpoint, newLogExpression, expectBoolean = false)
            if (problems.isNotEmpty()) {
                return invalidExpression(
                    selected.project,
                    breakpoint,
                    BreakpointExpressionGuard.KIND_LOG_EXPRESSION,
                    newLogExpression,
                    problems,
                )
            }
        }
        return try {
            StudioThreading.onIdeaThread {
                if (query.containsKey(BridgeQuery.ENABLED)) breakpoint.setEnabled(BridgeProtocol.booleanParam(query, BridgeQuery.ENABLED, breakpoint.isEnabled))
                if (breakpoint is XLineBreakpoint<*> && query.containsKey(BridgeQuery.TEMPORARY)) {
                    breakpoint.setTemporary(BridgeProtocol.booleanParam(query, BridgeQuery.TEMPORARY, breakpoint.isTemporary))
                }
                if (clearCondition) {
                    applyCondition(selected.project, breakpoint, null)
                } else if (query.containsKey(BridgeQuery.CONDITION)) {
                    applyCondition(selected.project, breakpoint, newCondition)
                }
                if (clearLogExpression) {
                    applyLogExpression(selected.project, breakpoint, null)
                } else if (query.containsKey(BridgeQuery.LOG_EXPRESSION)) {
                    applyLogExpression(selected.project, breakpoint, newLogExpression)
                }
                if (query.containsKey(BridgeQuery.LOG_MESSAGE)) breakpoint.setLogMessage(BridgeProtocol.booleanParam(query, BridgeQuery.LOG_MESSAGE, breakpoint.isLogMessage))
                if (query.containsKey(BridgeQuery.LOG_STACK)) breakpoint.setLogStack(BridgeProtocol.booleanParam(query, BridgeQuery.LOG_STACK, breakpoint.isLogStack))
                if (query.containsKey(BridgeQuery.SUSPEND)) {
                    val raw = query.getValue(BridgeQuery.SUSPEND)
                    val policy = SuspendPolicy.values().firstOrNull { it.name.equals(raw, ignoreCase = true) }
                        ?: throw IllegalArgumentException("invalid suspend policy: $raw (use all, thread, or none)")
                    breakpoint.setSuspendPolicy(policy)
                }
                val props = breakpoint.properties
                if (query.containsKey(BridgeQuery.PASS_COUNT) && props is JavaBreakpointProperties<*>) {
                    val count = BridgeProtocol.intParam(query, BridgeQuery.PASS_COUNT, 0, 0, Int.MAX_VALUE)
                    props.setCOUNT_FILTER_ENABLED(count > 0)
                    props.setCOUNT_FILTER(count)
                }
                null
            }
            BridgeProtocol.ok("ok", true, "breakpoint", breakpointInfo(selected.project, breakpoint))
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @JvmStatic
    fun remove(query: Map<String, String>, projects: List<Project>, requestedProject: Project?): Response {
        val selected = findBreakpoint(query, projects, requestedProject) ?: return BridgeProtocol.bad("breakpoint not found")
        return try {
            StudioThreading.onIdeaThread {
                // Prune hit stats first: the id derives from the source position,
                // which may no longer resolve once the breakpoint is removed.
                forget(selected.project, selected.breakpoint)
                XDebuggerManager.getInstance(selected.project).breakpointManager.removeBreakpoint(selected.breakpoint)
                null
            }
            BridgeProtocol.ok("ok", true, "removed", true, "id", query[BridgeQuery.ID])
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    private fun findBreakpoint(query: Map<String, String>, projects: List<Project>, requestedProject: Project?): ProjectBreakpoint? {
        val id = query[BridgeQuery.ID]
        if (id.isNullOrBlank()) return null
        for (project in projects) {
            if (requestedProject != null && requestedProject != project) continue
            for (breakpoint in XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints) {
                if (id == breakpointId(project, breakpoint)) return ProjectBreakpoint(project, breakpoint)
            }
        }
        return null
    }

    private fun findLineBreakpoint(project: Project, fileUrl: String, zeroBasedLine: Int): XLineBreakpoint<*>? =
        findLineBreakpoints(project, fileUrl, zeroBasedLine).firstOrNull()

    private fun findLineBreakpoints(project: Project, fileUrl: String, zeroBasedLine: Int): List<XLineBreakpoint<*>> =
        XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints
            .asSequence()
            .filterIsInstance<XLineBreakpoint<*>>()
            .filter { it.fileUrl == fileUrl && it.line == zeroBasedLine && it.type.id in LINE_TYPE_IDS }
            .toList()

    // Kotlin files get the Kotlin line breakpoint type so conditions compile
    // with the Kotlin evaluator. The Java type binds at Kotlin positions too,
    // but parses expressions as Java — every Kotlin-syntax condition would
    // fail at hit time.
    private fun lineBreakpointTypeFor(project: Project, file: VirtualFile, zeroBasedLine: Int): ChosenLineType {
        // Look types up by id, never by isInstance: KotlinLineBreakpointType
        // extends JavaLineBreakpointType and is registered order="first", so an
        // isInstance scan for the Java type would return the Kotlin one — and a
        // Java file would then evaluate its conditions with the Kotlin evaluator.
        val javaType = lineBreakpointTypeById(JAVA_LINE_TYPE_ID)
            ?: throw IllegalStateException("Java line breakpoint type is not available")
        val extension = file.extension?.lowercase()
        if (extension == "kt" || extension == "kts") {
            val kotlinType = lineBreakpointTypeById(KOTLIN_LINE_TYPE_ID)
            if (kotlinType != null && canPutAt(kotlinType, file, zeroBasedLine, project)) {
                return ChosenLineType(kotlinType, true)
            }
        }
        return ChosenLineType(javaType, canPutAt(javaType, file, zeroBasedLine, project))
    }

    private fun lineBreakpointTypeById(id: String): XLineBreakpointType<*>? =
        XBreakpointType.EXTENSION_POINT_NAME.extensionList
            .filterIsInstance<XLineBreakpointType<*>>()
            .firstOrNull { it.id == id }

    private fun canPutAt(type: XLineBreakpointType<*>, file: VirtualFile, zeroBasedLine: Int, project: Project): Boolean =
        runCatching { type.canPutAt(file, zeroBasedLine, project) }.getOrDefault(false)

    private fun findFieldBreakpoint(
        project: Project,
        fileUrl: String,
        zeroBasedLine: Int,
        typeId: String,
        field: String,
    ): XLineBreakpoint<*>? =
        XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints
            .asSequence()
            .filterIsInstance<XLineBreakpoint<*>>()
            .firstOrNull {
                it.fileUrl == fileUrl && it.line == zeroBasedLine && it.type.id == typeId &&
                    (it.properties as? JavaFieldBreakpointProperties)?.myFieldName == field
            }

    private fun findExceptionBreakpoint(project: Project, typeId: String, exception: String): XBreakpoint<*>? =
        XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints.firstOrNull {
            it.type.id == typeId &&
                (it.properties as? JavaExceptionBreakpointProperties)?.myQualifiedName == exception
        }

    private fun findMethodBreakpoint(project: Project, typeId: String, classPattern: String, method: String): XBreakpoint<*>? =
        XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints.firstOrNull {
            val props = it.properties as? JavaMethodBreakpointProperties
            it.type.id == typeId && props != null && props.myClassPattern == classPattern && props.myMethodName == method
        }

    private fun <T : Any> breakpointType(klass: Class<T>): T? =
        XBreakpointType.EXTENSION_POINT_NAME.extensionList.firstNotNullOfOrNull { klass.castOrNull(it) }

    private fun breakpointInfo(project: Project, breakpoint: XBreakpoint<*>): Map<String, Any?> {
        val pos: XSourcePosition? = breakpoint.sourcePosition
        val lineBreakpoint = breakpoint as? XLineBreakpoint<*>
        val javaProps = breakpoint.properties as? JavaBreakpointProperties<*>
        val id = breakpointId(project, breakpoint)
        val logpoint = isLogpoint(breakpoint)
        val owned = if (logpoint) activeOwnedLogpoint(breakpoint) else null
        return BridgeProtocol.map(
            "id", id,
            "project", projectInfo(project),
            "type", breakpoint.type.id,
            "kind", if (logpoint) "logpoint" else "breakpoint",
            "owner", owned?.owner,
            "managed", owned != null,
            "created_by_bridge", owned?.createdByBridge == true,
            "max_message_chars", owned?.maxMessageChars ?: if (logpoint) DEFAULT_LOGPOINT_MAX_MESSAGE_CHARS else null,
            "max_events_per_second", owned?.maxEventsPerSecond ?: if (logpoint) DEFAULT_LOGPOINT_MAX_EVENTS_PER_SECOND else null,
            "enabled", breakpoint.isEnabled,
            "temporary", lineBreakpoint?.isTemporary,
            "condition", breakpoint.conditionExpression?.expression,
            "log_message", breakpoint.isLogMessage,
            "log_stack", breakpoint.isLogStack,
            "log_expression", breakpoint.logExpressionObject?.expression,
            "suspend_policy", breakpoint.suspendPolicy.name,
            "pass_count_enabled", javaProps?.isCOUNT_FILTER_ENABLED,
            "pass_count", javaProps?.getCOUNT_FILTER(),
            "hit_count", breakpointHits.getOrDefault(id, 0L),
            "last_hit_at", breakpointLastHit[id],
            "hit_count_source", if (logpoint) {
                "shadowdroid_observed_log_callbacks"
            } else {
                "shadowdroid_observed_session_pauses"
            },
            "last_evaluation_error", BreakpointExpressionGuard.lastErrorFor(id),
            "properties", breakpointPropertiesInfo(breakpoint.properties),
            "file", owned?.file ?: pos?.file?.path,
            "url", lineBreakpoint?.fileUrl,
            "line", lineBreakpoint?.let { it.line + 1 },
            "timestamp", breakpoint.timeStamp,
        )
    }

    private fun breakpointId(project: Project, breakpoint: XBreakpoint<*>): String {
        val pos = breakpoint.sourcePosition
        val lineBreakpoint = breakpoint as? XLineBreakpoint<*>
        val raw = listOf(
            project.basePath ?: project.name,
            breakpoint.type.id,
            lineBreakpoint?.fileUrl.orEmpty(),
            pos?.file?.path.orEmpty(),
            (lineBreakpoint?.let { it.line + 1 } ?: -1).toString(),
            breakpointIdentityDetails(breakpoint.properties),
        ).joinToString("|")
        return "bp_" + Base64.getUrlEncoder().withoutPadding()
            .encodeToString(raw.toByteArray(StandardCharsets.UTF_8))
    }

    private fun breakpointIdentityDetails(props: Any?): String = when (props) {
        is JavaExceptionBreakpointProperties -> "exception:${props.myQualifiedName}"
        is JavaMethodBreakpointProperties -> "method:${props.myClassPattern}#${props.myMethodName}"
        is JavaFieldBreakpointProperties -> "field:${props.myClassName}#${props.myFieldName}"
        else -> ""
    }

    private fun breakpointPropertiesInfo(props: Any?): Map<String, Any?>? = when (props) {
        is JavaExceptionBreakpointProperties -> BridgeProtocol.map(
            "kind", "exception",
            "exception", props.myQualifiedName,
            "caught", props.NOTIFY_CAUGHT,
            "uncaught", props.NOTIFY_UNCAUGHT,
        )
        is JavaMethodBreakpointProperties -> BridgeProtocol.map(
            "kind", "method",
            "class", props.myClassPattern,
            "method", props.myMethodName,
            "entry", props.WATCH_ENTRY,
            "exit", props.WATCH_EXIT,
        )
        is JavaFieldBreakpointProperties -> BridgeProtocol.map(
            "kind", "field",
            "class", props.myClassName,
            "field", props.myFieldName,
            "access", props.WATCH_ACCESS,
            "modification", props.WATCH_MODIFICATION,
        )
        else -> null
    }

    private fun isLogpoint(breakpoint: XBreakpoint<*>): Boolean =
        breakpoint.suspendPolicy == SuspendPolicy.NONE &&
            (breakpoint.isLogMessage || breakpoint.isLogStack ||
                !breakpoint.logExpressionObject?.expression.isNullOrBlank())

    private fun ownedLogpoint(breakpoint: XBreakpoint<*>): OwnedLogpoint? =
        synchronized(ownedLogpoints) { ownedLogpoints[breakpoint] }

    /** Return current ownership, relinquishing it if the IDE configuration diverged. */
    private fun activeOwnedLogpoint(breakpoint: XBreakpoint<*>): OwnedLogpoint? {
        val owned = ownedLogpoint(breakpoint) ?: return null
        if (bridgeMutations.contains(breakpoint)) return owned
        val current = runCatching { logpointFingerprint(breakpoint) }.getOrNull()
        if (current == owned.fingerprint) return owned
        synchronized(ownedLogpoints) {
            if (ownedLogpoints[breakpoint] === owned) ownedLogpoints.remove(breakpoint)
        }
        return null
    }

    private inline fun <T> withBridgeMutation(breakpoint: XBreakpoint<*>, block: () -> T): T {
        bridgeMutations.add(breakpoint)
        try {
            return block()
        } finally {
            bridgeMutations.remove(breakpoint)
        }
    }

    private fun logpointFingerprint(breakpoint: XBreakpoint<*>): LogpointFingerprint {
        val line = breakpoint as? XLineBreakpoint<*>
        val javaProps = breakpoint.properties as? JavaBreakpointProperties<*>
        return LogpointFingerprint(
            enabled = breakpoint.isEnabled,
            temporary = line?.isTemporary,
            condition = breakpoint.conditionExpression?.expression,
            logExpression = breakpoint.logExpressionObject?.expression,
            logMessage = breakpoint.isLogMessage,
            logStack = breakpoint.isLogStack,
            suspendPolicy = breakpoint.suspendPolicy,
            passCountEnabled = javaProps?.isCOUNT_FILTER_ENABLED,
            passCount = javaProps?.getCOUNT_FILTER(),
        )
    }

    private fun breakpointSnapshot(breakpoint: XLineBreakpoint<*>): BreakpointSnapshot =
        BreakpointSnapshot(
            fingerprint = logpointFingerprint(breakpoint),
            ownership = ownedLogpoint(breakpoint),
        )

    private fun restoreBreakpoint(
        project: Project,
        breakpoint: XLineBreakpoint<*>,
        snapshot: BreakpointSnapshot,
    ) {
        val state = snapshot.fingerprint
        breakpoint.setEnabled(false)
        state.temporary?.let(breakpoint::setTemporary)
        applyCondition(project, breakpoint, state.condition)
        applyLogExpression(project, breakpoint, state.logExpression)
        breakpoint.setLogMessage(state.logMessage)
        breakpoint.setLogStack(state.logStack)
        breakpoint.setSuspendPolicy(state.suspendPolicy)
        (breakpoint.properties as? JavaBreakpointProperties<*>)?.let { props ->
            state.passCountEnabled?.let(props::setCOUNT_FILTER_ENABLED)
            state.passCount?.let(props::setCOUNT_FILTER)
        }
        snapshot.ownership?.let { ownership ->
            ownedLogpoints[breakpoint] = ownership.copy(fingerprint = logpointFingerprint(breakpoint))
        }
        breakpoint.setEnabled(state.enabled)
        snapshot.ownership?.let { ownership ->
            ownedLogpoints[breakpoint] = ownership.copy(fingerprint = logpointFingerprint(breakpoint))
        }
    }

    private fun projectInfo(project: Project): Map<String, Any?> =
        BridgeProtocol.map(
            "name", project.name,
            "base_path", project.basePath,
            "disposed", project.isDisposed,
        )

    private data class ProjectBreakpoint(
        val project: Project,
        val breakpoint: XBreakpoint<*>,
    )

    private data class OwnedLogpoint(
        val owner: String,
        val createdByBridge: Boolean,
        val file: String,
        val maxMessageChars: Int,
        val maxEventsPerSecond: Int,
        val fingerprint: LogpointFingerprint,
    )

    private data class LogpointFingerprint(
        val enabled: Boolean,
        val temporary: Boolean?,
        val condition: String?,
        val logExpression: String?,
        val logMessage: Boolean,
        val logStack: Boolean,
        val suspendPolicy: SuspendPolicy,
        val passCountEnabled: Boolean?,
        val passCount: Int?,
    )

    private data class BreakpointSnapshot(
        val fingerprint: LogpointFingerprint,
        val ownership: OwnedLogpoint?,
    )

    private class LogpointConflictException(
        message: String,
        val breakpointId: String,
        val owner: String?,
    ) : RuntimeException(message)

    private data class PreparedLine(
        val breakpoint: XLineBreakpoint<*>,
        val positionSupported: Boolean,
        val created: Boolean,
        val preparedBreakpointId: String? = null,
        val preparedFingerprint: LogpointFingerprint? = null,
    )

    private data class ChosenLineType(
        val type: XLineBreakpointType<*>,
        val positionSupported: Boolean,
    )
}

private fun <T : Any> Class<T>.castOrNull(value: Any?): T? =
    if (isInstance(value)) cast(value) else null
