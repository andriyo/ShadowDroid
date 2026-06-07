package io.github.andriyo.shadowdroid.studio;

import com.intellij.debugger.engine.JavaStackFrame;
import com.intellij.debugger.jdi.LocalVariableProxyImpl;
import com.intellij.debugger.jdi.StackFrameProxyImpl;
import com.intellij.debugger.jdi.ThreadReferenceProxyImpl;
import com.intellij.debugger.ui.breakpoints.JavaLineBreakpointType;
import com.intellij.openapi.actionSystem.ActionManager;
import com.intellij.openapi.actionSystem.ActionPlaces;
import com.intellij.openapi.actionSystem.AnAction;
import com.intellij.openapi.actionSystem.AnActionEvent;
import com.intellij.openapi.actionSystem.CommonDataKeys;
import com.intellij.openapi.actionSystem.DataContext;
import com.intellij.openapi.actionSystem.impl.SimpleDataContext;
import com.intellij.openapi.application.Application;
import com.intellij.openapi.application.ApplicationManager;
import com.intellij.openapi.project.Project;
import com.intellij.openapi.startup.StartupActivity;
import com.intellij.openapi.vfs.LocalFileSystem;
import com.intellij.openapi.vfs.VirtualFile;
import com.intellij.xdebugger.XDebugSession;
import com.intellij.xdebugger.XDebuggerManager;
import com.intellij.xdebugger.XSourcePosition;
import com.intellij.xdebugger.breakpoints.XBreakpoint;
import com.intellij.xdebugger.breakpoints.XBreakpointType;
import com.intellij.xdebugger.breakpoints.XLineBreakpoint;
import com.intellij.xdebugger.frame.XExecutionStack;
import com.intellij.xdebugger.frame.XStackFrame;
import com.intellij.xdebugger.frame.XSuspendContext;
import com.sun.jdi.Location;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.StringReference;
import com.sun.jdi.Value;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import org.jetbrains.java.debugger.breakpoints.properties.JavaLineBreakpointProperties;

import java.io.File;
import java.io.IOException;
import java.net.HttpURLConnection;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicReference;

public final class ShadowDroidDebuggerBridge implements StartupActivity {
    private static final int DEFAULT_PORT = 50576;
    private static final int API_VERSION = 1;
    private static final CopyOnWriteArrayList<Project> PROJECTS = new CopyOnWriteArrayList<>();
    private static final Object LOCK = new Object();

    private static volatile HttpServer server;
    private static volatile String serverUrl;

    @Override
    public void runActivity(Project project) {
        registerProject(project);
    }

    private static void registerProject(Project project) {
        if (!PROJECTS.contains(project)) {
            PROJECTS.add(project);
        }
        ensureStarted();
        writeRegistry();
    }

    private static void ensureStarted() {
        if (server != null) return;
        synchronized (LOCK) {
            if (server != null) return;
            int preferredPort = preferredPort();
            HttpServer created = createServer(preferredPort);
            if (created == null) created = createServer(0);
            if (created == null) throw new IllegalStateException("unable to start ShadowDroid debugger bridge");

            created.createContext("/", ShadowDroidDebuggerBridge::handle);
            created.setExecutor(Executors.newCachedThreadPool(runnable -> {
                Thread thread = new Thread(runnable, "ShadowDroid debugger bridge");
                thread.setDaemon(true);
                return thread;
            }));
            created.start();
            server = created;
            serverUrl = "http://127.0.0.1:" + created.getAddress().getPort();
        }
    }

    private static int preferredPort() {
        String property = System.getProperty("shadowdroid.debugger.port");
        if (property == null || property.isBlank()) {
            property = System.getenv("SHADOWDROID_STUDIO_DEBUGGER_PORT");
        }
        if (property != null) {
            try {
                return Integer.parseInt(property);
            } catch (NumberFormatException ignored) {
            }
        }
        return DEFAULT_PORT;
    }

