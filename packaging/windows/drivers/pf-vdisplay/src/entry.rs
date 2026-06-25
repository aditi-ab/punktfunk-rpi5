//! DriverEntry + driver_add — the IddCx device bring-up (STEP 2/3). wdk-build links the UMDF
//! `WdfDriverStubUm` whose `FxDriverEntryUm` forwards to the exported `DriverEntry`. Adapter creation is
//! deferred to the first `EvtDeviceD0Entry` (STEP 3); monitors are created on demand by the control
//! plane (STEP 4). Instrumented with `dbglog!` for on-glass bring-up.

use wdk_iddcx::nt_success;
use wdk_sys::{
    GUID, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDF_DRIVER_CONFIG,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS, WDFDEVICE, WDFDRIVER,
    call_unsafe_wdf_function_binding, iddcx,
};

use crate::callbacks;

/// A WDF device context, attached to the WDFDEVICE at WdfDeviceCreate. The working virtual-display-rs +
/// oracle both create the device with a context-typed `DeviceContext` (we previously passed
/// WDF_NO_OBJECT_ATTRIBUTES). `WDF_OBJECT_CONTEXT_TYPE_INFO` holds raw pointers (Sync wrapper for the
/// `static`); `UniqueType` self-references per `WDF_DECLARE_CONTEXT_TYPE`.
#[repr(C)]
struct DeviceContext {
    _device: WDFDEVICE,
}
#[repr(transparent)]
struct DevCtxInfo(wdk_sys::WDF_OBJECT_CONTEXT_TYPE_INFO);
// SAFETY: immutable 'static type metadata; the inner raw pointers are 'static and never written.
unsafe impl Sync for DevCtxInfo {}
static DEVICE_CTX: DevCtxInfo = DevCtxInfo(wdk_sys::WDF_OBJECT_CONTEXT_TYPE_INFO {
    Size: core::mem::size_of::<wdk_sys::WDF_OBJECT_CONTEXT_TYPE_INFO>() as u32,
    ContextName: c"PfVdDeviceCtx".as_ptr().cast(),
    ContextSize: core::mem::size_of::<DeviceContext>(),
    UniqueType: &DEVICE_CTX.0,
    EvtDriverGetUniqueContextType: None,
});

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
    // SAFETY: building a C POD — the all-zero bit pattern is a valid uninitialized
    // WDF_PNPPOWER_EVENT_CALLBACKS; the required `.Size` (+ the D0-entry callback) are set immediately below.
    let mut pnp: WDF_PNPPOWER_EVENT_CALLBACKS = unsafe { core::mem::zeroed() };
    pnp.Size = core::mem::size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG;
    pnp.EvtDeviceD0Entry = Some(callbacks::device_d0_entry);
    // SAFETY: init is the framework-provided device-init; pnp is valid for the call.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, init, &mut pnp);
    }

    // Build the IddCx client config and wire the SDR callbacks. `.Size` = size_of (1.10 structs, 1.10 fw).
    // SAFETY: building a C POD — the all-zero bit pattern is a valid uninitialized IDD_CX_CLIENT_CONFIG;
    // the required `.Size` (+ the IddCx client callbacks) are set immediately below.
    let mut cfg: iddcx::IDD_CX_CLIENT_CONFIG = unsafe { core::mem::zeroed() };
    cfg.Size = core::mem::size_of::<iddcx::IDD_CX_CLIENT_CONFIG>() as u32;
    cfg.EvtIddCxAdapterInitFinished = Some(callbacks::adapter_init_finished);
    cfg.EvtIddCxParseMonitorDescription = Some(callbacks::parse_monitor_description);
    cfg.EvtIddCxMonitorGetDefaultDescriptionModes = Some(callbacks::monitor_get_default_modes);
    cfg.EvtIddCxMonitorQueryTargetModes = Some(callbacks::monitor_query_modes);
    cfg.EvtIddCxAdapterCommitModes = Some(callbacks::adapter_commit_modes);
    // STEP 7 (HDR): the *2 mode DDIs + the gamma/HDR-metadata/query-target-info callbacks. The adapter
    // caps now set CAN_PROCESS_FP16 (adapter.rs), which OBLIGATES this whole set — without them the OS
    // rejects the adapter at init ("Failed to get adapter"). The proven oracle (entry.rs) registers the *2
    // variants ALONGSIDE the v1 callbacks above (NOT instead of them) — the OS prefers the *2 on IddCx
    // 1.10 and falls back to v1 down-level — so we replicate exactly: keep both. The framework no longer
    // rejects the *2 set because the FP16 cap is now present (the only reason STEP 3 had to drop them).
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
    // Attach a device context type (like the working virtual-display-rs/oracle), not WDF_NO_OBJECT_ATTRIBUTES.
    // SAFETY: building a C POD — the all-zero bit pattern is a valid uninitialized WDF_OBJECT_ATTRIBUTES;
    // the required `.Size` (+ execution/sync scope + context type) are set immediately below.
    let mut dev_attr: wdk_sys::WDF_OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    dev_attr.Size = core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32;
    dev_attr.ExecutionLevel = wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent;
    dev_attr.SynchronizationScope =
        wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent;
    dev_attr.ContextTypeInfo = &DEVICE_CTX.0;
    // SAFETY: init configured above; dev_attr is a valid context-typed attributes block.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceCreate, &mut init, &mut dev_attr, &mut device)
    };
    dbglog!("[pf-vd] WdfDeviceCreate -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    // SAFETY: device is the just-created WDFDEVICE.
    let status = unsafe { wdk_iddcx::IddCxDeviceInitialize(device) };
    dbglog!("[pf-vd] IddCxDeviceInitialize -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    // Expose the owned pf-vdisplay control interface: the host opens this GUID and drives the proto control
    // plane (IOCTL_ADD/REMOVE/PING/…) which arrives at EvtIddCxDeviceIoControl. NOT SudoVDA's GUID. (The
    // upstream uses a socket instead, so it has no interface; ours is IOCTL-based.)
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
    status
}
