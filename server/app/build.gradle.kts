// The ShadowDroid on-device server APK.
//
// Packaged as a *test* APK (instrumentation), because UI Automator can only
// run inside an Instrumentation context. The "test" classes live in
// src/androidTest/; src/main/ is a minimal stub.

plugins {
    id("com.android.application")
    // No `org.jetbrains.kotlin.android` — AGP 9 has built-in Kotlin.
    // See https://developer.android.com/build/migrate-to-built-in-kotlin
    id("org.jetbrains.kotlin.plugin.serialization")
}

// Single source of truth for the on-device server's versionName. Injected at
// release time via `-Pversion=<tag>` (see .github/workflows/release.yml, which
// already does this for the Studio plugin); local/dev builds fall back to the
// literal below. Dev APKs are matched by bytes (SHA-256), not versionName, so the
// fallback only needs to be a plausible default — but keep it in lockstep with
// cli/Cargo.toml so a locally-built APK reports the version the CLI expects.
// The running server reports this same value as `server_version` (read at runtime
// from the installed APK in StateRoutes), so the two can never drift again — the
// drift between this and a hand-rolled constant is what shipped v0.4.0 APKs
// labeled 0.3.1 and sent `connect` into an endless version-gate loop.
val serverVersionName: String =
    (project.findProperty("version") as? String)
        ?.takeIf { it.isNotBlank() && it != "unspecified" }
        ?: "0.4.1"

android {
    namespace = "io.github.andriyo.shadowdroid"
    // compileSdk tracks the latest GA SDK available on GitHub-hosted runners.
    compileSdk = 36

    defaultConfig {
        applicationId = "io.github.andriyo.shadowdroid"
        minSdk = 24 // covers ~98% of in-use devices; UA 2.3 requires 24+
        targetSdk = 36
        versionCode = 13
        versionName = serverVersionName

        // Use the standard AndroidJUnitRunner. We start the HTTP server from a
        // normal @Test method (see ShadowDroidServerTest.kt) rather than from a
        // custom runner subclass — that's openatx's proven pattern for keeping
        // an Instrumentation alive and UiAutomation properly wired (custom
        // runner subclasses race with the framework's UiAutomation init).
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildTypes {
        release {
            isMinifyEnabled = false // small, single APK — no need
            signingConfig = signingConfigs.getByName("debug") // ship signed-with-debug-key for now
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }

    packaging {
        resources.excludes +=
            setOf(
                "META-INF/INDEX.LIST",
                "META-INF/io.netty.versions.properties",
                "META-INF/AL2.0",
                "META-INF/LGPL2.1",
                "META-INF/versions/9/OSGI-INF/MANIFEST.MF",
            )
    }
}

// AGP 9 built-in Kotlin DSL: top-level `kotlin { compilerOptions { ... } }`,
// replaces `android { kotlinOptions { ... } }`. jvmTarget defaults to
// android.compileOptions.targetCompatibility, but we set it explicitly to
// be unambiguous when reading the build script.
kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_21)
    }
}

dependencies {
    // ── AndroidX UI Automator (the whole point of this APK) ────────────
    // 2.3.0 = last GA. We tried 2.4.0-beta02 (the absolute latest) but it
    // races with AndroidJUnitRunner.onStart's UiAutomation init on Android 16:
    // `UiDevice.getInstance` from @Before triggers a connect that the runner
    // tries to disconnect, throwing "Cannot call disconnect() while connecting".
    // openatx uses 2.3.0 successfully on the same emulator. Bump to 2.4.x
    // when GA + the race is fixed upstream.
    androidTestImplementation("androidx.test.uiautomator:uiautomator:2.3.0")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test:rules:1.7.0")

    // ── HTTP server: Ktor 3 (JetBrains-maintained, coroutines-native). ─
    // Engine: CIO (pure-Kotlin, no Netty).
    val ktor = "3.2.0"
    androidTestImplementation("io.ktor:ktor-server-core:$ktor")
    androidTestImplementation("io.ktor:ktor-server-cio:$ktor")
    androidTestImplementation("io.ktor:ktor-server-content-negotiation:$ktor")
    androidTestImplementation("io.ktor:ktor-serialization-kotlinx-json:$ktor")
    androidTestImplementation("io.ktor:ktor-server-status-pages:$ktor")
    androidTestImplementation("io.ktor:ktor-server-call-logging:$ktor")

    // ── Coroutines + serialization runtime ─────────────────────────────
    androidTestImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    androidTestImplementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.9.0")

    // ── JUnit 4 + AndroidX test core ───────────────────────────────────
    // We use a @Test method that loops forever to keep the Instrumentation
    // process alive. Standard AndroidJUnitRunner handles UiAutomation init.
    androidTestImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
}
