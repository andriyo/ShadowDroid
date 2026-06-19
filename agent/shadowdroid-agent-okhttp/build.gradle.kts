// ShadowDroid agent — OkHttp capture/intercept companion.
//
// Optional add-on to the core `shadowdroid-agent` AAR. It supplies the one
// piece that cannot be zero-app-code: an OkHttp `Interceptor` the host app adds
// to its client (debug-only) so the agent can see plaintext request/response
// **above TLS** — defeating cert pinning / Cronet / QUIC that the host MITM
// proxy can't.
//
// `compileOnly` OkHttp + the core agent: a local `files()` AAR carries no
// transitive deps, so the consuming app must already provide OkHttp (it does —
// that's why we're hooking it) and the core agent AAR (added alongside).

plugins {
    id("com.android.library")
}

android {
    namespace = "io.github.andriyo.shadowdroid.agent.okhttp"
    compileSdk = 36

    defaultConfig {
        minSdk = 21 // OkHttp 4.x floor; matches the core agent
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_21)
    }
}

dependencies {
    // Provided by the consuming app (core agent AAR + the app's own OkHttp).
    compileOnly(project(":shadowdroid-agent"))
    compileOnly("com.squareup.okhttp3:okhttp:4.12.0")
}
