package io.github.andriyo.shadowdroid.agent

import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.PrintStream
import java.lang.reflect.Method
import kotlin.coroutines.AbstractCoroutineContextElement
import kotlin.coroutines.Continuation
import kotlin.coroutines.CoroutineContext

/**
 * In-process coroutine dumps, backed by kotlinx-coroutines' `DebugProbes`.
 *
 * This is the low-overhead, no-debugger counterpart to `shadowdroid debug
 * coroutines` (which needs Android Studio + a JDWP session suspended at a
 * breakpoint). Here we ask the coroutines runtime — already inside the host
 * process — for every *live* coroutine: its state, context, and last-observed /
 * creation stack traces. That is what surfaces a leaked coroutine or a
 * collector clogged on a `SharedFlow` under load.
 *
 * **Framework-only, by reflection.** The AAR ships no coroutines dependency (a
 * local `files()` AAR can't carry transitive deps), so we reach *the host app's
 * own* `kotlinx-coroutines-core` reflectively. Kotlin mangles `internal` member
 * names per-module (`install` → `install$kotlinx_coroutines_core`), so lookups
 * go through [method], which matches on the base name before `$`.
 *
 * **Install vs active.** `DebugProbesImpl.install()` only flips the tracking
 * flag. The probe *call sites* live in `kotlin.coroutines.jvm.internal.
 * DebugProbesKt`, and kotlin-stdlib's copy of that class — the one that lands
 * in the dex — is a no-op. Tracking only works once that class is swapped for
 * the delegating variant (kotlinx-coroutines-core ships it as the
 * `DebugProbesKt.bin` resource). Android has no java-agents and ART's JDWP
 * rejects class redefinition, so ShadowDroid swaps it at *build time*:
 * `shadowdroid aar install --coroutine-probes` registers an ASM visitor that
 * rewrites the stub in debug builds. [probesActive] detects the difference at
 * runtime, so the dump can say "installed but inert" instead of silently
 * returning zero coroutines.
 */
internal object Coroutines {
    private const val IMPL_CLASS = "kotlinx.coroutines.debug.internal.DebugProbesImpl"

    /** The probe dispatch class the debugger swaps; also preloaded in [installBestEffort]. */
    private const val PROBES_CLASS = "kotlin.coroutines.jvm.internal.DebugProbesKt"

    /** `DebugProbesImpl.INSTANCE`, resolved once, or null if unavailable. */
    @Volatile
    private var impl: Any? = null

    /** Why the runtime is unavailable (class missing, install failed, …). */
    @Volatile
    private var unavailable: String? = null

    /** True once `install()` has succeeded on [impl]. */
    @Volatile
    private var installedOk = false

    /** Resolve `DebugProbesImpl.INSTANCE`, caching success and failure alike. */
    private fun impl(): Any? {
        impl?.let { return it }
        if (unavailable != null) return null
        return try {
            val cls = Class.forName(IMPL_CLASS)
            cls.getField("INSTANCE").get(null).also { impl = it }
        } catch (t: Throwable) {
            unavailable = describe(t)
            null
        }
    }

    /**
     * Install `DebugProbes` best-effort so coroutines are tracked from process
     * start. Idempotent and safe to call before any coroutine exists — probes
     * only observe coroutines created *after* they start feeding, so the
     * earlier the better. Called from [ShadowDroidAgent.start].
     */
    @Synchronized
    fun installBestEffort(): Boolean {
        if (installedOk) return true
        val i = impl() ?: return false
        return try {
            // Creation stack traces tie a leaked coroutine back to its launch
            // site — the most useful field for leak hunting. Pricier than the
            // default probes, but debug-only builds can afford it.
            runCatching {
                method(i.javaClass, "setEnableCreationStackTraces", Boolean::class.javaPrimitiveType!!)
                    ?.invoke(i, true)
            }
            val install = method(i.javaClass, "install")
                ?: throw NoSuchMethodException("DebugProbesImpl.install")
            install.invoke(i) // void return: invoke() yields null even on success
            // Force-load the probe dispatch class now: a JDWP `RedefineClasses`
            // (--activate, or the Studio debugger) can only target loaded classes.
            runCatching { Class.forName(PROBES_CLASS) }
            installedOk = true
            Log.i(
                ShadowDroidAgent.TAG,
                "coroutine DebugProbes installed (active=${probesActive()})",
            )
            true
        } catch (t: Throwable) {
            unavailable = "install failed: ${describe(t)}"
            Log.w(ShadowDroidAgent.TAG, "coroutine DebugProbes install failed", t)
            false
        }
    }

