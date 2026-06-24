//! Safe-ish typed wrappers over the wdk-sys IddCx DDIs, dispatched through the `IddFunctions` table
//! (indexed by `_IDDFUNCENUM::<Name>TableIndex`, with `IddDriverGlobals` as the implicit first arg) —
//! the same model wdk-sys uses for WDF. Graduates the proven dispatch from `wdk-probe/src/iddcx_rt.rs`
//! (M1 step 1) into the crate the pf-vdisplay driver builds on.
//!
//! STEP 0: re-export the wdk-sys `iddcx` module so the crate + the `iddcx` feature resolve in the
//! workspace. STEP 1 adds the 11 typed DDI wrappers (one fixed `(_IDDFUNCENUM index, PFN_*)` pair per
//! site) + `is_nt_error`/`other_is_error` + the inbound `PFN_IDD_CX_*` callback-type re-exports.
#![no_std]

pub use wdk_sys::iddcx;
