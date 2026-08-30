//! Embed the Windows version-info + icon resources into `punktfunk-client.exe`. The icon drives
//! Explorer / Alt-Tab / the unpackaged taskbar, and `app::run` stamps it onto the WinUI window's
//! title bar via `WM_SETICON` (the MSIX taskbar/Start icons come from the package assets instead).

fn main() {
    // cfg(windows) is the HOST (skips the Linux/macOS workspace stub build); CARGO_CFG_WINDOWS
    // is the TARGET (both the x64 and the cross-compiled ARM64 Windows builds pass).
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Stage the Windows App SDK runtime bootstrap + resources.pri next to the exe
        // (framework-dependent deployment; `main` calls `windows_reactor::bootstrap()`).
        windows_reactor_setup::as_framework_dependent();

        // The Lucide icon font, into `Assets/` beside the exe — where `ms-appx:///Assets/`
        // resolves, and so where `app::lucide::FAMILY` looks for it. A shipped build gets it
        // from packaging/assets instead (pack-msix.ps1 copies that directory into the layout);
        // this is the same file, staged for a plain `cargo run`. Without it the shell draws
        // private-use boxes where every icon should be.
        let font = "packaging/assets/lucide.ttf";
        println!("cargo:rerun-if-changed={font}");
        // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out; three up is target/<profile>,
        // the directory reactor-setup stages the bootstrap into. Same derivation it uses.
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        let exe_dir = out.ancestors().nth(3).expect("target/<profile>");
        let assets = exe_dir.join("Assets");
        std::fs::create_dir_all(&assets).expect("create the staged Assets directory");
        std::fs::copy(font, assets.join("lucide.ttf")).expect("stage the Lucide icon font");

        let icon = "../../packaging/windows/branding/punktfunk.ico";
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            // Ordinal 1 — app/mod.rs loads it by this id for WM_SETICON.
            .set_icon_with_id(icon, "1")
            .compile()
            .expect("embed windows icon resource");
    }
}
