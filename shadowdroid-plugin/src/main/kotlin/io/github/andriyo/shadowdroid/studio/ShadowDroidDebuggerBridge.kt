package io.github.andriyo.shadowdroid.studio

import com.intellij.debugger.engine.JavaStackFrame
import com.intellij.debugger.jdi.LocalVariableProxyImpl
import com.intellij.debugger.jdi.StackFrameProxyImpl
import com.intellij.openapi.diagnostic.Logger
import com.intellij.openapi.project.Project
import com.intellij.openapi.startup.ProjectActivity
import com.intellij.xdebugger.XDebugProcess
import com.intellij.xdebugger.XDebugSession
import com.intellij.xdebugger.XDebugSessionListener
import com.intellij.xdebugger.XDebuggerManager
import com.intellij.xdebugger.XDebuggerManagerListener
import com.intellij.xdebugger.XSourcePosition
import com.intellij.xdebugger.breakpoints.XBreakpoint
import com.intellij.xdebugger.breakpoints.XBreakpointListener
import com.intellij.xdebugger.breakpoints.XLineBreakpoint
import com.intellij.xdebugger.frame.XExecutionStack
import com.intellij.xdebugger.frame.XStackFrame
import com.sun.jdi.Field
import com.sun.jdi.ObjectReference
import com.sun.jdi.Value
import com.sun.net.httpserver.HttpExchange
import com.sun.net.httpserver.HttpServer
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.InetAddress
import java.net.InetSocketAddress
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.time.Instant
import java.util.Base64
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.Executors

class ShadowDroidDebuggerBridge : ProjectActivity {
    override suspend fun execute(project: Project) {
        registerProject(project)
    }

