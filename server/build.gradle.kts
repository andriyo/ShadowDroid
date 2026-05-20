// Root Gradle build file. Per-module config lives in app/build.gradle.kts.
//
// AGP 9.x has built-in Kotlin support — see
// https://developer.android.com/build/migrate-to-built-in-kotlin
// We deliberately do NOT apply `org.jetbrains.kotlin.android`. AGP 9 brings
// its own Kotlin compiler & DSL; declaring the standalone Kotlin Android
// plugin caused `Cannot add extension with name 'kotlin'`.
// The serialization plugin remains separate and is applied normally.

plugins {
    id("com.android.application") version "9.2.1" apply false
    id("org.jetbrains.kotlin.plugin.serialization") version "2.2.0" apply false
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
