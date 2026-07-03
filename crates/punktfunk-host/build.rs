//! Build script: stamps the build version. NVENC deliberately needs NOTHING here — the entry
//! points (`NvEncodeAPICreateInstance` / `NvEncodeAPIGetMaxSupportedVersion`) live in
//! `nvEncodeAPI64.dll`, which only exists where the NVIDIA driver is installed, so
//! `encode/windows/nvenc.rs` resolves them at RUNTIME (`LoadLibraryExW`). The former link-time
//! import (`cargo:rustc-link-lib=nvencodeapi`) made the Windows loader kill the all-vendor host
//! binary on every AMD/Intel-only box before `main` ("nvencodeapi64.dll was not found").
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

    // Windows identity resources: the branded icon + version info. Task Manager / Explorer show a
    // process by its version-info FileDescription — without one the host appears as a bare
    // "punktfunk-host.exe" with no icon. Same winresource pattern as clients/windows and
    // punktfunk-tray (cfg(windows) = build HOST, so Linux packaging builds skip it; CARGO_CFG_WINDOWS
    // = TARGET).
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let icon = "../../packaging/windows/branding/punktfunk.ico";
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            .set_icon_with_id(icon, "1")
            .set("FileDescription", "Punktfunk Host")
            .set("ProductName", "Punktfunk")
            .compile()
            .expect("embed windows icon/version resources");
    }
}