    companion object {
        private val LOG = Logger.getInstance(ShadowDroidDebuggerBridge::class.java)
        private const val DEFAULT_PORT = 50576
        private const val API_VERSION = 1

        private val projects = CopyOnWriteArrayList<Project>()
        private val watches = CopyOnWriteArrayList<WatchSpec>()
        private val listenedProjects = ConcurrentHashMap.newKeySet<String>()
        private val listenedSessions = ConcurrentHashMap.newKeySet<String>()
        private val watchValues = ConcurrentHashMap<String, WatchValue>()
        private val objectHandles = ConcurrentHashMap<String, ObjectHandleEntry>()
        private val sessionHandleEpochs = ConcurrentHashMap<String, Int>()
        private val lock = Any()

        @Volatile
        private var server: HttpServer? = null

        @Volatile
        private var serverUrl: String? = null

        private fun registerProject(project: Project) {
            if (!projects.contains(project)) {
                projects += project
            }
            installProjectListeners(project)
            installSessionListeners(project)
            ensureStarted()
            writeRegistry()
            LOG.info("ShadowDroid debugger bridge registered project ${project.name} at $serverUrl")
        }

        private fun installProjectListeners(project: Project) {
            val key = projectKey(project)
            if (!listenedProjects.add(key)) return
            project.messageBus.connect(project).subscribe(XDebuggerManager.TOPIC, object : XDebuggerManagerListener {
                override fun processStarted(debugProcess: XDebugProcess) {
                    installSessionListeners(project)
                }

                override fun currentSessionChanged(previousSession: XDebugSession?, currentSession: XDebugSession?) {
                    installSessionListeners(project)
                    if (currentSession != null && currentSession.isSuspended) {
                        recordSessionPause(currentSession)
                    }
                }
            })
            project.messageBus.connect(project).subscribe(XBreakpointListener.TOPIC, object : XBreakpointListener<XBreakpoint<*>> {
                override fun breakpointLogMessage(breakpoint: XBreakpoint<*>, session: XDebugSession, message: String) {
                    recordBreakpointHit(project, breakpoint)
                }
            })
        }

        private fun installSessionListeners(project: Project) {
            for (session in XDebuggerManager.getInstance(project).debugSessions) {
                val key = sessionKey(session)
                if (!listenedSessions.add(key)) continue
                session.addSessionListener(object : XDebugSessionListener {
                    override fun sessionPaused() {
                        recordSessionPause(session)
                    }

                    override fun stackFrameChanged() {
                        if (session.isSuspended) {
                            advanceHandleEpoch(session)
                            refreshWatchesForSession(session)
                        }
                    }

                    override fun sessionStopped() {
                        listenedSessions.remove(key)
                        advanceHandleEpoch(session)
                    }
                }, project)
                if (session.isSuspended) {
                    recordSessionPause(session)
                }
            }
        }

        private fun installAllSessionListeners() {
            liveProjects().forEach(::installSessionListeners)
        }

        private fun recordSessionPause(session: XDebugSession) {
            advanceHandleEpoch(session)
            try {
                recordLineBreakpointHit(session)
            } catch (t: Throwable) {
                LOG.debug("Unable to record breakpoint hit", t)
            }
            try {
                refreshWatchesForSession(session)
            } catch (t: Throwable) {
                LOG.debug("Unable to refresh watches", t)
            }
        }

        @Throws(Exception::class)
        private fun recordLineBreakpointHit(session: XDebugSession) {
            var pos: XSourcePosition? = session.currentPosition
            if (pos == null && session.currentStackFrame != null) {
                pos = session.currentStackFrame?.sourcePosition
            }
            if (pos?.file == null) return
            val fileUrl = pos.file.url
            val line = pos.line
            val project = session.project
            StudioThreading.onIdeaThread {
                for (breakpoint in XDebuggerManager.getInstance(project).breakpointManager.allBreakpoints) {
                    if (breakpoint is XLineBreakpoint<*> &&
                        fileUrl == breakpoint.fileUrl &&
                        breakpoint.line == line
                    ) {
                        recordBreakpointHit(project, breakpoint)
                    }
                }
                null
            }
        }

        private fun recordBreakpointHit(project: Project, breakpoint: XBreakpoint<*>) {
            BreakpointBridge.recordHit(project, breakpoint)
        }

        private fun refreshWatchesForSession(session: XDebugSession) {
            if (!session.isSuspended) return
            val project = session.project
            val projectKey = projectKey(project)
            val renderOptions = DebuggerValues.RenderOptions(1, 64, 32, handleProvider(session))
            for (watch in watches) {
                if (watch.project != null && watch.project != projectKey) continue
                try {
                    val value = StudioThreading.onDebuggerThread(session) {
                        val selected = DebuggerValues.selectedFrame(session, emptyMap())
                        if (selected == null) {
                            return@onDebuggerThread WatchValue.error(
                                BridgeProtocol.nowMs(),
                                sessionInfo(sessionIndex(session), session),
                                null,
                                "current frame is not a Java/Kotlin frame",
                            )
                        }
                        val result = DebuggerValues.evaluatePath(selected.proxy, watch.expression)
                        val rendered = DebuggerValues.valueToMap(
                            watch.expression,
                            result.value,
                            result.declaredType,
                            renderOptions,
                            hashSetOf(),
                        )
                        WatchValue.ok(BridgeProtocol.nowMs(), sessionInfo(sessionIndex(session), session), selected.info(), rendered)
                    }
                    watchValues[watch.id] = value
                } catch (t: Throwable) {
                    watchValues[watch.id] = WatchValue.error(
                        BridgeProtocol.nowMs(),
                        sessionInfo(sessionIndex(session), session),
                        null,
                        t.message,
                    )
                }
            }
        }

        private fun ensureStarted() {
            if (server != null) return
            synchronized(lock) {
                if (server != null) return
                val preferredPort = preferredPort()
                val created = createServer(preferredPort) ?: createServer(0)
                    ?: throw IllegalStateException("unable to start ShadowDroid debugger bridge")

                created.createContext(BridgeConfig.ROOT_CONTEXT, ::handle)
                created.executor = Executors.newCachedThreadPool { runnable ->
                    Thread(runnable, "ShadowDroid debugger bridge").apply { isDaemon = true }
                }
                created.start()
                server = created
                serverUrl = "http://127.0.0.1:${created.address.port}"
            }
        }

        private fun preferredPort(): Int {
            val property = System.getProperty(BridgeConfig.PORT_PROPERTY)
                ?.takeUnless { it.isBlank() }
                ?: System.getenv(BridgeConfig.PORT_ENV)
            return property?.toIntOrNull() ?: DEFAULT_PORT
        }

        private fun createServer(port: Int): HttpServer? =
            try {
                HttpServer.create(InetSocketAddress(InetAddress.getByName("127.0.0.1"), port), 0)
            } catch (_: IOException) {
                null
            }

        private fun handle(exchange: HttpExchange) {
            try {
                val path = exchange.requestURI.path
                val query = BridgeProtocol.parseQuery(exchange.requestURI.rawQuery)
                val response = dispatch(path, query)
                BridgeProtocol.send(exchange, response.status, response.body)
            } catch (t: Throwable) {
                BridgeProtocol.send(
                    exchange,
                    HttpURLConnection.HTTP_INTERNAL_ERROR,
                    BridgeProtocol.obj("ok", false, "error", t.message ?: t.javaClass.name),
                )
            }
        }

        private fun dispatch(path: String, query: Map<String, String>): Response =
            when (path) {
                BridgeRoutes.STATUS -> status()
                BridgeRoutes.SESSIONS -> sessions()
                BridgeRoutes.SESSION_CONTROL -> controlSession(query)
                BridgeRoutes.SESSION_STACK -> currentStack(query)
                BridgeRoutes.SESSION_THREADS -> threads(query)
                BridgeRoutes.SESSION_VARIABLES -> variables(query)
                BridgeRoutes.SESSION_EVALUATE -> evaluate(query)
                BridgeRoutes.SESSION_INSPECT -> inspect(query)
                BridgeRoutes.SESSION_COROUTINES -> coroutines(query)
                BridgeRoutes.SESSION_COROUTINES_THREADS -> coroutineThreads(query)
                BridgeRoutes.SESSION_COROUTINES_CONTINUATION -> coroutineContinuation(query)
                BridgeRoutes.SESSION_COROUTINES_FLOW -> coroutineFlow(query)
                BridgeRoutes.WATCHES -> watches(query)
                BridgeRoutes.WATCHES_ADD -> addWatch(query)
                BridgeRoutes.WATCHES_REMOVE -> removeWatch(query)
                BridgeRoutes.WATCHES_CLEAR -> clearWatches()
                BridgeRoutes.CLIENTS -> AndroidAttachBridge.clients(selectProject(query, null), query)
                BridgeRoutes.BREAKPOINTS -> breakpoints()
                BridgeRoutes.BREAKPOINT_LINE -> BreakpointBridge.addLine(query, selectProject(query, query[BridgeQuery.FILE]))
                BridgeRoutes.BREAKPOINT_EXCEPTION -> BreakpointBridge.addException(query, selectProject(query, null))
                BridgeRoutes.BREAKPOINT_METHOD -> BreakpointBridge.addMethod(query, selectProject(query, null))
                BridgeRoutes.BREAKPOINT_FIELD -> BreakpointBridge.addField(query, selectProject(query, query[BridgeQuery.FILE]))
                BridgeRoutes.BREAKPOINT_UPDATE -> BreakpointBridge.update(query, liveProjects(), selectProject(query, null))
                BridgeRoutes.BREAKPOINT_REMOVE -> BreakpointBridge.remove(query, liveProjects(), selectProject(query, null))
                BridgeRoutes.ATTACH -> AndroidAttachBridge.attach(selectProject(query, null), query)
                BridgeRoutes.LAYOUT_SNAPSHOT -> LayoutInspectorBridge.snapshot(selectProject(query, null), query)
                BridgeRoutes.LAYOUT_RECOMPOSITIONS -> LayoutInspectorBridge.recompositions(selectProject(query, null), query)
                BridgeRoutes.LAYOUT_SOURCE -> LayoutInspectorBridge.source(selectProject(query, null), query)
                else -> Response(
                    HttpURLConnection.HTTP_NOT_FOUND,
                    BridgeProtocol.obj("ok", false, "error", "not_found", "path", path),
                )
            }

        private fun status(): Response {
            installAllSessionListeners()
            val sessions = allSessions()
            val sessionPayload = sessions.mapIndexed { index, session -> sessionInfo(index, session) }
            return BridgeProtocol.ok(
                "ok", true,
                "api_version", API_VERSION,
                "url", serverUrl,
                "projects", projectPayload(),
                "sessions", sessionPayload,
            )
        }

        private fun sessions(): Response {
            installAllSessionListeners()
            val payload = allSessions().mapIndexed { index, session -> sessionInfo(index, session) }
            return BridgeProtocol.ok("ok", true, "sessions", payload)
        }

        private fun controlSession(query: Map<String, String>): Response {
            val action = query[BridgeQuery.ACTION] ?: return BridgeProtocol.bad("missing action")
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            return try {
                StudioThreading.onIdeaThread {
                    when (action) {
                        BridgeValues.ACTION_PAUSE -> session.pause()
                        BridgeValues.ACTION_RESUME -> session.resume()
                        BridgeValues.ACTION_STEP_OVER -> session.stepOver(false)
                        BridgeValues.ACTION_STEP_INTO -> session.stepInto()
                        BridgeValues.ACTION_STEP_OUT -> session.stepOut()
                        BridgeValues.ACTION_STOP -> session.stop()
                        else -> throw IllegalArgumentException("unsupported action: $action")
                    }
                    null
                }
                if (action == BridgeValues.ACTION_RESUME || action == BridgeValues.ACTION_STOP) {
                    advanceHandleEpoch(session)
                }
                BridgeProtocol.ok("ok", true, "action", action, "session", sessionInfo(sessionIndex(session), session))
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun currentStack(query: Map<String, String>): Response {
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) {
                return BridgeProtocol.ok(
                    "ok", true,
                    "session", sessionInfo(sessionIndex(session), session),
                    "frames", emptyList<Any>(),
                    "warning", "session is not suspended",
                )
            }
            val limit = BridgeProtocol.intParam(query, BridgeQuery.LIMIT, 64, 1, 512)
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            val frame = session.currentStackFrame
            val frames = mutableListOf<Any>()
            if (frame is JavaStackFrame) {
                frames.addAll(DebuggerValues.javaFrames(session, frame, limit, timeoutMs))
            } else if (frame != null) {
                frames += DebuggerValues.frameInfo(frame, 0)
            }
            return BridgeProtocol.ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "frames", frames)
        }

        private fun threads(query: Map<String, String>): Response {
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) {
                return BridgeProtocol.ok(
                    "ok", true,
                    "session", sessionInfo(sessionIndex(session), session),
                    "threads", emptyList<Any>(),
                    "warning", "session is not suspended",
                )
            }
            val limit = BridgeProtocol.intParam(query, BridgeQuery.LIMIT, 32, 1, 128)
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            val stacks = session.suspendContext?.executionStacks ?: XExecutionStack.EMPTY_ARRAY
            val payload = mutableListOf<Any>()
            for (index in stacks.indices) {
                val top: XStackFrame? = stacks[index].topFrame
                val frames = mutableListOf<Any>()
                if (top is JavaStackFrame) {
                    frames.addAll(DebuggerValues.javaFrames(session, top, limit, timeoutMs))
                } else if (top != null) {
                    frames += DebuggerValues.frameInfo(top, 0)
                }
                payload += BridgeProtocol.map(
                    "index", index,
                    "name", stacks[index].displayName,
                    "top_frame", top?.let { DebuggerValues.frameInfo(it, 0) },
                    "frames", frames,
                )
            }
            return BridgeProtocol.ok("ok", true, "session", sessionInfo(sessionIndex(session), session), "threads", payload)
        }

