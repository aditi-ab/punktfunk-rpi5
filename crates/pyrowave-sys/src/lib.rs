//! Raw FFI bindings to the vendored PyroWave C API (`pyrowave.h`).
//!
//! Empty on targets other than Linux/Windows — see build.rs. The safe wrapper
//! lives with its consumer (`punktfunk-host`'s encoder backend, and later the
//! Rust clients' decoder backend); this crate is bindings only.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
// Bindgen output for a C API: u128 layout warnings and the like are upstream's concern.
#![allow(improper_ctypes)]

#[cfg(any(target_os = "linux", target_os = "windows"))]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod tests {
    use super::*;

    /// Link sanity: statically linked archives resolve and the vendored pin is
    /// the API version we designed against. No GPU or Vulkan loader required —
    /// the version query touches no device state.
    #[test]
    fn api_version_matches_vendored_pin() {
        let (mut major, mut minor, mut patch) = (0u32, 0u32, 0u32);
        unsafe { pyrowave_get_api_version(&mut major, &mut minor, &mut patch) };
        assert_eq!((major, minor, patch), (0, 4, 0), "vendored pyrowave API version moved — re-check the §4.2 protocol coupling before bumping");
    }
}
