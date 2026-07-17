//! Build the vendored Intel VPL dispatcher (`vendor/libvpl`) as a static
//! archive via CMake and generate bindings over the C API (`vpl/mfx.h`).
//!
//! Windows only — the native QSV backend is a Windows-host feature
//! (design/native-qsv-encoder.md); Linux Intel is served by VAAPI/Vulkan
//! Video. Other targets get an empty bindings file so the workspace builds
//! everywhere.
//!
//! The dispatcher is MIT and statically linked: no new runtime DLL. At run
//! time it locates the Intel GPU runtimes (`libmfx64-gen.dll`, legacy
//! `libmfxhw64.dll`) in the driver store; a box without an Intel driver just
//! fails MFXCreateSession and the encoder open falls through — the same
//! degrade contract as the NVENC/AMF runtime loaders.
//!
//! Everything compiles from the committed vendor tree: no network, no system
//! libvpl, no pkg-config. Only cmake + a libclang for bindgen are required on
//! the build machine (both already in the build closure via pyrowave-sys).

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=CMakeLists.txt");
    println!("cargo:rerun-if-changed=vendor/libvpl");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_path = out.join("bindings.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        std::fs::write(
            &bindings_path,
            "// libvpl-sys: Windows-only, empty on this target\n",
        )
        .unwrap();
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let api_include = manifest_dir.join("vendor/libvpl/api");

    // Always Release: mirrors pyrowave-sys — a debug dispatcher buys nothing
    // and the MSVC debug CRT would clash with Rust's release CRT.
    let dst = cmake::Config::new(&manifest_dir)
        .profile("Release")
        .build_target("VPL")
        .build();
    let build = dst.join("build");
    println!("cargo:rustc-link-search=native={}", build.display());
    // MSVC multi-config generators put the archive in a Release/ subdir.
    println!(
        "cargo:rustc-link-search=native={}",
        build.join("Release").display()
    );
    println!("cargo:rustc-link-lib=static=vpl");
    // The dispatcher's Win32 import closure (registry, DXGI adapter probing
    // COM plumbing). d3d9.dll/dxgi.dll themselves are LoadLibrary'd at
    // runtime — no static import.
    for lib in ["advapi32", "ole32", "user32", "uuid", "gdi32"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    // ONEVPL_EXPERIMENTAL must match the CMake side (BUILD_EXPERIMENTAL=ON):
    // it gates the D3D11 surface-import API in both the headers and the
    // dispatcher object code.
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", api_include.display()))
        .clang_arg("-DONEVPL_EXPERIMENTAL")
        // Plain MFX_* constants instead of EnumName_MFX_* — the C API is used
        // by FourCC/flag value, never by enum type.
        .prepend_enum_name(false)
        .derive_default(true)
        .generate()
        .expect("bindgen failed for vpl/mfx.h");
    bindings
        .write_to_file(&bindings_path)
        .expect("failed to write libvpl bindings");
}
