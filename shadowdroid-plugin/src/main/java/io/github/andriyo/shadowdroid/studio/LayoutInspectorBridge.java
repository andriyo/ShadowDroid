package io.github.andriyo.shadowdroid.studio;

import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.bad;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.booleanParam;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.map;
import static io.github.andriyo.shadowdroid.studio.BridgeProtocol.ok;
import static io.github.andriyo.shadowdroid.studio.StudioThreading.onIdeaThread;

import com.android.ide.common.rendering.api.ResourceReference;
import com.android.tools.idea.layoutinspector.LayoutInspector;
import com.android.tools.idea.layoutinspector.LayoutInspectorProjectService;
import com.android.tools.idea.layoutinspector.model.AndroidWindow;
import com.android.tools.idea.layoutinspector.model.ComposeViewNode;
import com.android.tools.idea.layoutinspector.model.InspectorModel;
import com.android.tools.idea.layoutinspector.model.RecompositionData;
import com.android.tools.idea.layoutinspector.model.ViewNode;
import com.android.tools.idea.layoutinspector.pipeline.InspectorClient;
import com.intellij.openapi.application.ApplicationManager;
import com.intellij.openapi.editor.Document;
import com.intellij.openapi.fileEditor.FileDocumentManager;
import com.intellij.openapi.project.Project;
import com.intellij.openapi.util.Computable;
import com.intellij.openapi.vfs.VirtualFile;
import com.intellij.psi.xml.XmlTag;

import java.awt.Rectangle;
import java.io.File;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

final class LayoutInspectorBridge {
    private static final ConcurrentMap<String, SourceResolver> SOURCE_RESOLVERS = new ConcurrentHashMap<>();

    private LayoutInspectorBridge() {
    }

