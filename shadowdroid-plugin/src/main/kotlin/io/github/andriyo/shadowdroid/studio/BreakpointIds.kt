package io.github.andriyo.shadowdroid.studio

import java.nio.charset.StandardCharsets
import java.security.MessageDigest

/** Compact, stable breakpoint ids derived from a breakpoint's identity string. */
internal object BreakpointIds {
    /** `bp_` plus the first 64 bits of the identity's SHA-256, in hex. */
    fun short(identity: String): String {
        val digest = MessageDigest.getInstance("SHA-256").digest(identity.toByteArray(StandardCharsets.UTF_8))
        return "bp_" + digest.take(8).joinToString("") { "%02x".format(it) }
    }
}
