//! Generate `include/punktfunk_core.h` from the `extern "C"` surface.
//!
//! cbindgen failure is a warning, not a hard error, so the crate still builds without the
//! full toolchain. The header is checked in.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/abi.rs");
    println!("cargo:rerun-if-changed=src/config.rs");
    println!("cargo:rerun-if-changed=src/input.rs");
    println!("cargo:rerun-if-changed=src/error.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    let crate_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    // Workspace `include/`, two levels above this crate — not `OUT_DIR`.
    let out = PathBuf::from(&crate_dir)
        .join("..")
        .join("..")
        .join("include")
        .join("punktfunk_core.h");

    match cbindgen::generate(&crate_dir) {
        Ok(bindings) => {
            bindings.write_to_file(&out);
            println!("cargo:warning=punktfunk-core: wrote {}", out.display());
        }
        Err(e) => {
            println!("cargo:warning=punktfunk-core: cbindgen failed ({e}); header not regenerated");
        }
    }
}
