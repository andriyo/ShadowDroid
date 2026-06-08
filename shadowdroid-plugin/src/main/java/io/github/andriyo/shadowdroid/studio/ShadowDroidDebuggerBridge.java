package io.github.andriyo.shadowdroid.studio;

import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.bad;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.debuggerTimeoutMs;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.intParam;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.map;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.nowMs;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.obj;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.ok;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.parseQuery;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.send;
import static io.github.andriyo.shadowdroid.studio.StudioThreading.onDebuggerThread;
import static io.github.andriyo.shadowdroid.studio.StudioThreading.onIdeaThread;

import com.intellij.debugger.engine.JavaStackFrame;
import com.intellij.debugger.jdi.LocalVariableProxyImpl;
import com.intellij.debugger.jdi.StackFrameProxyImpl;
import com.intellij.openapi.diagnostic.Logger;
import com.intellij.openapi.project.Project;
import com.intellij.openapi.startup.ProjectActivity;
import com.intellij.xdebugger.XDebugSession;
import com.intellij.xdebugger.XDebugSessionListener;
import com.intellij.xdebugger.XDebuggerManager;
import com.intellij.xdebugger.XDebuggerManagerListener;
import com.intellij.xdebugger.XSourcePosition;
import com.intellij.xdebugger.breakpoints.XBreakpoint;
import com.intellij.xdebugger.breakpoints.XBreakpointListener;
import com.intellij.xdebugger.breakpoints.XLineBreakpoint;
import com.intellij.xdebugger.frame.XExecutionStack;
import com.intellij.xdebugger.frame.XStackFrame;
import com.intellij.xdebugger.frame.XSuspendContext;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.Value;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import kotlin.Unit;
import kotlin.coroutines.Continuation;

import java.io.File;
import java.io.IOException;
import java.net.HttpURLConnection;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Base64;
import java.util.Collections;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.Executors;

public final class ShadowDroidDebuggerBridge implements ProjectActivity {
    private static final Logger LOG = Logger.getInstance(ShadowDroidDebuggerBridge.class);
    private static final int DEFAULT_PORT = 50576;
    private static final int API_VERSION = 1;
    private static final CopyOnWriteArrayList<Project> PROJECTS = new CopyOnWriteArrayList<>();
    private static final CopyOnWriteArrayList<WatchSpec> WATCHES = new CopyOnWriteArrayList<>();
    private static final Set<String> LISTENED_PROJECTS = ConcurrentHashMap.newKeySet();
    private static final Set<String> LISTENED_SESSIONS = ConcurrentHashMap.newKeySet();
    private static final ConcurrentMap<String, WatchValue> WATCH_VALUES = new ConcurrentHashMap<>();
    private static final Object LOCK = new Object();

    private static volatile HttpServer server;
    private static volatile String serverUrl;

    @Override
    public Object execute(Project project, Continuation<? super Unit> continuation) {
        registerProject(project);
        return Unit.INSTANCE;
    }

    private static void registerProject(Project project) {
        if (!PROJECTS.contains(project)) {
            PROJECTS.add(project);
        }
        installProjectListeners(project);
        installSessionListeners(project);
        ensureStarted();
        writeRegistry();
        LOG.info("ShadowDroid debugger bridge registered project " + project.getName() + " at " + serverUrl);
    }

    private static void installProjectListeners(Project project) {
        String key = projectKey(project);
        if (!LISTENED_PROJECTS.add(key)) return;
        project.getMessageBus().connect(project).subscribe(XDebuggerManager.TOPIC, new XDebuggerManagerListener() {
            @Override
            public void processStarted(com.intellij.xdebugger.XDebugProcess debugProcess) {
                installSessionListeners(project);
            }

            @Override
            public void currentSessionChanged(XDebugSession previousSession, XDebugSession currentSession) {
                installSessionListeners(project);
                if (currentSession != null && currentSession.isSuspended()) {
                    recordSessionPause(currentSession);
                }
            }
        });
        project.getMessageBus().connect(project).subscribe(XBreakpointListener.TOPIC, new XBreakpointListener<XBreakpoint<?>>() {
            @Override
            public void breakpointLogMessage(XBreakpoint<?> breakpoint, XDebugSession session, String message) {
                recordBreakpointHit(project, breakpoint);
            }
        });
    }

