fn main() {
    // cfg(windows) is the HOST (skips the Linux/macOS stub build); CARGO_CFG_WINDOWS is the
    // TARGET — the same double gate clients/windows/build.rs uses.
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Self-contained deployment: stage the full WinAppSDK runtime next to the exe. An
        // installer cannot assume the runtime it is about to install, so unlike the client
        // (framework-dependent + bootstrap()) this ships its own copy (S1's verdict). This
        // call also embeds reactor's activatable-class manifest via /MANIFEST:EMBED +
        // /MANIFESTINPUT — the self-contained marker S1 left the elevation question open on.
        windows_reactor_setup::as_self_contained();

        // The S1-deferred manifest answer: link.exe merges every /MANIFESTINPUT into the one
        // embedded manifest, so requireAdministrator rides in as a second input instead of
        // patching reactor's. Per-bin on purpose — the client artifact's bin (M4) stays
        // unelevated. MSVC spelling only, matching the shipped toolchain; a gnu/llvm build
        // would need the -Wl, prefix reactor-setup itself uses.
        let manifest =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/elevation.manifest");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!(
            "cargo:rustc-link-arg-bin=punktfunk-setup-win=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
