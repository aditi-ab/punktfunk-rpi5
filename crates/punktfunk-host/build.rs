//! Build script. The only thing it does: with the `nvenc` feature (Windows GPU host), tell the
//! linker to pull the NVENC import library. The NVENC entry points
//! (`NvEncodeAPICreateInstance` / `NvEncodeAPIGetMaxSupportedVersion`) live in `nvEncodeAPI64.dll`
//! (shipped with the NVIDIA driver), so the host links against `nvencodeapi.lib`. Point
//! `PUNKTFUNK_NVENC_LIB_DIR` at a directory containing `nvencodeapi.lib` — from the NVIDIA Video
//! Codec SDK, or an import lib generated from the driver's `nvEncodeAPI64.dll`
//! (`lib /def:nvenc.def /machine:x64 /out:nvencodeapi.lib` with the two exports above).
fn main() {
    if std::env::var_os("CARGO_FEATURE_NVENC").is_some() {
        if let Some(dir) = std::env::var_os("PUNKTFUNK_NVENC_LIB_DIR") {
            println!("cargo:rustc-link-search=native={}", dir.to_string_lossy());
        }
        println!("cargo:rustc-link-lib=dylib=nvencodeapi");
        println!("cargo:rerun-if-env-changed=PUNKTFUNK_NVENC_LIB_DIR");
    }
}
