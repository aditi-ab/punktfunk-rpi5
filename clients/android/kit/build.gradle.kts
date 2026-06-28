import java.io.File
import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    // AGP 9 built-in Kotlin compiles this module's Kotlin (NativeBridge) — no kotlin.android plugin.
    id("com.android.library")
}

val ndkVer = "30.0.14904198" // r30-beta1 — matches the SDK NDK installed for cargo-ndk

android {
    namespace = "io.unom.punktfunk.kit"
    compileSdk = 37 // Android 17 — align with :app (androidx.core 1.19.0 requires it)
    ndkVersion = ndkVer

    defaultConfig {
        minSdk = 31
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
    packaging { jniLibs { useLegacyPackaging = false } } // 16 KB-page friendly
}

kotlin { compilerOptions { jvmTarget.set(JvmTarget.JVM_21) } }

dependencies {
    testImplementation("junit:junit:4.13.2") // JVM unit test for the pure TXT parser
}

// ------------------------------------------------------------------------------------------------
// cargo-ndk: cross-compile clients/android/native (punktfunk-client-android) into this module's jniLibs/<abi>/ so the
// resulting libpunktfunk_android.so is packaged into the app (and any AAR this module produces).
// NDK r28+ aligns to 16 KB pages by default — no extra linker flags. Prereqs (see clients/android
// /README.md): `cargo install cargo-ndk` + `rustup target add aarch64-linux-android x86_64-linux-android`.
// ------------------------------------------------------------------------------------------------
val repoRoot = rootDir.parentFile.parentFile // clients/android -> clients -> repo root
val cargoBin = "${System.getProperty("user.home")}/.cargo/bin"

// SDK location without depending on AGP's DSL (sdkDirectory isn't in AGP 9's library extension):
// env first (set by Android Studio and by our CLI shell), then local.properties, then the default.
fun androidSdkDir(): String {
    System.getenv("ANDROID_HOME")?.let { return it }
    System.getenv("ANDROID_SDK_ROOT")?.let { return it }
    val lp = rootProject.file("local.properties")
    if (lp.exists()) {
        val props = Properties()
        lp.inputStream().use { props.load(it) }
        props.getProperty("sdk.dir")?.let { return it }
    }
    return "${System.getProperty("user.home")}/Library/Android/sdk"
}

fun registerCargoNdk(taskName: String, release: Boolean) =
    tasks.register<Exec>(taskName) {
        group = "rust"
        description = "cargo-ndk build of punktfunk-client-android (${if (release) "release" else "debug"})"
        workingDir = repoRoot
        val sdk = androidSdkDir()
        // A GUI Android Studio launch does not source the login shell, so make cargo, the NDK, and
        // cmake (libopus builds via the cmake crate) discoverable explicitly — same as a bare CLI.
        val cmakeBin = "$sdk/cmake/3.22.1/bin"
        environment(
            "PATH",
            cargoBin + File.pathSeparator + cmakeBin + File.pathSeparator + System.getenv("PATH"),
        )
        environment("ANDROID_HOME", sdk)
        environment("ANDROID_NDK_HOME", "$sdk/ndk/$ndkVer")
        // CMake's built-in Android support (used by the cmake crate for libopus) finds the NDK via
        // these, and uses Ninja (bundled next to the SDK cmake) since there's no `make`.
        environment("ANDROID_NDK_ROOT", "$sdk/ndk/$ndkVer")
        environment("ANDROID_NDK", "$sdk/ndk/$ndkVer")
        environment("CMAKE_GENERATOR", "Ninja")
        // audiopus_sys picks static-vs-dynamic by HOST not target — force the bundled static libopus
        // (pure C) so the android .so links it instead of looking for the host's libopus.so.
        environment("LIBOPUS_STATIC", "1")
        environment("LIBOPUS_NO_PKG", "1")
        // Resolve cargo by ABSOLUTE path: Gradle's Exec resolves command[0] via the JVM's
        // inherited PATH, NOT the environment("PATH", …) set above (that only reaches the spawned
        // child). A GUI Android Studio launch (and any daemon it started) has no ~/.cargo/bin on
        // its PATH, so a bare "cargo" fails to start. The env PATH above still lets cargo/cargo-ndk
        // find their subtools.
        val cmd = mutableListOf(
            "$cargoBin/cargo", "ndk",
            "-t", "arm64-v8a", "-t", "x86_64",
            // Link against the minSdk-31 sysroot so libaaudio (API 26+) is found.
            "--platform", "31",
            "-o", file("src/main/jniLibs").absolutePath,
            "build", "-p", "punktfunk-client-android",
        )
        if (release) cmd += "--release"
        commandLine(cmd)
    }

val cargoNdkDebug = registerCargoNdk("cargoNdkDebug", release = false)
val cargoNdkRelease = registerCargoNdk("cargoNdkRelease", release = true)

afterEvaluate {
    // `-PskipRustBuild` skips the cargo-ndk native build — for JVM-only tasks (the Roborazzi
    // screenshot unit tests render Compose on the JVM and never load libpunktfunk_android.so), so
    // CI/local screenshot runs don't need the Rust toolchain or NDK. The native build stays wired
    // for every normal APK/AAR build.
    if (!project.hasProperty("skipRustBuild")) {
        tasks.named("preDebugBuild").configure { dependsOn(cargoNdkDebug) }
        tasks.named("preReleaseBuild").configure { dependsOn(cargoNdkRelease) }
    }
}
