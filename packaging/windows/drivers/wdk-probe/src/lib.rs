//! Minimal UMDF2 driver — the M0/M1 toolchain + `/INTEGRITYCHECK` + IddCx-binding probe (see this
//! crate's Cargo.toml). DriverEntry → WdfDriverCreate → (EvtDeviceAdd) IddCxDeviceInitConfig →
//! WdfDeviceCreate → IddCxDeviceInitialize → IddCxAdapterInitAsync: enough to exercise the wdk-sys WDF
//! stub link AND prove the `iddcx` subset is callable + links against `IddCxStub`. Also force-links the
//! shared `pf-driver-proto` ABI crate (no_std + bytemuck) across the workspace boundary.

#![allow(non_snake_case, clippy::missing_safety_doc)]

mod iddcx_rt;
mod iddcx_surface_assert;

use wdk_sys::iddcx::{IDARG_IN_ADAPTER_INIT, IDARG_OUT_ADAPTER_INIT, IDD_CX_CLIENT_CONFIG};
use wdk_sys::{
    NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDF_DRIVER_CONFIG,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDFDEVICE, WDFDRIVER,
    call_unsafe_wdf_function_binding,
};

const STATUS_SUCCESS: NTSTATUS = 0;

/// Force `pf-driver-proto` to actually link into the driver build graph (validates the cross-workspace
/// path-dep + that the no_std bytemuck ABI crate compiles for a UMDF cdylib). `#[used]` keeps it.
#[used]
static PROTO_GUID_LO: u64 = pf_driver_proto::PF_VDISPLAY_INTERFACE_GUID_U128 as u64;

/// IddCx (stub mode) requires the driver to export the minimum IddCx framework version it needs — the
/// `#ifndef IDD_STUB` branch of `IddCxFuncEnum.h` (which normally emits it) is compiled out under
/// `IDD_STUB`, so the driver provides it. `4` matches the proven `wdf-umdf` oracle.
#[unsafe(no_mangle)]
pub static IddMinimumVersionRequired: ULONG = 4;

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    // SAFETY: zeroed then Size + the device-add callback set, per the WDF_DRIVER_CONFIG contract.
    let mut config: WDF_DRIVER_CONFIG = unsafe { core::mem::zeroed() };
    config.Size = core::mem::size_of::<WDF_DRIVER_CONFIG>() as ULONG;
    config.EvtDriverDeviceAdd = Some(evt_device_add);
    // SAFETY: driver + registry_path are the loader-provided pointers; config is valid for the call.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>()
        )
    }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    // Configure the device for IddCx BEFORE WdfDeviceCreate. The required `EvtIddCx*` callbacks are left
    // null in this probe — its purpose is to prove the `iddcx` DDIs are callable and link against
    // IddCxStub (CI), and that a self-signed IddCx driver loads + dispatches (box); the full callback
    // surface + a valid adapter come with the driver port. At runtime IddCxDeviceInitConfig will reject
    // the null callbacks, but the call site is what links IddCxStub and exercises table dispatch.
    let mut cfg: IDD_CX_CLIENT_CONFIG = unsafe { core::mem::zeroed() };
    cfg.Size = core::mem::size_of::<IDD_CX_CLIENT_CONFIG>() as ULONG;
    // SAFETY: device_init is the framework-provided init; cfg is a valid (if minimal) config.
    let _ = unsafe { iddcx_rt::IddCxDeviceInitConfig(device_init, &cfg) };

    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: device_init is the framework-provided init; attributes null; device receives the handle.
    let _ = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device
        )
    };

    // SAFETY: device is the just-created WDFDEVICE.
    let _ = unsafe { iddcx_rt::IddCxDeviceInitialize(device) };

    // SAFETY: zeroed adapter-init args — a link/dispatch reference, not a valid adapter (see above).
    let in_args: IDARG_IN_ADAPTER_INIT = unsafe { core::mem::zeroed() };
    let mut out_args: IDARG_OUT_ADAPTER_INIT = unsafe { core::mem::zeroed() };
    // SAFETY: in/out args are valid local storage for the call.
    let _ = unsafe { iddcx_rt::IddCxAdapterInitAsync(&in_args, &mut out_args) };

    STATUS_SUCCESS
}
