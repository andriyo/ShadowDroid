package io.github.andriyo.shadowdroid.studio;

import com.sun.net.httpserver.HttpExchange;

import java.io.IOException;
import java.net.HttpURLConnection;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

final class BridgeProtocol {
    static final int DEFAULT_DEBUGGER_TIMEOUT_MS = 2_500;

    private BridgeProtocol() {
    }

    static Response ok(Object... fields) {
        return new Response(HttpURLConnection.HTTP_OK, obj(fields));
    }

    static Response bad(String message) {
        return new Response(HttpURLConnection.HTTP_BAD_REQUEST, obj("ok", false, "error", message));
    }

    static void send(HttpExchange exchange, int status, String body) {
        try {
            byte[] bytes = body.getBytes(StandardCharsets.UTF_8);
            exchange.getResponseHeaders().set("content-type", "application/json; charset=utf-8");
            exchange.sendResponseHeaders(status, bytes.length);
            try (var out = exchange.getResponseBody()) {
                out.write(bytes);
            }
        } catch (IOException ignored) {
        }
    }

    static Map<String, String> parseQuery(String raw) {
        if (raw == null || raw.isBlank()) return Collections.emptyMap();
        Map<String, String> params = new LinkedHashMap<>();
        for (String part : raw.split("&")) {
            int index = part.indexOf('=');
            if (index < 0) continue;
            params.put(decode(part.substring(0, index)), decode(part.substring(index + 1)));
        }
        return params;
    }

    static int intParam(Map<String, String> query, String key, int defaultValue, int min, int max) {
        String value = query.get(key);
        if (value == null) return defaultValue;
        try {
            int parsed = Integer.parseInt(value);
            return Math.max(min, Math.min(max, parsed));
        } catch (NumberFormatException ignored) {
            return defaultValue;
        }
    }

    static int debuggerTimeoutMs(Map<String, String> query) {
        return intParam(query, "timeout_ms", DEFAULT_DEBUGGER_TIMEOUT_MS, 50, 30_000);
    }

    static boolean booleanParam(Map<String, String> query, String key, boolean defaultValue) {
        String value = query.get(key);
        return value == null ? defaultValue : Boolean.parseBoolean(value);
    }

    static long nowMs() {
        return System.currentTimeMillis();
    }

    static Map<String, Object> map(Object... fields) {
        Map<String, Object> map = new LinkedHashMap<>();
        for (int i = 0; i + 1 < fields.length; i += 2) {
            map.put(fields[i].toString(), fields[i + 1]);
        }
        return map;
    }

    static String obj(Object... fields) {
        return json(map(fields));
    }

    @SuppressWarnings("unchecked")
    static String json(Object value) {
        if (value == null) return "null";
        if (value instanceof String string) return "\"" + escape(string) + "\"";
        if (value instanceof Number || value instanceof Boolean) return value.toString();
        if (value instanceof Map<?, ?> map) {
            StringBuilder builder = new StringBuilder("{");
            boolean first = true;
            for (Map.Entry<?, ?> entry : map.entrySet()) {
                if (!first) builder.append(',');
                first = false;
                builder.append(json(String.valueOf(entry.getKey()))).append(':').append(json(entry.getValue()));
            }
            return builder.append('}').toString();
        }
        if (value instanceof Iterable<?> iterable) {
            StringBuilder builder = new StringBuilder("[");
            boolean first = true;
            for (Object item : iterable) {
                if (!first) builder.append(',');
                first = false;
                builder.append(json(item));
            }
            return builder.append(']').toString();
        }
        if (value.getClass().isArray()) {
            List<Object> list = new ArrayList<>();
            Object[] array = (Object[]) value;
            Collections.addAll(list, array);
            return json(list);
        }
        return json(value.toString());
    }

    private static String decode(String value) {
        return URLDecoder.decode(value, StandardCharsets.UTF_8);
    }

    private static String escape(String value) {
        StringBuilder builder = new StringBuilder(value.length() + 16);
        for (int i = 0; i < value.length(); i++) {
            char c = value.charAt(i);
            switch (c) {
                case '\\' -> builder.append("\\\\");
                case '"' -> builder.append("\\\"");
                case '\n' -> builder.append("\\n");
                case '\r' -> builder.append("\\r");
                case '\t' -> builder.append("\\t");
                default -> {
                    if (c < 0x20) builder.append(String.format("\\u%04x", (int) c));
                    else builder.append(c);
                }
            }
        }
        return builder.toString();
    }
}
