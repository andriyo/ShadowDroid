package io.github.andriyo.shadowdroid

/**
 * Build-time constants. Kept hand-rolled instead of via BuildConfig so we
 * don't have to enable the AGP feature for a 2-line file.
 *
 * SERVER_VERSION is now only a *fallback*: the server reports its version from
 * the installed APK's versionName at runtime (see StateRoutes.resolveServerVersion),
 * which build.gradle.kts sets from `-Pversion` at release time. Keep this in sync
 * with build.gradle.kts's fallback and cli/Cargo.toml anyway, so the fallback path
 * (PackageManager unavailable) still reports a sensible value. API_VERSION and
 * UI_AUTOMATOR_VERSION don't track the release version and are authoritative here.
 */
object BuildInfo {
    const val SERVER_VERSION: String = "0.7.2"
    const val API_VERSION: String = "1"
    const val UI_AUTOMATOR_VERSION: String = "2.3.0"
}
