plugins {
    id("com.android.application") version "9.2.1" apply false
    // Match AGP 9.2.1's built-in Kotlin compiler.
    id("org.jetbrains.kotlin.plugin.compose") version "2.3.10" apply false
    id("org.jetbrains.kotlin.jvm") version "2.3.10" apply false
}
