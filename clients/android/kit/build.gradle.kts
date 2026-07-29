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
        minSdk = 28 // Android 9 — reaches older TV boxes; API 31+ features are runtime-gated.
        // Keep in lockstep with :app — 32-bit armeabi-v7a for the many 32-bit Google TV / Android TV
        // boxes, 64-bit arm64-v8a for phones + modern TV, x86_64 for the emulator.
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
    packaging { jniLibs { useLegacyPackaging = false } } // 16 KB-page friendly
}

kotlin { compilerOptions { jvmTarget.set(JvmTarget.JVM_21) } }

dependencies {
    // mTLS HTTPS client for the host's management API (the game-library fetch + cover-art loads).
    // OkHttp lets us present the paired client cert and pin the host's self-signed cert by SHA-256.
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    testImplementation("junit:junit:4.13.2") // JVM unit tests for the pure parsers/migrations
    // A REAL org.json on the unit-test classpath. android.jar's org.json is stubs that throw
    // "Stub!", so the host-store migration test — which asserts over the very JSON blobs the store
    // reads and writes — cannot run without it. Explicit test deps precede the mockable android.jar.
    testImplementation("org.json:json:20250107")
}

// ------------------------------------------------------------------------------------------------
// cargo-ndk: cross-compile clients/android/native (punktfunk-client-android) into this module's jniLibs/<abi>/ so the
// resulting libpunktfunk_android.so is packaged into the app (and any AAR this module produces).
// NDK r28+ aligns to 16 KB pages by default — no extra linker flags. Prereqs (see clients/android
// /README.md): `cargo install cargo-ndk` + `rustup target add aarch64-linux-android x86_64-linux-android`.
// ------------------------------------------------------------------------------------------------
val repoRoot = rootDir.parentFile.parentFile // clients/android -> clients -> repo root
// CARGO_HOME first: rustup puts every binary in $CARGO_HOME/bin, and the CI image
// (ci/android-ci.Dockerfile) installs the shared toolchain at /usr/local/cargo — the
// historical ~/.cargo fallback is what a GUI Android Studio launch (no env) still needs.
val cargoBin = System.getenv("CARGO_HOME")?.let { "$it/bin" }
    ?: "${System.getProperty("user.home")}/.cargo/bin"

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
            "-t", "arm64-v8a", "-t", "armeabi-v7a", "-t", "x86_64",
            // Link against the minSdk-28 sysroot (libaaudio, API 26, is present). NOTE: this does
            // NOT reject an accidental >28 hard import — a cdylib link permits undefined symbols,
            // which then fail at System.loadLibrary on every device below the symbol's API level
            // (the 0.9.0 Android-≤12 regression). The checkJniImports* task after this build is
            // what actually enforces the floor; >28 entry points must be dlsym-resolved (see
            // decode::try_set_frame_rate, decode::install_render_callback, adpf).
            "--platform", "28",
            "-o", file("src/main/jniLibs").absolutePath,
            "build", "-p", "punktfunk-client-android",
        )
        if (release) cmd += "--release"
        commandLine(cmd)
    }

// Post-link floor check: every undefined symbol in the built .so must exist in the API-28 stubs,
// else System.loadLibrary fails on devices at the minSdk floor (see the script header for the
// 0.9.0 incident this guards against). Runs right after its cargo-ndk task; the APK build depends
// on this task (not the cargo one directly), so a violation fails the build, local and CI alike.
fun registerCheckJniImports(taskName: String, cargoTask: TaskProvider<Exec>) =
    tasks.register<Exec>(taskName) {
        group = "rust"
        description = "verify libpunktfunk_android.so imports stay within the API-28 floor"
        dependsOn(cargoTask)
        workingDir = repoRoot
        commandLine(
            "sh", File(repoRoot, "scripts/ci/check-android-jni-imports.sh").absolutePath,
            "${androidSdkDir()}/ndk/$ndkVer",
            file("src/main/jniLibs").absolutePath,
            "28",
        )
    }

val cargoNdkDebug = registerCargoNdk("cargoNdkDebug", release = false)
val cargoNdkRelease = registerCargoNdk("cargoNdkRelease", release = true)
val checkJniImportsDebug = registerCheckJniImports("checkJniImportsDebug", cargoNdkDebug)
val checkJniImportsRelease = registerCheckJniImports("checkJniImportsRelease", cargoNdkRelease)

afterEvaluate {
    // `-PskipRustBuild` skips the cargo-ndk native build — for JVM-only tasks (the Roborazzi
    // screenshot unit tests render Compose on the JVM and never load libpunktfunk_android.so), so
    // CI/local screenshot runs don't need the Rust toolchain or NDK. The native build stays wired
    // for every normal APK/AAR build.
    //
    // DEBUG APKs SHIP RELEASE RUST. Cargo's debug profile is not "a bit slower" for this library —
    // it is unusable: the AES-GCM data-plane decrypt runs through generic-array iterator closures
    // with per-byte UB checks instead of ARMv8 hardware AES. Profiled live on a phone (simpleperf):
    // ~800 µs of user CPU per 1.4 KB packet, the receive pump pinned over a full core yet unable to
    // drain a 20 Mbps stream — every debug-APK on-device test was silently benchmarking unoptimized
    // crypto, not the streaming pipeline. Kotlin debuggability is untouched (the APK is still a
    // debug build); only the cargo profile changes. `-PrustDebug` restores a debug-profile native
    // build for the rare session that actually steps through Rust.
    if (!project.hasProperty("skipRustBuild")) {
        val debugRust =
            if (project.hasProperty("rustDebug")) checkJniImportsDebug else checkJniImportsRelease
        tasks.named("preDebugBuild").configure { dependsOn(debugRust) }
        tasks.named("preReleaseBuild").configure { dependsOn(checkJniImportsRelease) }
    }
}
