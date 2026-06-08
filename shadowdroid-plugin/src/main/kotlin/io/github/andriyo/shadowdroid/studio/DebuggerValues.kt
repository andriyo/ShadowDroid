package io.github.andriyo.shadowdroid.studio

import com.intellij.debugger.engine.JavaStackFrame
import com.intellij.debugger.jdi.LocalVariableProxyImpl
import com.intellij.debugger.jdi.StackFrameProxyImpl
import com.intellij.debugger.jdi.ThreadReferenceProxyImpl
import com.intellij.xdebugger.XDebugSession
import com.intellij.xdebugger.XSourcePosition
import com.intellij.xdebugger.frame.XExecutionStack
import com.intellij.xdebugger.frame.XStackFrame
import com.sun.jdi.ArrayReference
import com.sun.jdi.Field
import com.sun.jdi.Location
import com.sun.jdi.ObjectReference
import com.sun.jdi.StringReference
import com.sun.jdi.Value
import kotlin.math.max
import kotlin.math.min

internal object DebuggerValues {
    @JvmStatic
    fun javaFrames(session: XDebugSession, frame: JavaStackFrame, limit: Int, timeoutMs: Int): List<Any> =
        try {
            StudioThreading.onDebuggerThread(session, timeoutMs) {
                val thread = frame.stackFrameProxy.threadProxy()
                val frames = mutableListOf<Any>()
                var index = 0
                for (stackFrame in thread.frames()) {
                    if (index >= limit) break
                    frames += frameInfo(stackFrame.location(), index, thread.name())
                    index++
                }
                frames
            }
        } catch (t: Throwable) {
            listOf(BridgeProtocol.map("error", t.message))
        }

    @JvmStatic
    fun frameInfo(frame: XStackFrame, index: Int): Map<String, Any?> {
        val pos: XSourcePosition? = frame.sourcePosition
        return BridgeProtocol.map(
            "index", index,
            "kind", frame.javaClass.name,
            "file", pos?.file?.path,
            "line", pos?.let { it.line + 1 },
        )
    }

    @JvmStatic
    fun frameInfo(location: Location, index: Int, threadName: String?): Map<String, Any?> {
        val source = try {
            location.sourceName()
        } catch (_: Throwable) {
            null
        }
        return BridgeProtocol.map(
            "index", index,
            "thread", threadName,
            "class", location.declaringType()?.name(),
            "method", location.method()?.name(),
            "line", location.lineNumber().takeIf { it >= 0 },
            "source", source,
        )
    }

    @JvmStatic
    fun valueToMap(name: String, value: Value?): Map<String, Any?> =
        valueToMap(name, value, null, RenderOptions.shallow(), hashSetOf())

    @JvmStatic
    fun valueToMap(
        name: String,
        value: Value?,
        declaredType: String?,
        options: RenderOptions,
        visiting: MutableSet<Long>,
    ): MutableMap<String, Any?> {
        val payload = BridgeProtocol.map(
            "name", name,
            "declared_type", declaredType,
            "type", valueTypeName(value),
            "value", valueText(value),
        )
        val objectReference = value as? ObjectReference ?: return payload

        val objectId = objectReference.uniqueID()
        payload["object_id"] = objectId
        payload["object_handle"] = "obj_$objectId"
        if (options.depth <= 0) return payload
        if (!visiting.add(objectId)) {
            payload["cycle"] = true
            return payload
        }
        try {
            when (objectReference) {
                is StringReference -> payload["string"] = objectReference.value()
                is ArrayReference -> {
                    payload["length"] = objectReference.length()
                    payload["items"] = arrayItems(objectReference, options.child(), visiting)
                    val truncated = max(0, objectReference.length() - options.maxArrayItems)
                    if (truncated > 0) payload["truncated_items"] = truncated
                }
                else -> payload["fields"] = objectFields(objectReference, options.child(), visiting)
            }
            return payload
        } catch (t: Throwable) {
            payload["error"] = t.message
            return payload
        } finally {
            visiting.remove(objectId)
        }
    }

    @JvmStatic
    fun valueTypeName(value: Value?): String? =
        try {
            value?.type()?.name()
        } catch (_: Throwable) {
            null
        }

    @JvmStatic
    @Throws(Exception::class)
    fun selectedFrame(session: XDebugSession, query: Map<String, String>): SelectedFrame? {
        val requestedFrame = BridgeProtocol.intParam(query, "frame", 0, 0, 512)
        val requestedThread = query["thread"]
        val currentFrame = session.currentStackFrame
        if (requestedThread.isNullOrBlank()) {
            val javaFrame = currentFrame as? JavaStackFrame ?: return null
            return frameFromThread(javaFrame.stackFrameProxy.threadProxy(), 0, requestedFrame)
        }

        val stacks = session.suspendContext?.executionStacks ?: XExecutionStack.EMPTY_ARRAY
        val requestedIndex = parseIndex(requestedThread)
        for (index in stacks.indices) {
            val stack = stacks[index]
            if (requestedIndex != null) {
                if (requestedIndex != index) continue
            } else if (requestedThread != stack.displayName) {
                continue
            }
            val javaFrame = stack.topFrame as? JavaStackFrame ?: return null
            return frameFromThread(javaFrame.stackFrameProxy.threadProxy(), index, requestedFrame)
        }
        throw IllegalArgumentException("thread not found: $requestedThread")
    }

