//! `cargo run -p gen-headers`: regenerate `include/punktfunk_core.h` and
//! `include/punktfunk_console.h` from each crate's `cbindgen.toml`. A parse failure
//! exits non-zero and leaves the checked-in header as it was.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// (crate dir, header) pairs, relative to the workspace root.
const HEADERS: [(&str, &str); 2] = [
    ("crates/client/punktfunk-ffi", "include/punktfunk_core.h"),
    ("clients/apple/native", "include/punktfunk_console.h"),
];

/// The workspace root. `cargo run` names this checkout at run time. A target dir shared between
/// checkouts can hand one checkout's build to another, so the compiled-in path is only the fallback.
fn root() -> PathBuf {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR");
    let manifest = manifest
        .as_deref()
        .map_or(Path::new(env!("CARGO_MANIFEST_DIR")), Path::new);
    manifest.join("../..")
}

fn main() -> ExitCode {
    let root = root();
    let mut failed = false;
    for (crate_dir, header) in HEADERS {
        match cbindgen::generate(root.join(crate_dir)) {
            Ok(bindings) => {
                let changed = bindings.write_to_file(root.join(header));
                println!(
                    "{header}: {}",
                    if changed { "written" } else { "unchanged" }
                );
            }
            Err(e) => {
                eprintln!("{header}: cbindgen {crate_dir}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::{root, HEADERS};

    /// FNV-1a over the header's code: comments, blank lines and the version define drop out.
    fn code_hash(header: &str) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for line in header.lines() {
            // Block comments are single-line here; one that spans lines would enter the hash.
            let line = line.split("//").next().unwrap_or("");
            let line = line.split("/*").next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with("#define PUNKTFUNK_ABI_VERSION ") {
                continue;
            }
            for b in line.bytes().chain([b'\n']) {
                hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
        }
        hash
    }

    /// The C header and `ABI_VERSION` move together. The header is generated as `main` does, so
    /// this fails before the stale-header gate does.
    #[test]
    fn abi_header_hash() {
        let bindings = cbindgen::generate(root().join(HEADERS[0].0)).expect("generate the header");
        let mut header = Vec::new();
        bindings.write(&mut header);
        let got = code_hash(&String::from_utf8(header).expect("header is utf-8"));
        assert_eq!(
            got,
            punktfunk_core::ABI_HEADER_HASH,
            "the C header changed (hash {got:#018x}). In punktfunk-core/src/lib.rs bump \
             ABI_VERSION and set ABI_HEADER_HASH to this value, then update abi_version_is_pinned \
             in punktfunk-ffi: one change"
        );
    }
}
