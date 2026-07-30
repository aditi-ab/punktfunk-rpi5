//! Compile the shell's embedded assets (`data/` — the host-card OS-mark symbolic icons)
//! into a gresource bundle, registered at startup via `gio::resources_register_include!`,
//! and stamp the build version the update check compares against.

fn main() {
    // Build provenance, identical to the host's (crates/punktfunk-host/build.rs): the packaging
    // jobs set PUNKTFUNK_BUILD_VERSION to the full package version (`0.23.0~ci10250.gab12cd34`
    // on deb, `0.23.0-0.ci10250.g…` on rpm, …), a plain `cargo build` falls back to the crate
    // version. This is what `--version` prints and what `--check-update` compares against the
    // signed manifest — and the canary suffix is load-bearing there, because canary channels
    // compare CI run numbers rather than patch fields (pf_update_check::version).
    let version = std::env::var("PUNKTFUNK_BUILD_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into()));
    println!("cargo:rustc-env=PUNKTFUNK_VERSION={version}");
    println!("cargo:rerun-if-env-changed=PUNKTFUNK_BUILD_VERSION");

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
