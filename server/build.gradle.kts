// Root Gradle build file. Per-module config lives in app/build.gradle.kts.
//
// AGP 9.x has built-in Kotlin support — see
// https://developer.android.com/build/migrate-to-built-in-kotlin
// We deliberately do NOT apply `org.jetbrains.kotlin.android`. AGP 9 brings
// its own Kotlin compiler & DSL; declaring the standalone Kotlin Android
// plugin caused `Cannot add extension with name 'kotlin'`.
// The serialization plugin remains separate and is applied normally.

plugins {
    id("com.android.application") version "9.3.1" apply false
    // The serialization compiler plugin determines the built-in Kotlin
    // compiler/runtime level used by this build. Keep it aligned with the
    // current stable Kotlin release so Ktor and kotlinx.serialization can use
    // their current stable runtimes without missing stdlib classes on-device.
    id("org.jetbrains.kotlin.plugin.serialization") version "2.4.10" apply false
    id("org.jlleitschuh.gradle.ktlint") version "14.2.0"
}

subprojects {
    apply(plugin = "org.jlleitschuh.gradle.ktlint")
}

tasks.named("ktlintCheck") {
    dependsOn(subprojects.map { "${it.path}:ktlintCheck" })
}

tasks.named("ktlintFormat") {
    dependsOn(subprojects.map { "${it.path}:ktlintFormat" })
}
