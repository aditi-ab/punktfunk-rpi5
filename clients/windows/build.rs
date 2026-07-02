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

        let icon = "../../packaging/windows/branding/punktfunk.ico";
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            // Ordinal 1 — app/mod.rs loads it by this id for WM_SETICON.
            .set_icon_with_id(icon, "1")
            .compile()
            .expect("embed windows icon resource");
    }
}
