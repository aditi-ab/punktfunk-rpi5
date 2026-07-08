//! Generate bindings for `libavutil/hwcontext_vulkan.h` against the SYSTEM headers.
//!
//! ffmpeg-sys-next binds a curated header list that omits every hwcontext_*.h; the
//! Vulkan hwcontext structs (`AVVulkanDeviceContext`, `AVVkFrame`) are what let us run
//! FFmpeg's Vulkan Video decoder on the presenter's own VkDevice and read the decoded
//! VkImages back. Their layout depends on compile-time FF_API_* deprecation gates in
//! libavutil/version.h, so bindgen over the installed header is the only ABI-safe
//! source of truth — hand transcription would silently skew on the next FFmpeg bump.
//!
//! Header discovery is per-OS: Linux asks pkg-config; Windows reuses the FFMPEG_DIR
//! tree ffmpeg-sys-next links against (BtbN trees ship no .pc files) plus an explicit
//! Vulkan-Headers include dir, since Windows has no system <vulkan/vulkan.h>. Other
//! targets get an empty file: the workspace builds on macOS (clients/apple is the
//! client there).

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=PF_FFVK_VULKAN_INCLUDE");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let includes = match target_os.as_str() {
        "linux" => linux_includes(),
        "windows" => windows_includes(),
        _ => {
            std::fs::write(
                &out,
                "// pf-ffvk: Linux/Windows-only, empty on this target\n",
            )
            .unwrap();
            return;
        }
    };

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        // The whole point of this crate: the Vulkan hwcontext surface…
        .allowlist_type("AVVulkan.*")
        .allowlist_type("AVVkFrame.*")
        .allowlist_function("av_vk_frame_alloc")
        .allowlist_function("av_vkfmt_from_pixfmt")
        // The feature structs chained into AVVulkanDeviceContext.device_features (plain
        // vulkan.h types; generating them here keeps the chain in one type system).
        .allowlist_type("VkPhysicalDeviceVulkan11Features")
        .allowlist_type("VkPhysicalDeviceVulkan12Features")
        .allowlist_type("VkPhysicalDeviceVulkan13Features")
        // AVVulkanFramesContext.img_flags values (plane views need MUTABLE_FORMAT).
        .allowlist_type("VkImageCreateFlagBits")
        // Timeline-semaphore wait — the pump measures true GPU decode completion.
        .allowlist_type("VkSemaphoreWaitInfo")
        .allowlist_type("PFN_vkWaitSemaphores")
        .allowlist_type("PFN_vkGetDeviceProcAddr")
        // …plus nothing else of FFmpeg: the core types these structs reference only
        // ever appear behind pointers here, so keep them opaque instead of duplicating
        // ffmpeg-sys-next's definitions (callers cast pointers between the crates).
        .opaque_type("AVHWDeviceContext")
        .opaque_type("AVHWFramesContext")
        .opaque_type("AVBufferRef")
        .opaque_type("AVFrame")
        .derive_debug(false)
        .layout_tests(true);
    for dir in &includes {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    let bindings = builder.generate().expect(
        "bindgen over libavutil/hwcontext_vulkan.h failed — is `vulkan-headers` installed? \
         (the header includes <vulkan/vulkan.h>)",
    );
    bindings.write_to_file(&out).unwrap();

    // The av_vk_* symbols live in libavutil, which ffmpeg-sys-next already links into
    // every consumer of this crate; no extra link flags needed. Emitting the lib anyway
    // keeps `cargo test -p pf-ffvk` linking standalone — which on Windows also needs the
    // import-lib search path (there is no system linker path for FFmpeg there).
    if target_os == "windows" {
        // windows_includes() already required FFMPEG_DIR.
        let ff = PathBuf::from(env::var("FFMPEG_DIR").unwrap());
        println!(
            "cargo:rustc-link-search=native={}",
            ff.join("lib").display()
        );
    }
    println!("cargo:rustc-link-lib=avutil");
}

/// Include paths from pkg-config (libavutil for the hwcontext header; the Vulkan
/// headers usually live in /usr/include, but honor a registered vulkan.pc too).
/// PF_FFVK_VULKAN_INCLUDE prepends an explicit Vulkan-Headers include dir — for
/// cross builds and boxes without the system package.
fn linux_includes() -> Vec<PathBuf> {
    let mut includes: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = env::var("PF_FFVK_VULKAN_INCLUDE") {
        includes.push(PathBuf::from(dir));
    }
    let avutil = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libavutil")
        .expect("pkg-config: libavutil not found — install the FFmpeg dev package");
    includes.extend(avutil.include_paths);
    if let Ok(vk) = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("vulkan")
    {
        includes.extend(vk.include_paths);
    }
    includes
}

/// No pkg-config on Windows: headers come from the FFMPEG_DIR tree (the same BtbN
/// lgpl-shared tree ffmpeg-sys-next links against) plus an explicit Vulkan-Headers
/// dir — PF_FFVK_VULKAN_INCLUDE (provision-windows-punktfunk-extras.ps1 stages
/// C:\Users\Public\vulkan-headers) or an installed Vulkan SDK. Only headers are
/// needed at build time; the loader (vulkan-1.dll) is a GPU-driver component and is
/// never linked here.
fn windows_includes() -> Vec<PathBuf> {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");
    let mut includes: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = env::var("PF_FFVK_VULKAN_INCLUDE") {
        includes.push(PathBuf::from(dir));
    } else if let Ok(sdk) = env::var("VULKAN_SDK") {
        includes.push(PathBuf::from(sdk).join("Include"));
    } else {
        panic!(
            "pf-ffvk: no Vulkan headers — set PF_FFVK_VULKAN_INCLUDE to a Vulkan-Headers \
             include dir (scripts/ci/provision-windows-punktfunk-extras.ps1 stages \
             C:\\Users\\Public\\vulkan-headers\\include) or install the Vulkan SDK (VULKAN_SDK)"
        );
    }
    let ff = env::var("FFMPEG_DIR").expect(
        "pf-ffvk: FFMPEG_DIR not set — point it at the FFmpeg tree \
         (scripts/ci/provision-windows-punktfunk-extras.ps1 stages C:\\Users\\Public\\ffmpeg)",
    );
    includes.push(PathBuf::from(ff).join("include"));
    includes
}
