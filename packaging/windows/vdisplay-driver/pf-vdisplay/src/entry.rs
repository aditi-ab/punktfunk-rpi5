//! Driver entry + WDF device-add. Adapted from virtual-display-rs (its event-log/boot-retry logger
//! dance is replaced by the `OutputDebugString` logger in `logger.rs`).

use log::{error, info};
use wdf_umdf::{
    IddCxDeviceInitConfig, IddCxDeviceInitialize, WdfDeviceCreate, WdfDeviceCreateDeviceInterface,
    WdfDeviceInitSetPnpPowerEventCallbacks, WdfDriverCreate,
};
use wdf_umdf_sys::{
    GUID, IDD_CX_CLIENT_CONFIG, NTSTATUS, WDFDEVICE_INIT, WDFDRIVER__, WDFOBJECT, WDF_DRIVER_CONFIG,
    WDF_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS, _DRIVER_OBJECT, _UNICODE_STRING,
};

use crate::callbacks::{
    adapter_commit_modes, adapter_init_finished, assign_swap_chain, device_d0_entry,
    monitor_get_default_modes, monitor_query_modes, parse_monitor_description, unassign_swap_chain,
};
use crate::context::DeviceContext;
use crate::control::device_io_control;

// SudoVDA control-interface GUID — the host opens this to drive the ADD/REMOVE/PING IOCTLs.
// {e5bcc234-1e0c-418a-a0d4-ef8b7501414d}
const SUVDA_INTERFACE_GUID: GUID = GUID {
    Data1: 0xe5bc_c234,
    Data2: 0x1e0c,
    Data3: 0x418a,
    Data4: [0xa0, 0xd4, 0xef, 0x8b, 0x75, 0x01, 0x41, 0x4d],
};

/// Driver entry point (called by the framework via `FxDriverEntryUm`).
#[no_mangle]
extern "C-unwind" fn DriverEntry(
    driver_object: *mut _DRIVER_OBJECT,
    registry_path: *mut _UNICODE_STRING,
) -> NTSTATUS {
    crate::logger::init();
    crate::panic::set_hook();
    info!("pf-vdisplay v{} starting", env!("CARGO_PKG_VERSION"));

    let mut attributes = WDF_OBJECT_ATTRIBUTES::init();
    let mut config = WDF_DRIVER_CONFIG::init(Some(driver_add));

    unsafe {
        WdfDriverCreate(
            driver_object,
            registry_path,
            Some(&mut attributes),
            &mut config,
            None,
        )
    }
    .into()
}

extern "C-unwind" fn driver_add(
    _driver: *mut WDFDRIVER__,
    mut init: *mut WDFDEVICE_INIT,
) -> NTSTATUS {
    let mut callbacks = WDF_PNPPOWER_EVENT_CALLBACKS::init();
    callbacks.EvtDeviceD0Entry = Some(device_d0_entry);

    unsafe {
        _ = WdfDeviceInitSetPnpPowerEventCallbacks(init, &mut callbacks);
    }

    let Some(mut config) = IDD_CX_CLIENT_CONFIG::init() else {
        error!("Failed to create IDD_CX_CLIENT_CONFIG");
        return NTSTATUS::STATUS_NOT_FOUND;
    };

    config.EvtIddCxAdapterInitFinished = Some(adapter_init_finished);
    config.EvtIddCxParseMonitorDescription = Some(parse_monitor_description);
    config.EvtIddCxMonitorGetDefaultDescriptionModes = Some(monitor_get_default_modes);
    config.EvtIddCxMonitorQueryTargetModes = Some(monitor_query_modes);
    config.EvtIddCxAdapterCommitModes = Some(adapter_commit_modes);
    config.EvtIddCxMonitorAssignSwapChain = Some(assign_swap_chain);
    config.EvtIddCxMonitorUnassignSwapChain = Some(unassign_swap_chain);
    // IddCx redirects device IOCTLs to this callback — our SudoVDA-compatible control plane.
    config.EvtIddCxDeviceIoControl = Some(device_io_control);

    let init_data = unsafe { &mut *init };
    let status = unsafe { IddCxDeviceInitConfig(init_data, &config) };
    if let Err(e) = status {
        error!("Failed to init iddcx config: {e:?}");
        return e.into();
    }

    let mut attributes =
        WDF_OBJECT_ATTRIBUTES::init_context_type(unsafe { DeviceContext::get_type_info() });
    attributes.EvtCleanupCallback = Some(event_cleanup);

    let mut device = std::ptr::null_mut();
    let status = unsafe { WdfDeviceCreate(&mut init, Some(&mut attributes), &mut device) };
    if let Err(e) = status {
        error!("Failed to create device: {e:?}");
        return e.into();
    }

    // Register the SudoVDA control interface so the host can open it + send the control IOCTLs.
    let status =
        unsafe { WdfDeviceCreateDeviceInterface(device, &SUVDA_INTERFACE_GUID, std::ptr::null()) };
    if let Err(e) = status {
        error!("Failed to create control device interface: {e:?}");
        return e.into();
    }

    let status = unsafe { IddCxDeviceInitialize(device) };
    if let Err(e) = status {
        error!("Failed to init iddcx device: {e:?}");
        return e.into();
    }

    let context = DeviceContext::new(device);
    unsafe { context.init(device as WDFOBJECT).into() }
}

unsafe extern "C-unwind" fn event_cleanup(wdf_object: WDFOBJECT) {
    _ = unsafe { DeviceContext::drop(wdf_object) };
}
