package io.github.andriyo.shadowdroid

/**
 * Build-time constants. Kept hand-rolled instead of via BuildConfig so we
 * don't have to enable the AGP feature for a 2-line file. Synced manually
 * with the matching values in cli/Cargo.toml + server/app/build.gradle.kts.
 */
object BuildInfo {
    const val SERVER_VERSION: String = "0.1.9"
    const val API_VERSION: String = "1"
    const val UI_AUTOMATOR_VERSION: String = "2.3.0"
}
