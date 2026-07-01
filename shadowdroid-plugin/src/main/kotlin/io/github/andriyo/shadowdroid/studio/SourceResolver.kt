package io.github.andriyo.shadowdroid.studio

import com.intellij.openapi.diagnostic.Logger
import java.io.IOException
import java.nio.file.FileVisitResult
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.SimpleFileVisitor
import java.nio.file.attribute.BasicFileAttributes

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
        private val SKIPPED_DIRECTORIES = setOf("build", ".gradle", ".idea", ".git", "node_modules")

        @JvmStatic
        fun build(basePath: String?): SourceResolver {
            if (basePath.isNullOrBlank()) return SourceResolver(emptyMap())
            val byName = linkedMapOf<String, MutableList<String>>()
            val root = Path.of(basePath)
            try {
                Files.walkFileTree(root, object : SimpleFileVisitor<Path>() {
                    override fun preVisitDirectory(dir: Path, attrs: BasicFileAttributes): FileVisitResult =
                        if (dir.fileName?.toString() in SKIPPED_DIRECTORIES) FileVisitResult.SKIP_SUBTREE
                        else FileVisitResult.CONTINUE

                    override fun visitFile(file: Path, attrs: BasicFileAttributes): FileVisitResult {
                        if (attrs.isRegularFile) {
                            val name = file.fileName?.toString().orEmpty()
                            if (name.isNotBlank() && isSourceFile(name)) {
                                byName.getOrPut(name) { mutableListOf() }.add(file.toAbsolutePath().toString())
                            }
                        }
                        return FileVisitResult.CONTINUE
                    }

                    override fun visitFileFailed(file: Path, exc: IOException): FileVisitResult =
                        FileVisitResult.CONTINUE
                })
            } catch (t: Throwable) {
                LOG.debug("Unable to build source resolver for $basePath", t)
            }
            byName.values.forEach { it.sort() }
            return SourceResolver(byName)
        }

        private fun isSourceFile(name: String): Boolean =
            name.endsWith(".kt") || name.endsWith(".java") || name.endsWith(".xml")
    }
}