    private static void installSessionListeners(Project project) {
        for (XDebugSession session : XDebuggerManager.getInstance(project).getDebugSessions()) {
            String key = sessionKey(session);
            if (!LISTENED_SESSIONS.add(key)) continue;
            session.addSessionListener(new XDebugSessionListener() {
                @Override
                public void sessionPaused() {
                    recordSessionPause(session);
                }

                @Override
                public void stackFrameChanged() {
                    if (session.isSuspended()) {
                        refreshWatchesForSession(session);
                    }
                }

                @Override
                public void sessionStopped() {
                    LISTENED_SESSIONS.remove(key);
                }
            }, project);
            if (session.isSuspended()) {
                recordSessionPause(session);
            }
        }
    }

    private static void installAllSessionListeners() {
        for (Project project : liveProjects()) {
            installSessionListeners(project);
        }
    }

    private static void recordSessionPause(XDebugSession session) {
        try {
            recordLineBreakpointHit(session);
        } catch (Throwable t) {
            LOG.debug("Unable to record breakpoint hit", t);
        }
        try {
            refreshWatchesForSession(session);
        } catch (Throwable t) {
            LOG.debug("Unable to refresh watches", t);
        }
    }

    private static void recordLineBreakpointHit(XDebugSession session) throws Exception {
        XSourcePosition pos = session.getCurrentPosition();
        if (pos == null && session.getCurrentStackFrame() != null) {
            pos = session.getCurrentStackFrame().getSourcePosition();
        }
        if (pos == null || pos.getFile() == null) return;
        String fileUrl = pos.getFile().getUrl();
        int line = pos.getLine();
        Project project = session.getProject();
        onIdeaThread(() -> {
            for (XBreakpoint<?> breakpoint : XDebuggerManager.getInstance(project).getBreakpointManager().getAllBreakpoints()) {
                if (breakpoint instanceof XLineBreakpoint<?> lineBreakpoint
                    && fileUrl.equals(lineBreakpoint.getFileUrl())
                    && lineBreakpoint.getLine() == line) {
                    recordBreakpointHit(project, breakpoint);
                }
            }
            return null;
        });
    }

    private static void recordBreakpointHit(Project project, XBreakpoint<?> breakpoint) {
        BreakpointBridge.recordHit(project, breakpoint);
    }

