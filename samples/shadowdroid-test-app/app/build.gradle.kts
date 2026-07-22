plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "io.github.andriyo.shadowdroid.sample"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.github.andriyo.shadowdroid.sample"
        minSdk = 24
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        debug {
            isDebuggable = true
        }
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
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
    // shadowdroid-agent (managed by `shadowdroid aar`) — debug-only in-app debug agent
    debugImplementation(files(rootProject.file("shadowdroid/shadowdroid-agent.aar")))
    // Exercises the in-app coroutine dump (`shadowdroid aar coroutines`).
    // Tracking additionally needs the probes activation block below (wired by
    // `shadowdroid aar install --coroutine-probes`).
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
    implementation(platform("androidx.compose:compose-bom:2024.12.01"))
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("com.squareup.okhttp3:okhttp:5.3.0")
}

// >>> shadowdroid coroutine probes (managed by `shadowdroid aar`) — do not edit
// Activates kotlinx-coroutines DebugProbes in DEBUG builds so `shadowdroid aar
// coroutines` can dump live coroutines (state, context, stacks) from the
// running app. kotlin-stdlib's `kotlin.coroutines.jvm.internal.DebugProbesKt`
// — the class that lands in the dex — is a no-op stub; the JVM swaps it via a
// java-agent, which Android lacks (and ART's JDWP rejects class redefinition).
// This ASM visitor does the same swap at build time: the three probe methods
// are rewritten to delegate to `kotlinx.coroutines.debug.internal.DebugProbesKt`
// (what kotlinx-coroutines-core's own `DebugProbesKt.bin` replacement does).
// Debug variants only; release bytecode is untouched. Bodies are guarded, so a
// debug build without kotlinx-coroutines-core keeps the no-op behaviour.
abstract class ShadowDroidCoroutineProbesFactory :
    com.android.build.api.instrumentation.AsmClassVisitorFactory<
        com.android.build.api.instrumentation.InstrumentationParameters.None> {

    override fun isInstrumentable(
        classData: com.android.build.api.instrumentation.ClassData,
    ): Boolean = classData.className == "kotlin.coroutines.jvm.internal.DebugProbesKt"

    override fun createClassVisitor(
        classContext: com.android.build.api.instrumentation.ClassContext,
        nextClassVisitor: org.objectweb.asm.ClassVisitor,
    ): org.objectweb.asm.ClassVisitor =
        object : org.objectweb.asm.ClassVisitor(org.objectweb.asm.Opcodes.ASM9, nextClassVisitor) {
            override fun visitMethod(
                access: Int,
                name: String,
                descriptor: String,
                signature: String?,
                exceptions: Array<out String>?,
            ): org.objectweb.asm.MethodVisitor {
                val target = super.visitMethod(access, name, descriptor, signature, exceptions)
                if (name !in setOf(
                        "probeCoroutineCreated",
                        "probeCoroutineResumed",
                        "probeCoroutineSuspended",
                    )
                ) {
                    return target
                }
                // Drop the original no-op body; emit a guarded delegating one.
                return object : org.objectweb.asm.MethodVisitor(
                    org.objectweb.asm.Opcodes.ASM9,
                    null,
                ) {
                    override fun visitCode() {
                        val returnsValue =
                            descriptor.endsWith(")Lkotlin/coroutines/Continuation;")
                        val start = org.objectweb.asm.Label()
                        val end = org.objectweb.asm.Label()
                        val handler = org.objectweb.asm.Label()
                        target.visitCode()
                        target.visitTryCatchBlock(start, end, handler, "java/lang/Throwable")
                        target.visitLabel(start)
                        target.visitVarInsn(org.objectweb.asm.Opcodes.ALOAD, 0)
                        target.visitMethodInsn(
                            org.objectweb.asm.Opcodes.INVOKESTATIC,
                            "kotlinx/coroutines/debug/internal/DebugProbesKt",
                            name,
                            descriptor,
                            false,
                        )
                        if (returnsValue) {
                            target.visitInsn(org.objectweb.asm.Opcodes.ARETURN)
                        } else {
                            target.visitInsn(org.objectweb.asm.Opcodes.RETURN)
                        }
                        target.visitLabel(end)
                        target.visitLabel(handler)
                        // kotlinx-coroutines-core absent → original no-op shape.
                        target.visitInsn(org.objectweb.asm.Opcodes.POP)
                        if (returnsValue) {
                            target.visitVarInsn(org.objectweb.asm.Opcodes.ALOAD, 0)
                            target.visitInsn(org.objectweb.asm.Opcodes.ARETURN)
                        } else {
                            target.visitInsn(org.objectweb.asm.Opcodes.RETURN)
                        }
                        target.visitMaxs(2, 1) // recomputed by the frames mode
                        target.visitEnd()
                    }
                }
            }
        }
}
plugins.withId("com.android.application") {
    val shadowdroidComponents = extensions.getByType(
        com.android.build.api.variant.ApplicationAndroidComponentsExtension::class.java,
    )
    shadowdroidComponents.onVariants(
        shadowdroidComponents.selector().withBuildType("debug"),
    ) { variant ->
        variant.instrumentation.transformClassesWith(
            ShadowDroidCoroutineProbesFactory::class.java,
            com.android.build.api.instrumentation.InstrumentationScope.ALL,
        ) {}
        variant.instrumentation.setAsmFramesComputationMode(
            com.android.build.api.instrumentation.FramesComputationMode
                .COMPUTE_FRAMES_FOR_INSTRUMENTED_METHODS,
        )
    }
}
// <<< shadowdroid coroutine probes (managed by `shadowdroid aar`)
