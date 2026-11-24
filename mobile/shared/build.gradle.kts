plugins {
    alias(libs.plugins.kotlin.multiplatform)
    alias(libs.plugins.kotlin.serialization)
}

// The protocol, the conversation model and the connection logic are plain
// Kotlin, so they can serve an iOS app later. The Android app consumes the JVM
// target, and the tests run on it without a device.
kotlin {
    jvmToolchain(21)

    compilerOptions {
        allWarningsAsErrors.set(true)
    }

    jvm {
        compilerOptions {
            jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
        }
    }

    sourceSets {
        commonMain.dependencies {
            api(libs.kotlinx.coroutines.core)
            api(libs.kotlinx.serialization.json)
            implementation(libs.atomicfu)
        }
        commonTest.dependencies {
            implementation(libs.kotlin.test)
            implementation(libs.kotlinx.coroutines.test)
        }
    }
}

// The recorded example of every message the bridge's API version 1 has had.
// The app must keep decoding it; see docs/mobile-api.md#compatibility.
val apiFixture = rootProject.layout.projectDirectory.file("../crates/mobile-api/fixtures/v1.jsonl")

tasks.withType<Test>().configureEach {
    inputs.file(apiFixture).withPathSensitivity(PathSensitivity.NONE)
    systemProperty("cagents.mobileApiFixture", apiFixture.asFile.absolutePath)
}
