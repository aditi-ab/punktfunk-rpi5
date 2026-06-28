import org.jetbrains.kotlin.gradle.dsl.JvmTarget

import java.util.Properties

plugins {
    id("com.android.application")
    // AGP 9 built-in Kotlin: NO org.jetbrains.kotlin.android. The Compose compiler plugin is
    // supplied by AGP, so it's applied without a version.
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "io.unom.punktfunk"
    compileSdk = 37 // Android 17 — required by androidx.core 1.19.0; targetSdk stays 36 for now.

    defaultConfig {
        // Load from .env if it exists (local dev), otherwise from System.getenv (CI)
        val envFile = project.rootProject.file(".env")
        val props = Properties()
        if (envFile.exists()) {
            envFile.inputStream().use { props.load(it) }
        }

        applicationId = "io.unom.punktfunk"
        minSdk = 31
        targetSdk = 36
        val vCode = (props.getProperty("VERSION_CODE") ?: System.getenv("VERSION_CODE"))
        versionCode = vCode?.toInt() ?: 1
        // versionName is the single project version, threaded from CI (a vX.Y.Z release or a
        // canary string). versionCode stays the monotonic run number (Play rejects regressions).
        versionName = (props.getProperty("VERSION_NAME") ?: System.getenv("VERSION_NAME")) ?: "0.0.2"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    signingConfigs {
        create("release") {
            // Load from .env if it exists (local dev), otherwise from System.getenv (CI)
            val envFile = project.rootProject.file(".env")
            val props = Properties()
            if (envFile.exists()) {
                envFile.inputStream().use { props.load(it) }
            }

            val ksFile = props.getProperty("RELEASE_KEYSTORE_FILE") ?: System.getenv("RELEASE_KEYSTORE_FILE")
            if (ksFile != null) {
                storeFile = file(ksFile)
                storePassword = props.getProperty("RELEASE_KEYSTORE_PASSWORD") ?: System.getenv("RELEASE_KEYSTORE_PASSWORD")
                keyAlias = props.getProperty("RELEASE_KEY_ALIAS") ?: System.getenv("RELEASE_KEY_ALIAS")
                keyPassword = props.getProperty("RELEASE_KEY_PASSWORD") ?: System.getenv("RELEASE_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.getByName("release")
        }
    }

    buildFeatures { compose = true }

    // Roborazzi/Robolectric render Compose on the host JVM (the CI screenshot harness) and need the
    // merged Android resources + the app's manifest/theme available to the unit tests.
    testOptions { unitTests { isIncludeAndroidResources = true } }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
    packaging {
        jniLibs {
            useLegacyPackaging = false
            // punktfunk-core is statically linked into libpunktfunk_android.so (rlib). Its standalone
            // cdylib (built because the core crate also declares crate-type = cdylib) is never loaded
            // by Kotlin — drop it from the APK rather than ship ~5–9 MB of dead code.
            excludes += "**/libpunktfunk_core.so"
        }
    }
}

kotlin { compilerOptions { jvmTarget.set(JvmTarget.JVM_21) } }

dependencies {
    implementation(project(":kit"))

    val composeBom = platform("androidx.compose:compose-bom:2026.05.01")
    implementation(composeBom)

    implementation("androidx.core:core-ktx:1.19.0")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.10.0")

    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core") // bottom-bar tab icons
    debugImplementation("androidx.compose.ui:ui-tooling")

    // Android TV components (we target phone + TV) land in the TV-UI milestone:
    //   implementation("androidx.tv:tv-material:1.1.0")
    // The manifest already declares leanback so the scaffold installs on TV.

    // --- CI screenshot harness (Roborazzi on the JVM via Robolectric — no emulator/GPU). The
    // screenshot tests render the real Compose UI with mock state; never load the JNI core, so the
    // job runs `:app:testDebugUnitTest -PskipRustBuild` (see kit/build.gradle.kts). ---
    testImplementation(composeBom)
    testImplementation("androidx.compose.ui:ui-test-junit4")
    debugImplementation("androidx.compose.ui:ui-test-manifest") // the ComponentActivity test host
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.16.1")
    testImplementation("io.github.takahirom.roborazzi:roborazzi:1.64.0")
    testImplementation("io.github.takahirom.roborazzi:roborazzi-compose:1.64.0")
}

// Record (write) the screenshots when the unit tests run. These tests exist to GENERATE marketing
// images, not to diff goldens, so always capture rather than verify.
tasks.withType<Test>().configureEach {
    systemProperty("roborazzi.test.record", "true")
}
