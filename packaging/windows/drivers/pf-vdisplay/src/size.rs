//! `.Size` for `IDD_CX_CLIENT_CONFIG` — the oracle's `IDD_STRUCTURE_SIZE!`, ported.
//!
//! IddCx structs are versioned. If the client (our headers) is NEWER than the running framework
//! (`IddClientVersionHigherThanFramework != 0`), `size_of` of our struct is too big, so the framework's
//! own `IddStructures[INDEX]` size must be used. Otherwise `size_of` is correct. `IddStructures` is null
//! until IddCx initialises, so it must ONLY be read in the `higher` branch (the framework populates it
//! exactly then). build.rs links the `iddcx>=1.3` stub that exports these symbols. Logged so we can see
//! which branch + value the box actually takes.

use wdk_sys::iddcx;

/// The correct `.Size` for `IDD_CX_CLIENT_CONFIG`, or `None` if the struct is unusable on this framework.
#[must_use]
pub fn idd_cx_client_config_size() -> Option<u32> {
    let local = core::mem::size_of::<iddcx::IDD_CX_CLIENT_CONFIG>();
    // SAFETY: BOOLEAN static, read-only.
    let higher = unsafe { (&raw const iddcx::IddClientVersionHigherThanFramework).read() } != 0;
    dbglog!("[pf-vd] cfg size: higher={higher} size_of={local}");
    if !higher {
        return u32::try_from(local).ok();
    }
    // SAFETY: read-only; the framework populates the size table exactly when `higher` is true.
    let count = unsafe { (&raw const iddcx::IddStructureCount).read() };
    let index = iddcx::_IDDSTRUCTENUM::INDEX_IDD_CX_CLIENT_CONFIG as u32;
    if index >= count {
        dbglog!("[pf-vd] cfg size: index {index} >= count {count} -> None");
        return None;
    }
    // SAFETY: `IddStructures` is the framework's size table; `index` validated `< count`.
    let table = unsafe { (&raw const iddcx::IddStructures).read() };
    let fw = unsafe { table.add(index as usize).read() };
    dbglog!("[pf-vd] cfg size: framework={fw}");
    u32::try_from(fw).ok()
}

/// The framework's expected `.Size` for the `_IDDSTRUCTENUM` struct at `index`, or `None` if the size
/// table isn't usable (index out of range / `IddStructures` not yet populated). The framework binds a
/// specific IddCx version (the INF's `UmdfExtensions`), which can be OLDER than our headers — newer
/// fields (e.g. `IDDCX_ADAPTER_CAPS::StaticDesktopReencodeFrameCount`) make `size_of` too big and
/// `IddCxAdapterInitAsync` rejects it; this returns the size the framework actually expects.
///
/// Only call once the IddCx context is initialised (e.g. from `EvtDeviceD0Entry`, after
/// `IddCxDeviceInitialize`) — `IddStructures` may be null before that.
#[must_use]
pub fn framework_struct_size(index: u32) -> Option<u32> {
    // SAFETY: read-only.
    let count = unsafe { (&raw const iddcx::IddStructureCount).read() };
    if index >= count {
        return None;
    }
    // SAFETY: read-only; `IddStructures` is the framework size table.
    let table = unsafe { (&raw const iddcx::IddStructures).read() };
    if table.is_null() {
        return None;
    }
    // SAFETY: `index` validated `< count`; `table` non-null.
    let size = unsafe { table.add(index as usize).read() };
    u32::try_from(size).ok()
}
