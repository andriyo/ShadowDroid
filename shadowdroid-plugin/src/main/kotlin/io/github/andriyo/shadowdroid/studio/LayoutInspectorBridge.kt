package io.github.andriyo.shadowdroid.studio

import com.android.ide.common.rendering.api.ResourceReference
import com.android.tools.idea.layoutinspector.LayoutInspector
import com.android.tools.idea.layoutinspector.LayoutInspectorProjectService
import com.android.tools.idea.layoutinspector.setLayoutInspectorSelectedProcess
import com.android.tools.idea.layoutinspector.pipeline.appinspection.AppInspectionInspectorClient
import com.android.tools.idea.appinspection.inspector.api.process.ProcessDescriptor
import com.android.tools.idea.layoutinspector.model.AndroidWindow
import com.android.tools.idea.layoutinspector.model.ComposeViewNode
import com.android.tools.idea.layoutinspector.model.InspectorModel
import com.android.tools.idea.layoutinspector.model.RecompositionData
import com.android.tools.idea.layoutinspector.model.ViewNode
import com.android.tools.idea.layoutinspector.pipeline.InspectorClient
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.editor.Document
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Computable
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.psi.xml.XmlTag
import java.awt.Rectangle
import java.io.File
import java.util.concurrent.ConcurrentHashMap
import kotlin.math.min

internal object LayoutInspectorBridge {
    private const val DEFAULT_LAYOUT_WAIT_MS = 5_000
    private const val LAYOUT_POLL_MS = 100L
    private const val SOURCE_RESOLVER_TTL_MS = 30_000L

    private val sourceResolvers = ConcurrentHashMap<String, CachedSourceResolver>()

    @JvmStatic
    fun snapshot(project: Project?, query: Map<String, String>): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            val sourceResolver = sourceResolver(project)
            val activation = activateAndWait(project, query)
            StudioThreading.onIdeaThread {
                val state = layoutState(project)
                if (!state.available) {
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", BridgeValues.LAYOUT_SNAPSHOT,
                        "available", false,
                        "reason", state.reason,
                        "project", projectInfo(project),
                        "activation", activation,
                        "features", layoutFeatures(false, false, false),
                    )
                }

