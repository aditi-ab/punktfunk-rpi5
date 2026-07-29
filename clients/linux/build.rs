//! Compile the shell's embedded assets (`data/` — the host-card OS-mark symbolic icons)
//! into a gresource bundle, registered at startup via `gio::resources_register_include!`.

fn main() {
    // Host cfg gate mirrors this crate's `#[cfg(target_os = "linux")]` modules: on any other
    // host the crate compiles to an empty stub and `glib-compile-resources` may not exist.
    #[cfg(target_os = "linux")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        glib_build_tools::compile_resources(
            &["data"],
            "data/resources.gresource.xml",
            "punktfunk-client.gresource",
        );
    }
}
