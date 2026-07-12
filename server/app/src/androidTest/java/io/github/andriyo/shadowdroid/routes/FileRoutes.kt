package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.ktor.http.ContentType
import io.ktor.server.http.content.LocalFileContent
import io.ktor.server.request.receiveChannel
import io.ktor.server.response.respond
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import io.ktor.server.routing.put
import io.ktor.utils.io.ByteReadChannel
import io.ktor.utils.io.jvm.javaio.copyTo
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.security.MessageDigest

object FileRoutes {
    /** PUT/GET /v1/files{path}. Limited to app storage and shared /sdcard paths. */
    fun register(
        route: Route,
        instr: Instrumentation,
    ) {
        route.put("/files/{path...}") {
            val file = resolveRequestedFile(call.parameters.getAll("path"), instr)
            val requestedMode = parseMode(call.request.queryParameters["mode"])
            val bytes = writeFileAtomically(call.receiveChannel(), file, requestedMode)
            call.respond(FileWriteResp(path = file.path, bytes = bytes, mode = fileMode(file)))
        }

        route.get("/files/{path...}") {
            val file = resolveRequestedFile(call.parameters.getAll("path"), instr)
            if (call.request.queryParameters["list"] == "true") {
                if (!file.exists()) throw NotFound("file_not_found", "no such directory: ${file.path}")
                if (!file.isDirectory) throw BadRequest("not_directory", "not a directory: ${file.path}")
                call.respond(
                    FileListResp(
                        entries =
                            file.listFiles().orEmpty().map {
                                FileEntry(name = it.name, size = it.length(), is_dir = it.isDirectory)
                            },
                    ),
                )
                return@get
            }
            if (!file.exists()) throw NotFound("file_not_found", "no such file: ${file.path}")
            if (file.isDirectory) throw BadRequest("is_directory", "path is a directory: ${file.path}")
            call.respond(LocalFileContent(file, ContentType.Application.OctetStream))
        }
    }
}

internal suspend fun writeFileAtomically(
    source: ByteReadChannel,
    destination: File,
    requestedMode: Int?,
): Long {
    val parent = destination.parentFile ?: throw IOException("file has no parent: ${destination.path}")
    if (!parent.isDirectory && !parent.mkdirs()) {
        throw IOException("cannot create directory: ${parent.path}")
    }
    rejectNonRegularDestination(destination)
    val preservedMode = if (requestedMode == null && destination.exists()) fileMode(destination) else null
    val temp = File.createTempFile(transferTempPrefix(destination.name), ".tmp", parent)
    try {
        val bytes =
            withContext(Dispatchers.IO) {
                FileOutputStream(temp).use { output ->
                    val copied = source.copyTo(output)
                    output.flush()
                    output.fd.sync()
                    copied
                }
            }
        (requestedMode ?: preservedMode)?.let { applyMode(temp, it) }
        Os.rename(temp.path, destination.path)
        return bytes
    } finally {
        temp.delete()
    }
}

private fun rejectNonRegularDestination(destination: File) {
    val stat =
        try {
            Os.lstat(destination.path)
        } catch (error: ErrnoException) {
            if (error.errno == OsConstants.ENOENT) return
            throw IOException("cannot inspect destination: ${destination.path}", error)
        }
    if ((stat.st_mode and OsConstants.S_IFMT) != OsConstants.S_IFREG) {
        throw IOException("refusing to replace non-regular destination: ${destination.path}")
    }
}

private fun transferTempPrefix(name: String): String {
    val digest = MessageDigest.getInstance("SHA-256").digest(name.toByteArray(Charsets.UTF_8))
    val shortHash = digest.take(8).joinToString("") { byte -> "%02x".format(byte.toInt() and 0xFF) }
    return ".shadowdroid-$shortHash-"
}

internal fun resolveRequestedFile(
    parts: List<String>?,
    instr: Instrumentation,
): File {
    val joined = parts.orEmpty().joinToString("/")
    if (joined.isBlank()) throw BadRequest("missing_path", "file path is required")
    val path = if (joined.startsWith("/")) joined else "/$joined"
    if (path.split('/').any { it == ".." }) {
        throw BadRequest("bad_path", "path traversal is not allowed")
    }

    return if (path.startsWith("/sdcard/")) {
        resolveUnderRoot(File("/sdcard"), path.removePrefix("/sdcard/"))
    } else if (path.startsWith("/storage/emulated/0/")) {
        resolveUnderRoot(
            File("/storage/emulated/0"),
            path.removePrefix("/storage/emulated/0/"),
        )
    } else {
        val root = instr.targetContext.getExternalFilesDir(null) ?: instr.targetContext.filesDir
        val relative = path.trimStart('/')
        resolveUnderRoot(root, relative)
    }
}

internal fun resolveUnderRoot(
    root: File,
    relative: String,
): File =
    File(root, relative).canonicalFile.also { file ->
        val canonicalRoot = root.canonicalFile
        // Path components, not string prefixes: `/root-evil` is not a
        // descendant of `/root`, even though its text begins the same way.
        if (!file.toPath().startsWith(canonicalRoot.toPath())) {
            throw BadRequest("bad_path", "path escapes server storage")
        }
    }

private fun parseMode(value: String?): Int? {
    if (value == null) return null
    val mode =
        value.toIntOrNull()
            ?: throw BadRequest("bad_mode", "mode must be an integer permission bitmask")
    if (mode !in 0..0x1FF) {
        throw BadRequest("bad_mode", "mode must be between 0 and 511")
    }
    return mode
}

private fun applyMode(
    file: File,
    mode: Int,
) {
    try {
        Os.chmod(file.path, mode)
    } catch (_: ErrnoException) {
        // Shared/external storage may reject chmod. The response reports the
        // actual mode when stat is available, so callers can tell what happened.
    }
}

private fun fileMode(
    file: File,
): Int =
    try {
        Os.stat(file.path).st_mode and 0x1FF
    } catch (_: ErrnoException) {
        0
    }

@Serializable
private data class FileWriteResp(
    val path: String,
    val bytes: Long,
    val mode: Int,
)

@Serializable
private data class FileListResp(
    val entries: List<FileEntry>,
)

@Serializable
private data class FileEntry(
    val name: String,
    val size: Long,
    val is_dir: Boolean,
)
