package io.github.andriyo.shadowdroid.studio;

import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.intParam;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.map;
import static io.github.andriyo.shadowdroid.studio.StudioThreading.onDebuggerThread;

import com.intellij.debugger.engine.JavaStackFrame;
import com.intellij.debugger.jdi.LocalVariableProxyImpl;
import com.intellij.debugger.jdi.StackFrameProxyImpl;
import com.intellij.debugger.jdi.ThreadReferenceProxyImpl;
import com.intellij.xdebugger.XDebugSession;
import com.intellij.xdebugger.XSourcePosition;
import com.intellij.xdebugger.frame.XExecutionStack;
import com.intellij.xdebugger.frame.XStackFrame;
import com.intellij.xdebugger.frame.XSuspendContext;
import com.sun.jdi.ArrayReference;
import com.sun.jdi.Field;
import com.sun.jdi.Location;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.StringReference;
import com.sun.jdi.Value;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

final class DebuggerValues {
    private DebuggerValues() {
    }

    static List<Object> javaFrames(XDebugSession session, JavaStackFrame frame, int limit, int timeoutMs) {
        try {
            return onDebuggerThread(session, timeoutMs, () -> {
                ThreadReferenceProxyImpl thread = frame.getStackFrameProxy().threadProxy();
                List<Object> frames = new ArrayList<>();
                int index = 0;
                for (StackFrameProxyImpl stackFrame : thread.frames()) {
                    if (index >= limit) break;
                    frames.add(frameInfo(stackFrame.location(), index, thread.name()));
                    index++;
                }
                return frames;
            });
        } catch (Throwable t) {
            return List.of(map("error", t.getMessage()));
        }
    }

    static Map<String, Object> frameInfo(XStackFrame frame, int index) {
        XSourcePosition pos = frame.getSourcePosition();
        return map(
            "index", index,
            "kind", frame.getClass().getName(),
            "file", pos == null ? null : pos.getFile().getPath(),
            "line", pos == null ? null : pos.getLine() + 1
        );
    }

    static Map<String, Object> frameInfo(Location location, int index, String threadName) {
        String source = null;
        try {
            source = location.sourceName();
        } catch (Throwable ignored) {
        }
        return map(
            "index", index,
            "thread", threadName,
            "class", location.declaringType() == null ? null : location.declaringType().name(),
            "method", location.method() == null ? null : location.method().name(),
            "line", location.lineNumber() >= 0 ? location.lineNumber() : null,
            "source", source
        );
    }

    static Map<String, Object> valueToMap(String name, Value value) {
        return valueToMap(name, value, null, RenderOptions.shallow(), new HashSet<>());
    }

    static Map<String, Object> valueToMap(
        String name,
        Value value,
        String declaredType,
        RenderOptions options,
        Set<Long> visiting
    ) {
        Map<String, Object> payload = map(
            "name", name,
            "declared_type", declaredType,
            "type", valueTypeName(value),
            "value", valueText(value)
        );
        if (!(value instanceof ObjectReference objectReference)) {
            return payload;
        }

        long objectId = objectReference.uniqueID();
        payload.put("object_id", objectId);
        payload.put("object_handle", "obj_" + objectId);
        if (options.depth <= 0) {
            return payload;
        }
        if (!visiting.add(objectId)) {
            payload.put("cycle", true);
            return payload;
        }
        try {
            if (objectReference instanceof StringReference stringReference) {
                payload.put("string", stringReference.value());
            } else if (objectReference instanceof ArrayReference arrayReference) {
                payload.put("length", arrayReference.length());
                payload.put("items", arrayItems(arrayReference, options.child(), visiting));
                int truncated = Math.max(0, arrayReference.length() - options.maxArrayItems);
                if (truncated > 0) payload.put("truncated_items", truncated);
            } else {
                payload.put("fields", objectFields(objectReference, options.child(), visiting));
            }
            return payload;
        } catch (Throwable t) {
            payload.put("error", t.getMessage());
            return payload;
        } finally {
            visiting.remove(objectId);
        }
    }

    static String valueTypeName(Value value) {
        if (value == null) return null;
        try {
            return value.type().name();
        } catch (Throwable t) {
            return null;
        }
    }

    static SelectedFrame selectedFrame(XDebugSession session, Map<String, String> query) throws Exception {
        int requestedFrame = intParam(query, "frame", 0, 0, 512);
        String requestedThread = query.get("thread");
        XStackFrame currentFrame = session.getCurrentStackFrame();
        if (requestedThread == null || requestedThread.isBlank()) {
            if (!(currentFrame instanceof JavaStackFrame javaFrame)) return null;
            ThreadReferenceProxyImpl thread = javaFrame.getStackFrameProxy().threadProxy();
            return frameFromThread(thread, 0, requestedFrame);
        }

        XSuspendContext context = session.getSuspendContext();
        XExecutionStack[] stacks = context == null ? XExecutionStack.EMPTY_ARRAY : context.getExecutionStacks();
        Integer requestedIndex = parseIndex(requestedThread);
        for (int i = 0; i < stacks.length; i++) {
            XExecutionStack stack = stacks[i];
            if (requestedIndex != null) {
                if (requestedIndex != i) continue;
            } else if (!requestedThread.equals(stack.getDisplayName())) {
                continue;
            }
            XStackFrame top = stack.getTopFrame();
            if (!(top instanceof JavaStackFrame javaFrame)) return null;
            ThreadReferenceProxyImpl thread = javaFrame.getStackFrameProxy().threadProxy();
            return frameFromThread(thread, i, requestedFrame);
        }
        throw new IllegalArgumentException("thread not found: " + requestedThread);
    }

