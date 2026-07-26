pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

plugins {
    // Gradle 10 requires toolchain download repositories to be declared
    // explicitly. The chat-server fixture targets JDK 21 and should remain
    // buildable on a clean host without relying on a preinstalled JDK.
    id("org.gradle.toolchains.foojay-resolver-convention") version "1.0.0"
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "shadowdroid-test-app"
include(":app")
include(":chat-server")