    /** True if the coroutines runtime was found and probes are installed. */
    fun available(): Boolean = installedOk

    /**
     * Whether the probe pipeline is actually feeding `DebugProbesImpl` — i.e.
     * the stdlib no-op `DebugProbesKt` has been replaced by the delegating
     * variant. Detected by pushing a canary continuation through
     * `probeCoroutineCreated`: the no-op returns it unchanged, the real probe
     * wraps it in a `CoroutineOwner`. The wrapper is completed immediately so
     * the canary never lingers in dumps.
     */
    fun probesActive(): Boolean {
        if (!installedOk) return false
        return try {
            val probes = Class.forName(PROBES_CLASS)
            val probe = method(probes, "probeCoroutineCreated", Continuation::class.java)
                ?: return false
            val canary = CanaryContinuation()
            val wrapped = probe.invoke(null, canary)
            val active = wrapped !== canary && wrapped is Continuation<*>
            if (active) {
                // Complete the owner so DebugProbesImpl unregisters it.
                @Suppress("UNCHECKED_CAST")
                (wrapped as Continuation<Unit>).resumeWith(Result.success(Unit))
            }
            active
        } catch (t: Throwable) {
            false
        }
    }

    /** Non-empty context so `ignoreCoroutinesWithEmptyContext` doesn't skip it. */
    private class CanaryContinuation : Continuation<Unit> {
        override val context: CoroutineContext = CanaryElement
        override fun resumeWith(result: Result<Unit>) = Unit

        private object CanaryElement :
            AbstractCoroutineContextElement(Key), CoroutineContext.Element {
            override fun toString(): String = "ShadowDroidProbeCanary"
        }

        private object Key : CoroutineContext.Key<CanaryElement>
    }

    /**
     * Handle the `coroutines` control verb.
     *
     * Flags (space-separated, order-free):
     * - `--dump`         include the full `DebugProbes` text dump (job tree +
     *                    stacks) alongside the structured summary
     * - `--frames <n>`   stack frames per coroutine in the structured list (0 = none)
     * - `--limit <n>`    cap the structured coroutine list (counts are unaffected)
     */
    fun dump(rest: String): String {
        val i = impl() ?: return error(
            "kotlinx-coroutines DebugProbes unavailable: ${unavailable ?: "not found"}",
            hint = "the host app must depend on kotlinx-coroutines-core (1.6+) and be " +
                "a non-minified (debug) build for probes to install",
        )
        if (!installBestEffort()) {
            return error("DebugProbes present but not installed: ${unavailable ?: "unknown"}")
        }

        val wantText = rest.contains("--dump")
        val frames = optInt(rest, "--frames", 6).coerceIn(0, 64)
        val limit = optInt(rest, "--limit", 200).coerceAtLeast(0)
        val active = probesActive()

        val infos: List<Any?> = try {
            (method(i.javaClass, "dumpCoroutinesInfo")?.invoke(i) as? List<*>).orEmpty()
        } catch (t: Throwable) {
            emptyList()
        }

        val byState = LinkedHashMap<String, Int>()
        val list = JSONArray()
        infos.forEachIndexed { idx, info ->
            if (info == null) return@forEachIndexed
            val state = str(info, "getState") ?: "UNKNOWN"
            byState[state] = (byState[state] ?: 0) + 1
            if (idx >= limit) return@forEachIndexed
            list.put(
                JSONObject().apply {
                    put("state", state)
                    longOf(info, "getSequenceNumber")?.let { put("seq", it) }
                    threadName(info)?.let { put("thread", it) }
                    str(info, "getContext")?.let { put("context", it) }
                    if (frames > 0) {
                        stackArray(info, "lastObservedStackTrace", frames)
                            .takeIf { it.length() > 0 }?.let { put("stack", it) }
                        stackArray(info, "getCreationStackTrace", frames)
                            .takeIf { it.length() > 0 }?.let { put("creationStack", it) }
                    }
                },
            )
        }

        return JSONObject().apply {
            put("ok", true)
            put("installed", true)
            put("active", active)
            if (!active) {
                put(
                    "hint",
                    "probes are installed but inert: the dex still has kotlin-stdlib's " +
                        "no-op DebugProbesKt. Wire build-time activation with " +
                        "`shadowdroid aar install --coroutine-probes`, rebuild the debug " +
                        "app, and relaunch.",
                )
            }
            put("total", infos.size)
            put("byState", JSONObject(byState as Map<*, *>))
            put("coroutines", list)
            if (wantText) put("dump", textDump(i))
        }.toString()
    }

