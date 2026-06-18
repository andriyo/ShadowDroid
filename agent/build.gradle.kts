// Root build for the ShadowDroid in-app debug agent (AAR).
//
// AGP 9.x ships built-in Kotlin support, so we deliberately do NOT apply
// `org.jetbrains.kotlin.android` (matches server/). Per-module config lives in
// shadowdroid-agent/build.gradle.kts.

plugins {
    id("com.android.library") version "9.2.1" apply false
}
