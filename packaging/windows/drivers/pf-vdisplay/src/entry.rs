//! DriverEntry + driver_add — the IddCx device bring-up (STEP 2/3). wdk-build links the UMDF
//! `WdfDriverStubUm` whose `FxDriverEntryUm` forwards to the exported `DriverEntry`. Adapter creation is
//! deferred to the first `EvtDeviceD0Entry` (STEP 3); monitors are created on demand by the control
//! plane (STEP 4). Instrumented with `dbglog!` for on-glass bring-up.

use wdk_iddcx::nt_success;
use wdk_sys::{
    call_unsafe_wdf_function_binding, iddcx, GUID, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT,
    PWDFDEVICE_INIT, ULONG, WDFDEVICE, WDFDRIVER, WDF_DRIVER_CONFIG, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS,
};

use crate::{callbacks, size, STATUS_NOT_FOUND};

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    dbglog!("[pf-vd] DriverEntry");
    // SAFETY: zeroed then Size + the device-add callback set, per the WDF_DRIVER_CONFIG contract.
    let mut config: WDF_DRIVER_CONFIG = unsafe { core::mem::zeroed() };
    config.Size = core::mem::size_of::<WDF_DRIVER_CONFIG>() as ULONG;
    config.EvtDriverDeviceAdd = Some(driver_add);
    // SAFETY: driver + registry_path are loader-provided; config is valid for the call.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>()
        )
    };
    dbglog!("[pf-vd] WdfDriverCreate -> {st:#x}");
    st
}

extern "C" fn driver_add(_driver: WDFDRIVER, mut init: PWDFDEVICE_INIT) -> NTSTATUS {
    dbglog!("[pf-vd] driver_add");
    // Defer adapter creation to the first D0 entry.
    let mut pnp: WDF_PNPPOWER_EVENT_CALLBACKS = unsafe { core::mem::zeroed() };
    pnp.Size = core::mem::size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG;
    pnp.EvtDeviceD0Entry = Some(callbacks::device_d0_entry);
    // SAFETY: init is the framework-provided device-init; pnp is valid for the call.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, init, &mut pnp);
    }

    // Build + size the IddCx client config (versioned size) and wire the 14 callbacks.
    let Some(cfg_size) = size::idd_cx_client_config_size() else {
        dbglog!("[pf-vd] config size unavailable");
        return STATUS_NOT_FOUND;
    };
    let mut cfg: iddcx::IDD_CX_CLIENT_CONFIG = unsafe { core::mem::zeroed() };
    cfg.Size = cfg_size;
    cfg.EvtIddCxAdapterInitFinished = Some(callbacks::adapter_init_finished);
    cfg.EvtIddCxParseMonitorDescription = Some(callbacks::parse_monitor_description);
    cfg.EvtIddCxMonitorGetDefaultDescriptionModes = Some(callbacks::monitor_get_default_modes);
    cfg.EvtIddCxMonitorQueryTargetModes = Some(callbacks::monitor_query_modes);
    cfg.EvtIddCxAdapterCommitModes = Some(callbacks::adapter_commit_modes);
    cfg.EvtIddCxParseMonitorDescription2 = Some(callbacks::parse_monitor_description2);
    cfg.EvtIddCxMonitorQueryTargetModes2 = Some(callbacks::monitor_query_modes2);
    cfg.EvtIddCxAdapterCommitModes2 = Some(callbacks::adapter_commit_modes2);
    cfg.EvtIddCxAdapterQueryTargetInfo = Some(callbacks::query_target_info);
    cfg.EvtIddCxMonitorSetDefaultHdrMetaData = Some(callbacks::set_default_hdr_metadata);
    cfg.EvtIddCxMonitorSetGammaRamp = Some(callbacks::set_gamma_ramp);
    cfg.EvtIddCxMonitorAssignSwapChain = Some(callbacks::assign_swap_chain);
    cfg.EvtIddCxMonitorUnassignSwapChain = Some(callbacks::unassign_swap_chain);
    cfg.EvtIddCxDeviceIoControl = Some(callbacks::device_io_control);

    // SAFETY: init is the framework device-init; cfg is fully populated + sized. (Links IddCxStub.)
    let status = unsafe { wdk_iddcx::IddCxDeviceInitConfig(init, &cfg) };
    dbglog!("[pf-vd] IddCxDeviceInitConfig -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: init configured above; no context attributes yet (STEP 3 adds DeviceContext + cleanup).
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device
        )
    };
    dbglog!("[pf-vd] WdfDeviceCreate -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    // Expose the owned pf-vdisplay control interface (the host opens this GUID; STEP 4 wires the host
    // side in lockstep). NOT SudoVDA's GUID.
    let (d1, d2, d3, d4) = pf_vdisplay_proto::interface_guid_fields();
    let guid = GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    };
    // SAFETY: device is the just-created WDFDEVICE; guid lives for the call; no reference string.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &guid,
            core::ptr::null()
        )
    };
    dbglog!("[pf-vd] WdfDeviceCreateDeviceInterface -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    // SAFETY: device is the just-created WDFDEVICE.
    let status = unsafe { wdk_iddcx::IddCxDeviceInitialize(device) };
    dbglog!("[pf-vd] IddCxDeviceInitialize -> {status:#x}");
    status
}
