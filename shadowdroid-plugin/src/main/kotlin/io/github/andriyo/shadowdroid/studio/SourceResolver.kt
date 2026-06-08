package io.github.andriyo.shadowdroid.studio

import com.intellij.openapi.diagnostic.Logger
import java.nio.file.Files
import java.nio.file.Path

internal data class SourceResolver(
    private val byName: Map<String, List<String>>,
) {
    fun resolve(composeFilename: String?): String? {
        if (composeFilename.isNullOrBlank()) return null
        val normalized = composeFilename.replace('\\', '/')
        val fileName = normalized.substringAfterLast('/')
        if (fileName.isBlank()) return null
        val candidates = byName[fileName].orEmpty()
        if (candidates.isEmpty()) return null
        return candidates.firstOrNull { it.replace('\\', '/').endsWith(normalized) } ?: candidates.first()
    }

    companion object {
        private val LOG = Logger.getInstance(SourceResolver::class.java)

        @JvmStatic
        fun build(basePath: String?): SourceResolver {
            if (basePath.isNullOrBlank()) return SourceResolver(emptyMap())
            val byName = linkedMapOf<String, MutableList<String>>()
            val root = Path.of(basePath)
            try {
                Files.walk(root).use { paths ->
                    paths
                        .filter(Files::isRegularFile)
                        .forEach { path ->
                            val normalized = path.toAbsolutePath().toString().replace('\\', '/')
                            if (!isSourcePath(normalized)) return@forEach
                            val name = path.fileName?.toString().orEmpty()
                            if (name.isBlank()) return@forEach
                            byName.getOrPut(name) { mutableListOf() }.add(path.toAbsolutePath().toString())
                        }
                }
            } catch (t: Throwable) {
                LOG.debug("Unable to build source resolver for $basePath", t)
            }
            byName.values.forEach { it.sort() }
            return SourceResolver(byName)
        }

        private fun isSourcePath(path: String): Boolean {
            if (path.contains("/build/") || path.contains("/.gradle/") || path.contains("/.idea/")) return false
            return path.endsWith(".kt") || path.endsWith(".java") || path.endsWith(".xml")
        }
    }
}
