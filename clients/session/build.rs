//! Embed the Windows version-info + icon resources into `punktfunk-session.exe`. The
//! icon drives Explorer, and `pf-presenter`'s `win32::stamp_window_icon` loads it by
//! ordinal 1 onto the SDL window's title bar / taskbar / Alt-Tab (SDL's own window-class
//! icon is the generic default).

fn main() {
    // cfg(windows) is the HOST (skips Linux/macOS builds of this cross-platform binary);
    // CARGO_CFG_WINDOWS is the TARGET (x64 and cross-compiled ARM64 both pass).
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let icon = "../../packaging/windows/branding/punktfunk.ico";
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            // Ordinal 1 — pf-presenter's win32.rs loads it by this id for WM_SETICON.
            .set_icon_with_id(icon, "1")
            // The version-info strings Windows surfaces for the bare exe — UAC prompts,
            // Task Manager and Properties→Details all show FileDescription (without one
            // the exe appears as its raw filename).
            .set("FileDescription", "Punktfunk Session")
            .set("ProductName", "Punktfunk")
            .compile()
            .expect("embed windows icon resource");
    }
}
