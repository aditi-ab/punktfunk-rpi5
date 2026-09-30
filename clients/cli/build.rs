//! Embed the Windows version-info + icon resources into `punktfunk.exe`. UAC, firewall
//! prompts and Task Manager name the exe by FileDescription, not its file name.

fn main() {
    // cfg(windows) is the HOST (skips Linux/macOS builds); CARGO_CFG_WINDOWS is the TARGET.
    #[cfg(windows)]
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let icon = "../../packaging/windows/branding/punktfunk.ico";
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            .set_icon_with_id(icon, "1")
            .set("FileDescription", "Punktfunk")
            .set("ProductName", "Punktfunk")
            .set("CompanyName", "unom")
            .compile()
            .expect("embed windows icon/version resources");
    }
}
