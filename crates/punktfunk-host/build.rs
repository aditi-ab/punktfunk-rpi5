//! Build script. The only thing it does: with the `nvenc` feature (Windows GPU host), tell the
//! linker to pull the NVENC import library. The NVENC entry points
//! (`NvEncodeAPICreateInstance` / `NvEncodeAPIGetMaxSupportedVersion`) live in `nvEncodeAPI64.dll`
//! (shipped with the NVIDIA driver), so the host links against `nvencodeapi.lib`. Point
//! `PUNKTFUNK_NVENC_LIB_DIR` at a directory containing `nvencodeapi.lib` — from the NVIDIA Video
//! Codec SDK, or an import lib generated from the driver's `nvEncodeAPI64.dll`
//! (`lib /def:nvenc.def /machine:x64 /out:nvencodeapi.lib` with the two exports above).
fn main() {
    // Build provenance: stamp the exact package/build version into the binary so a running host
    // can report what it is (mgmt /health, the startup log, `--version`) and a stale/shadowed
    // install is detectable. CI (deb.yml / rpm.yml / the RPM spec / the PKGBUILD) sets
    // PUNKTFUNK_BUILD_VERSION to the full package version (e.g. `0.2.0~ci120.g802e98d`); a plain
    // `cargo build` falls back to the crate version. Deliberately NOT git-derived — the RPM builds
    // from a `git archive` tarball with no .git, and a hard git dependency would break it.
    let version = std::env::var("PUNKTFUNK_BUILD_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into()));
    println!("cargo:rustc-env=PUNKTFUNK_VERSION={version}");
    println!("cargo:rerun-if-env-changed=PUNKTFUNK_BUILD_VERSION");

    if std::env::var_os("CARGO_FEATURE_NVENC").is_some() {
        if let Some(dir) = std::env::var_os("PUNKTFUNK_NVENC_LIB_DIR") {
            println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
        }
        println!("cargo:rustc-link-lib=dylib=nvencodeapi");
        println!("cargo:rerun-if-env-changed=PUNKTFUNK_NVENC_LIB_DIR");
    }
}
