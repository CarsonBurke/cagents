plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
}

// The release packager sets these from the source commit's Cargo version.
// Gradle builds the unsigned APK. Signing inputs stay out of its configuration.
val releaseVersion = providers.gradleProperty("cagents.releaseVersion").orNull
val releaseVersionCode = providers.gradleProperty("cagents.releaseVersionCode").orNull
require((releaseVersion == null) == (releaseVersionCode == null)) {
    "Set both cagents.releaseVersion and cagents.releaseVersionCode"
}

android {
    namespace = "dev.cagents.mobile.android"
    compileSdk = libs.versions.compileSdk.get().toInt()

    defaultConfig {
        applicationId = "dev.cagents.mobile"
        minSdk = libs.versions.minSdk.get().toInt()
        targetSdk = libs.versions.targetSdk.get().toInt()
        versionCode = releaseVersionCode?.toInt() ?: 1
        versionName = releaseVersion ?: "0.1.0"
    }

    // scripts/gradle-remote passes the debug key of the machine that asked
    // for the build, so its APKs install over what that machine installed.
    providers.gradleProperty("cagents.debugKeystore").orNull?.let { keystore ->
        signingConfigs.getByName("debug").storeFile = rootProject.file(keystore)
    }

    buildTypes {
        debug {
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
        }
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
        // Release code with the development identity and debug key. It
        // installs over the debug build, keeping that build's
        // phone key and trusted hosts. Use it on a phone: a debuggable build
        // runs Compose several times slower, and its lists stutter.
        create("preview") {
            initWith(getByName("release"))
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-preview"
            signingConfig = signingConfigs.getByName("debug")
            matchingFallbacks += "release"
        }
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        resources {
            excludes += setOf(
                "META-INF/versions/9/OSGI-INF/MANIFEST.MF",
                "META-INF/DEPENDENCIES",
                "META-INF/LICENSE*",
                "META-INF/NOTICE*",
            )
        }
    }
}

kotlin {
    jvmToolchain(21)
    compilerOptions {
        allWarningsAsErrors.set(true)
    }
}

dependencies {
    implementation(project(":shared"))

    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.compose.material.icons.extended)
    implementation(libs.compose.ui.tooling.preview)
    debugImplementation(libs.compose.ui.tooling)
    implementation(libs.activity.compose)
    implementation(libs.lifecycle.runtime.compose)
    implementation(libs.lifecycle.viewmodel.compose)
    implementation(libs.lifecycle.process)
    implementation(libs.navigation3.runtime)
    implementation(libs.navigation3.ui)
    implementation(libs.lifecycle.viewmodel.navigation3)
    implementation(libs.kotlinx.coroutines.android)

    implementation(libs.camera.camera2)
    implementation(libs.camera.lifecycle)
    implementation(libs.camera.view)
    implementation(libs.zxing.core)

    implementation(libs.sshj)
    implementation(libs.bouncycastle.prov)
    implementation(libs.bouncycastle.pkix)
    implementation(libs.slf4j.nop)

    testImplementation(libs.kotlin.test.junit)
}