    static EvaluationResult evaluatePath(StackFrameProxyImpl proxy, String expression) throws Exception {
        String expr = expression.trim();
        if (expr.isEmpty()) throw new IllegalArgumentException("empty expression");
        int pos = 0;
        while (pos < expr.length() && expr.charAt(pos) != '.' && expr.charAt(pos) != '[') pos++;
        String base = expr.substring(0, pos);
        Value value;
        String declaredType = null;
        if ("this".equals(base)) {
            value = proxy.thisObject();
            declaredType = valueTypeName(value);
        } else {
            LocalVariableProxyImpl variable = null;
            for (LocalVariableProxyImpl local : proxy.visibleVariables()) {
                if (base.equals(local.name())) {
                    variable = local;
                    break;
                }
            }
            if (variable == null) throw new IllegalArgumentException("unknown local: " + base);
            value = proxy.getValue(variable);
            declaredType = variable.typeName();
        }

        while (pos < expr.length()) {
            char ch = expr.charAt(pos);
            if (ch == '.') {
                int start = ++pos;
                while (pos < expr.length() && expr.charAt(pos) != '.' && expr.charAt(pos) != '[') pos++;
                String fieldName = expr.substring(start, pos);
                if (fieldName.isBlank()) throw new IllegalArgumentException("empty field in expression: " + expression);
                if (!(value instanceof ObjectReference objectReference)) {
                    throw new IllegalArgumentException("cannot read field " + fieldName + " from non-object value");
                }
                Field field = findField(objectReference, fieldName);
                if (field == null) throw new IllegalArgumentException("field not found: " + fieldName);
                value = objectReference.getValue(field);
                declaredType = field.typeName();
            } else if (ch == '[') {
                int end = expr.indexOf(']', pos);
                if (end < 0) throw new IllegalArgumentException("missing closing ] in expression: " + expression);
                int index;
                try {
                    index = Integer.parseInt(expr.substring(pos + 1, end).trim());
                } catch (NumberFormatException e) {
                    throw new IllegalArgumentException("array index must be an integer");
                }
                if (!(value instanceof ArrayReference arrayReference)) {
                    throw new IllegalArgumentException("cannot index non-array value");
                }
                if (index < 0 || index >= arrayReference.length()) {
                    throw new IllegalArgumentException("array index out of bounds: " + index);
                }
                value = arrayReference.getValue(index);
                declaredType = valueTypeName(value);
                pos = end + 1;
            } else {
                throw new IllegalArgumentException("unsupported expression syntax near: " + expr.substring(pos));
            }
        }
        return new EvaluationResult(value, declaredType);
    }

    private static List<Object> arrayItems(ArrayReference arrayReference, RenderOptions options, Set<Long> visiting) {
        List<Object> items = new ArrayList<>();
        int count = Math.min(arrayReference.length(), options.maxArrayItems);
        for (int i = 0; i < count; i++) {
            items.add(valueToMap("[" + i + "]", arrayReference.getValue(i), null, options, visiting));
        }
        return items;
    }

    private static List<Object> objectFields(ObjectReference objectReference, RenderOptions options, Set<Long> visiting) {
        List<Object> fields = new ArrayList<>();
        int instanceFieldCount = 0;
        for (Field field : objectReference.referenceType().allFields()) {
            if (field.isStatic()) continue;
            instanceFieldCount++;
            if (fields.size() >= options.maxFields) continue;
            fields.add(fieldToMap(objectReference, field, options, visiting));
        }
        int truncated = instanceFieldCount - fields.size();
        if (truncated > 0) {
            fields.add(map("name", "<truncated>", "truncated_fields", truncated));
        }
        return fields;
    }

    private static Map<String, Object> fieldToMap(ObjectReference objectReference, Field field, RenderOptions options, Set<Long> visiting) {
        try {
            return valueToMap(field.name(), objectReference.getValue(field), field.typeName(), options, visiting);
        } catch (Throwable t) {
            return map(
                "name", field.name(),
                "declared_type", field.typeName(),
                "error", t.getMessage()
            );
        }
    }

    private static String valueText(Value value) {
        if (value == null) return null;
        if (value instanceof StringReference stringReference) return stringReference.value();
        return value.toString();
    }

    private static SelectedFrame frameFromThread(ThreadReferenceProxyImpl thread, int threadIndex, int frameIndex) throws Exception {
        List<StackFrameProxyImpl> frames = thread.frames();
        if (frameIndex >= frames.size()) {
            throw new IllegalArgumentException("frame index out of bounds: " + frameIndex);
        }
        return new SelectedFrame(frames.get(frameIndex), threadIndex, frameIndex, thread.name());
    }

    private static Integer parseIndex(String value) {
        if (value == null || value.isBlank()) return null;
        try {
            return Integer.parseInt(value);
        } catch (NumberFormatException ignored) {
            return null;
        }
    }

    private static Field findField(ObjectReference objectReference, String fieldName) {
        for (Field field : objectReference.referenceType().allFields()) {
            if (fieldName.equals(field.name())) return field;
        }
        return null;
    }

    record SelectedFrame(StackFrameProxyImpl proxy, int threadIndex, int frameIndex, String threadName) {
        Map<String, Object> info() {
            return map("thread", threadIndex, "thread_name", threadName, "frame", frameIndex);
        }
    }

    record EvaluationResult(Value value, String declaredType) {
    }

    record RenderOptions(int depth, int maxFields, int maxArrayItems) {
        static RenderOptions shallow() {
            return new RenderOptions(0, 64, 32);
        }

        RenderOptions child() {
            return new RenderOptions(Math.max(0, depth - 1), maxFields, maxArrayItems);
        }
    }
}
