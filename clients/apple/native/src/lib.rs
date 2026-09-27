//! The Apple client's one static library: the punktfunk C ABI, linked whole, plus the
//! console's (`punktfunk_console_*`). Swift imports both as `PunktfunkCore`.

pub use punktfunk_ffi;

#[cfg(target_vendor = "apple")]
mod console;
