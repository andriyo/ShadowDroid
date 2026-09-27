package io.github.andriyo.shadowdroid.studio

import com.intellij.openapi.application.ReadAction
import com.intellij.openapi.project.DumbService
import com.intellij.openapi.project.Project
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.psi.search.FilenameIndex
import com.intellij.psi.search.GlobalSearchScope
import java.io.File

/**
 * Turns the file an agent names into a project file: an absolute path as is,
 * else a path relative to the project root, else a unique project file whose
 * path ends with it (`MainActivity.kt`, `sample/MainActivity.kt`).
 */
internal object SourceFiles {
    private const val MAX_LISTED = 10

    fun resolve(project: Project, requested: String): VirtualFile {
        val fileSystem = LocalFileSystem.getInstance()
        val asFile = File(requested)
        if (asFile.isAbsolute) {
            return fileSystem.refreshAndFindFileByIoFile(asFile)
                ?: throw IllegalArgumentException("file not found in IDE VFS: $requested")
        }
        project.basePath
            ?.let { base -> fileSystem.refreshAndFindFileByIoFile(File(base, requested)) }
            ?.takeIf { !it.isDirectory }
            ?.let { return it }
        if (DumbService.isDumb(project)) {
            throw IllegalArgumentException(
                "Android Studio is indexing, so $requested can't be looked up by name yet; " +
                    "pass an absolute path or retry when indexing finishes",
            )
        }
        val byName = ReadAction.compute<Collection<VirtualFile>, RuntimeException> {
            FilenameIndex.getVirtualFilesByName(asFile.name, GlobalSearchScope.projectScope(project))
        }
        val matches = matchingPaths(requested, byName.map { it.path })
        return when (matches.size) {
            1 -> byName.first { it.path == matches.single() }
            0 -> throw IllegalArgumentException(
                "no file matching $requested in project ${project.name}; pass an absolute path",
            )
            else -> throw IllegalArgumentException(
                "$requested matches ${matches.size} files in project ${project.name}; " +
                    "pass more of the path: ${matches.take(MAX_LISTED).joinToString()}",
            )
        }
    }

    /** The paths that end with [requested] at a path-segment boundary, sorted. */
    fun matchingPaths(requested: String, paths: Collection<String>): List<String> {
        val suffix = "/" + requested.replace('\\', '/').removePrefix("./").trimStart('/')
        return paths.filter { it.replace('\\', '/').endsWith(suffix) }.sorted()
    }
}
