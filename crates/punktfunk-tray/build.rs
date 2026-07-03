//! Embed the Windows version-info + icon resources into `punktfunk-tray.exe`: ordinal 1 is the
//! exe/file icon, ordinals 2–6 are the status-variant tray icons `src/win.rs` loads by id
//! (running / stopped / error / streaming / degraded). Same winresource pattern as
//! `clients/windows/build.rs`.

fn main() {
    // cfg(windows) is the HOST (skips the Linux/macOS workspace stub build); CARGO_CFG_WINDOWS
    // is the TARGET (mirrors the Windows client's build.rs).
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let branding = "../../packaging/windows/branding";
        let icons = [
            (format!("{branding}/punktfunk.ico"), "1"),
            (format!("{branding}/punktfunk-tray-running.ico"), "2"),
            (format!("{branding}/punktfunk-tray-stopped.ico"), "3"),
            (format!("{branding}/punktfunk-tray-error.ico"), "4"),
            (format!("{branding}/punktfunk-tray-streaming.ico"), "5"),
            (format!("{branding}/punktfunk-tray-degraded.ico"), "6"),
        ];
        let mut res = winresource::WindowsResource::new();
        for (path, id) in &icons {
            println!("cargo:rerun-if-changed={path}");
            res.set_icon_with_id(path, id);
        }
        res.compile().expect("embed windows icon resources");
    }
}
