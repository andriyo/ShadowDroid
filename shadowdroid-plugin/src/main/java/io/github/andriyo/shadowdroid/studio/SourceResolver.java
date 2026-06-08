package io.github.andriyo.shadowdroid.studio;

import com.intellij.openapi.diagnostic.Logger;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

record SourceResolver(Map<String, List<String>> byName) {
    private static final Logger LOG = Logger.getInstance(SourceResolver.class);

    static SourceResolver build(String basePath) {
        if (basePath == null || basePath.isBlank()) {
            return new SourceResolver(Collections.emptyMap());
        }
        Map<String, List<String>> byName = new LinkedHashMap<>();
        Path root = Path.of(basePath);
        try (var paths = Files.walk(root)) {
            paths
                .filter(Files::isRegularFile)
                .forEach(path -> {
                    String normalized = path.toAbsolutePath().toString().replace('\\', '/');
                    if (!isSourcePath(normalized)) return;
                    String name = path.getFileName() == null ? "" : path.getFileName().toString();
                    if (name.isBlank()) return;
                    byName.computeIfAbsent(name, ignored -> new ArrayList<>()).add(path.toAbsolutePath().toString());
                });
        } catch (Throwable t) {
            LOG.debug("Unable to build source resolver for " + basePath, t);
        }
        for (List<String> paths : byName.values()) {
            Collections.sort(paths);
        }
        return new SourceResolver(byName);
    }

    String resolve(String composeFilename) {
        if (composeFilename == null || composeFilename.isBlank()) return null;
        String normalized = composeFilename.replace('\\', '/');
        String fileName = normalized.substring(normalized.lastIndexOf('/') + 1);
        if (fileName.isBlank()) return null;
        List<String> candidates = byName.get(fileName);
        if (candidates == null || candidates.isEmpty()) return null;
        String fallback = null;
        for (String candidate : candidates) {
            String path = candidate.replace('\\', '/');
            if (fallback == null) fallback = candidate;
            if (path.endsWith(normalized)) return candidate;
        }
        return fallback;
    }

    private static boolean isSourcePath(String path) {
        if (path.contains("/build/") || path.contains("/.gradle/") || path.contains("/.idea/")) return false;
        return path.endsWith(".kt") || path.endsWith(".java") || path.endsWith(".xml");
    }
}
