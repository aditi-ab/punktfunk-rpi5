//! pf-vdisplay — the all-Rust UMDF IddCx virtual-display driver (M1 step-2 rewrite, on wdk-sys + the
//! owned pf-vdisplay-proto ABI). See docs/windows-host-rewrite.md §14 for the full port plan.
//!
//! STEP 0 (this commit): the workspace scaffold + the std-under-UMDF LINK GATE. DriverEntry →
//! WdfDriverCreate → (EvtDeviceAdd) WdfDeviceCreate, plus a `#[used]` probe that forces `std::thread`
//! + `OwnedHandle` to link — the SwapChainProcessor (STEP 5) depends on both, and the wdk-build UMDF
//! link settings `/NODEFAULTLIB:kernel32.lib`, so std must resolve via OneCoreUAP. If this fails to
//! link, the worker-thread/handle design needs a CreateThread shim BEFORE any callback work (port-plan
//! critique gap #9). The adapter/monitor/swapchain/IDD-push logic lands in STEP 2-6.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT,
    ULONG, WDFDEVICE, WDFDRIVER, WDF_DRIVER_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

const STATUS_SUCCESS: NTSTATUS = 0;

/// STEP-0 link gate (port-plan critique #9). Forces `std::thread` + `std::os::windows::io::OwnedHandle`
/// to be linked into the UMDF cdylib so STEP 0's `cargo build` actually proves the std surface resolves
/// under the wdk-build link settings (kernel32 is `/NODEFAULTLIB`'d → std must come via OneCoreUAP).
/// Never called; `#[used]` keeps the symbol so the linker pulls std. Also touches `wdk-iddcx` to prove
/// the pf-vdisplay → wdk-iddcx → wdk-sys/iddcx dependency graph resolves.
fn _std_link_gate() {
    use std::os::windows::io::OwnedHandle;
    let t = std::thread::spawn(|| 0u32);
    let _ = t.join();
    let _drop_owned: fn(OwnedHandle) = core::mem::drop;
    // touch the iddcx surface via wdk-iddcx (forces the feature-enabled dep graph)
    let _adapter: Option<wdk_iddcx::iddcx::IDDCX_ADAPTER> = None;
}
#[used]
static _STD_LINK_GATE: fn() = _std_link_gate;

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    // SAFETY: zeroed then Size + the device-add callback set, per the WDF_DRIVER_CONFIG contract.
    let mut config: WDF_DRIVER_CONFIG = unsafe { core::mem::zeroed() };
    config.Size = core::mem::size_of::<WDF_DRIVER_CONFIG>() as ULONG;
    config.EvtDriverDeviceAdd = Some(evt_device_add);
    // SAFETY: driver + registry_path are loader-provided; config is valid for the call.
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