    /** Parse `--flag <n>` out of the raw argument string, or [default]. */
    private fun optInt(rest: String, flag: String, default: Int): Int {
        val tokens = rest.split(Regex("\\s+"))
        val at = tokens.indexOf(flag)
        if (at < 0 || at + 1 >= tokens.size) return default
        return tokens[at + 1].toIntOrNull() ?: default
    }

    // ── reflection helpers (all guarded; failures degrade to omitted fields) ──

    /**
     * Find a (JVM-public) method by Kotlin source name, tolerating the
     * `internal`-visibility mangling (`name$module_name`). Exact match wins;
     * otherwise the first method whose name up to `$` matches, with the same
     * parameter types.
     */
    private fun method(cls: Class<*>, name: String, vararg params: Class<*>): Method? {
        runCatching { return cls.getMethod(name, *params) }
        return cls.methods.firstOrNull { m ->
            m.name.substringBefore('$') == name && m.parameterTypes.contentEquals(params)
        }
    }

    /** Canonical `DebugProbesImpl.dumpCoroutines(PrintStream)` text dump. */
    private fun textDump(i: Any): String =
        try {
            val bos = ByteArrayOutputStream()
            PrintStream(bos, true, "UTF-8").use { ps ->
                method(i.javaClass, "dumpCoroutines", PrintStream::class.java)!!.invoke(i, ps)
            }
            bos.toString("UTF-8")
        } catch (t: Throwable) {
            "<dump unavailable: ${describe(t)}>"
        }

    private fun str(target: Any, getter: String): String? =
        runCatching {
            method(target.javaClass, getter)?.invoke(target)?.toString()
        }.getOrNull()

    private fun longOf(target: Any, getter: String): Long? =
        runCatching {
            (method(target.javaClass, getter)?.invoke(target) as? Number)?.toLong()
        }.getOrNull()

    private fun threadName(info: Any): String? =
        runCatching {
            (method(info.javaClass, "getLastObservedThread")?.invoke(info) as? Thread)?.name
        }.getOrNull()

    private fun stackArray(info: Any, name: String, frames: Int): JSONArray {
        val arr = JSONArray()
        val elements = runCatching {
            method(info.javaClass, name)?.invoke(info) as? List<*>
        }.getOrNull().orEmpty()
        for (element in elements.take(frames)) {
            (element as? StackTraceElement)?.let { arr.put(it.toString()) }
        }
        return arr
    }

    private fun describe(t: Throwable): String {
        val cause = t.cause ?: t
        return cause.javaClass.simpleName + (cause.message?.let { ": $it" } ?: "")
    }

    private fun error(message: String, hint: String? = null): String =
        JSONObject().apply {
            put("ok", false)
            put("error", message)
            if (hint != null) put("hint", hint)
        }.toString()
}
