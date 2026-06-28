//! Runs the C ABI harness under `cargo test`: compiles `tests/c/harness.c`, links it
//! against the freshly built `libpunktfunk_core.a`, and asserts it round-trips frames
//! through the lossy loopback. The cross-platform canonical path (querying rustc for
//! link flags) is `tests/c/run.sh`; this mirrors it so `cargo test` alone covers the
//! C boundary.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Native libs the Rust staticlib needs, minus the ones `cc` already links by default
/// (`-lSystem`/`-lc`), to avoid duplicate-library linker warnings. See
/// `rustc --print native-static-libs`.
fn native_libs() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        // The workspace build unifies features into the staticlib, and `quic` pulls
        // rustls's platform verifier → Security/CoreFoundation, plus libopus (the in-core
        // `next_audio_pcm` decode path) which the `abi.rs` object references.
        &[
            "-lopus",
            "-liconv",
            "-lm",
            "-framework",
            "Security",
            "-framework",
            "CoreFoundation",
        ]
    } else if cfg!(target_os = "linux") {
        // `-lopus`: the `quic` feature pulls in-core Opus decode (`next_audio_pcm`), whose
        // symbols the linked `abi.rs` object references. Before `-lm` (opus needs libm).
        &[
            "-lopus",
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
        ]
    } else {
        &[]
    }
}

fn ensure_staticlib(profile_dir: &Path) -> PathBuf {
    let staticlib = profile_dir.join("libpunktfunk_core.a");
    if !staticlib.exists() {
        // `cargo test` doesn't always emit the standalone staticlib; build it. The
        // outer cargo's build lock is released during test execution, so this is safe.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let _ = Command::new(cargo)
            .args(["build", "-p", "punktfunk-core"])
            .status();
    }
    staticlib
}

#[test]
fn c_abi_harness_round_trips() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // crates/punktfunk-core
    let harness = manifest.join("tests/c/harness.c");
    let include = manifest.join("../../include");

    let exe = std::env::current_exe().expect("current_exe");
    // .../target/<profile>/deps/c_abi-<hash> -> target/<profile>
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("profile dir")
        .to_path_buf();

    let staticlib = ensure_staticlib(&profile_dir);
    assert!(
        staticlib.exists(),
        "staticlib not found at {} (run `cargo build -p punktfunk-core`)",
        staticlib.display()
    );
    assert!(
        include.join("punktfunk_core.h").exists(),
        "generated header missing; build punktfunk-core to regenerate it"
    );

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let out = profile_dir.join("punktfunk_c_harness");

    let mut compile = Command::new(&cc);
    compile
        .args(["-std=c11", "-Wall", "-Wextra", "-O2", "-I"])
        .arg(&include)
        .arg(&harness)
        .arg(&staticlib)
        .args(native_libs())
        .arg("-o")
        .arg(&out);

    match compile.status() {
        Ok(s) => assert!(s.success(), "C harness failed to compile/link"),
        Err(e) => {
            // No C toolchain (unusual) — don't fail the whole suite; run.sh covers CI.
            eprintln!("skipping C ABI test: cannot invoke `{cc}`: {e}");
            return;
        }
    }

    let run = Command::new(&out).status().expect("run C harness");
    assert!(run.success(), "C harness reported a round-trip failure");
}