    static Response snapshot(Project project, Map<String, String> query) {
        if (project == null) return bad("no project");
        try {
            SourceResolver sourceResolver = sourceResolver(project);
            return onIdeaThread(() -> {
                LayoutState state = layoutState(project);
                if (!state.available()) {
                    return ok(
                        "ok", true,
                        "type", "layout_snapshot",
                        "available", false,
                        "reason", state.reason(),
                        "project", projectInfo(project),
                        "features", layoutFeatures(false, false, false)
                    );
                }

                List<Object> windows = layoutWindows(project, state.model(), sourceResolver);
                int nodeCount = windows.stream()
                    .mapToInt(window -> ((List<?>) ((Map<?, ?>) window).get("nodes")).size())
                    .sum();
                boolean hasCompose = windows.stream().anyMatch(window -> windowHas((Map<?, ?>) window, "compose"));
                boolean hasSemantics = windows.stream().anyMatch(window -> windowHas((Map<?, ?>) window, "semantics"));
                boolean hasSource = windows.stream().anyMatch(window -> windowHas((Map<?, ?>) window, "source"));
                return ok(
                    "ok", true,
                    "type", "layout_snapshot",
                    "available", true,
                    "project", projectInfo(project),
                    "client", layoutClientInfo(state.client()),
                    "window_count", windows.size(),
                    "node_count", nodeCount,
                    "features", layoutFeatures(hasCompose, hasSemantics, hasSource),
                    "windows", windows
                );
            });
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    static Response recompositions(Project project, Map<String, String> query) {
        if (project == null) return bad("no project");
        boolean reset = booleanParam(query, "reset", false);
        try {
            SourceResolver sourceResolver = sourceResolver(project);
            return onIdeaThread(() -> {
                LayoutState state = layoutState(project);
                if (!state.available()) {
                    return ok(
                        "ok", true,
                        "type", "layout_recompositions",
                        "available", false,
                        "reset_requested", reset,
                        "reason", state.reason(),
                        "project", projectInfo(project)
                    );
                }
                if (reset) {
                    state.model().resetRecompositionCounts();
                }
                List<Object> nodes = new ArrayList<>();
                int count = 0;
                int skips = 0;
                int childCount = 0;
                for (ViewNode node : allLayoutNodes(state.model())) {
                    if (!(node instanceof ComposeViewNode)) continue;
                    RecompositionData data = node.getRecompositions();
                    Map<String, Object> info = recompositionInfo(data);
                    nodes.add(map(
                        "draw_id", node.getDrawId(),
                        "qualified_name", node.getQualifiedName(),
                        "source", layoutSourceInfo(project, node, sourceResolver),
                        "recomposition", info
                    ));
                    count += data == null ? 0 : data.getCount();
                    skips += data == null ? 0 : data.getSkips();
                    childCount += data == null ? 0 : data.getChildCount();
                }
                return ok(
                    "ok", true,
                    "type", "layout_recompositions",
                    "available", true,
                    "reset_requested", reset,
                    "project", projectInfo(project),
                    "summary", map("nodes", nodes.size(), "count", count, "skips", skips, "child_count", childCount),
                    "nodes", nodes
                );
            });
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    static Response source(Project project, Map<String, String> query) {
        if (project == null) return bad("no project");
        try {
            SourceResolver sourceResolver = sourceResolver(project);
            return onIdeaThread(() -> {
                LayoutState state = layoutState(project);
                if (!state.available()) {
                    return ok(
                        "ok", true,
                        "type", "layout_source",
                        "available", false,
                        "reason", state.reason(),
                        "project", projectInfo(project)
                    );
                }
                for (ViewNode node : allLayoutNodes(state.model())) {
                    if (!layoutNodeMatches(project, node, query, sourceResolver)) continue;
                    Map<String, Object> source = layoutSourceInfo(project, node, sourceResolver);
                    return ok(
                        "ok", true,
                        "type", "layout_source",
                        "available", Boolean.TRUE.equals(source.get("available")),
                        "project", projectInfo(project),
                        "matched", layoutNodeInfo(project, node, sourceResolver),
                        "source", source
                    );
                }
                return ok(
                    "ok", true,
                    "type", "layout_source",
                    "available", false,
                    "reason", "no matching Android Studio Layout Inspector node",
                    "project", projectInfo(project)
                );
            });
        } catch (Throwable t) {
            return bad(t.getMessage());
        }
    }

    private static LayoutState layoutState(Project project) {
        try {
            LayoutInspector inspector = LayoutInspectorProjectService.getInstance(project).getLayoutInspector();
            InspectorModel model = inspector.getInspectorModel();
            InspectorClient client = inspector.getCurrentClient();
            if (model == null || model.isEmpty() || allLayoutNodes(model).isEmpty()) {
                return new LayoutState(project, inspector, model, client, false, "Android Studio Layout Inspector has no active model; open/toggle Layout Inspector for the running app first");
            }
            return new LayoutState(project, inspector, model, client, true, null);
        } catch (Throwable t) {
            return new LayoutState(project, null, null, null, false, t.getMessage() == null ? t.getClass().getName() : t.getMessage());
        }
    }

    private static List<Object> layoutWindows(Project project, InspectorModel model, SourceResolver sourceResolver) {
        List<Object> windows = new ArrayList<>();
        for (AndroidWindow window : model.getWindows().values()) {
            ViewNode root = window.getRoot();
            List<Object> nodes = new ArrayList<>();
            if (root != null) {
                for (ViewNode node : root.flattenedList()) {
                    nodes.add(layoutNodeInfo(project, node, sourceResolver));
                }
            }
            windows.add(map(
                "id", String.valueOf(window.getId()),
                "display_id", window.getDisplayId(),
                "width", window.getWidth(),
                "height", window.getHeight(),
                "image_type", window.getImageType() == null ? null : window.getImageType().name(),
                "root_draw_id", root == null ? null : root.getDrawId(),
                "nodes", nodes
            ));
        }
        if (windows.isEmpty() && model.getRoot() != null) {
            List<Object> nodes = new ArrayList<>();
            for (ViewNode node : model.getRoot().flattenedList()) {
                nodes.add(layoutNodeInfo(project, node, sourceResolver));
            }
            windows.add(map("id", "root", "root_draw_id", model.getRoot().getDrawId(), "nodes", nodes));
        }
        return windows;
    }

    private static List<ViewNode> allLayoutNodes(InspectorModel model) {
        List<ViewNode> nodes = new ArrayList<>();
        if (model == null) return nodes;
        for (AndroidWindow window : model.getWindows().values()) {
            if (window.getRoot() != null) {
                nodes.addAll(window.getRoot().flattenedList());
            }
        }
        if (nodes.isEmpty() && model.getRoot() != null) {
            nodes.addAll(model.getRoot().flattenedList());
        }
        return nodes;
    }

    private static Map<String, Object> layoutNodeInfo(Project project, ViewNode node, SourceResolver sourceResolver) {
        ViewNode parent = parentNode(node);
        Map<String, Object> payload = map(
            "draw_id", node.getDrawId(),
            "parent_draw_id", parent == null ? null : parent.getDrawId(),
            "kind", node instanceof ComposeViewNode ? "compose" : "view",
            "qualified_name", node.getQualifiedName(),
            "unqualified_name", node.getUnqualifiedName(),
            "text", node.getTextValue(),
            "view_id", resourceInfo(node.getViewId()),
            "layout", resourceInfo(node.getLayout()),
            "bounds", rectangleInfo(node.getLayoutBounds()),
            "render_bounds", rectangleInfo(node.getRenderBounds() == null ? null : node.getRenderBounds().getBounds()),
            "flags", node.getLayoutFlags(),
            "system", node.isSystemNode(),
            "derived_from_webview", node.isDerivedFromWebView(),
            "semantics", map(
                "merged", node.getHasMergedSemantics(),
                "unmerged", node.getHasUnmergedSemantics()
            ),
            "recomposition", recompositionInfo(node.getRecompositions()),
            "source", layoutSourceInfo(project, node, sourceResolver)
        );
        if (node instanceof ComposeViewNode composeNode) {
            payload.put("compose", composeInfo(composeNode));
        }
        return payload;
    }

    private static Map<String, Object> layoutSourceInfo(Project project, ViewNode node, SourceResolver sourceResolver) {
        if (node instanceof ComposeViewNode composeNode) {
            String file = sourceResolver.resolve(composeNode.getComposeFilename());
            int line = composeNode.getComposeLineNumber();
            boolean available = composeNode.getHasSourceCodeInformation() && (file != null || composeNode.getComposeFilename() != null);
            return map(
                "available", available,
                "kind", "compose",
                "file", file,
                "url", file == null ? null : new File(file).toURI().toString(),
                "filename", composeNode.getComposeFilename(),
                "line", line > 0 ? line : null,
                "offset", composeNode.getComposeOffset() >= 0 ? composeNode.getComposeOffset() : null,
                "package_hash", composeNode.getComposePackageHash(),
                "anchor_hash", composeNode.getAnchorHash(),
                "has_source_code_information", composeNode.getHasSourceCodeInformation()
            );
        }

        XmlTag tag = readActionOrNull(node::getTag);
        if (tag == null || tag.getContainingFile() == null || tag.getContainingFile().getVirtualFile() == null) {
            return map("available", false, "kind", "view", "reason", "no source location in active Layout Inspector node");
        }
        VirtualFile file = tag.getContainingFile().getVirtualFile();
        int offset = tag.getTextOffset();
        return map(
            "available", true,
            "kind", "view",
            "file", file.getPath(),
            "url", file.getUrl(),
            "line", lineForOffset(file, offset),
            "offset", offset,
            "tag", tag.getName()
        );
    }

    private static Map<String, Object> composeInfo(ComposeViewNode node) {
        return map(
            "filename", node.getComposeFilename(),
            "line", node.getComposeLineNumber() > 0 ? node.getComposeLineNumber() : null,
            "offset", node.getComposeOffset() >= 0 ? node.getComposeOffset() : null,
            "package_hash", node.getComposePackageHash(),
            "flags", node.getComposeFlags(),
            "anchor_hash", node.getAnchorHash(),
            "inlined", node.isInlined(),
            "has_draw_modifier", node.getHasComposeDrawModifier(),
            "has_child_draw_modifier", node.getHasChildComposeDrawModifier(),
            "has_source_code_information", node.getHasSourceCodeInformation()
        );
    }

    private static Map<String, Object> recompositionInfo(RecompositionData data) {
        if (data == null) {
            return map("available", false);
        }
        return map(
            "available", true,
            "count", data.getCount(),
            "skips", data.getSkips(),
            "child_count", data.getChildCount(),
            "highlight_count", data.getHighlightCount(),
            "empty", data.isEmpty()
        );
    }

    private static Map<String, Object> resourceInfo(ResourceReference ref) {
        if (ref == null) return null;
        return map(
            "name", ref.getName(),
            "qualified_name", ref.getQualifiedName(),
            "type", ref.getResourceType() == null ? null : ref.getResourceType().getName(),
            "namespace", ref.getNamespace() == null ? null : ref.getNamespace().toString(),
            "framework", ref.isFramework(),
            "value", ref.toString()
        );
    }

    private static Map<String, Object> rectangleInfo(Rectangle rectangle) {
        if (rectangle == null) return null;
        return map(
            "x", rectangle.x,
            "y", rectangle.y,
            "width", rectangle.width,
            "height", rectangle.height,
            "left", rectangle.x,
            "top", rectangle.y,
            "right", rectangle.x + rectangle.width,
            "bottom", rectangle.y + rectangle.height
        );
    }

    private static Map<String, Object> layoutClientInfo(InspectorClient client) {
        if (client == null) {
            return map("connected", false);
        }
        Object process = null;
        try {
            var descriptor = client.getProcess();
            process = descriptor == null ? null : map(
                "name", descriptor.getName(),
                "package", descriptor.getPackageName(),
                "pid", descriptor.getPid(),
                "running", descriptor.isRunning(),
                "stream_id", descriptor.getStreamId(),
                "device", descriptor.getDevice() == null ? null : map(
                    "serial", descriptor.getDevice().getSerial(),
                    "manufacturer", descriptor.getDevice().getManufacturer(),
                    "model", descriptor.getDevice().getModel(),
                    "emulator", descriptor.getDevice().isEmulator(),
                    "api", descriptor.getDevice().getApiLevel() == null ? null : descriptor.getDevice().getApiLevel().toString(),
                    "version", descriptor.getDevice().getVersion(),
                    "codename", descriptor.getDevice().getCodename()
                )
            );
        } catch (Throwable ignored) {
        }
        return map(
            "connected", client.isConnected(),
            "state", client.getState() == null ? null : client.getState().name(),
            "live", client.getInLiveMode(),
            "client_type", client.getClientType() == null ? null : client.getClientType().name(),
            "capabilities", client.getCapabilities().stream().map(Enum::name).toList(),
            "process", process
        );
    }

    private static Map<String, Object> layoutFeatures(boolean compose, boolean semantics, boolean sourceMap) {
        return map(
            "compose", map("available", compose, "source", "android_studio_layout_inspector"),
            "semantics", map("available", semantics, "source", "android_studio_layout_inspector"),
            "source_map", map("available", sourceMap, "source", "android_studio_layout_inspector")
        );
    }

    private static boolean windowHas(Map<?, ?> window, String feature) {
        Object nodesValue = window.get("nodes");
        if (!(nodesValue instanceof List<?> nodes)) return false;
        for (Object nodeValue : nodes) {
            if (!(nodeValue instanceof Map<?, ?> node)) continue;
            if ("compose".equals(feature) && "compose".equals(node.get("kind"))) return true;
            if ("semantics".equals(feature) && node.get("semantics") instanceof Map<?, ?> semantics) {
                if (Boolean.TRUE.equals(semantics.get("merged")) || Boolean.TRUE.equals(semantics.get("unmerged"))) return true;
            }
            if ("source".equals(feature) && node.get("source") instanceof Map<?, ?> source) {
                if (Boolean.TRUE.equals(source.get("available"))) return true;
            }
        }
        return false;
    }

    private static boolean layoutNodeMatches(Project project, ViewNode node, Map<String, String> query, SourceResolver sourceResolver) {
        Long drawId = optionalLong(query.get("draw_id"));
        if (drawId != null && node.getDrawId() != drawId) return false;
        if (!containsIgnoreCase(node.getTextValue(), query.get("text"))) return false;
        if (!containsIgnoreCase(resourceSearchText(node.getViewId()), query.get("rid"))) return false;
        if (!containsIgnoreCase(node.getQualifiedName(), query.get("class"))) return false;

        String file = query.get("file");
        if (file != null && !file.isBlank()) {
            Object sourceFile = layoutSourceInfo(project, node, sourceResolver).get("file");
            if (!(sourceFile instanceof String sourcePath) || !sourcePath.endsWith(file)) return false;
        }
        return true;
    }

    private static String resourceSearchText(ResourceReference ref) {
        if (ref == null) return null;
        return String.join(" ", ref.getName(), ref.getQualifiedName(), ref.toString());
    }

    private static boolean containsIgnoreCase(String actual, String expected) {
        if (expected == null || expected.isBlank()) return true;
        return actual != null && actual.toLowerCase().contains(expected.toLowerCase());
    }

    private static Long optionalLong(String value) {
        if (value == null || value.isBlank()) return null;
        try {
            return Long.parseLong(value);
        } catch (NumberFormatException e) {
            throw new IllegalArgumentException("invalid integer: " + value);
        }
    }

    private static ViewNode parentNode(ViewNode node) {
        try {
            var method = ViewNode.class.getDeclaredMethod("access$getParent$p", ViewNode.class);
            method.setAccessible(true);
            return (ViewNode) method.invoke(null, node);
        } catch (Throwable ignored) {
            return null;
        }
    }

    private static SourceResolver sourceResolver(Project project) {
        return SOURCE_RESOLVERS.computeIfAbsent(projectKey(project), ignored -> SourceResolver.build(project.getBasePath()));
    }

    private static String projectKey(Project project) {
        return project.getBasePath() == null ? project.getName() : project.getBasePath();
    }

    private static Map<String, Object> projectInfo(Project project) {
        return map(
            "name", project.getName(),
            "base_path", project.getBasePath(),
            "disposed", project.isDisposed()
        );
    }

    private static Integer lineForOffset(VirtualFile file, int offset) {
        if (file == null || offset < 0) return null;
        Document document = FileDocumentManager.getInstance().getDocument(file);
        if (document == null || offset > document.getTextLength()) return null;
        return document.getLineNumber(offset) + 1;
    }

    private static <T> T readActionOrNull(ThrowingSupplier<T> supplier) {
        try {
            return ApplicationManager.getApplication().runReadAction((Computable<T>) () -> {
                try {
                    return supplier.get();
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            });
        } catch (Throwable ignored) {
            return null;
        }
    }

    private record LayoutState(
        Project project,
        LayoutInspector inspector,
        InspectorModel model,
        InspectorClient client,
        boolean available,
        String reason
    ) {
    }
}
