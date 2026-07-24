package io.github.andriyo.shadowdroid.studio

import com.intellij.debugger.ui.breakpoints.JavaExceptionBreakpointType
import com.intellij.debugger.ui.breakpoints.JavaFieldBreakpointType
import com.intellij.debugger.ui.breakpoints.JavaWildcardMethodBreakpointType
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
import java.util.concurrent.ConcurrentHashMap

internal object BreakpointBridge {
    // One physical hit can surface through more than one listener (pause
    // position match, log-message callback — observed duplicated with
    // concurrent sessions); collapse records landing within this window.
    // hit_count is documented as observed/approximate, so a burst of real
    // hits inside the window under-counts rather than double-counting.
    private const val HIT_DEDUPE_WINDOW_MS = 250L

    private const val JAVA_LINE_TYPE_ID = "java-line"
    private const val KOTLIN_LINE_TYPE_ID = "kotlin-line"

    // Plain line breakpoints only; field watchpoints are also XLineBreakpoints
    // at a file/line and must never be reused as a line breakpoint.
    private val LINE_TYPE_IDS = setOf(JAVA_LINE_TYPE_ID, KOTLIN_LINE_TYPE_ID)

    private val breakpointHits = ConcurrentHashMap<String, Int>()
    private val breakpointLastHit = ConcurrentHashMap<String, Long>()

    @JvmStatic
    fun recordHit(project: Project, breakpoint: XBreakpoint<*>) {
        val id = breakpointId(project, breakpoint)
        val now = System.currentTimeMillis()
        val last = breakpointLastHit.put(id, now)
        if (last != null && now - last < HIT_DEDUPE_WINDOW_MS) return
        breakpointHits.merge(id, 1, Int::plus)
    }

    @JvmStatic
    fun forget(project: Project, breakpoint: XBreakpoint<*>) {
        BreakpointExpressionGuard.forget(breakpoint)
        val id = try {
            breakpointId(project, breakpoint)
        } catch (_: Throwable) {
            return
        }
        breakpointHits.remove(id)
        breakpointLastHit.remove(id)
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
                "expression no longer blocks Android Studio — the session pauses at the breakpoint " +
                "and the error is reported on it"
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
        XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints
            .asSequence()
            .filterIsInstance<XLineBreakpoint<*>>()
            .firstOrNull { it.fileUrl == fileUrl && it.line == zeroBasedLine && it.type.id in LINE_TYPE_IDS }

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
        return BridgeProtocol.map(
            "id", id,
            "project", projectInfo(project),
            "type", breakpoint.type.id,
            "enabled", breakpoint.isEnabled,
            "temporary", lineBreakpoint?.isTemporary,
            "condition", breakpoint.conditionExpression?.expression,
            "log_message", breakpoint.isLogMessage,
            "log_stack", breakpoint.isLogStack,
            "log_expression", breakpoint.logExpressionObject?.expression,
            "suspend_policy", breakpoint.suspendPolicy.name,
            "pass_count_enabled", javaProps?.isCOUNT_FILTER_ENABLED,
            "pass_count", javaProps?.getCOUNT_FILTER(),
            "hit_count", breakpointHits.getOrDefault(id, 0),
            "last_hit_at", breakpointLastHit[id],
            "hit_count_source", "shadowdroid_observed_session_pauses",
            "last_evaluation_error", BreakpointExpressionGuard.lastErrorFor(id),
            "properties", breakpointPropertiesInfo(breakpoint.properties),
            "file", pos?.file?.path,
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

    private data class PreparedLine(
        val breakpoint: XLineBreakpoint<*>,
        val positionSupported: Boolean,
        val created: Boolean,
    )

    private data class ChosenLineType(
        val type: XLineBreakpointType<*>,
        val positionSupported: Boolean,
    )
}

private fun <T : Any> Class<T>.castOrNull(value: Any?): T? =
    if (isInstance(value)) cast(value) else null
