package io.github.andriyo.shadowdroid.studio;

import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.bad;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.booleanParam;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.intParam;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.map;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.ok;
import static io.github.andriyo.shadowdroid.studio.StudioThreading.onIdeaThread;

import com.intellij.debugger.ui.breakpoints.JavaExceptionBreakpointType;
import com.intellij.debugger.ui.breakpoints.JavaFieldBreakpointType;
import com.intellij.debugger.ui.breakpoints.JavaLineBreakpointType;
import com.intellij.debugger.ui.breakpoints.JavaWildcardMethodBreakpointType;
import com.intellij.openapi.project.Project;
import com.intellij.openapi.vfs.LocalFileSystem;
import com.intellij.openapi.vfs.VirtualFile;
import com.intellij.xdebugger.XDebuggerManager;
import com.intellij.xdebugger.XSourcePosition;
import com.intellij.xdebugger.breakpoints.SuspendPolicy;
import com.intellij.xdebugger.breakpoints.XBreakpoint;
import com.intellij.xdebugger.breakpoints.XBreakpointType;
import com.intellij.xdebugger.breakpoints.XLineBreakpoint;
import org.jetbrains.java.debugger.breakpoints.properties.JavaBreakpointProperties;
import org.jetbrains.java.debugger.breakpoints.properties.JavaExceptionBreakpointProperties;
import org.jetbrains.java.debugger.breakpoints.properties.JavaFieldBreakpointProperties;
import org.jetbrains.java.debugger.breakpoints.properties.JavaLineBreakpointProperties;
import org.jetbrains.java.debugger.breakpoints.properties.JavaMethodBreakpointProperties;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

final class BreakpointBridge {
    private static final ConcurrentMap<String, Integer> BREAKPOINT_HITS = new ConcurrentHashMap<>();
    private static final ConcurrentMap<String, Long> BREAKPOINT_LAST_HIT = new ConcurrentHashMap<>();

    private BreakpointBridge() {
    }

