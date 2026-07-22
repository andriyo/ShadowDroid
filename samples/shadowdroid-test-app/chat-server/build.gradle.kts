plugins {
    application
    id("org.jetbrains.kotlin.jvm")
}

group = "io.github.andriyo.shadowdroid.sample"
version = "0.1.0"

val ktorVersion = "3.5.1"

kotlin {
    jvmToolchain(21)
}

application {
    mainClass.set("io.github.andriyo.shadowdroid.sample.chat.ApplicationKt")
}

dependencies {
    implementation("io.ktor:ktor-server-core:$ktorVersion")
    implementation("io.ktor:ktor-server-netty:$ktorVersion")
    implementation("io.ktor:ktor-server-websockets:$ktorVersion")
    implementation("io.ktor:ktor-network-tls-certificates:$ktorVersion")
    implementation("ch.qos.logback:logback-classic:1.5.18")

    testImplementation(kotlin("test-junit5"))
    testImplementation("io.ktor:ktor-server-test-host:$ktorVersion")
    testImplementation("io.ktor:ktor-client-websockets:$ktorVersion")
    testRuntimeOnly("org.junit.platform:junit-platform-launcher")
}

tasks.test {
    useJUnitPlatform()
}
