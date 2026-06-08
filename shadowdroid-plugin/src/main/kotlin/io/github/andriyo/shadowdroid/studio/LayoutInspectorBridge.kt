package io.github.andriyo.shadowdroid.studio

import com.android.ide.common.rendering.api.ResourceReference
import com.android.tools.idea.layoutinspector.LayoutInspector
import com.android.tools.idea.layoutinspector.LayoutInspectorProjectService
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

internal object LayoutInspectorBridge {
    private val sourceResolvers = ConcurrentHashMap<String, SourceResolver>()

    @JvmStatic
    fun snapshot(project: Project?, query: Map<String, String>): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            val sourceResolver = sourceResolver(project)
            StudioThreading.onIdeaThread {
                val state = layoutState(project)
                if (!state.available) {
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", "layout_snapshot",
                        "available", false,
                        "reason", state.reason,
                        "project", projectInfo(project),
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
                    "type", "layout_snapshot",
                    "available", true,
                    "project", projectInfo(project),
                    "client", layoutClientInfo(state.client),
                    "window_count", windows.size,
                    "node_count", nodeCount,
                    "features", layoutFeatures(hasCompose, hasSemantics, hasSource),
                    "windows", windows,
                )
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t.message)
        }
    }

    @JvmStatic
    fun recompositions(project: Project?, query: Map<String, String>): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        val reset = BridgeProtocol.booleanParam(query, "reset", false)
        return try {
            val sourceResolver = sourceResolver(project)
            StudioThreading.onIdeaThread {
                val state = layoutState(project)
                if (!state.available) {
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", "layout_recompositions",
                        "available", false,
                        "reset_requested", reset,
                        "reason", state.reason,
                        "project", projectInfo(project),
                    )
                }
                if (reset) {
                    state.model?.resetRecompositionCounts()
                }
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
                BridgeProtocol.ok(
                    "ok", true,
                    "type", "layout_recompositions",
                    "available", true,
                    "reset_requested", reset,
                    "project", projectInfo(project),
                    "summary", BridgeProtocol.map("nodes", nodes.size, "count", count, "skips", skips, "child_count", childCount),
                    "nodes", nodes,
                )
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t.message)
        }
    }

    @JvmStatic
    fun source(project: Project?, query: Map<String, String>): Response {
        if (project == null) return BridgeProtocol.bad("no project")
        return try {
            val sourceResolver = sourceResolver(project)
            StudioThreading.onIdeaThread {
                val state = layoutState(project)
                if (!state.available) {
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", "layout_source",
                        "available", false,
                        "reason", state.reason,
                        "project", projectInfo(project),
                    )
                }
                for (node in allLayoutNodes(state.model)) {
                    if (!layoutNodeMatches(project, node, query, sourceResolver)) continue
                    val source = layoutSourceInfo(project, node, sourceResolver)
                    return@onIdeaThread BridgeProtocol.ok(
                        "ok", true,
                        "type", "layout_source",
                        "available", source["available"] == true,
                        "project", projectInfo(project),
                        "matched", layoutNodeInfo(project, node, sourceResolver),
                        "source", source,
                    )
                }
                BridgeProtocol.ok(
                    "ok", true,
                    "type", "layout_source",
                    "available", false,
                    "reason", "no matching Android Studio Layout Inspector node",
                    "project", projectInfo(project),
                )
            }
        } catch (t: Throwable) {
            BridgeProtocol.bad(t.message)
        }
    }

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

    private fun layoutFeatures(compose: Boolean, semantics: Boolean, sourceMap: Boolean): Map<String, Any?> =
        BridgeProtocol.map(
            "compose", BridgeProtocol.map("available", compose, "source", "android_studio_layout_inspector"),
            "semantics", BridgeProtocol.map("available", semantics, "source", "android_studio_layout_inspector"),
            "source_map", BridgeProtocol.map("available", sourceMap, "source", "android_studio_layout_inspector"),
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

    private fun layoutNodeMatches(
        project: Project,
        node: ViewNode,
        query: Map<String, String>,
        sourceResolver: SourceResolver,
    ): Boolean {
        val drawId = optionalLong(query["draw_id"])
        if (drawId != null && node.drawId != drawId) return false
        if (!containsIgnoreCase(node.textValue, query["text"])) return false
        if (!containsIgnoreCase(resourceSearchText(node.viewId), query["rid"])) return false
        if (!containsIgnoreCase(node.qualifiedName, query["class"])) return false

        val file = query["file"]
        if (!file.isNullOrBlank()) {
            val sourceFile = layoutSourceInfo(project, node, sourceResolver)["file"] as? String
            if (sourceFile == null || !sourceFile.endsWith(file)) return false
        }
        return true
    }

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

    private fun parentNode(node: ViewNode): ViewNode? =
        try {
            val method = ViewNode::class.java.getDeclaredMethod("access\$getParent\$p", ViewNode::class.java)
            method.isAccessible = true
            method.invoke(null, node) as? ViewNode
        } catch (_: Throwable) {
            null
        }

    private fun sourceResolver(project: Project): SourceResolver =
        sourceResolvers.computeIfAbsent(projectKey(project)) { SourceResolver.build(project.basePath) }

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
