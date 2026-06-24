//! Minimal IddCx table-dispatch over the wdk-sys `iddcx` bindings — the M1 proof that the generated
//! IddCx surface is *callable* and links against `IddCxStub`. IddCx DDIs dispatch through the
//! `IddFunctions` table (indexed by `_IDDFUNCENUM::<Name>TableIndex`, with `IddDriverGlobals` as the
//! implicit first argument) — the same model wdk-sys already uses for WDF. This mirrors the proven
//! `wdf-umdf` oracle, but on wdk-sys's ModuleConsts enums (`_IDDFUNCENUM::<Name>TableIndex`, an `i32`
//! const — not the oracle's NewType `.0`) and plain `i32` NTSTATUS. The full DDI surface + a proper
//! `wdk-iddcx` wrapper crate land with the driver port.
#![allow(non_snake_case)]

use wdk_sys::iddcx::{
    IDARG_IN_ADAPTER_INIT, IDARG_OUT_ADAPTER_INIT, IDD_CX_CLIENT_CONFIG, IddDriverGlobals,
    IddFunctions, PFN_IDDCXADAPTERINITASYNC, PFN_IDDCXDEVICEINITCONFIG, PFN_IDDCXDEVICEINITIALIZE,
    PFN_IDD_CX, PIDD_DRIVER_GLOBALS, _IDDFUNCENUM,
};
use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, WDFDEVICE};

/// Read the IddCx DDI at `index` from the stub-provided `IddFunctions` table and reinterpret it as the
/// concrete `PFN_*` type. `IddFunctions` is a flexible array (`[PFN_IDD_CX; 0]`) whose real entries are
/// populated by `IddCxStub` once the driver is loaded.
///
/// # Safety
/// `index` and `T` must be the matching `_IDDFUNCENUM` index and `PFN_*` type for the same DDI, and the
/// table must be populated (true after the driver is loaded by the IddCx runtime).
#[inline]
unsafe fn ddi<T: Copy>(index: i32) -> T {
    let table = (&raw const IddFunctions).cast::<PFN_IDD_CX>();
    // SAFETY: `index` is a valid IddCx table slot; the slot holds a `PFN_*` whose layout is `T`.
    let slot = unsafe { table.add(index as usize) };
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
    let f: PFN_IDDCXDEVICEINITCONFIG =
        unsafe { ddi(_IDDFUNCENUM::IddCxDeviceInitConfigTableIndex) };
    let g = unsafe { globals() };
    // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
    unsafe { (f.unwrap())(g, device_init, config) }
}

/// # Safety
/// `device` must be a valid `WDFDEVICE` previously configured via [`IddCxDeviceInitConfig`].
pub unsafe fn IddCxDeviceInitialize(device: WDFDEVICE) -> NTSTATUS {
    let f: PFN_IDDCXDEVICEINITIALIZE =
        unsafe { ddi(_IDDFUNCENUM::IddCxDeviceInitializeTableIndex) };
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
    let f: PFN_IDDCXADAPTERINITASYNC =
        unsafe { ddi(_IDDFUNCENUM::IddCxAdapterInitAsyncTableIndex) };
    let g = unsafe { globals() };
    // SAFETY: dispatching a populated DDI with the stub globals and caller-valid args.
    unsafe { (f.unwrap())(g, in_args, out_args) }
}
