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
    let mut pnp: WDF_PNPPOWER_EVENT_CALLBACKS = unsafe { core::mem::zeroed() };
    pnp.Size = core::mem::size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG;
    pnp.EvtDeviceD0Entry = Some(callbacks::device_d0_entry);
    // SAFETY: init is the framework-provided device-init; pnp is valid for the call.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, init, &mut pnp);
    }

    // Build the IddCx client config and wire the SDR callbacks. `.Size` = size_of (1.10 structs, 1.10 fw).
    let mut cfg: iddcx::IDD_CX_CLIENT_CONFIG = unsafe { core::mem::zeroed() };
    cfg.Size = core::mem::size_of::<iddcx::IDD_CX_CLIENT_CONFIG>() as u32;
    cfg.EvtIddCxAdapterInitFinished = Some(callbacks::adapter_init_finished);
    cfg.EvtIddCxParseMonitorDescription = Some(callbacks::parse_monitor_description);
    cfg.EvtIddCxMonitorGetDefaultDescriptionModes = Some(callbacks::monitor_get_default_modes);
    cfg.EvtIddCxMonitorQueryTargetModes = Some(callbacks::monitor_query_modes);
    cfg.EvtIddCxAdapterCommitModes = Some(callbacks::adapter_commit_modes);
    // SDR config — matches the working upstream virtual-display-rs. The *2 / gamma / HDR-metadata /
    // query-target-info callbacks are FP16-OBLIGATED: registering them while caps declare no FP16 makes the
    // framework's adapter Validate reject (ddivalidation.cpp:797 -> INVALID_PARAMETER). They return with
    // FP16 caps under a higher UmdfExtensions binding in STEP 7 (HDR).
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
        call_unsafe_wdf_function_binding!(WdfDeviceCreateDeviceInterface, device, &guid, core::ptr::null())
    };
    dbglog!("[pf-vd] WdfDeviceCreateDeviceInterface -> {status:#x}");
    status
}