    private static HttpServer createServer(int port) {
        try {
            return HttpServer.create(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), port), 0);
        } catch (IOException ignored) {
            return null;
        }
    }

    private static void handle(HttpExchange exchange) {
        try {
            String path = exchange.getRequestURI().getPath();
            Map<String, String> query = parseQuery(exchange.getRequestURI().getRawQuery());
            Response response;
            switch (path) {
                case "/v1/status" -> response = status();
                case "/v1/sessions" -> response = sessions();
                case "/v1/session/control" -> response = controlSession(query);
                case "/v1/session/stack" -> response = currentStack(query);
                case "/v1/session/threads" -> response = threads(query);
                case "/v1/session/variables" -> response = variables(query);
                case "/v1/breakpoints" -> response = breakpoints();
                case "/v1/breakpoints/line" -> response = addLineBreakpoint(query);
                case "/v1/attach" -> response = openAndroidAttachDebugger(query);
                default -> response = new Response(HttpURLConnection.HTTP_NOT_FOUND, obj("ok", false, "error", "not_found", "path", path));
            }
            send(exchange, response.status, response.body);
        } catch (Throwable t) {
            send(exchange, HttpURLConnection.HTTP_INTERNAL_ERROR, obj("ok", false, "error", t.getMessage() == null ? t.getClass().getName() : t.getMessage()));
        }
    }

    private static Response status() {
        List<Object> sessionPayload = new ArrayList<>();
        List<XDebugSession> sessions = allSessions();
        for (int i = 0; i < sessions.size(); i++) {
            sessionPayload.add(sessionInfo(i, sessions.get(i)));
        }
        return ok("ok", true, "api_version", API_VERSION, "url", serverUrl, "projects", projectPayload(), "sessions", sessionPayload);
    }

    private static Response sessions() {
        List<Object> payload = new ArrayList<>();
        List<XDebugSession> sessions = allSessions();
        for (int i = 0; i < sessions.size(); i++) {
            payload.add(sessionInfo(i, sessions.get(i)));
        }
        return ok("ok", true, "sessions", payload);
    }

    private static Response controlSession(Map<String, String> query) {
        String action = query.get("action");
        if (action == null) return bad("missing action");
        XDebugSession session = selectSession(query);
        if (session == null) return bad("no debugger session");
        try {
            onIdeaThread(() -> {
                switch (action) {
                    case "pause" -> session.pause();
                    case "resume" -> session.resume();
                    case "step_over" -> session.stepOver(false);
                    case "step_into" -> session.stepInto();
                    case "step_out" -> session.stepOut();
                    case "stop" -> session.stop();
                    default -> throw new IllegalArgumentException("unsupported action: " + action);
                }
                return null;
            });
            return ok("ok", true, "action", action, "session", sessionInfo(sessionIndex(session), session));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    private static Response currentStack(Map<String, String> query) {
        XDebugSession session = selectSession(query);
        if (session == null) return bad("no debugger session");
        int limit = intParam(query, "limit", 64, 1, 512);
        XStackFrame frame = session.getCurrentStackFrame();
        List<Object> frames = new ArrayList<>();
        if (frame instanceof JavaStackFrame javaFrame) {
            frames.addAll(javaFrames(javaFrame, limit));
        } else if (frame != null) {
            frames.add(frameInfo(frame, 0));
        }
        return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "frames", frames);
    }

    private static Response threads(Map<String, String> query) {
        XDebugSession session = selectSession(query);
        if (session == null) return bad("no debugger session");
        int limit = intParam(query, "limit", 32, 1, 128);
        XSuspendContext context = session.getSuspendContext();
        XExecutionStack[] stacks = context == null ? XExecutionStack.EMPTY_ARRAY : context.getExecutionStacks();
        List<Object> payload = new ArrayList<>();
        for (int i = 0; i < stacks.length; i++) {
            XStackFrame top = stacks[i].getTopFrame();
            List<Object> frames = new ArrayList<>();
            if (top instanceof JavaStackFrame javaFrame) {
                frames.addAll(javaFrames(javaFrame, limit));
            } else if (top != null) {
                frames.add(frameInfo(top, 0));
            }
            payload.add(map("index", i, "name", stacks[i].getDisplayName(), "top_frame", top == null ? null : frameInfo(top, 0), "frames", frames));
        }
        return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "threads", payload);
    }

    private static Response variables(Map<String, String> query) {
        XDebugSession session = selectSession(query);
        if (session == null) return bad("no debugger session");
        XStackFrame frame = session.getCurrentStackFrame();
        if (!(frame instanceof JavaStackFrame javaFrame)) {
            return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "variables", Collections.emptyList(), "warning", "current frame is not a Java/Kotlin frame");
        }
        try {
            StackFrameProxyImpl proxy = javaFrame.getStackFrameProxy();
            List<Object> locals = new ArrayList<>();
            for (LocalVariableProxyImpl local : proxy.visibleVariables()) {
                Value value = proxy.getValue(local);
                locals.add(map(
                    "name", local.name(),
                    "declared_type", local.typeName(),
                    "type", value == null ? null : value.type().name(),
                    "value", valueText(value)
                ));
            }
            ObjectReference thisObject = proxy.thisObject();
            return ok(
                "ok", true,
                "session", sessionInfo(sessionIndex(session), session),
                "this", thisObject == null ? null : valueToMap("this", thisObject),
                "variables", locals
            );
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    private static Response addLineBreakpoint(Map<String, String> query) {
        String file = query.get("file");
        if (file == null || file.isBlank()) return bad("missing file");
        int line = intParam(query, "line", -1, 1, Integer.MAX_VALUE);
        if (line < 1) return bad("missing or invalid line");
        boolean enabled = booleanParam(query, "enabled", true);
        boolean temporary = booleanParam(query, "temporary", false);
        Project project = selectProject(query, file);
        if (project == null) return bad("no project");
        try {
            XLineBreakpoint<?> breakpoint = onIdeaThread(() -> {
                VirtualFile virtualFile = LocalFileSystem.getInstance().refreshAndFindFileByIoFile(new File(file));
                if (virtualFile == null) throw new IllegalArgumentException("file not found in IDE VFS: " + file);
                JavaLineBreakpointType type = null;
                for (XBreakpointType<?, ?> candidate : XBreakpointType.EXTENSION_POINT_NAME.getExtensionList()) {
                    if (candidate instanceof JavaLineBreakpointType javaType) {
                        type = javaType;
                        break;
                    }
                }
                if (type == null) throw new IllegalStateException("Java line breakpoint type is not available");
                JavaLineBreakpointProperties props = type.createBreakpointProperties(virtualFile, line - 1);
                XLineBreakpoint<?> created = XDebuggerManager.getInstance(project).getBreakpointManager()
                    .addLineBreakpoint(type, virtualFile.getUrl(), line - 1, props, temporary);
                created.setEnabled(enabled);
                return created;
            });
            return ok("ok", true, "breakpoint", breakpointInfo(project, breakpoint));
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    private static Response breakpoints() {
        List<Object> payload = new ArrayList<>();
        for (Project project : liveProjects()) {
            for (XBreakpoint<?> breakpoint : XDebuggerManager.getInstance(project).getBreakpointManager().getAllBreakpoints()) {
                if (breakpoint instanceof XLineBreakpoint<?> lineBreakpoint) {
                    payload.add(breakpointInfo(project, lineBreakpoint));
                }
            }
        }
        return ok("ok", true, "breakpoints", payload);
    }

    private static Response openAndroidAttachDebugger(Map<String, String> query) {
        Project project = selectProject(query, null);
        if (project == null) return bad("no project");
        AnAction action = ActionManager.getInstance().getAction("AndroidConnectDebuggerAction");
        if (action == null) return bad("AndroidConnectDebuggerAction is not available");
        ApplicationManager.getApplication().invokeLater(() -> {
            DataContext dataContext = SimpleDataContext.builder()
                .add(CommonDataKeys.PROJECT, project)
                .build();
            AnActionEvent event = AnActionEvent.createFromAnAction(action, null, ActionPlaces.UNKNOWN, dataContext);
            action.actionPerformed(event);
        });
        return ok("ok", true, "action", "AndroidConnectDebuggerAction", "project", projectInfo(project));
    }

    private static List<Object> javaFrames(JavaStackFrame frame, int limit) {
        try {
            ThreadReferenceProxyImpl thread = frame.getStackFrameProxy().threadProxy();
            List<Object> frames = new ArrayList<>();
            int index = 0;
            for (StackFrameProxyImpl stackFrame : thread.frames()) {
                if (index >= limit) break;
                frames.add(frameInfo(stackFrame.location(), index, thread.name()));
                index++;
            }
            return frames;
        } catch (Throwable t) {
            return List.of(map("error", t.getMessage()));
        }
    }

    private static Map<String, Object> frameInfo(XStackFrame frame, int index) {
        XSourcePosition pos = frame.getSourcePosition();
        return map(
            "index", index,
            "kind", frame.getClass().getName(),
            "file", pos == null ? null : pos.getFile().getPath(),
            "line", pos == null ? null : pos.getLine() + 1
        );
    }

    private static Map<String, Object> frameInfo(Location location, int index, String threadName) {
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

    private static Map<String, Object> valueToMap(String name, Value value) {
        return map("name", name, "type", value == null ? null : value.type().name(), "value", valueText(value));
    }

    private static String valueText(Value value) {
        if (value == null) return null;
        if (value instanceof StringReference stringReference) return stringReference.value();
        return value.toString();
    }

    private static Map<String, Object> sessionInfo(int index, XDebugSession session) {
        XSourcePosition pos = session.getCurrentPosition();
        if (pos == null && session.getCurrentStackFrame() != null) {
            pos = session.getCurrentStackFrame().getSourcePosition();
        }
        return map(
            "index", index,
            "name", session.getSessionName(),
            "project", projectInfo(session.getProject()),
            "suspended", session.isSuspended(),
            "mixed_mode", session.isMixedMode(),
            "process", session.getDebugProcess().getClass().getName(),
            "position", sourcePositionInfo(pos)
        );
    }

    private static Map<String, Object> sourcePositionInfo(XSourcePosition pos) {
        if (pos == null) return null;
        return map(
            "file", pos.getFile().getPath(),
            "url", pos.getFile().getUrl(),
            "line", pos.getLine() + 1,
            "offset", pos.getOffset()
        );
    }

    private static Map<String, Object> breakpointInfo(Project project, XLineBreakpoint<?> breakpoint) {
        XSourcePosition pos = breakpoint.getSourcePosition();
        return map(
            "project", projectInfo(project),
            "type", breakpoint.getType().getId(),
            "enabled", breakpoint.isEnabled(),
            "temporary", breakpoint.isTemporary(),
            "file", pos == null ? null : pos.getFile().getPath(),
            "url", breakpoint.getFileUrl(),
            "line", breakpoint.getLine() + 1
        );
    }

    private static Map<String, Object> projectInfo(Project project) {
        return map(
            "name", project.getName(),
            "base_path", project.getBasePath(),
            "disposed", project.isDisposed()
        );
    }

    private static XDebugSession selectSession(Map<String, String> query) {
        List<XDebugSession> sessions = allSessions();
        String index = query.get("session");
        if (index != null) {
            try {
                int parsed = Integer.parseInt(index);
                if (parsed >= 0 && parsed < sessions.size()) return sessions.get(parsed);
            } catch (NumberFormatException ignored) {
            }
        }
        for (Project project : liveProjects()) {
            XDebugSession current = XDebuggerManager.getInstance(project).getCurrentSession();
            if (current != null) return current;
        }
        return sessions.isEmpty() ? null : sessions.get(0);
    }

    private static Project selectProject(Map<String, String> query, String file) {
        String requested = query.get("project");
        if (requested != null) {
            for (Project project : liveProjects()) {
                if (requested.equals(project.getName()) || requested.equals(project.getBasePath())) {
                    return project;
                }
            }
        }
        if (file != null) {
            String normalized = new File(file).getAbsolutePath();
            for (Project project : liveProjects()) {
                String basePath = project.getBasePath();
                if (basePath != null && normalized.startsWith(new File(basePath).getAbsolutePath() + File.separator)) {
                    return project;
                }
            }
        }
        List<Project> projects = liveProjects();
        return projects.isEmpty() ? null : projects.get(0);
    }

    private static int sessionIndex(XDebugSession session) {
        List<XDebugSession> sessions = allSessions();
        for (int i = 0; i < sessions.size(); i++) {
            if (sessions.get(i) == session) return i;
        }
        return 0;
    }

    private static List<XDebugSession> allSessions() {
        List<XDebugSession> sessions = new ArrayList<>();
        for (Project project : liveProjects()) {
            Collections.addAll(sessions, XDebuggerManager.getInstance(project).getDebugSessions());
        }
        return sessions;
    }

    private static List<Project> liveProjects() {
        List<Project> live = new ArrayList<>();
        for (Project project : PROJECTS) {
            if (!project.isDisposed()) live.add(project);
        }
        if (live.size() != PROJECTS.size()) {
            PROJECTS.clear();
            PROJECTS.addAll(live);
            writeRegistry();
        }
        return live;
    }

    private static List<Object> projectPayload() {
        List<Object> payload = new ArrayList<>();
        for (Project project : liveProjects()) {
            payload.add(projectInfo(project));
        }
        return payload;
    }

    private static void writeRegistry() {
        String url = serverUrl;
        if (url == null) return;
        try {
            File dir = new File(System.getProperty("user.home"), ".shadowdroid");
            Files.createDirectories(dir.toPath());
            String body = obj(
                "api_version", API_VERSION,
                "url", url,
                "pid", ProcessHandle.current().pid(),
                "updated_at", Instant.now().toString(),
                "projects", projectPayload()
            );
            Files.writeString(new File(dir, "studio-debugger.json").toPath(), body, StandardCharsets.UTF_8);
        } catch (Throwable ignored) {
        }
    }

    private static <T> T onIdeaThread(ThrowingSupplier<T> supplier) throws Exception {
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

    private static Response ok(Object... fields) {
        return new Response(HttpURLConnection.HTTP_OK, obj(fields));
    }

    private static Response bad(String message) {
        return new Response(HttpURLConnection.HTTP_BAD_REQUEST, obj("ok", false, "error", message));
    }

    private static void send(HttpExchange exchange, int status, String body) {
        try {
            byte[] bytes = body.getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("content-type", "application/json; charset=utf-8");
            exchange.sendResponseHeaders(status, bytes.length);
            try (var out = exchange.getResponseBody()) {
                out.write(bytes);
            }
        } catch (IOException ignored) {
        }
    }

    private static Map<String, String> parseQuery(String raw) {
        if (raw == null || raw.isBlank()) return Collections.emptyMap();
        Map<String, String> params = new LinkedHashMap<>();
        for (String part : raw.split("&")) {
            int index = part.indexOf('=');
            if (index < 0) continue;
            params.put(decode(part.substring(0, index)), decode(part.substring(index + 1)));
        }
        return params;
    }

    private static String decode(String value) {
        return URLDecoder.decode(value, StandardCharsets.UTF_8);
    }

    private static int intParam(Map<String, String> query, String key, int defaultValue, int min, int max) {
        String value = query.get(key);
        if (value == null) return defaultValue;
        try {
            int parsed = Integer.parseInt(value);
            return Math.max(min, Math.min(max, parsed));
        } catch (NumberFormatException ignored) {
            return defaultValue;
        }
    }

    private static boolean booleanParam(Map<String, String> query, String key, boolean defaultValue) {
        String value = query.get(key);
        return value == null ? defaultValue : Boolean.parseBoolean(value);
    }

    private static Map<String, Object> map(Object... fields) {
        Map<String, Object> map = new LinkedHashMap<>();
        for (int i = 0; i + 1 < fields.length; i += 2) {
            map.put(fields[i].toString(), fields[i + 1]);
        }
        return map;
    }

    private static String obj(Object... fields) {
        return json(map(fields));
    }

    @SuppressWarnings("unchecked")
    private static String json(Object value) {
        if (value == null) return "null";
        if (value instanceof String string) return "\"" + escape(string) + "\"";
        if (value instanceof Number || value instanceof Boolean) return value.toString();
        if (value instanceof Map<?, ?> map) {
            StringBuilder builder = new StringBuilder("{");
            boolean first = true;
            for (Map.Entry<?, ?> entry : map.entrySet()) {
                if (!first) builder.append(',');
                first = false;
                builder.append(json(String.valueOf(entry.getKey()))).append(':').append(json(entry.getValue()));
            }
            return builder.append('}').toString();
        }
        if (value instanceof Iterable<?> iterable) {
            StringBuilder builder = new StringBuilder("[");
            boolean first = true;
            for (Object item : iterable) {
                if (!first) builder.append(',');
                first = false;
                builder.append(json(item));
            }
            return builder.append(']').toString();
        }
        if (value.getClass().isArray()) {
            List<Object> list = new ArrayList<>();
            Object[] array = (Object[]) value;
            Collections.addAll(list, array);
            return json(list);
        }
        return json(value.toString());
    }

    private static String escape(String value) {
        StringBuilder builder = new StringBuilder(value.length() + 16);
        for (int i = 0; i < value.length(); i++) {
            char c = value.charAt(i);
            switch (c) {
                case '\\' -> builder.append("\\\\");
                case '"' -> builder.append("\\\"");
                case '\n' -> builder.append("\\n");
                case '\r' -> builder.append("\\r");
                case '\t' -> builder.append("\\t");
                default -> {
                    if (c < 0x20) builder.append(String.format("\\u%04x", (int) c));
                    else builder.append(c);
                }
            }
        }
        return builder.toString();
    }

    private record Response(int status, String body) {
    }

    private interface ThrowingSupplier<T> {
        T get() throws Exception;
    }
}
