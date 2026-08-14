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
    compileSdk = 37 // Android 17 — required by androidx.core 1.19.0.

    defaultConfig {
        // Load from .env if it exists (local dev), otherwise from System.getenv (CI)
        val envFile = project.rootProject.file(".env")
        val props = Properties()
        if (envFile.exists()) {
            envFile.inputStream().use { props.load(it) }
        }

        applicationId = "io.unom.punktfunk"
        // Android 9. Reaches older Android TV boxes (e.g. Amlogic streamers still on Android 9–11);
        // the handful of API 31+ APIs we use are runtime-gated (Material You → brand palette, rumble
        // → legacy Vibrator, NEARBY_WIFI/lights/ADPF already gated), so nothing is lost above 28.
        minSdk = 28
        // Android 17: targeting 37 makes Local Network Protection MANDATORY — all LAN traffic (the
        // QUIC dial, mDNS, WoL, the library fetch) is blocked until the user grants the
        // ACCESS_LOCAL_NETWORK runtime permission. ConnectScreen owns that request/rationale flow;
        // don't bump past 37 without re-checking the next release's behavior changes.
        targetSdk = 37
        val vCode = (props.getProperty("VERSION_CODE") ?: System.getenv("VERSION_CODE"))
        versionCode = vCode?.toInt() ?: 1
        // versionName is the single project version, threaded from CI (a vX.Y.Z release or a
        // canary string). versionCode stays the monotonic run number (Play rejects regressions).
        // Local dev (no VERSION_NAME) falls back to the workspace version from the root Cargo.toml —
        // the single source of truth — so an on-device build shows the real current version, not a
        // stale placeholder.
        val workspaceVersion = runCatching {
            project.rootProject.file("../../Cargo.toml").readLines()
                .dropWhile { !it.trim().startsWith("[workspace.package]") }
                .firstOrNull { it.trim().startsWith("version") }
                ?.substringAfter('=')?.trim()?.trim('"')
        }.getOrNull()
        versionName = (props.getProperty("VERSION_NAME") ?: System.getenv("VERSION_NAME"))
            ?: workspaceVersion ?: "0.0.0"
        // Ship 32-bit armeabi-v7a alongside 64-bit arm64-v8a: many Google TV / Android TV streamers
        // (Walmart onn. 4K, Chromecast with Google TV, budget Amlogic boxes) run a 32-bit Android
        // userspace, and because this app carries native code, Google Play (and a sideload installer)
        // filters it as "not compatible" on those devices unless an armeabi-v7a variant is present.
        // x86_64 stays for the emulator. Google keeps delivering to 32-bit TV devices (see the Aug
        // 2025 "64-bit app compatibility for Google TV and Android TV" post) — the 64-bit lib is the
        // required half; the 32-bit lib is what actually reaches the boxes people report failing.
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
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
    implementation("androidx.compose.material:material-icons-core") // bottom-bar / rail tab icons
    implementation("androidx.compose.material:material-icons-extended") // settings-category icons
    debugImplementation("androidx.compose.ui:ui-tooling")

    // Cover-art loading for the game-library coverflow. Coil 2.x uses OkHttp under the hood, so we
    // feed it the same mTLS OkHttpClient the library fetch uses (reaching the host's own art proxy).
    implementation("io.coil-kt:coil-compose:2.7.0")

    // Real backdrop blur for the floating console legends (RenderEffect on API 31+, a translucent
    // scrim below). The gamepad UI's frosted pills sample + blur whatever scrolls behind them.
    implementation("dev.chrisbanes.haze:haze:1.6.0")

    // Android TV components (we target phone + TV) land in the TV-UI milestone:
    //   implementation("androidx.tv:tv-material:1.1.0")
    // The manifest already declares leanback so the scaffold installs on TV.

    // --- CI screenshot harness (Roborazzi on the JVM via Robolectric — no emulator/GPU). The
    // screenshot tests render the real Compose UI with mock state; never load the JNI core, so the
    // job runs `:app:testDebugUnitTest -PskipRustBuild` (see kit/build.gradle.kts). ---
    testImplementation(composeBom)
    testImplementation("androidx.compose.ui:ui-test-junit4")
    // Deterministic cover art for the library scene: FakeImageLoaderEngine answers the coverflow's
    // AsyncImage synchronously with generated posters — no network, no async race under the frozen
    // animation clock.
    testImplementation("io.coil-kt:coil-test:2.7.0")
    debugImplementation("androidx.compose.ui:ui-test-manifest") // the ComponentActivity test host
    testImplementation("junit:junit:4.13.2")
    // Real `org.json` for the shared-vectors test: the `org.json` inside `android.jar` is a stub
    // set whose every method throws "Stub!", so a plain JVM unit test cannot parse with it. Same
    // dependency, same reason, as the kit module's deeplink-vectors test.
    testImplementation("org.json:json:20250107")
    testImplementation("org.robolectric:robolectric:4.16.1")
    testImplementation("io.github.takahirom.roborazzi:roborazzi:1.64.0")
    testImplementation("io.github.takahirom.roborazzi:roborazzi-compose:1.64.0")
}

// Record (write) the screenshots when the unit tests run. These tests exist to GENERATE marketing
// images, not to diff goldens, so always capture rather than verify.
tasks.withType<Test>().configureEach {
    systemProperty("roborazzi.test.record", "true")
}
