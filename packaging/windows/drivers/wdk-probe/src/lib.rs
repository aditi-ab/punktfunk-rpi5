//! Minimal UMDF2 driver — the M0/M1 toolchain + `/INTEGRITYCHECK` probe (see this crate's Cargo.toml).
//! DriverEntry → WdfDriverCreate → (EvtDeviceAdd) WdfDeviceCreate: enough to exercise the wdk-sys WDF
//! stub link without any device logic. Also force-links the shared `pf-vdisplay-proto` ABI crate to
//! prove it compiles + links into a driver (no_std + bytemuck) across the workspace boundary.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT,
    ULONG, WDFDEVICE, WDFDRIVER, WDF_DRIVER_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

const STATUS_SUCCESS: NTSTATUS = 0;

/// Force `pf-vdisplay-proto` to actually link into the driver build graph (validates the cross-workspace
/// path-dep + that the no_std bytemuck ABI crate compiles for a UMDF cdylib). `#[used]` keeps it.
#[used]
static PROTO_GUID_LO: u64 = pf_vdisplay_proto::PF_VDISPLAY_INTERFACE_GUID_U128 as u64;

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
    STATUS_SUCCESS
}
