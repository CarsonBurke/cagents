import org.jetbrains.kotlin.gradle.plugin.mpp.apple.XCFramework

plugins {
    alias(libs.plugins.kotlin.multiplatform)
    alias(libs.plugins.kotlin.serialization)
}

// Compile the Rust recordings into the common test binary. Native tests also
// run in the simulator, where the build tree needn't be visible.
val apiFixture = rootProject.layout.projectDirectory.file("../crates/mobile-api/fixtures/v1.jsonl")
val pairingCodeFixture = rootProject.layout.projectDirectory.file("../crates/mobile-api/fixtures/pairing-code-v1.txt")

abstract class GenerateRecordedFixtures : DefaultTask() {
    @get:InputFiles
    @get:PathSensitive(PathSensitivity.NONE)
    abstract val recordings: ConfigurableFileCollection

    @get:OutputDirectory
    abstract val destination: DirectoryProperty

    @TaskAction
    fun generate() {
        val source = buildString {
            appendLine("package dev.cagents.mobile")
            appendLine()
            for (file in recordings) {
                val name = when (file.name) {
                    "v1.jsonl" -> "mobileApiRecording"
                    "pairing-code-v1.txt" -> "pairingCodeRecording"
                    else -> error("Unknown fixture: ${file.name}")
                }
                appendLine("internal val $name = listOf(")
                for (line in file.readLines()) {
                    append("    \"")
                    for (char in line) {
                        if (char == '\\' || char == '"' || char == '$') append('\\')
                        append(char)
                    }
                    appendLine("\",")
                }
                appendLine(")")
            }
        }
        val file = destination.file("RecordedFixtures.kt").get().asFile
        file.parentFile.mkdirs()
        file.writeText(source)
    }
}

val generateRecordedFixtures = tasks.register<GenerateRecordedFixtures>("generateRecordedFixtures") {
    recordings.from(apiFixture, pairingCodeFixture)
    destination.set(layout.buildDirectory.dir("generated/recordedFixtures"))
}

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

    val xcframework = XCFramework("CagentsMobileShared")
    listOf(iosArm64(), iosSimulatorArm64()).forEach { target ->
        target.binaries.framework {
            baseName = "CagentsMobileShared"
            isStatic = true
            xcframework.add(this)
        }
    }

    sourceSets {
        commonMain.dependencies {
            api(libs.kotlinx.coroutines.core)
            api(libs.kotlinx.serialization.json)
            implementation(libs.atomicfu)
        }
        commonTest {
            kotlin.srcDir(generateRecordedFixtures)
            dependencies {
                implementation(libs.kotlin.test)
                implementation(libs.kotlinx.coroutines.test)
            }
        }
    }
}
