// The ShadowDroid in-app debug agent, packaged as an AAR.
//
// Added to an app you own via `debugImplementation(files(".../shadowdroid-agent.aar"))`
// (the `shadowdroid aar install` CLI verb does this). It auto-installs through a
// merged ContentProvider — no app code required — and is a base for many
// debugging/development capabilities, not just network capture.
//
// Framework-only (no AndroidX / third-party deps) on purpose: a local `files()`
// AAR dependency does not resolve transitive deps, so the agent must be
// self-contained.

plugins {
    id("com.android.library")
}

android {
    namespace = "io.github.andriyo.shadowdroid.agent"
    compileSdk = 36

    defaultConfig {
        minSdk = 24 // <= any reasonable app's minSdk; matches the server APK
    }

    buildTypes {
        release {
            isMinifyEnabled = false // tiny library; consumers are debug builds
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
}

// AGP 9 built-in Kotlin DSL (matches server/app/build.gradle.kts).
kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_21)
    }
}
