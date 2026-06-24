//! Versioned IddCx struct sizing — the oracle's `IDD_STRUCTURE_SIZE!` ported to wdk-sys.
//!
//! IddCx structs are versioned: if the running framework is OLDER than the (1.10) headers we built
//! against, our locally-compiled struct may be LARGER than the framework understands, so `.Size` must
//! come from the framework's own size table (`IddStructures[INDEX_<struct>]`), not `size_of`. `None`
//! means the struct is unusable on this framework. When the framework is at least our version,
//! `size_of` is correct. (wdk-sys uses ModuleConsts: `_IDDSTRUCTENUM::INDEX_*`, not the oracle's
//! NewType `.0`.)

use wdk_sys::iddcx;

/// Correct `.Size` for `IDD_CX_CLIENT_CONFIG`, or `None` if it can't be used on this framework.
#[must_use]
pub fn idd_cx_client_config_size() -> Option<u32> {
    // SAFETY: read-only access to the stub-provided framework globals.
    let higher = unsafe { (&raw const iddcx::IddClientVersionHigherThanFramework).read() } != 0;
    if !higher {
        return u32::try_from(core::mem::size_of::<iddcx::IDD_CX_CLIENT_CONFIG>()).ok();
    }
    // SAFETY: read-only.
    let count = unsafe { (&raw const iddcx::IddStructureCount).read() };
    let index = iddcx::_IDDSTRUCTENUM::INDEX_IDD_CX_CLIENT_CONFIG as u32;
    if index >= count {
        return None; // struct cannot be used on this (older) framework
    }
    // SAFETY: `IddStructures` is the framework's size table; `index` is validated `< count`.
    let table = unsafe { (&raw const iddcx::IddStructures).read() };
    let size = unsafe { table.add(index as usize).read() };
    u32::try_from(size).ok()
}
