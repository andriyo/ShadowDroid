package io.github.andriyo.shadowdroid.routes

import android.app.Instrumentation
import io.github.andriyo.shadowdroid.BadRequest
import io.github.andriyo.shadowdroid.NotFound
import io.ktor.http.ContentType
import io.ktor.server.request.receiveChannel
import io.ktor.server.response.respond
import io.ktor.server.response.respondBytes
import io.ktor.server.routing.Route
import io.ktor.server.routing.get
import io.ktor.server.routing.put
import io.ktor.utils.io.core.readBytes
import io.ktor.utils.io.readRemaining
import kotlinx.serialization.Serializable
import java.io.File

object FileRoutes {
    /** PUT/GET /v1/files{path}. Limited to app storage and shared /sdcard paths. */
    fun register(
        route: Route,
        instr: Instrumentation,
    ) {
        route.put("/files/{path...}") {
            val file = resolveRequestedFile(call.parameters.getAll("path"), instr)
            file.parentFile?.mkdirs()
            @Suppress("DEPRECATION")
            val bytes = call.receiveChannel().readRemaining().readBytes()
            file.writeBytes(bytes)
            call.respond(FileWriteResp(path = file.path, bytes = bytes.size.toLong(), mode = 420))
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
            call.respondBytes(file.readBytes(), ContentType.Application.OctetStream)
        }
    }
}

private fun resolveRequestedFile(
    parts: List<String>?,
    instr: Instrumentation,
): File {
    val joined = parts.orEmpty().joinToString("/")
    if (joined.isBlank()) throw BadRequest("missing_path", "file path is required")
    val path = if (joined.startsWith("/")) joined else "/$joined"
    if (path.split('/').any { it == ".." }) {
        throw BadRequest("bad_path", "path traversal is not allowed")
    }

    return if (path.startsWith("/sdcard/") || path.startsWith("/storage/emulated/0/")) {
        File(path)
    } else {
        val root = instr.targetContext.getExternalFilesDir(null) ?: instr.targetContext.filesDir
        val relative = path.trimStart('/')
        File(root, relative).canonicalFile.also { file ->
            val canonicalRoot = root.canonicalFile
            if (!file.path.startsWith(canonicalRoot.path)) {
                throw BadRequest("bad_path", "path escapes server storage")
            }
        }
    }
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
