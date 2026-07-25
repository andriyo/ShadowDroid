plugins {
    id("com.android.application") version "9.3.1" apply false
    // Keep the Compose compiler and JVM fixture on the same stable Kotlin line.
    id("org.jetbrains.kotlin.plugin.compose") version "2.4.10" apply false
    id("org.jetbrains.kotlin.jvm") version "2.4.10" apply false
}