                val model = state.model ?: throw IllegalStateException("layout model unavailable")
                val windows = layoutWindows(project, model, sourceResolver)
                val nodeCount = windows.sumOf { ((it as Map<*, *>)["nodes"] as List<*>).size }
                val hasCompose = windows.any { windowHas(it as Map<*, *>, "compose") }
                val hasSemantics = windows.any { windowHas(it as Map<*, *>, "semantics") }
                val hasSource = windows.any { windowHas(it as Map<*, *>, "source") }
                BridgeProtocol.ok(
                    "ok", true,
                    "type", BridgeValues.LAYOUT_SNAPSHOT,
                    "available", true,
                    "project", projectInfo(project),
                    "activation", activation,
                    "client", layoutClientInfo(state.client),
                    "window_count", windows.size,
                    "node_count", nodeCount,
                    "features", layoutFeatures(hasCompose, hasSemantics, hasSource),
                    "windows", windows,
                )
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @JvmStatic
    fun recompositions(project: Project?, query: Map<String, String>): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        val reset = BridgeProtocol.booleanParam(query, BridgeQuery.RESET, false)
        return try {
            val sourceResolver = sourceResolver(project)
            val activation = activateAndWait(project, query)
            StudioThreading.onIdeaThread {
                val state = layoutState(project)
                if (!state.available) {
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", BridgeValues.LAYOUT_RECOMPOSITIONS,
                        "available", false,
                        "reset_requested", reset,
                        "reason", state.reason,
                        "project", projectInfo(project),
                        "activation", activation,
                    )
                }
                state.model?.recompositionModel?.observeAll()
                (state.client as? AppInspectionInspectorClient)?.updateRecompositionCountSettings()
                val nodes = mutableListOf<Any>()
                var count = 0
                var skips = 0
                var childCount = 0
                for (node in allLayoutNodes(state.model)) {
                    if (node !is ComposeViewNode) continue
                    val data = node.recompositions
                    nodes += BridgeProtocol.map(
                        "draw_id", node.drawId,
                        "qualified_name", node.qualifiedName,
                        "source", layoutSourceInfo(project, node, sourceResolver),
                        "recomposition", recompositionInfo(data),
                    )
                    count += data.count
                    skips += data.skips
                    childCount += data.childCount
                }
                // Reset only after the snapshot is built: a reset request
                // reports the counts it reset, not the zeroed state.
                if (reset) {
                    state.model?.resetRecompositionCounts()
                }
                BridgeProtocol.ok(
                    "ok", true,
                    "type", BridgeValues.LAYOUT_RECOMPOSITIONS,
                    "available", true,
                    "reset_requested", reset,
                    "project", projectInfo(project),
                    "activation", activation,
                    "summary", BridgeProtocol.map("nodes", nodes.size, "count", count, "skips", skips, "child_count", childCount),
                    "nodes", nodes,
                )
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    @JvmStatic
    fun source(project: Project?, query: Map<String, String>): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            val sourceResolver = sourceResolver(project)
            val activation = activateAndWait(project, query)
            StudioThreading.onIdeaThread {
                val state = layoutState(project)
                if (!state.available) {
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", BridgeValues.LAYOUT_SOURCE,
                        "available", false,
                        "reason", state.reason,
                        "project", projectInfo(project),
                        "activation", activation,
                    )
                }
                val node = bestLayoutNode(project, allLayoutNodes(state.model), query, sourceResolver)
                if (node != null) {
                    val source = layoutSourceInfo(project, node, sourceResolver)
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", BridgeValues.LAYOUT_SOURCE,
                        "available", source["available"] == true,
                        "project", projectInfo(project),
                        "activation", activation,
                        "matched", layoutNodeInfo(project, node, sourceResolver),
                        "source", source,
                    )
                }
                BridgeProtocol.ok(
                    "ok", true,
                    "type", BridgeValues.LAYOUT_SOURCE,
                    "available", false,
                    "reason", "no matching Android Studio Layout Inspector node",
                    "project", projectInfo(project),
                    "activation", activation,
                )
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t)
        }
    }

    private fun activateAndWait(project: Project, query: Map<String, String>): MutableMap<String, Any?> {
        val timeoutMs = layoutTimeoutMs(query)
        val deadline = BridgeProtocol.nowMs() + timeoutMs
        var activation = StudioThreading.onIdeaThread { activateLayoutInspector(project, query) }

        while (activation["requested"] == true && activation["ok"] != true && BridgeProtocol.nowMs() < deadline) {
            Thread.sleep(min(LAYOUT_POLL_MS, (deadline - BridgeProtocol.nowMs()).coerceAtLeast(1)))
            activation = StudioThreading.onIdeaThread { activateLayoutInspector(project, query) }
        }

        var state = StudioThreading.onIdeaThread { layoutState(project) }
        while (!state.available && BridgeProtocol.nowMs() < deadline) {
            Thread.sleep(min(LAYOUT_POLL_MS, (deadline - BridgeProtocol.nowMs()).coerceAtLeast(1)))
            state = StudioThreading.onIdeaThread { layoutState(project) }
        }

        activation["timeout_ms"] = timeoutMs
        activation["model_ready"] = state.available
        if (state.reason != null) {
            activation["final_reason"] = state.reason
        }
        return activation
    }

    private fun activateLayoutInspector(project: Project, query: Map<String, String>): MutableMap<String, Any?> {
        val inspector = LayoutInspectorProjectService.getInstance(project).getLayoutInspector()
        val target = LayoutTarget.from(query)
        val currentClient = inspector.currentClient
        val currentProcess = currentClient.process
        val payload = BridgeProtocol.map(
            "requested", target.requested,
            "target", target.info(),
            "project", projectInfo(project),
            "current_client", processInfo(currentProcess),
        )

        if (!target.requested) {
            payload["ok"] = currentClient.isConnected && !inspector.inspectorModel.isEmpty
            payload["reason"] = "no package or pid target supplied"
            return payload
        }

        val processModel = inspector.processModel ?: run {
            payload["ok"] = false
            payload["reason"] = "Android Studio Layout Inspector process model is unavailable"
            return payload
        }
        val launcher = inspector.launcher ?: run {
            payload["ok"] = false
            payload["reason"] = "Android Studio Layout Inspector launcher is unavailable"
            return payload
        }

        if (!target.device.isNullOrBlank()) {
            // The query's device may be a serial or a model name (deviceMatches
            // accepts both); the forced-serial API needs the actual serial.
            val serial = processModel.processes
                .map { it.device }
                .firstOrNull { target.device == it.serial || target.device == it.model }
                ?.serial ?: target.device
            inspector.deviceModel?.forcedDeviceSerialNumber = serial
            inspector.foregroundProcessDetection?.start(serial)
        }
        launcher.enabled = true
        inspector.inspectorClientSettings.inLiveMode = true

        if (target.matches(currentProcess) && currentClient.isConnected) {
            currentClient.refresh()
            payload["ok"] = true
            payload["selected"] = processInfo(currentProcess)
            payload["reused_client"] = true
            return payload
        }

        val visibleProcesses = processModel.processes
        // Exact process-name match outranks package match: an app's
        // ":subprocess" shares the package, and picking it yields an empty
        // UI-less layout model.
        val candidates = visibleProcesses
            .filter { target.matches(it) }
            .sortedWith(compareByDescending<ProcessDescriptor> { it.isRunning }
                .thenByDescending { target.exactProcessNameMatch(it) }
                .thenByDescending { target.exactPackageMatch(it) }
                .thenBy { it.pid })

        payload["visible_process_count"] = visibleProcesses.size
        if (candidates.isEmpty()) {
            payload["ok"] = false
            payload["reason"] = "no matching process visible to Android Studio Layout Inspector"
            payload["visible_processes"] = visibleProcesses.take(20).map { processInfo(it) }
            return payload
        }

        val selected = candidates.first()
        inspector.deviceModel?.setSelectedDevice(selected.device)
        processModel.setLayoutInspectorSelectedProcess(selected)
        inspector.currentClient.refresh()

        payload["ok"] = true
        payload["selected"] = processInfo(selected)
        payload["candidate_count"] = candidates.size
        return payload
    }

    private fun layoutTimeoutMs(query: Map<String, String>): Int =
        BridgeProtocol.intParam(query, BridgeQuery.TIMEOUT_MS, DEFAULT_LAYOUT_WAIT_MS, 100, 30_000)

    private fun layoutState(project: Project): LayoutState =
        try {
            val inspector: LayoutInspector = LayoutInspectorProjectService.getInstance(project).getLayoutInspector()
            val model = inspector.inspectorModel
            val client = inspector.currentClient
            if (model.isEmpty || allLayoutNodes(model).isEmpty()) {
                LayoutState(
                    project = project,
                    inspector = inspector,
                    model = model,
                    client = client,
                    available = false,
                    reason = "Android Studio Layout Inspector has no active model; open/toggle Layout Inspector for the running app first",
                )
            } else {
                LayoutState(project, inspector, model, client, true, null)
            }
        } catch (t: Throwable) {
            LayoutState(project, null, null, null, false, t.message ?: t.javaClass.name)
        }

    private fun layoutWindows(project: Project, model: InspectorModel, sourceResolver: SourceResolver): List<Any> {
        val windows = mutableListOf<Any>()
        for (window: AndroidWindow in model.windows.values) {
            val root = window.root
            val nodes = root.flattenedList().map { layoutNodeInfo(project, it, sourceResolver) }
            windows += BridgeProtocol.map(
                "id", window.id.toString(),
                "display_id", window.displayId,
                "width", window.width,
                "height", window.height,
                "image_type", window.imageType.name,
                "root_draw_id", root.drawId,
                "nodes", nodes,
            )
        }
        val root = model.root
        if (windows.isEmpty()) {
            windows += BridgeProtocol.map(
                "id", "root",
                "root_draw_id", root.drawId,
                "nodes", root.flattenedList().map { layoutNodeInfo(project, it, sourceResolver) },
            )
        }
        return windows
    }

    private fun allLayoutNodes(model: InspectorModel?): List<ViewNode> {
        if (model == null) return emptyList()
        val nodes = mutableListOf<ViewNode>()
        for (window in model.windows.values) {
            nodes += window.root.flattenedList()
        }
        if (nodes.isEmpty()) {
            nodes += model.root.flattenedList()
        }
        return nodes
    }

    private fun layoutNodeInfo(project: Project, node: ViewNode, sourceResolver: SourceResolver): MutableMap<String, Any?> {
        val parent = parentNode(node)
        val payload = BridgeProtocol.map(
            "draw_id", node.drawId,
            "parent_draw_id", parent?.drawId,
            "kind", if (node is ComposeViewNode) "compose" else "view",
            "qualified_name", node.qualifiedName,
            "unqualified_name", node.unqualifiedName,
            "text", node.textValue,
            "view_id", resourceInfo(node.viewId),
            "layout", resourceInfo(node.layout),
            "bounds", rectangleInfo(node.layoutBounds),
            "render_bounds", rectangleInfo(node.renderBounds.bounds),
            "flags", node.layoutFlags,
            "system", node.isSystemNode,
            "derived_from_webview", node.isDerivedFromWebView,
            "semantics", BridgeProtocol.map(
                "merged", node.hasMergedSemantics,
                "unmerged", node.hasUnmergedSemantics,
            ),
            "recomposition", recompositionInfo(node.recompositions),
            "source", layoutSourceInfo(project, node, sourceResolver),
        )
        if (node is ComposeViewNode) {
            payload["compose"] = composeInfo(node)
        }
        return payload
    }

    private fun layoutSourceInfo(project: Project, node: ViewNode, sourceResolver: SourceResolver): Map<String, Any?> {
        if (node is ComposeViewNode) {
            val file = sourceResolver.resolve(node.composeFilename)
            val line = node.composeLineNumber
            val available = node.hasSourceCodeInformation
            return BridgeProtocol.map(
                "available", available,
                "kind", "compose",
                "file", file,
                "url", file?.let { File(it).toURI().toString() },
                "filename", node.composeFilename,
                "line", line.takeIf { it > 0 },
                "offset", node.composeOffset.takeIf { it >= 0 },
                "package_hash", node.composePackageHash,
                "anchor_hash", node.anchorHash,
                "has_source_code_information", node.hasSourceCodeInformation,
            )
        }

        val tag: XmlTag = readActionOrNull { node.tag } ?: return BridgeProtocol.map(
            "available", false,
            "kind", "view",
            "reason", "no source location in active Layout Inspector node",
        )
        val file: VirtualFile = tag.containingFile?.virtualFile ?: return BridgeProtocol.map(
            "available", false,
            "kind", "view",
            "reason", "no source location in active Layout Inspector node",
        )
        val offset = tag.textOffset
        return BridgeProtocol.map(
            "available", true,
            "kind", "view",
            "file", file.path,
            "url", file.url,
            "line", lineForOffset(file, offset),
            "offset", offset,
            "tag", tag.name,
        )
    }

    private fun composeInfo(node: ComposeViewNode): Map<String, Any?> =
        BridgeProtocol.map(
            "filename", node.composeFilename,
            "line", node.composeLineNumber.takeIf { it > 0 },
            "offset", node.composeOffset.takeIf { it >= 0 },
            "package_hash", node.composePackageHash,
            "flags", node.composeFlags,
            "anchor_hash", node.anchorHash,
            "inlined", node.isInlined,
            "has_draw_modifier", node.hasComposeDrawModifier,
            "has_child_draw_modifier", node.hasChildComposeDrawModifier,
            "has_source_code_information", node.hasSourceCodeInformation,
        )

    private fun recompositionInfo(data: RecompositionData?): Map<String, Any?> {
        if (data == null) return BridgeProtocol.map("available", false)
        return BridgeProtocol.map(
            "available", true,
            "count", data.count,
            "skips", data.skips,
            "child_count", data.childCount,
            "highlight_count", data.highlightCount,
            "empty", data.isEmpty,
        )
    }

    private fun resourceInfo(ref: ResourceReference?): Map<String, Any?>? {
        if (ref == null) return null
        return BridgeProtocol.map(
            "name", ref.name,
            "qualified_name", ref.qualifiedName,
            "type", ref.resourceType?.name,
            "namespace", ref.namespace?.toString(),
            "framework", isFramework(ref),
            "value", ref.toString(),
        )
    }

    @Suppress("DEPRECATION")
    private fun isFramework(ref: ResourceReference): Boolean = ref.isFramework

    private fun rectangleInfo(rectangle: Rectangle?): Map<String, Any?>? {
        if (rectangle == null) return null
        return BridgeProtocol.map(
            "x", rectangle.x,
            "y", rectangle.y,
            "width", rectangle.width,
            "height", rectangle.height,
            "left", rectangle.x,
            "top", rectangle.y,
            "right", rectangle.x + rectangle.width,
            "bottom", rectangle.y + rectangle.height,
        )
    }

    private fun layoutClientInfo(client: InspectorClient?): Map<String, Any?> {
        if (client == null) return BridgeProtocol.map("connected", false)
        val process = try {
            val descriptor = client.process
            BridgeProtocol.map(
                "name", descriptor.name,
                "package", descriptor.packageName,
                "pid", descriptor.pid,
                "running", descriptor.isRunning,
                "stream_id", descriptor.streamId,
                "device", descriptor.device.let { device ->
                    BridgeProtocol.map(
                        "serial", device.serial,
                        "manufacturer", device.manufacturer,
                        "model", device.model,
                        "emulator", device.isEmulator,
                        "api", device.apiLevel.toString(),
                        "version", device.version,
                        "codename", device.codename,
                    )
                },
            )
        } catch (_: Throwable) {
            null
        }
        return BridgeProtocol.map(
            "connected", client.isConnected,
            "state", client.state.name,
            "live", client.inLiveMode,
            "client_type", client.clientType.name,
            "capabilities", client.capabilities.map { it.name },
            "process", process,
        )
    }

    private fun processInfo(process: ProcessDescriptor): Map<String, Any?> =
        BridgeProtocol.map(
            "name", process.name,
            "package", process.packageName,
            "pid", process.pid,
            "running", process.isRunning,
            "stream_id", process.streamId,
            "abi", process.abiCpuArch,
            "device", BridgeProtocol.map(
                "serial", process.device.serial,
                "manufacturer", process.device.manufacturer,
                "model", process.device.model,
                "emulator", process.device.isEmulator,
                "api", process.device.apiLevel.toString(),
                "version", process.device.version,
                "codename", process.device.codename,
            ),
        )

    private data class LayoutTarget(
        val device: String?,
        val packageName: String?,
        val pid: Int?,
    ) {
        val requested: Boolean
            get() = !packageName.isNullOrBlank() || pid != null

        fun matches(process: ProcessDescriptor): Boolean =
            deviceMatches(process) && pidMatches(process) && packageMatches(process)

        fun exactPackageMatch(process: ProcessDescriptor): Boolean {
            if (packageName.isNullOrBlank()) return false
            return packageName == process.packageName || packageName == process.name
        }

        fun exactProcessNameMatch(process: ProcessDescriptor): Boolean {
            if (packageName.isNullOrBlank()) return false
            return packageName == process.name
        }

        fun info(): Map<String, Any?> =
            BridgeProtocol.map(
                "device", device,
                "package", packageName,
                "pid", pid,
            )

        private fun deviceMatches(process: ProcessDescriptor): Boolean {
            if (device.isNullOrBlank()) return true
            val descriptor = process.device
            return device == descriptor.serial || device == descriptor.model
        }

        private fun pidMatches(process: ProcessDescriptor): Boolean =
            pid == null || pid == process.pid

        private fun packageMatches(process: ProcessDescriptor): Boolean {
            if (packageName.isNullOrBlank()) return true
            return packageName == process.packageName ||
                packageName == process.name ||
                process.name.startsWith("$packageName:")
        }

        companion object {
            fun from(query: Map<String, String>): LayoutTarget =
                LayoutTarget(
                    device = query[BridgeQuery.DEVICE]?.takeIf { it.isNotBlank() },
                    packageName = query[BridgeQuery.PACKAGE]?.takeIf { it.isNotBlank() },
                    pid = optionalInt(query[BridgeQuery.PID]),
                )

            private fun optionalInt(value: String?): Int? {
                if (value.isNullOrBlank()) return null
                return value.toIntOrNull() ?: throw IllegalArgumentException("invalid integer: $value")
            }
        }
    }

    private fun layoutFeatures(compose: Boolean, semantics: Boolean, sourceMap: Boolean): Map<String, Any?> =
        BridgeProtocol.map(
            "compose", BridgeProtocol.map("available", compose, "source", BridgeValues.LAYOUT_INSPECTOR_SOURCE),
            "semantics", BridgeProtocol.map("available", semantics, "source", BridgeValues.LAYOUT_INSPECTOR_SOURCE),
            "source_map", BridgeProtocol.map("available", sourceMap, "source", BridgeValues.LAYOUT_INSPECTOR_SOURCE),
        )

    private fun windowHas(window: Map<*, *>, feature: String): Boolean {
        val nodes = window["nodes"] as? List<*> ?: return false
        for (nodeValue in nodes) {
            val node = nodeValue as? Map<*, *> ?: continue
            if (feature == "compose" && node["kind"] == "compose") return true
            val semantics = node["semantics"] as? Map<*, *>
            if (feature == "semantics" && (semantics?.get("merged") == true || semantics?.get("unmerged") == true)) return true
            val source = node["source"] as? Map<*, *>
            if (feature == "source" && source?.get("available") == true) return true
        }
        return false
    }

    private fun bestLayoutNode(
        project: Project,
        nodes: List<ViewNode>,
        query: Map<String, String>,
        sourceResolver: SourceResolver,
    ): ViewNode? {
        val requestedBounds = Bounds.parse(query[BridgeQuery.BOUNDS])
        return nodes.mapNotNull { node ->
            layoutNodeScore(project, node, query, requestedBounds, sourceResolver)?.let { score ->
                ScoredLayoutNode(score, nodeArea(node), node)
            }
        }.minWithOrNull(compareBy<ScoredLayoutNode> { it.score }.thenBy { it.area })?.node
    }

    private fun layoutNodeScore(
        project: Project,
        node: ViewNode,
        query: Map<String, String>,
        requestedBounds: Bounds?,
        sourceResolver: SourceResolver,
    ): Double? {
        val drawId = optionalLong(query[BridgeQuery.DRAW_ID])
        if (drawId != null) return if (node.drawId == drawId) 0.0 else null

        val file = query[BridgeQuery.FILE]
        if (!file.isNullOrBlank()) {
            val sourceFile = layoutSourceInfo(project, node, sourceResolver)["file"] as? String
            if (sourceFile == null || !sourceFile.endsWith(file)) return null
        }

        if (requestedBounds != null) {
            val nodeBounds = nodeBounds(node)
            val coverage = requestedBounds.coverageBy(nodeBounds)
            val centerDistance = requestedBounds.centerDistance(nodeBounds)
            if (coverage <= 0.0 && centerDistance > 96.0) return null
            val areaDelta = kotlin.math.abs(nodeBounds.area - requestedBounds.area).toDouble() /
                requestedBounds.area.coerceAtLeast(1).toDouble()
            var score = ((1.0 - coverage) * 10.0) + areaDelta + (centerDistance / 10_000.0)
            if (containsIgnoreCase(node.textValue, query[BridgeQuery.TEXT])) score -= 0.20
            if (containsIgnoreCase(resourceSearchText(node.viewId), query[BridgeQuery.RID])) score -= 0.10
            if (containsIgnoreCase(node.qualifiedName, query[BridgeQuery.CLASS])) score -= 0.05
            val source = layoutSourceInfo(project, node, sourceResolver)
            val sourceFile = source["file"] as? String
            when {
                source["available"] == true && sourceFile != null -> score -= 1.00
                source["available"] == true -> score -= 0.10
                else -> score += 0.25
            }
            if (node.isSystemNode) score += 0.50
            return score
        }

        if (!containsIgnoreCase(node.textValue, query[BridgeQuery.TEXT])) return null
        if (!containsIgnoreCase(resourceSearchText(node.viewId), query[BridgeQuery.RID])) return null
        if (!containsIgnoreCase(node.qualifiedName, query[BridgeQuery.CLASS])) return null
        return 0.0
    }

    private data class ScoredLayoutNode(
        val score: Double,
        val area: Int,
        val node: ViewNode,
    )

    private data class Bounds(
        val left: Int,
        val top: Int,
        val right: Int,
        val bottom: Int,
    ) {
        val width: Int
            get() = (right - left).coerceAtLeast(0)
        val height: Int
            get() = (bottom - top).coerceAtLeast(0)
        val area: Int
            get() = width * height

        fun coverageBy(other: Bounds): Double {
            val intersectionWidth = (minOf(right, other.right) - maxOf(left, other.left)).coerceAtLeast(0)
            val intersectionHeight = (minOf(bottom, other.bottom) - maxOf(top, other.top)).coerceAtLeast(0)
            val intersection = intersectionWidth * intersectionHeight
            return intersection.toDouble() / area.coerceAtLeast(1).toDouble()
        }

        fun centerDistance(other: Bounds): Double {
            val dx = ((left + right) / 2.0) - ((other.left + other.right) / 2.0)
            val dy = ((top + bottom) / 2.0) - ((other.top + other.bottom) / 2.0)
            return kotlin.math.sqrt(dx * dx + dy * dy)
        }

        companion object {
            fun parse(value: String?): Bounds? {
                if (value.isNullOrBlank()) return null
                val parts = value.split(',').map { it.trim().toIntOrNull() }
                if (parts.size != 4 || parts.any { it == null }) {
                    throw IllegalArgumentException("invalid bounds: $value")
                }
                return Bounds(parts[0]!!, parts[1]!!, parts[2]!!, parts[3]!!)
            }
        }
    }

    private fun nodeBounds(node: ViewNode): Bounds {
        val rect = node.renderBounds.bounds
        return Bounds(rect.x, rect.y, rect.x + rect.width, rect.y + rect.height)
    }

    private fun nodeArea(node: ViewNode): Int = nodeBounds(node).area

    private fun resourceSearchText(ref: ResourceReference?): String? {
        if (ref == null) return null
        return listOf(ref.name, ref.qualifiedName, ref.toString()).joinToString(" ")
    }

    private fun containsIgnoreCase(actual: String?, expected: String?): Boolean =
        expected.isNullOrBlank() || actual?.contains(expected, ignoreCase = true) == true

    private fun optionalLong(value: String?): Long? {
        if (value.isNullOrBlank()) return null
        return value.toLongOrNull() ?: throw IllegalArgumentException("invalid integer: $value")
    }

    // ViewNode.readAccess is the supported way to read tree structure; the
    // previous reflection on the Kotlin synthetic accessor silently returns
    // null on current Studio builds (the accessor is no longer generated).
    private fun parentNode(node: ViewNode): ViewNode? =
        try {
            ViewNode.readAccess { node.parent }
        } catch (_: Throwable) {
            null
        }

    // Rebuilt after a short TTL so files created since the last walk resolve;
    // the walk prunes build/VCS/dependency directories, so rebuilds stay cheap.
    private fun sourceResolver(project: Project): SourceResolver {
        val key = projectKey(project)
        val now = BridgeProtocol.nowMs()
        val cached = sourceResolvers[key]
        if (cached != null && now - cached.builtAtMs < SOURCE_RESOLVER_TTL_MS) return cached.resolver
        val resolver = SourceResolver.build(project.basePath)
        sourceResolvers[key] = CachedSourceResolver(now, resolver)
        return resolver
    }

    private data class CachedSourceResolver(
        val builtAtMs: Long,
        val resolver: SourceResolver,
    )

    private fun projectKey(project: Project): String = project.basePath ?: project.name

    private fun projectInfo(project: Project): Map<String, Any?> =
        BridgeProtocol.map(
            "name", project.name,
            "base_path", project.basePath,
            "disposed", project.isDisposed,
        )

    private fun lineForOffset(file: VirtualFile?, offset: Int): Int? {
        if (file == null || offset < 0) return null
        val document: Document = FileDocumentManager.getInstance().getDocument(file) ?: return null
        if (offset > document.textLength) return null
        return document.getLineNumber(offset) + 1
    }

    private fun <T> readActionOrNull(supplier: ThrowingSupplier<T>): T? =
        try {
            ApplicationManager.getApplication().runReadAction(Computable {
                try {
                    supplier.get()
                } catch (e: Exception) {
                    throw RuntimeException(e)
                }
            })
        } catch (_: Throwable) {
            null
        }

    private data class LayoutState(
        val project: Project,
        val inspector: LayoutInspector?,
        val model: InspectorModel?,
        val client: InspectorClient?,
        val available: Boolean,
        val reason: String?,
    )
}
