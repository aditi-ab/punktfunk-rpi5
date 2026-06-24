//! The IddCx client-config callbacks + the PnP `EvtDeviceD0Entry`.
//!
//! STEP 2: stubs with the correct PFN signatures (so the config wires up + the driver loads); the real
//! mode/EDID logic (STEP 4), adapter init (STEP 3), and swap-chain handoff (STEP 5) fill them in. Every
//! callback is `unsafe extern "C"` to match the wdk-sys `PFN_IDD_CX_*` types; with `panic = "abort"`
//! (workspace profile) a panic across the FFI boundary aborts rather than being UB. `query_target_info`
//! is implemented now because it gates HDR (`HIGH_COLOR_SPACE`) and the adapter (STEP 3) sets FP16.

use wdk_sys::iddcx;
use wdk_sys::{NTSTATUS, WDFDEVICE, WDFREQUEST};

use crate::{STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS};

/// PnP `EvtDeviceD0Entry` (not an IddCx config callback). Adapter creation is deferred to the first D0
/// (the adapter object is only valid after D0), not driver_add.
pub unsafe extern "C" fn device_d0_entry(
    device: WDFDEVICE,
    _previous_state: wdk_sys::WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    dbglog!("[pf-vd] device_d0_entry");
    crate::adapter::init_adapter(device)
}

/// Async completion of `IddCxAdapterInitAsync`: stash the adapter for later DDIs. STEP 4 also starts the
/// watchdog here.
pub unsafe extern "C" fn adapter_init_finished(
    adapter: iddcx::IDDCX_ADAPTER,
    _p_in: *const iddcx::IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    dbglog!("[pf-vd] adapter_init_finished");
    crate::adapter::set_adapter(adapter);
    STATUS_SUCCESS
}

/// SDR mode list for an EDID monitor. STEP 4: EDID-serial lookup + count-then-fill `IDDCX_MONITOR_MODE`.
pub unsafe extern "C" fn parse_monitor_description(
    _p_in: *const iddcx::IDARG_IN_PARSEMONITORDESCRIPTION,
    _p_out: *mut iddcx::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// HDR (`*2`) mode list — writes `IDDCX_MONITOR_MODE2` (+BitsPerComponent). Mandatory under FP16.
pub unsafe extern "C" fn parse_monitor_description2(
    _p_in: *const iddcx::IDARG_IN_PARSEMONITORDESCRIPTION2,
    _p_out: *mut iddcx::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// Only called for EDID-less monitors; ours always carry an EDID, so this stays NOT_IMPLEMENTED.
pub unsafe extern "C" fn monitor_get_default_modes(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    _p_out: *mut iddcx::IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    STATUS_NOT_IMPLEMENTED
}

/// SDR target (scan-out) modes. STEP 4: pointer-match the monitor + fill `IDDCX_TARGET_MODE`.
pub unsafe extern "C" fn monitor_query_modes(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_QUERYTARGETMODES,
    _p_out: *mut iddcx::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// HDR (`*2`) target modes — writes `IDDCX_TARGET_MODE2`. Mandatory under FP16.
pub unsafe extern "C" fn monitor_query_modes2(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_QUERYTARGETMODES2,
    _p_out: *mut iddcx::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// Diagnostic only — assign drives everything. STEP 4 logs the committed paths.
pub unsafe extern "C" fn adapter_commit_modes(
    _adapter: iddcx::IDDCX_ADAPTER,
    _p_in: *const iddcx::IDARG_IN_COMMITMODES,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// HDR (`*2`) commit over `IDDCX_PATH2`. Mandatory under FP16.
pub unsafe extern "C" fn adapter_commit_modes2(
    _adapter: iddcx::IDDCX_ADAPTER,
    _p_in: *const iddcx::IDARG_IN_COMMITMODES2,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// Report `HIGH_COLOR_SPACE` so the OS enables the HDR10 wide-gamut/PQ target. Mandatory under FP16.
pub unsafe extern "C" fn query_target_info(
    _adapter: iddcx::IDDCX_ADAPTER,
    _p_in: *mut iddcx::IDARG_IN_QUERYTARGET_INFO,
    p_out: *mut iddcx::IDARG_OUT_QUERYTARGET_INFO,
) -> NTSTATUS {
    // SAFETY: p_out is the framework's (uninitialised) out buffer; zero then set the one field we report.
    unsafe {
        core::ptr::write(p_out, core::mem::zeroed());
        (*p_out).TargetCaps = iddcx::IDDCX_TARGET_CAPS::IDDCX_TARGET_CAPS_HIGH_COLOR_SPACE;
    }
    STATUS_SUCCESS
}

/// Accept the OS's default HDR10 static metadata (the host/client own the stream's final metadata).
/// Mandatory under FP16.
pub unsafe extern "C" fn set_default_hdr_metadata(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_MONITOR_SET_DEFAULT_HDR_METADATA,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// Accept (do not apply) the gamma ramp — the client display applies its own transform. MANDATORY once
/// FP16 is set, or the OS rejects the adapter at init ("Failed to get adapter").
pub unsafe extern "C" fn set_gamma_ramp(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_SET_GAMMARAMP,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// A swap-chain was assigned to the monitor. STEP 5: spawn the `SwapChainProcessor`.
pub unsafe extern "C" fn assign_swap_chain(
    _monitor: iddcx::IDDCX_MONITOR,
    _p_in: *const iddcx::IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    STATUS_SUCCESS
}

/// The monitor went inactive. STEP 5: drop the processor (RAII joins the worker thread).
pub unsafe extern "C" fn unassign_swap_chain(_monitor: iddcx::IDDCX_MONITOR) -> NTSTATUS {
    STATUS_SUCCESS
}

/// The pf-vdisplay-proto control plane. Returns `()` and completes the request itself (matches the C
/// `EVT_IDD_CX_DEVICE_IO_CONTROL` shape). STEP 4: dispatch the proto IOCTLs; for now just complete.
pub unsafe extern "C" fn device_io_control(
    _device: WDFDEVICE,
    request: WDFREQUEST,
    _output_len: usize,
    _input_len: usize,
    ioctl_code: u32,
) {
    // SAFETY: `request` is the framework-provided WDFREQUEST; `control::dispatch` completes it exactly once.
    unsafe { crate::control::dispatch(request, ioctl_code) };
}
