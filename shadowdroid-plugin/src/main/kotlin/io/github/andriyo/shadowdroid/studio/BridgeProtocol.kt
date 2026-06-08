package io.github.andriyo.shadowdroid.studio

import com.sun.net.httpserver.HttpExchange
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URLDecoder
import java.nio.charset.StandardCharsets
import kotlin.math.max
import kotlin.math.min

internal object BridgeProtocol {
    const val DEFAULT_DEBUGGER_TIMEOUT_MS: Int = 2_500

    @JvmStatic
    fun ok(vararg fields: Any?): Response = Response(HttpURLConnection.HTTP_OK, obj(*fields))

    @JvmStatic
    fun bad(message: String?): Response =
        Response(HttpURLConnection.HTTP_BAD_REQUEST, obj("ok", false, "error", message))

    @JvmStatic
    fun send(exchange: HttpExchange, status: Int, body: String) {
        try {
            val bytes = body.toByteArray(StandardCharsets.UTF_8)
            exchange.responseHeaders["content-type"] = "application/json; charset=utf-8"
            exchange.sendResponseHeaders(status, bytes.size.toLong())
            exchange.responseBody.use { it.write(bytes) }
        } catch (_: IOException) {
        }
    }

    @JvmStatic
    fun parseQuery(raw: String?): Map<String, String> {
        if (raw.isNullOrBlank()) return emptyMap()
        val params = linkedMapOf<String, String>()
        for (part in raw.split("&")) {
            val index = part.indexOf('=')
            if (index < 0) continue
            params[decode(part.substring(0, index))] = decode(part.substring(index + 1))
        }
        return params
    }

    @JvmStatic
    fun intParam(query: Map<String, String>, key: String, defaultValue: Int, min: Int, max: Int): Int {
        val value = query[key] ?: return defaultValue
        return value.toIntOrNull()?.let { parsed -> max(min, min(max, parsed)) } ?: defaultValue
    }

    @JvmStatic
    fun debuggerTimeoutMs(query: Map<String, String>): Int =
        intParam(query, BridgeQuery.TIMEOUT_MS, DEFAULT_DEBUGGER_TIMEOUT_MS, 50, 30_000)

    @JvmStatic
    fun booleanParam(query: Map<String, String>, key: String, defaultValue: Boolean): Boolean =
        query[key]?.toBoolean() ?: defaultValue

    @JvmStatic
    fun nowMs(): Long = System.currentTimeMillis()

    @JvmStatic
    fun map(vararg fields: Any?): MutableMap<String, Any?> {
        val payload = linkedMapOf<String, Any?>()
        var index = 0
        while (index + 1 < fields.size) {
            payload[fields[index].toString()] = fields[index + 1]
            index += 2
        }
        return payload
    }

    @JvmStatic
    fun obj(vararg fields: Any?): String = json(map(*fields))

    @JvmStatic
    fun json(value: Any?): String = when (value) {
        null -> "null"
        is String -> "\"${escape(value)}\""
        is Number, is Boolean -> value.toString()
        is Map<*, *> -> value.entries.joinToString(prefix = "{", postfix = "}") { (key, item) ->
            "${json(key.toString())}:${json(item)}"
        }
        is Iterable<*> -> value.joinToString(prefix = "[", postfix = "]") { json(it) }
        is Array<*> -> value.joinToString(prefix = "[", postfix = "]") { json(it) }
        else -> json(value.toString())
    }

    private fun decode(value: String): String = URLDecoder.decode(value, StandardCharsets.UTF_8)

    private fun escape(value: String): String = buildString(value.length + 16) {
        for (char in value) {
            when (char) {
                '\\' -> append("\\\\")
                '"' -> append("\\\"")
                '\n' -> append("\\n")
                '\r' -> append("\\r")
                '\t' -> append("\\t")
                else -> {
                    if (char < ' ') append("\\u%04x".format(char.code)) else append(char)
                }
            }
        }
    }
}