    @JvmStatic
    @Throws(Exception::class)
    fun evaluatePath(proxy: StackFrameProxyImpl, expression: String): EvaluationResult {
        val expr = expression.trim()
        if (expr.isEmpty()) throw IllegalArgumentException("empty expression")
        var pos = 0
        while (pos < expr.length && expr[pos] != '.' && expr[pos] != '[') pos++
        val base = expr.substring(0, pos)
        var value: Value?
        var declaredType: String? = null
        if (base == "this") {
            value = proxy.thisObject()
            declaredType = valueTypeName(value)
        } else {
            val variable: LocalVariableProxyImpl = proxy.visibleVariables().firstOrNull { it.name() == base }
                ?: throw IllegalArgumentException("unknown local: $base")
            value = proxy.getValue(variable)
            declaredType = variable.typeName()
        }

        while (pos < expr.length) {
            when (expr[pos]) {
                '.' -> {
                    val start = ++pos
                    while (pos < expr.length && expr[pos] != '.' && expr[pos] != '[') pos++
                    val fieldName = expr.substring(start, pos)
                    if (fieldName.isBlank()) throw IllegalArgumentException("empty field in expression: $expression")
                    val objectReference = value as? ObjectReference
                        ?: throw IllegalArgumentException("cannot read field $fieldName from non-object value")
                    val field = findField(objectReference, fieldName)
                        ?: throw IllegalArgumentException("field not found: $fieldName")
                    value = objectReference.getValue(field)
                    declaredType = field.typeName()
                }
                '[' -> {
                    val end = expr.indexOf(']', pos)
                    if (end < 0) throw IllegalArgumentException("missing closing ] in expression: $expression")
                    val index = expr.substring(pos + 1, end).trim().toIntOrNull()
                        ?: throw IllegalArgumentException("array index must be an integer")
                    val arrayReference = value as? ArrayReference
                        ?: throw IllegalArgumentException("cannot index non-array value")
                    if (index < 0 || index >= arrayReference.length()) {
                        throw IllegalArgumentException("array index out of bounds: $index")
                    }
                    value = arrayReference.getValue(index)
                    declaredType = valueTypeName(value)
                    pos = end + 1
                }
                else -> throw IllegalArgumentException("unsupported expression syntax near: ${expr.substring(pos)}")
            }
        }
        return EvaluationResult(value, declaredType)
    }

    private fun arrayItems(arrayReference: ArrayReference, options: RenderOptions, visiting: MutableSet<Long>): List<Any?> {
        val count = min(arrayReference.length(), options.maxArrayItems)
        return (0 until count).map { index ->
            valueToMap("[$index]", arrayReference.getValue(index), null, options, visiting)
        }
    }

    private fun objectFields(objectReference: ObjectReference, options: RenderOptions, visiting: MutableSet<Long>): List<Any?> {
        val fields = mutableListOf<Any?>()
        var instanceFieldCount = 0
        for (field in objectReference.referenceType().allFields()) {
            if (field.isStatic) continue
            instanceFieldCount++
            if (fields.size >= options.maxFields) continue
            fields += fieldToMap(objectReference, field, options, visiting)
        }
        val truncated = instanceFieldCount - fields.size
        if (truncated > 0) {
            fields += BridgeProtocol.map("name", "<truncated>", "truncated_fields", truncated)
        }
        return fields
    }

    private fun fieldToMap(objectReference: ObjectReference, field: Field, options: RenderOptions, visiting: MutableSet<Long>): Map<String, Any?> =
        try {
            valueToMap(field.name(), objectReference.getValue(field), field.typeName(), options, visiting)
        } catch (t: Throwable) {
            BridgeProtocol.map(
                "name", field.name(),
                "declared_type", field.typeName(),
                "error", t.message,
            )
        }

    private fun valueText(value: Value?): String? = when (value) {
        null -> null
        is StringReference -> value.value()
        else -> value.toString()
    }

    @Throws(Exception::class)
    private fun frameFromThread(thread: ThreadReferenceProxyImpl, threadIndex: Int, frameIndex: Int): SelectedFrame {
        val frames = thread.frames()
        if (frameIndex >= frames.size) {
            throw IllegalArgumentException("frame index out of bounds: $frameIndex")
        }
        return SelectedFrame(frames[frameIndex], threadIndex, frameIndex, thread.name())
    }

    private fun parseIndex(value: String?): Int? =
        value?.takeUnless { it.isBlank() }?.toIntOrNull()

    private fun findField(objectReference: ObjectReference, fieldName: String): Field? =
        objectReference.referenceType().allFields().firstOrNull { it.name() == fieldName }

    data class SelectedFrame(
        val proxy: StackFrameProxyImpl,
        val threadIndex: Int,
        val frameIndex: Int,
        val threadName: String?,
    ) {
        fun proxy(): StackFrameProxyImpl = proxy
        fun info(): Map<String, Any?> =
            BridgeProtocol.map("thread", threadIndex, "thread_name", threadName, "frame", frameIndex)
    }

    data class EvaluationResult(
        val value: Value?,
        val declaredType: String?,
    ) {
        fun value(): Value? = value
        fun declaredType(): String? = declaredType
    }

    data class RenderOptions(
        val depth: Int,
        val maxFields: Int,
        val maxArrayItems: Int,
    ) {
        fun child(): RenderOptions = RenderOptions(max(0, depth - 1), maxFields, maxArrayItems)

        companion object {
            @JvmStatic
            fun shallow(): RenderOptions = RenderOptions(0, 64, 32)
        }
    }
}
