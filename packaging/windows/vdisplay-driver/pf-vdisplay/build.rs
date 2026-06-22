fn main() {
    // UMDF includes need the static C runtime linked.
    println!("cargo::rustc-link-lib=static=ucrt");
    println!("cargo::rerun-if-changed=build.rs");
}