    static void recordHit(Project project, XBreakpoint<?> breakpoint) {
        String id = breakpointId(project, breakpoint);
        BREAKPOINT_HITS.merge(id, 1, Integer::sum);
        BREAKPOINT_LAST_HIT.put(id, System.currentTimeMillis());
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Response addLine(Map<String, String> query, Project project) {
        String file = query.get("file");
        if (file == null || file.isBlank()) return bad("missing file");
        int line = intParam(query, "line", -1, 1, Integer.MAX_VALUE);
        if (line < 1) return bad("missing or invalid line");
        boolean enabled = booleanParam(query, "enabled", true);
        boolean temporary = booleanParam(query, "temporary", false);
        String condition = query.get("condition");
        boolean clearCondition = booleanParam(query, "clear_condition", false);
        if (project == null) return bad("no project");
        try {
            XLineBreakpoint<?> breakpoint = onIdeaThread(() -> {
                VirtualFile virtualFile = LocalFileSystem.getInstance().refreshAndFindFileByIoFile(new File(file));
                if (virtualFile == null) throw new IllegalArgumentException("file not found in IDE VFS: " + file);
                JavaLineBreakpointType type = breakpointType(JavaLineBreakpointType.class);
                if (type == null) throw new IllegalStateException("Java line breakpoint type is not available");
                JavaLineBreakpointProperties props = type.createBreakpointProperties(virtualFile, line - 1);
                XLineBreakpoint<?> target = findLineBreakpoint(project, virtualFile.getUrl(), line - 1, type.getId());
                if (target == null) {
                    target = XDebuggerManager.getInstance(project).getBreakpointManager()
                        .addLineBreakpoint(type, virtualFile.getUrl(), line - 1, props, temporary);
                }
                target.setEnabled(enabled);
                if (clearCondition) {
                    target.setCondition(null);
                } else if (condition != null) {
                    target.setCondition(condition.isBlank() ? null : condition);
                }
                return target;
            });
            return ok("ok", true, "breakpoint", breakpointInfo(project, breakpoint));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Response addException(Map<String, String> query, Project project) {
        String exception = query.get("exception");
        if (exception == null || exception.isBlank()) return bad("missing exception");
        if (project == null) return bad("no project");
        try {
            XBreakpoint<?> breakpoint = onIdeaThread(() -> {
                JavaExceptionBreakpointType type = breakpointType(JavaExceptionBreakpointType.class);
                if (type == null) throw new IllegalStateException("Java exception breakpoint type is not available");
                JavaExceptionBreakpointProperties props = new JavaExceptionBreakpointProperties(exception);
                props.NOTIFY_CAUGHT = booleanParam(query, "caught", true);
                props.NOTIFY_UNCAUGHT = booleanParam(query, "uncaught", true);
                XBreakpoint<?> created = XDebuggerManager.getInstance(project).getBreakpointManager()
                    .addBreakpoint((XBreakpointType) type, props);
                created.setEnabled(booleanParam(query, "enabled", true));
                return created;
            });
            return ok("ok", true, "breakpoint", breakpointInfo(project, breakpoint));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Response addMethod(Map<String, String> query, Project project) {
        String classPattern = query.get("class");
        String method = query.get("method");
        if (classPattern == null || classPattern.isBlank()) return bad("missing class");
        if (method == null || method.isBlank()) return bad("missing method");
        if (project == null) return bad("no project");
        try {
            XBreakpoint<?> breakpoint = onIdeaThread(() -> {
                JavaWildcardMethodBreakpointType type = breakpointType(JavaWildcardMethodBreakpointType.class);
                if (type == null) throw new IllegalStateException("Java wildcard method breakpoint type is not available");
                JavaMethodBreakpointProperties props = new JavaMethodBreakpointProperties(classPattern, method);
                props.WATCH_ENTRY = booleanParam(query, "entry", true);
                props.WATCH_EXIT = booleanParam(query, "exit", false);
                XBreakpoint<?> created = XDebuggerManager.getInstance(project).getBreakpointManager()
                    .addBreakpoint((XBreakpointType) type, props);
                created.setEnabled(booleanParam(query, "enabled", true));
                return created;
            });
            return ok("ok", true, "breakpoint", breakpointInfo(project, breakpoint));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Response addField(Map<String, String> query, Project project) {
        String file = query.get("file");
        String className = query.get("class");
        String field = query.get("field");
        if (file == null || file.isBlank()) return bad("missing file");
        if (className == null || className.isBlank()) return bad("missing class");
        if (field == null || field.isBlank()) return bad("missing field");
        int line = intParam(query, "line", -1, 1, Integer.MAX_VALUE);
        if (line < 1) return bad("missing or invalid line");
        boolean temporary = booleanParam(query, "temporary", false);
        if (project == null) return bad("no project");
        try {
            XLineBreakpoint<?> target = onIdeaThread(() -> {
                JavaFieldBreakpointType type = breakpointType(JavaFieldBreakpointType.class);
                if (type == null) throw new IllegalStateException("Java field breakpoint type is not available");
                VirtualFile virtualFile = LocalFileSystem.getInstance().refreshAndFindFileByIoFile(new File(file));
                if (virtualFile == null) throw new IllegalArgumentException("file not found in IDE VFS: " + file);
                JavaFieldBreakpointProperties props = new JavaFieldBreakpointProperties(className, field);
                props.WATCH_ACCESS = booleanParam(query, "access", false);
                props.WATCH_MODIFICATION = booleanParam(query, "modification", true);
                XLineBreakpoint<?> breakpoint = findLineBreakpoint(project, virtualFile.getUrl(), line - 1, type.getId());
                if (breakpoint == null) {
                    breakpoint = XDebuggerManager.getInstance(project).getBreakpointManager()
                        .addLineBreakpoint(type, virtualFile.getUrl(), line - 1, props, temporary);
                }
                breakpoint.setEnabled(booleanParam(query, "enabled", true));
                breakpoint.setTemporary(temporary);
                return breakpoint;
            });
            return ok("ok", true, "breakpoint", breakpointInfo(project, target));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    static Response list(List<Project> projects) {
        List<Object> payload = new ArrayList<>();
        for (Project project : projects) {
            for (XBreakpoint<?> breakpoint : XDebuggerManager.getInstance(project).getBreakpointManager().getAllBreakpoints()) {
                if (breakpoint instanceof XLineBreakpoint<?> lineBreakpoint) {
                    payload.add(breakpointInfo(project, lineBreakpoint));
                }
            }
        }
        return ok("ok", true, "breakpoints", payload);
    }

    static Response update(Map<String, String> query, List<Project> projects, Project requestedProject) {
        ProjectBreakpoint selected = findBreakpoint(query, projects, requestedProject);
        if (selected == null) return bad("breakpoint not found");
        XBreakpoint<?> breakpoint = selected.breakpoint;
        try {
            onIdeaThread(() -> {
                if (query.containsKey("enabled")) breakpoint.setEnabled(booleanParam(query, "enabled", breakpoint.isEnabled()));
                if (breakpoint instanceof XLineBreakpoint<?> lineBreakpoint && query.containsKey("temporary")) {
                    lineBreakpoint.setTemporary(booleanParam(query, "temporary", lineBreakpoint.isTemporary()));
                }
                if (booleanParam(query, "clear_condition", false)) {
                    breakpoint.setCondition(null);
                } else if (query.containsKey("condition")) {
                    String condition = query.get("condition");
                    breakpoint.setCondition(condition == null || condition.isBlank() ? null : condition);
                }
                if (booleanParam(query, "clear_log_expression", false)) {
                    breakpoint.setLogExpression(null);
                } else if (query.containsKey("log_expression")) {
                    String expression = query.get("log_expression");
                    breakpoint.setLogExpression(expression == null || expression.isBlank() ? null : expression);
                }
                if (query.containsKey("log_message")) breakpoint.setLogMessage(booleanParam(query, "log_message", breakpoint.isLogMessage()));
                if (query.containsKey("log_stack")) breakpoint.setLogStack(booleanParam(query, "log_stack", breakpoint.isLogStack()));
                if (query.containsKey("suspend")) breakpoint.setSuspendPolicy(SuspendPolicy.valueOf(query.get("suspend")));
                if (query.containsKey("pass_count") && breakpoint.getProperties() instanceof JavaBreakpointProperties<?> props) {
                    int count = intParam(query, "pass_count", 0, 0, Integer.MAX_VALUE);
                    props.setCOUNT_FILTER_ENABLED(count > 0);
                    props.setCOUNT_FILTER(count);
                }
                return null;
            });
            return ok("ok", true, "breakpoint", breakpointInfo(selected.project, breakpoint));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    static Response remove(Map<String, String> query, List<Project> projects, Project requestedProject) {
        ProjectBreakpoint selected = findBreakpoint(query, projects, requestedProject);
        if (selected == null) return bad("breakpoint not found");
        try {
            onIdeaThread(() -> {
                XDebuggerManager.getInstance(selected.project).getBreakpointManager().removeBreakpoint(selected.breakpoint);
                return null;
            });
            return ok("ok", true, "removed", true, "id", query.get("id"));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    private static ProjectBreakpoint findBreakpoint(Map<String, String> query, List<Project> projects, Project requestedProject) {
        String id = query.get("id");
        if (id == null || id.isBlank()) return null;
        for (Project project : projects) {
            if (requestedProject != null && requestedProject != project) continue;
            for (XBreakpoint<?> breakpoint : XDebuggerManager.getInstance(project).getBreakpointManager().getAllBreakpoints()) {
                if (id.equals(breakpointId(project, breakpoint))) {
                    return new ProjectBreakpoint(project, breakpoint);
                }
            }
        }
        return null;
    }

    private static XLineBreakpoint<?> findLineBreakpoint(Project project, String fileUrl, int zeroBasedLine, String typeId) {
        for (XBreakpoint<?> breakpoint : XDebuggerManager.getInstance(project).getBreakpointManager().getAllBreakpoints()) {
            if (breakpoint instanceof XLineBreakpoint<?> lineBreakpoint
                && fileUrl.equals(lineBreakpoint.getFileUrl())
                && lineBreakpoint.getLine() == zeroBasedLine
                && typeId.equals(lineBreakpoint.getType().getId())) {
                return lineBreakpoint;
            }
        }
        return null;
    }

    private static <T> T breakpointType(Class<T> klass) {
        for (XBreakpointType<?, ?> candidate : XBreakpointType.EXTENSION_POINT_NAME.getExtensionList()) {
            if (klass.isInstance(candidate)) return klass.cast(candidate);
        }
        return null;
    }

    private static Map<String, Object> breakpointInfo(Project project, XBreakpoint<?> breakpoint) {
        XSourcePosition pos = breakpoint.getSourcePosition();
        String fileUrl = breakpoint instanceof XLineBreakpoint<?> lineBreakpoint ? lineBreakpoint.getFileUrl() : null;
        Integer line = breakpoint instanceof XLineBreakpoint<?> lineBreakpoint ? lineBreakpoint.getLine() + 1 : null;
        Boolean temporary = breakpoint instanceof XLineBreakpoint<?> lineBreakpoint ? lineBreakpoint.isTemporary() : null;
        JavaBreakpointProperties<?> javaProps = breakpoint.getProperties() instanceof JavaBreakpointProperties<?> props ? props : null;
        String id = breakpointId(project, breakpoint);
        return map(
            "id", id,
            "project", projectInfo(project),
            "type", breakpoint.getType().getId(),
            "enabled", breakpoint.isEnabled(),
            "temporary", temporary,
            "condition", breakpoint.getConditionExpression() == null ? null : breakpoint.getConditionExpression().getExpression(),
            "log_message", breakpoint.isLogMessage(),
            "log_stack", breakpoint.isLogStack(),
            "log_expression", breakpoint.getLogExpressionObject() == null ? null : breakpoint.getLogExpressionObject().getExpression(),
            "suspend_policy", breakpoint.getSuspendPolicy() == null ? null : breakpoint.getSuspendPolicy().name(),
            "pass_count_enabled", javaProps == null ? null : javaProps.isCOUNT_FILTER_ENABLED(),
            "pass_count", javaProps == null ? null : javaProps.getCOUNT_FILTER(),
            "hit_count", BREAKPOINT_HITS.getOrDefault(id, 0),
            "last_hit_at", BREAKPOINT_LAST_HIT.get(id),
            "hit_count_source", "shadowdroid_observed_session_pauses",
            "properties", breakpointPropertiesInfo(breakpoint.getProperties()),
            "file", pos == null ? null : pos.getFile().getPath(),
            "url", fileUrl,
            "line", line,
            "timestamp", breakpoint.getTimeStamp()
        );
    }

    private static String breakpointId(Project project, XBreakpoint<?> breakpoint) {
        XSourcePosition pos = breakpoint.getSourcePosition();
        String fileUrl = breakpoint instanceof XLineBreakpoint<?> lineBreakpoint ? lineBreakpoint.getFileUrl() : "";
        int line = breakpoint instanceof XLineBreakpoint<?> lineBreakpoint ? lineBreakpoint.getLine() + 1 : -1;
        String raw = String.join("|",
            project.getBasePath() == null ? project.getName() : project.getBasePath(),
            breakpoint.getType().getId(),
            fileUrl == null ? "" : fileUrl,
            pos == null || pos.getFile() == null ? "" : pos.getFile().getPath(),
            Integer.toString(line),
            breakpointIdentityDetails(breakpoint.getProperties())
        );
        return "bp_" + Base64.getUrlEncoder().withoutPadding().encodeToString(raw.getBytes(StandardCharsets.UTF_8));
    }

    private static String breakpointIdentityDetails(Object props) {
        if (props instanceof JavaExceptionBreakpointProperties exceptionProps) {
            return "exception:" + exceptionProps.myQualifiedName;
        }
        if (props instanceof JavaMethodBreakpointProperties methodProps) {
            return "method:" + methodProps.myClassPattern + "#" + methodProps.myMethodName;
        }
        if (props instanceof JavaFieldBreakpointProperties fieldProps) {
            return "field:" + fieldProps.myClassName + "#" + fieldProps.myFieldName;
        }
        return "";
    }

    private static Map<String, Object> breakpointPropertiesInfo(Object props) {
        if (props instanceof JavaExceptionBreakpointProperties exceptionProps) {
            return map(
                "kind", "exception",
                "exception", exceptionProps.myQualifiedName,
                "caught", exceptionProps.NOTIFY_CAUGHT,
                "uncaught", exceptionProps.NOTIFY_UNCAUGHT
            );
        }
        if (props instanceof JavaMethodBreakpointProperties methodProps) {
            return map(
                "kind", "method",
                "class", methodProps.myClassPattern,
                "method", methodProps.myMethodName,
                "entry", methodProps.WATCH_ENTRY,
                "exit", methodProps.WATCH_EXIT
            );
        }
        if (props instanceof JavaFieldBreakpointProperties fieldProps) {
            return map(
                "kind", "field",
                "class", fieldProps.myClassName,
                "field", fieldProps.myFieldName,
                "access", fieldProps.WATCH_ACCESS,
                "modification", fieldProps.WATCH_MODIFICATION
            );
        }
        return null;
    }

    private static Map<String, Object> projectInfo(Project project) {
        return map(
            "name", project.getName(),
            "base_path", project.getBasePath(),
            "disposed", project.isDisposed()
        );
    }

    private record ProjectBreakpoint(Project project, XBreakpoint<?> breakpoint) {
    }
}
