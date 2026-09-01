//! Minimal IddCx table-dispatch over the wdk-sys `iddcx` bindings — the M1 proof that the generated
//! IddCx surface is *callable* and links against `IddCxStub`. IddCx DDIs dispatch through the
//! `IddFunctions` table (indexed by `_IDDFUNCENUM::<Name>TableIndex`, with `IddDriverGlobals` as the
//! implicit first argument) — the same model wdk-sys already uses for WDF. This mirrors the proven
//! `wdf-umdf` oracle, but on wdk-sys's ModuleConsts enums (`_IDDFUNCENUM::<Name>TableIndex`, an `i32`
//! const — not the oracle's NewType `.0`) and plain `i32` NTSTATUS. The full DDI surface + a proper
//! `wdk-iddcx` wrapper crate land with the driver port.
#![allow(non_snake_case)]

use wdk_sys::iddcx::{
    _IDDFUNCENUM, IDARG_IN_ADAPTER_INIT, IDARG_OUT_ADAPTER_INIT, IDD_CX_CLIENT_CONFIG,
    IddDriverGlobals, IddFunctions, PFN_IDD_CX, PFN_IDDCXADAPTERINITASYNC,
    PFN_IDDCXDEVICEINITCONFIG, PFN_IDDCXDEVICEINITIALIZE, PIDD_DRIVER_GLOBALS,
};
use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, WDFDEVICE};

/// Read one typed DDI from IddCxStub's runtime-populated flexible table.
///
/// The binding declares a zero-length array, so `wrapping_add` addresses the foreign tail without
/// falsely asserting it lies inside that Rust object. Runtime assertions reject negative indices
/// and a `T` whose width differs from the table's function-pointer slots.
///
/// # Safety
/// `index` and `T` name the same DDI, and IddCxStub has populated that slot.
#[inline]
unsafe fn ddi<T: Copy>(index: i32) -> T {
    assert!(index >= 0, "negative IddCx DDI index");
    assert_eq!(
        core::mem::size_of::<T>(),
        core::mem::size_of::<PFN_IDD_CX>(),
        "IddCx DDI type has the wrong width"
    );
    let table = (&raw const IddFunctions).cast::<PFN_IDD_CX>();
    let slot = table.wrapping_add(index as usize);
    // SAFETY: the contract says IddCxStub populated this foreign tail slot with the matching `T`.
    unsafe { slot.cast::<T>().read() }
}

/// The IddCx driver globals (set by `IddCxStub`), passed as the implicit first arg to every DDI.
///
/// # Safety
/// Only valid once the driver is loaded by the IddCx runtime.
#[inline]
unsafe fn globals() -> PIDD_DRIVER_GLOBALS {
    // SAFETY: we only read the pointer value of the stub-provided global.
    unsafe { (&raw const IddDriverGlobals).read() }
}

/// # Safety
/// `device_init`/`config` must be valid per the IddCx contract; call only during `EvtDriverDeviceAdd`.
pub unsafe fn IddCxDeviceInitConfig(
    device_init: PWDFDEVICE_INIT,
    config: *const IDD_CX_CLIENT_CONFIG,
) -> NTSTATUS {
    // SAFETY: index and PFN type name the same DDI (IddCxDeviceInitConfig), and this function's
    // contract — call only during EvtDriverDeviceAdd — implies the driver is loaded and the
    // stub's table populated.
    let f: PFN_IDDCXDEVICEINITCONFIG =
        unsafe { ddi(_IDDFUNCENUM::IddCxDeviceInitConfigTableIndex) };
    // SAFETY: the driver is loaded (same premise as above), so the stub globals are set.
    let g = unsafe { globals() };
    // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
    unsafe { (f.unwrap())(g, device_init, config) }
}

/// # Safety
/// `device` must be a valid `WDFDEVICE` previously configured via [`IddCxDeviceInitConfig`].
pub unsafe fn IddCxDeviceInitialize(device: WDFDEVICE) -> NTSTATUS {
    // SAFETY: index and PFN type name the same DDI (IddCxDeviceInitialize), and this function's
    // contract — `device` came out of a WdfDeviceCreate on an IddCx-configured init — implies
    // the driver is loaded and the stub's table populated.
    let f: PFN_IDDCXDEVICEINITIALIZE =
        unsafe { ddi(_IDDFUNCENUM::IddCxDeviceInitializeTableIndex) };
    // SAFETY: the driver is loaded (same premise as above), so the stub globals are set.
    let g = unsafe { globals() };
    // SAFETY: dispatching a populated DDI with the stub globals and a caller-valid device.
    unsafe { (f.unwrap())(g, device) }
}

/// # Safety
/// `in_args`/`out_args` must be valid per the IddCx contract.
pub unsafe fn IddCxAdapterInitAsync(
    in_args: *const IDARG_IN_ADAPTER_INIT,
    out_args: *mut IDARG_OUT_ADAPTER_INIT,
) -> NTSTATUS {
    // SAFETY: index and PFN type name the same DDI (IddCxAdapterInitAsync), and IddCx DDIs are
    // only reachable from framework callbacks — i.e. with the driver loaded and the stub's
    // table populated.
    let f: PFN_IDDCXADAPTERINITASYNC =
        unsafe { ddi(_IDDFUNCENUM::IddCxAdapterInitAsyncTableIndex) };
    // SAFETY: the driver is loaded (same premise as above), so the stub globals are set.
    let g = unsafe { globals() };
    // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
    unsafe { (f.unwrap())(g, in_args, out_args) }
}
