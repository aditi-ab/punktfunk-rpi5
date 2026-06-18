import org.jetbrains.kotlin.gradle.dsl.JvmTarget

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
        applicationId = "io.unom.punktfunk"
        minSdk = 31
        targetSdk = 36
        versionCode = System.getenv("VERSION_CODE")?.toInt() ?: 1
        versionName = "0.0.1"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    signingConfigs {
        create("release") {
            // These are provided by CI secrets as environment variables
            val keystoreFile = System.getenv("RELEASE_KEYSTORE_FILE")
            if (keystoreFile != null) {
                storeFile = file(keystoreFile)
                storePassword = System.getenv("RELEASE_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("RELEASE_KEY_ALIAS")
                keyPassword = System.getenv("RELEASE_KEY_PASSWORD")
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
}