    private static void refreshWatchesForSession(XDebugSession session) {
        if (!session.isSuspended()) return;
        Project project = session.getProject();
        String projectKey = projectKey(project);
        DebuggerValues.RenderOptions renderOptions = new DebuggerValues.RenderOptions(1, 64, 32);
        for (WatchSpec watch : WATCHES) {
            if (watch.project != null && !watch.project.equals(projectKey)) continue;
            try {
                WatchValue value = onDebuggerThread(session, () -> {
                    DebuggerValues.SelectedFrame selected = DebuggerValues.selectedFrame(session, Collections.emptyMap());
                    if (selected == null) {
                        return WatchValue.error(nowMs(), sessionInfo(sessionIndex(session), session), null, "current frame is not a Java/Kotlin frame");
                    }
                    DebuggerValues.EvaluationResult result = DebuggerValues.evaluatePath(selected.proxy(), watch.expression);
                    Object rendered = DebuggerValues.valueToMap(watch.expression, result.value(), result.declaredType(), renderOptions, new HashSet<>());
                    return WatchValue.ok(nowMs(), sessionInfo(sessionIndex(session), session), selected.info(), rendered);
                });
                WATCH_VALUES.put(watch.id, value);
            } catch (Throwable t) {
                WATCH_VALUES.put(watch.id, WatchValue.error(nowMs(), sessionInfo(sessionIndex(session), session), null, t.getMessage()));
            }
        }
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
            Response response = dispatch(path, query);
            send(exchange, response.status(), response.body());
        } catch (Throwable t) {
            send(exchange, HttpURLConnection.HTTP_INTERNAL_ERROR, obj("ok", false, "error", t.getMessage() == null ? t.getClass().getName() : t.getMessage()));
        }
    }

    private static Response dispatch(String path, Map<String, String> query) {
        return switch (path) {
            case "/v1/status" -> status();
            case "/v1/sessions" -> sessions();
            case "/v1/session/control" -> controlSession(query);
            case "/v1/session/stack" -> currentStack(query);
            case "/v1/session/threads" -> threads(query);
            case "/v1/session/variables" -> variables(query);
            case "/v1/session/evaluate" -> evaluate(query);
            case "/v1/watches" -> watches(query);
            case "/v1/watches/add" -> addWatch(query);
            case "/v1/watches/remove" -> removeWatch(query);
            case "/v1/watches/clear" -> clearWatches();
            case "/v1/clients" -> AndroidAttachBridge.clients(selectProject(query, null), query);
            case "/v1/breakpoints" -> breakpoints();
            case "/v1/breakpoints/line" -> BreakpointBridge.addLine(query, selectProject(query, query.get("file")));
            case "/v1/breakpoints/exception" -> BreakpointBridge.addException(query, selectProject(query, null));
            case "/v1/breakpoints/method" -> BreakpointBridge.addMethod(query, selectProject(query, null));
            case "/v1/breakpoints/field" -> BreakpointBridge.addField(query, selectProject(query, query.get("file")));
            case "/v1/breakpoints/update" -> BreakpointBridge.update(query, liveProjects(), selectProject(query, null));
            case "/v1/breakpoints/remove" -> BreakpointBridge.remove(query, liveProjects(), selectProject(query, null));
            case "/v1/attach" -> AndroidAttachBridge.attach(selectProject(query, null), query);
            case "/v1/layout/snapshot" -> LayoutInspectorBridge.snapshot(selectProject(query, null), query);
            case "/v1/layout/recompositions" -> LayoutInspectorBridge.recompositions(selectProject(query, null), query);
            case "/v1/layout/source" -> LayoutInspectorBridge.source(selectProject(query, null), query);
            default -> new Response(HttpURLConnection.HTTP_NOT_FOUND, obj("ok", false, "error", "not_found", "path", path));
        };
    }

    private static Response status() {
        installAllSessionListeners();
        List<Object> sessionPayload = new ArrayList<>();
        List<XDebugSession> sessions = allSessions();
        for (int i = 0; i < sessions.size(); i++) {
            sessionPayload.add(sessionInfo(i, sessions.get(i)));
        }
        return ok("ok", true, "api_version", API_VERSION, "url", serverUrl, "projects", projectPayload(), "sessions", sessionPayload);
    }

    private static Response sessions() {
        installAllSessionListeners();
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
        if (!session.isSuspended()) {
            return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "frames", Collections.emptyList(), "warning", "session is not suspended");
        }
        int limit = intParam(query, "limit", 64, 1, 512);
        int timeoutMs = debuggerTimeoutMs(query);
        XStackFrame frame = session.getCurrentStackFrame();
        List<Object> frames = new ArrayList<>();
        if (frame instanceof JavaStackFrame javaFrame) {
            frames.addAll(DebuggerValues.javaFrames(session, javaFrame, limit, timeoutMs));
        } else if (frame != null) {
            frames.add(DebuggerValues.frameInfo(frame, 0));
        }
        return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "frames", frames);
    }

    private static Response threads(Map<String, String> query) {
        XDebugSession session = selectSession(query);
        if (session == null) return bad("no debugger session");
        if (!session.isSuspended()) {
            return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "threads", Collections.emptyList(), "warning", "session is not suspended");
        }
        int limit = intParam(query, "limit", 32, 1, 128);
        int timeoutMs = debuggerTimeoutMs(query);
        XSuspendContext context = session.getSuspendContext();
        XExecutionStack[] stacks = context == null ? XExecutionStack.EMPTY_ARRAY : context.getExecutionStacks();
        List<Object> payload = new ArrayList<>();
        for (int i = 0; i < stacks.length; i++) {
            XStackFrame top = stacks[i].getTopFrame();
            List<Object> frames = new ArrayList<>();
            if (top instanceof JavaStackFrame javaFrame) {
                frames.addAll(DebuggerValues.javaFrames(session, javaFrame, limit, timeoutMs));
            } else if (top != null) {
                frames.add(DebuggerValues.frameInfo(top, 0));
            }
            payload.add(map("index", i, "name", stacks[i].getDisplayName(), "top_frame", top == null ? null : DebuggerValues.frameInfo(top, 0), "frames", frames));
        }
        return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "threads", payload);
    }

    private static Response variables(Map<String, String> query) {
        XDebugSession session = selectSession(query);
        if (session == null) return bad("no debugger session");
        if (!session.isSuspended()) {
            return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "variables", Collections.emptyList(), "warning", "session is not suspended");
        }
        DebuggerValues.RenderOptions renderOptions = new DebuggerValues.RenderOptions(
            intParam(query, "depth", 0, 0, 8),
            intParam(query, "max_fields", 64, 1, 512),
            intParam(query, "max_array_items", 32, 0, 512)
        );
        int timeoutMs = debuggerTimeoutMs(query);
        try {
            return onDebuggerThread(session, timeoutMs, () -> {
                DebuggerValues.SelectedFrame selected = DebuggerValues.selectedFrame(session, query);
                if (selected == null) {
                    return ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "variables", Collections.emptyList(), "warning", "current frame is not a Java/Kotlin frame");
                }
                StackFrameProxyImpl proxy = selected.proxy();
                List<Object> locals = new ArrayList<>();
                for (LocalVariableProxyImpl local : proxy.visibleVariables()) {
                    Value value = proxy.getValue(local);
                    locals.add(DebuggerValues.valueToMap(local.name(), value, local.typeName(), renderOptions, new HashSet<>()));
                }
                ObjectReference thisObject = proxy.thisObject();
                return ok(
                    "ok", true,
                    "session", sessionInfo(sessionIndex(session), session),
                    "selected_frame", selected.info(),
                    "this", thisObject == null ? null : DebuggerValues.valueToMap("this", thisObject, null, renderOptions, new HashSet<>()),
                    "variables", locals
                );
            });
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    private static Response evaluate(Map<String, String> query) {
        String expression = query.get("expression");
        if (expression == null || expression.isBlank()) return bad("missing expression");
        XDebugSession session = selectSession(query);
        if (session == null) return bad("no debugger session");
        if (!session.isSuspended()) return bad("session is not suspended");
        DebuggerValues.RenderOptions renderOptions = new DebuggerValues.RenderOptions(
            intParam(query, "depth", 1, 0, 8),
            intParam(query, "max_fields", 64, 1, 512),
            intParam(query, "max_array_items", 32, 0, 512)
        );
        int timeoutMs = debuggerTimeoutMs(query);
        try {
            return onDebuggerThread(session, timeoutMs, () -> {
                DebuggerValues.SelectedFrame selected = DebuggerValues.selectedFrame(session, query);
                if (selected == null) throw new IllegalArgumentException("current frame is not a Java/Kotlin frame");
                DebuggerValues.EvaluationResult result = DebuggerValues.evaluatePath(selected.proxy(), expression);
                return ok(
                    "ok", true,
                    "session", sessionInfo(sessionIndex(session), session),
                    "selected_frame", selected.info(),
                    "expression", expression,
                    "mode", "jdi_path",
                    "result", DebuggerValues.valueToMap(expression, result.value(), result.declaredType(), renderOptions, new HashSet<>())
                );
            });
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    private static Response addWatch(Map<String, String> query) {
        String expression = query.get("expression");
        if (expression == null || expression.isBlank()) return bad("missing expression");
        Project project = selectProject(query, null);
        String projectKey = project == null ? null : projectKey(project);
        String name = query.get("name");
        if (name == null || name.isBlank()) name = expression;
        WatchSpec watch = new WatchSpec(watchId(projectKey, name, expression), projectKey, name, expression, true);
        WATCHES.removeIf(existing -> existing.id.equals(watch.id));
        WATCHES.add(watch);
        installAllSessionListeners();
        return ok("ok", true, "watch", watchInfo(watch, null));
    }

    private static Response removeWatch(Map<String, String> query) {
        String id = query.get("id");
        if (id == null || id.isBlank()) return bad("missing id");
        boolean removed = WATCHES.removeIf(watch -> watch.id.equals(id));
        WATCH_VALUES.remove(id);
        return ok("ok", true, "id", id, "removed", removed);
    }

    private static Response clearWatches() {
        int removed = WATCHES.size();
        WATCHES.clear();
        WATCH_VALUES.clear();
        return ok("ok", true, "removed", removed);
    }

    private static Response watches(Map<String, String> query) {
        installAllSessionListeners();
        XDebugSession session = selectSession(query);
        DebuggerValues.RenderOptions renderOptions = new DebuggerValues.RenderOptions(
            intParam(query, "depth", 1, 0, 8),
            intParam(query, "max_fields", 64, 1, 512),
            intParam(query, "max_array_items", 32, 0, 512)
        );
        int timeoutMs = debuggerTimeoutMs(query);
        List<Object> payload = new ArrayList<>();
        for (WatchSpec watch : WATCHES) {
            Object value = null;
            if (session != null && session.isSuspended() && session.getCurrentStackFrame() instanceof JavaStackFrame javaFrame) {
                try {
                    value = onDebuggerThread(session, timeoutMs, () -> {
                        DebuggerValues.EvaluationResult result = DebuggerValues.evaluatePath(javaFrame.getStackFrameProxy(), watch.expression);
                        Object rendered = DebuggerValues.valueToMap(watch.expression, result.value(), result.declaredType(), renderOptions, new HashSet<>());
                        WATCH_VALUES.put(watch.id, WatchValue.ok(nowMs(), sessionInfo(sessionIndex(session), session), null, rendered));
                        return rendered;
                    });
                } catch (Throwable t) {
                    value = map("ok", false, "error", t.getMessage());
                }
            }
            payload.add(watchInfo(watch, value));
        }
        return ok(
            "ok", true,
            "session", session == null ? null : sessionInfo(sessionIndex(session), session),
            "warning", session != null && !session.isSuspended() ? "session is not suspended; returning cached watch values" : null,
            "watches", payload
        );
    }

    private static Response breakpoints() {
        installAllSessionListeners();
        return BreakpointBridge.list(liveProjects());
    }

    private static Map<String, Object> watchInfo(WatchSpec watch, Object value) {
        WatchValue cached = WATCH_VALUES.get(watch.id);
        Object effectiveValue = value != null ? value : cached == null ? null : cached.value;
        return map(
            "id", watch.id,
            "project", watch.project,
            "name", watch.name,
            "expression", watch.expression,
            "enabled", watch.enabled,
            "value", effectiveValue,
            "updated_at", cached == null ? null : cached.updatedAt,
            "session", cached == null ? null : cached.session,
            "selected_frame", cached == null ? null : cached.selectedFrame,
            "error", cached == null ? null : cached.error
        );
    }

    private static String watchId(String project, String name, String expression) {
        String raw = (project == null ? "" : project) + "|" + name + "|" + expression;
        return "watch_" + Base64.getUrlEncoder().withoutPadding().encodeToString(raw.getBytes(StandardCharsets.UTF_8));
    }

    private static Map<String, Object> sessionInfo(int index, XDebugSession session) {
        XSourcePosition pos = null;
        if (session.isSuspended()) {
            try {
                pos = session.getCurrentPosition();
                if (pos == null && session.getCurrentStackFrame() != null) {
                    pos = session.getCurrentStackFrame().getSourcePosition();
                }
            } catch (Throwable t) {
                LOG.debug("Unable to read current debugger source position", t);
            }
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

    private static Map<String, Object> projectInfo(Project project) {
        return map(
            "name", project.getName(),
            "base_path", project.getBasePath(),
            "disposed", project.isDisposed()
        );
    }

    private static String projectKey(Project project) {
        return project.getBasePath() == null ? project.getName() : project.getBasePath();
    }

    private static String sessionKey(XDebugSession session) {
        return projectKey(session.getProject()) + "|" + session.getSessionName() + "|" + System.identityHashCode(session);
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

    private record WatchSpec(String id, String project, String name, String expression, boolean enabled) {
    }

    private record WatchValue(long updatedAt, Object session, Object selectedFrame, Object value, String error) {
        static WatchValue ok(long updatedAt, Object session, Object selectedFrame, Object value) {
            return new WatchValue(updatedAt, session, selectedFrame, value, null);
        }

        static WatchValue error(long updatedAt, Object session, Object selectedFrame, String error) {
            return new WatchValue(updatedAt, session, selectedFrame, null, error);
        }
    }
}