        private fun variables(query: Map<String, String>): Response {
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) {
                return BridgeProtocol.ok(
                    "ok", true,
                    "session", sessionInfo(sessionIndex(session), session),
                    "variables", emptyList<Any>(),
                    "warning", "session is not suspended",
                )
            }
            val renderOptions = DebuggerValues.RenderOptions(
                BridgeProtocol.intParam(query, BridgeQuery.DEPTH, 0, 0, 8),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_FIELDS, 64, 1, 512),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_ARRAY_ITEMS, 32, 0, 512),
                handleProvider(session),
            )
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            return try {
                StudioThreading.onDebuggerThread(session, timeoutMs) {
                    val selected = DebuggerValues.selectedFrame(session, query)
                    if (selected == null) {
                        return@onDebuggerThread BridgeProtocol.ok(
                            "ok", true,
                            "session", sessionInfo(sessionIndex(session), session),
                            "variables", emptyList<Any>(),
                            "warning", "current frame is not a Java/Kotlin frame",
                        )
                    }
                    val proxy: StackFrameProxyImpl = selected.proxy
                    val locals = mutableListOf<Any>()
                    for (local: LocalVariableProxyImpl in proxy.visibleVariables()) {
                        val value: Value? = proxy.getValue(local)
                        locals += DebuggerValues.valueToMap(local.name(), value, local.typeName(), renderOptions, hashSetOf())
                    }
                    val thisObject: ObjectReference? = proxy.thisObject()
                    BridgeProtocol.ok(
                        "ok", true,
                        "session", sessionInfo(sessionIndex(session), session),
                        "selected_frame", selected.info(),
                        "this", thisObject?.let { DebuggerValues.valueToMap("this", it, null, renderOptions, hashSetOf()) },
                        "variables", locals,
                    )
                }
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun evaluate(query: Map<String, String>): Response {
            val expression = query[BridgeQuery.EXPRESSION]
            if (expression.isNullOrBlank()) return BridgeProtocol.bad("missing expression")
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) return BridgeProtocol.bad("session is not suspended")
            val renderOptions = DebuggerValues.RenderOptions(
                BridgeProtocol.intParam(query, BridgeQuery.DEPTH, 1, 0, 8),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_FIELDS, 64, 1, 512),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_ARRAY_ITEMS, 32, 0, 512),
                handleProvider(session),
            )
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            return try {
                StudioThreading.onDebuggerThread(session, timeoutMs) {
                    val selected = DebuggerValues.selectedFrame(session, query)
                        ?: throw IllegalArgumentException("current frame is not a Java/Kotlin frame")
                    val result = DebuggerValues.evaluatePath(selected.proxy, expression)
                    BridgeProtocol.ok(
                        "ok", true,
                        "session", sessionInfo(sessionIndex(session), session),
                        "selected_frame", selected.info(),
                        "expression", expression,
                        "mode", BridgeValues.EVAL_MODE_JDI_PATH,
                        "result", DebuggerValues.valueToMap(
                            expression,
                            result.value,
                            result.declaredType,
                            renderOptions,
                            hashSetOf(),
                        ),
                    )
                }
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun inspect(query: Map<String, String>): Response {
            val expression = query[BridgeQuery.EXPRESSION]?.takeUnless { it.isBlank() }
            val handle = query[BridgeQuery.HANDLE]?.takeUnless { it.isBlank() }
            if (expression == null && handle == null) return BridgeProtocol.bad("missing expression or handle")
            if (expression != null && handle != null) return BridgeProtocol.bad("use expression or handle, not both")
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) return BridgeProtocol.bad("session is not suspended")
            val renderOptions = DebuggerValues.RenderOptions(
                BridgeProtocol.intParam(query, BridgeQuery.DEPTH, 1, 0, 8),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_FIELDS, 64, 1, 512),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_ARRAY_ITEMS, 32, 0, 512),
                handleProvider(session),
            )
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            return try {
                StudioThreading.onDebuggerThread(session, timeoutMs) {
                    val selected = if (handle == null) {
                        DebuggerValues.selectedFrame(session, query)
                            ?: throw IllegalArgumentException("current frame is not a Java/Kotlin frame")
                    } else {
                        null
                    }
                    val result = if (handle != null) {
                        val entry = resolveObjectHandle(session, handle)
                            ?: throw IllegalArgumentException("stale or unknown object handle: $handle")
                        DebuggerValues.evaluateRelativePath(
                            entry.reference,
                            DebuggerValues.valueTypeName(entry.reference),
                            query[BridgeQuery.PATH],
                        )
                    } else {
                        DebuggerValues.evaluatePath(selected!!.proxy, expression!!)
                    }
                    val name = handle ?: expression ?: "result"
                    BridgeProtocol.ok(
                        "ok", true,
                        "type", "debug_inspect",
                        "schema_version", 2,
                        "mode", if (handle != null) BridgeValues.EVAL_MODE_OBJECT_HANDLE else BridgeValues.EVAL_MODE_JDI_PATH,
                        "session", sessionInfo(sessionIndex(session), session),
                        "selected_frame", selected?.info(),
                        "expression", expression,
                        "handle", handle,
                        "path", query[BridgeQuery.PATH],
                        "handle_scope", handleScope(session),
                        "limits", BridgeProtocol.map(
                            "depth", renderOptions.depth,
                            "max_fields", renderOptions.maxFields,
                            "max_array_items", renderOptions.maxArrayItems,
                        ),
                        "result", DebuggerValues.valueToMap(
                            name,
                            result.value,
                            result.declaredType,
                            renderOptions,
                            hashSetOf(),
                        ),
                    )
                }
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun coroutines(query: Map<String, String>): Response {
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) {
                return BridgeProtocol.ok(
                    "ok", true,
                    "available", false,
                    "type", "coroutine_snapshot",
                    "reason", "session is not suspended",
                    "session", sessionInfo(sessionIndex(session), session),
                )
            }
            val limit = BridgeProtocol.intParam(query, BridgeQuery.LIMIT, 64, 1, 256)
            val renderOptions = DebuggerValues.RenderOptions(
                BridgeProtocol.intParam(query, BridgeQuery.DEPTH, 1, 0, 8),
                48,
                24,
                handleProvider(session),
            )
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            return try {
                StudioThreading.onDebuggerThread(session, timeoutMs) {
                    val threads = coroutineThreadsPayload(session, limit, timeoutMs)
                    val continuations = continuationPayload(session, query, renderOptions, limit)
                    BridgeProtocol.ok(
                        "ok", true,
                        "available", true,
                        "type", "coroutine_snapshot",
                        "schema_version", 1,
                        "source", "jdi_suspended_frame",
                        "session", sessionInfo(sessionIndex(session), session),
                        "summary", BridgeProtocol.map(
                            "threads", threads.size,
                            "continuations", continuations.size,
                        ),
                        "threads", threads,
                        "continuations", continuations,
                    )
                }
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun coroutineThreads(query: Map<String, String>): Response {
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) return BridgeProtocol.bad("session is not suspended")
            val limit = BridgeProtocol.intParam(query, BridgeQuery.LIMIT, 32, 1, 128)
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            return try {
                StudioThreading.onDebuggerThread(session, timeoutMs) {
                    BridgeProtocol.ok(
                        "ok", true,
                        "type", "coroutine_threads",
                        "schema_version", 1,
                        "session", sessionInfo(sessionIndex(session), session),
                        "threads", coroutineThreadsPayload(session, limit, timeoutMs),
                    )
                }
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun coroutineContinuation(query: Map<String, String>): Response {
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) return BridgeProtocol.bad("session is not suspended")
            val renderOptions = DebuggerValues.RenderOptions(
                BridgeProtocol.intParam(query, BridgeQuery.DEPTH, 2, 0, 8),
                48,
                24,
                handleProvider(session),
            )
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            return try {
                StudioThreading.onDebuggerThread(session, timeoutMs) {
                    val selected = DebuggerValues.selectedFrame(session, query)
                        ?: throw IllegalArgumentException("current frame is not a Java/Kotlin frame")
                    val continuations = continuationCandidates(selected.proxy, renderOptions, 64)
                    BridgeProtocol.ok(
                        "ok", true,
                        "type", "coroutine_continuation",
                        "schema_version", 1,
                        "source", "jdi_suspended_frame",
                        "session", sessionInfo(sessionIndex(session), session),
                        "selected_frame", selected.info(),
                        "continuations", continuations,
                    )
                }
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun coroutineFlow(query: Map<String, String>): Response {
            val expression = query[BridgeQuery.EXPRESSION]
            if (expression.isNullOrBlank()) return BridgeProtocol.bad("missing expression")
            val session = selectSession(query) ?: return BridgeProtocol.bad("no debugger session")
            if (!session.isSuspended) return BridgeProtocol.bad("session is not suspended")
            val renderOptions = DebuggerValues.RenderOptions(
                BridgeProtocol.intParam(query, BridgeQuery.DEPTH, 2, 0, 8),
                64,
                32,
                handleProvider(session),
            )
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            return try {
                StudioThreading.onDebuggerThread(session, timeoutMs) {
                    val selected = DebuggerValues.selectedFrame(session, query)
                        ?: throw IllegalArgumentException("current frame is not a Java/Kotlin frame")
                    val result = DebuggerValues.evaluatePath(selected.proxy, expression)
                    val type = DebuggerValues.valueTypeName(result.value)
                    BridgeProtocol.ok(
                        "ok", true,
                        "type", "coroutine_flow",
                        "schema_version", 1,
                        "source", "jdi_field_only",
                        "session", sessionInfo(sessionIndex(session), session),
                        "selected_frame", selected.info(),
                        "expression", expression,
                        "kind", flowKind(type),
                        "confidence", if (flowKind(type) != null) "medium" else "low",
                        "observation", "field_only_no_collection_no_getters",
                        "value", DebuggerValues.valueToMap(expression, result.value, result.declaredType, renderOptions, hashSetOf()),
                    )
                }
            } catch (t: Throwable) {
                BridgeProtocol.bad(t.message)
            }
        }

        private fun addWatch(query: Map<String, String>): Response {
            val expression = query[BridgeQuery.EXPRESSION]
            if (expression.isNullOrBlank()) return BridgeProtocol.bad("missing expression")
            val project = selectProject(query, null)
            val projectKey = project?.let(::projectKey)
            val name = query[BridgeQuery.NAME]?.takeUnless { it.isBlank() } ?: expression
            val watch = WatchSpec(watchId(projectKey, name, expression), projectKey, name, expression, true)
            watches.removeIf { it.id == watch.id }
            watches += watch
            installAllSessionListeners()
            return BridgeProtocol.ok("ok", true, "watch", watchInfo(watch, null))
        }

        private fun removeWatch(query: Map<String, String>): Response {
            val id = query[BridgeQuery.ID]
            if (id.isNullOrBlank()) return BridgeProtocol.bad("missing id")
            val removed = watches.removeIf { it.id == id }
            watchValues.remove(id)
            return BridgeProtocol.ok("ok", true, "id", id, "removed", removed)
        }

        private fun clearWatches(): Response {
            val removed = watches.size
            watches.clear()
            watchValues.clear()
            return BridgeProtocol.ok("ok", true, "removed", removed)
        }

        private fun watches(query: Map<String, String>): Response {
            installAllSessionListeners()
            val session = selectSession(query)
            val renderOptions = DebuggerValues.RenderOptions(
                BridgeProtocol.intParam(query, BridgeQuery.DEPTH, 1, 0, 8),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_FIELDS, 64, 1, 512),
                BridgeProtocol.intParam(query, BridgeQuery.MAX_ARRAY_ITEMS, 32, 0, 512),
            )
            val timeoutMs = BridgeProtocol.debuggerTimeoutMs(query)
            val payload = mutableListOf<Any>()
            for (watch in watches) {
                var value: Any? = null
                val frame = session?.currentStackFrame
                if (session != null && session.isSuspended && frame is JavaStackFrame) {
                    try {
                        value = StudioThreading.onDebuggerThread(session, timeoutMs) {
                            val result = DebuggerValues.evaluatePath(frame.stackFrameProxy, watch.expression)
                            val rendered = DebuggerValues.valueToMap(
                                watch.expression,
                                result.value,
                                result.declaredType,
                                renderOptions,
                                hashSetOf(),
                            )
                            watchValues[watch.id] = WatchValue.ok(
                                BridgeProtocol.nowMs(),
                                sessionInfo(sessionIndex(session), session),
                                null,
                                rendered,
                            )
                            rendered
                        }
                    } catch (t: Throwable) {
                        value = BridgeProtocol.map("ok", false, "error", t.message)
                    }
                }
                payload += watchInfo(watch, value)
            }
            return BridgeProtocol.ok(
                "ok", true,
                "session", session?.let { sessionInfo(sessionIndex(it), it) },
                "warning", if (session != null && !session.isSuspended) "session is not suspended; returning cached watch values" else null,
                "watches", payload,
            )
        }

        private fun breakpoints(): Response {
            installAllSessionListeners()
            return BreakpointBridge.list(liveProjects())
        }

        private fun watchInfo(watch: WatchSpec, value: Any?): Map<String, Any?> {
            val cached = watchValues[watch.id]
            val effectiveValue = value ?: cached?.value
            return BridgeProtocol.map(
                "id", watch.id,
                "project", watch.project,
                "name", watch.name,
                "expression", watch.expression,
                "enabled", watch.enabled,
                "value", effectiveValue,
                "updated_at", cached?.updatedAt,
                "session", cached?.session,
                "selected_frame", cached?.selectedFrame,
                "error", cached?.error,
            )
        }

        private fun watchId(project: String?, name: String, expression: String): String {
            val raw = "${project ?: ""}|$name|$expression"
            return "watch_" + Base64.getUrlEncoder().withoutPadding()
                .encodeToString(raw.toByteArray(StandardCharsets.UTF_8))
        }

        private fun handleProvider(session: XDebugSession): (ObjectReference) -> String =
            { reference -> registerObjectHandle(session, reference) }

        private fun registerObjectHandle(session: XDebugSession, reference: ObjectReference): String {
            val key = sessionKey(session)
            val epoch = sessionHandleEpochs[key] ?: 0
            val handle = "obj_s${sessionIndex(session)}_e${epoch}_${reference.uniqueID()}"
            objectHandles[handle] = ObjectHandleEntry(key, epoch, reference)
            return handle
        }

        private fun resolveObjectHandle(session: XDebugSession, handle: String): ObjectHandleEntry? {
            val entry = objectHandles[handle] ?: return null
            val key = sessionKey(session)
            val epoch = sessionHandleEpochs[key] ?: 0
            if (entry.sessionKey != key || entry.epoch != epoch) {
                objectHandles.remove(handle)
                return null
            }
            return entry
        }

        private fun advanceHandleEpoch(session: XDebugSession) {
            val key = sessionKey(session)
            objectHandles.entries.removeIf { it.value.sessionKey == key }
            sessionHandleEpochs.compute(key) { _, current -> (current ?: 0) + 1 }
        }

        private fun handleScope(session: XDebugSession): Map<String, Any?> =
            BridgeProtocol.map(
                "session", sessionIndex(session),
                "suspend_epoch", sessionHandleEpochs[sessionKey(session)] ?: 0,
                "valid_until", "resume",
            )

        private fun coroutineThreadsPayload(session: XDebugSession, limit: Int, timeoutMs: Int): List<Any> {
            val stacks = session.suspendContext?.executionStacks ?: XExecutionStack.EMPTY_ARRAY
            val payload = mutableListOf<Any>()
            for (index in stacks.indices) {
                if (payload.size >= limit) break
                val top = stacks[index].topFrame
                val frames = mutableListOf<Any>()
                if (top is JavaStackFrame) {
                    frames.addAll(DebuggerValues.javaFrames(session, top, limit, timeoutMs))
                } else if (top != null) {
                    frames += DebuggerValues.frameInfo(top, 0)
                }
                payload += BridgeProtocol.map(
                    "index", index,
                    "name", stacks[index].displayName,
                    "dispatcher", dispatcherHint(stacks[index].displayName),
                    "top_frame", top?.let { DebuggerValues.frameInfo(it, 0) },
                    "frames", frames,
                )
            }
            return payload
        }

        private fun continuationPayload(
            session: XDebugSession,
            query: Map<String, String>,
            renderOptions: DebuggerValues.RenderOptions,
            limit: Int,
        ): List<Any> {
            val selected = DebuggerValues.selectedFrame(session, query)
            if (selected != null) {
                return continuationCandidates(selected.proxy, renderOptions, limit)
            }
            val stacks = session.suspendContext?.executionStacks ?: XExecutionStack.EMPTY_ARRAY
            val payload = mutableListOf<Any>()
            for (stack in stacks) {
                if (payload.size >= limit) break
                val top = stack.topFrame as? JavaStackFrame ?: continue
                payload.addAll(continuationCandidates(top.stackFrameProxy, renderOptions, limit - payload.size))
            }
            return payload
        }

        private fun continuationCandidates(
            proxy: StackFrameProxyImpl,
            renderOptions: DebuggerValues.RenderOptions,
            limit: Int,
        ): List<Any> {
            val payload = mutableListOf<Any>()
            val thisObject = proxy.thisObject()
            continuationInfo("this", thisObject, DebuggerValues.valueTypeName(thisObject), renderOptions)?.let {
                payload += it
            }
            for (local in proxy.visibleVariables()) {
                if (payload.size >= limit) break
                val value = proxy.getValue(local)
                continuationInfo(local.name(), value, local.typeName(), renderOptions)?.let {
                    payload += it
                }
            }
            return payload
        }

        private fun continuationInfo(
            name: String,
            value: Value?,
            declaredType: String?,
            renderOptions: DebuggerValues.RenderOptions,
        ): Map<String, Any?>? {
            val reference = value as? ObjectReference ?: return null
            val type = DebuggerValues.valueTypeName(reference)
            val fields = try {
                reference.referenceType().allFields()
            } catch (_: Throwable) {
                emptyList<Field>()
            }
            val fieldNames = fields.map { it.name() }.toSet()
            val continuationLike = type?.contains("Continuation") == true ||
                (fieldNames.contains("label") && fieldNames.contains("completion")) ||
                fields.any { isSpilledCoroutineField(it.name()) }
            if (!continuationLike) return null
            val label = readField(reference, fields, "label")?.toString()
            val completion = readField(reference, fields, "completion") as? ObjectReference
            val spilled = fields
                .filter { isSpilledCoroutineField(it.name()) }
                .take(renderOptions.maxFields)
                .map { field ->
                    try {
                        DebuggerValues.valueToMap(
                            field.name(),
                            reference.getValue(field),
                            field.typeName(),
                            renderOptions.child(),
                            hashSetOf(),
                        )
                    } catch (t: Throwable) {
                        BridgeProtocol.map("name", field.name(), "error", t.message)
                    }
                }
            return BridgeProtocol.map(
                "name", name,
                "class", type,
                "declared_type", declaredType,
                "object_id", reference.uniqueID(),
                "object_handle", renderOptions.handleProvider?.invoke(reference),
                "label", label,
                "completion_handle", completion?.let { renderOptions.handleProvider?.invoke(it) },
                "spilled_locals", spilled,
                "confidence", if (type?.contains("Continuation") == true) "medium" else "low",
            )
        }

        private fun readField(reference: ObjectReference, fields: List<Field>, name: String): Value? =
            try {
                fields.firstOrNull { it.name() == name }?.let { reference.getValue(it) }
            } catch (_: Throwable) {
                null
            }

        private fun isSpilledCoroutineField(name: String): Boolean {
            if (name.length < 3 || name[1] != '$') return false
            if (name[0] !in charArrayOf('L', 'I', 'J', 'F', 'D', 'Z')) return false
            return name.substring(2).all { it.isDigit() }
        }

        private fun dispatcherHint(threadName: String?): Map<String, Any?> {
            val lower = threadName?.lowercase().orEmpty()
            return when {
                lower.contains("main") -> BridgeProtocol.map("name", "Dispatchers.Main", "confidence", "medium")
                lower.contains("defaultdispatcher") || lower.contains("default") ->
                    BridgeProtocol.map("name", "Dispatchers.Default", "confidence", "low")
                lower.contains("io") -> BridgeProtocol.map("name", "Dispatchers.IO", "confidence", "low")
                else -> BridgeProtocol.map("name", null, "confidence", "none")
            }
        }

        private fun flowKind(type: String?): String? {
            val value = type ?: return null
            return when {
                value.contains("StateFlow") -> "StateFlow"
                value.contains("SharedFlow") -> "SharedFlow"
                value.endsWith("Flow") || value.contains(".Flow") -> "Flow"
                else -> null
            }
        }

        private fun sessionInfo(index: Int, session: XDebugSession): Map<String, Any?> {
            var pos: XSourcePosition? = null
            if (session.isSuspended) {
                try {
                    pos = session.currentPosition ?: session.currentStackFrame?.sourcePosition
                } catch (t: Throwable) {
                    LOG.debug("Unable to read current debugger source position", t)
                }
            }
            return BridgeProtocol.map(
                "index", index,
                "name", session.sessionName,
                "project", projectInfo(session.project),
                "suspended", session.isSuspended,
                "mixed_mode", session.isMixedMode,
                "process", session.debugProcess.javaClass.name,
                "position", sourcePositionInfo(pos),
            )
        }

        private fun sourcePositionInfo(pos: XSourcePosition?): Map<String, Any?>? {
            if (pos == null) return null
            return BridgeProtocol.map(
                "file", pos.file.path,
                "url", pos.file.url,
                "line", pos.line + 1,
                "offset", pos.offset,
            )
        }

        private fun projectInfo(project: Project): Map<String, Any?> =
            BridgeProtocol.map(
                "name", project.name,
                "base_path", project.basePath,
                "disposed", project.isDisposed,
            )

        private fun projectKey(project: Project): String = project.basePath ?: project.name

        private fun sessionKey(session: XDebugSession): String =
            "${projectKey(session.project)}|${session.sessionName}|${System.identityHashCode(session)}"

        private fun selectSession(query: Map<String, String>): XDebugSession? {
            val sessions = allSessions()
            query[BridgeQuery.SESSION]?.toIntOrNull()?.let { index ->
                if (index in sessions.indices) return sessions[index]
            }
            for (project in liveProjects()) {
                XDebuggerManager.getInstance(project).currentSession?.let { return it }
            }
            return sessions.firstOrNull()
        }

        private fun selectProject(query: Map<String, String>, file: String?): Project? {
            val requested = query[BridgeQuery.PROJECT]
            if (requested != null) {
                for (project in liveProjects()) {
                    if (requested == project.name || requested == project.basePath) return project
                }
            }
            if (file != null) {
                val normalized = File(file).absolutePath
                for (project in liveProjects()) {
                    val basePath = project.basePath
                    if (basePath != null && normalized.startsWith(File(basePath).absolutePath + File.separator)) {
                        return project
                    }
                }
            }
            return liveProjects().firstOrNull()
        }

        private fun sessionIndex(session: XDebugSession): Int =
            allSessions().indexOfFirst { it === session }.takeIf { it >= 0 } ?: 0

        private fun allSessions(): List<XDebugSession> =
            liveProjects().flatMap { project -> XDebuggerManager.getInstance(project).debugSessions.toList() }

        private fun liveProjects(): List<Project> {
            val live = projects.filterNot { it.isDisposed }
            if (live.size != projects.size) {
                projects.clear()
                projects.addAll(live)
                writeRegistry()
            }
            return live
        }

        private fun projectPayload(): List<Map<String, Any?>> =
            liveProjects().map(::projectInfo)

        private fun writeRegistry() {
            val url = serverUrl ?: return
            try {
                val dir = File(System.getProperty("user.home"), BridgeConfig.REGISTRY_DIR)
                Files.createDirectories(dir.toPath())
                val body = BridgeProtocol.obj(
                    "api_version", API_VERSION,
                    "url", url,
                    "pid", ProcessHandle.current().pid(),
                    "updated_at", Instant.now().toString(),
                    "projects", projectPayload(),
                )
                Files.writeString(File(dir, BridgeConfig.REGISTRY_FILE).toPath(), body, StandardCharsets.UTF_8)
            } catch (_: Throwable) {
            }
        }
    }

    private data class WatchSpec(
        val id: String,
        val project: String?,
        val name: String,
        val expression: String,
        val enabled: Boolean,
    )

    private data class WatchValue(
        val updatedAt: Long,
        val session: Any?,
        val selectedFrame: Any?,
        val value: Any?,
        val error: String?,
    ) {
        companion object {
            fun ok(updatedAt: Long, session: Any?, selectedFrame: Any?, value: Any?): WatchValue =
                WatchValue(updatedAt, session, selectedFrame, value, null)

            fun error(updatedAt: Long, session: Any?, selectedFrame: Any?, error: String?): WatchValue =
                WatchValue(updatedAt, session, selectedFrame, null, error)
        }
    }

    private data class ObjectHandleEntry(
        val sessionKey: String,
        val epoch: Int,
        val reference: ObjectReference,
    )
}
