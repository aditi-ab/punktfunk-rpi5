//! The IddCx client-config callbacks + the PnP `EvtDeviceD0Entry`.
//!
//! The mode/EDID logic (STEP 4), adapter init (STEP 3), and swap-chain handoff (STEP 5) are wired in; the
//! `*2`/HDR-metadata/gamma callbacks remain stubs (STEP 7). Every callback is `unsafe extern "C"` to match
//! the wdk-sys `PFN_IDD_CX_*` types; a panic unwinding across that `extern "C"` boundary aborts the process
//! (Rust >= 1.81 default) rather than being UB. (The swap-chain WORKER is a plain `thread::spawn`, so a
//! panic there only unwinds + ends that thread — it must not panic.) `query_target_info` is implemented
//! because it gates HDR (`HIGH_COLOR_SPACE`) and the adapter (STEP 3) sets FP16.

use wdk_sys::iddcx;
use wdk_sys::{NTSTATUS, WDFDEVICE, WDFOBJECT, WDFREQUEST, call_unsafe_wdf_function_binding};

use crate::{
    STATUS_BUFFER_TOO_SMALL, STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND, STATUS_NOT_IMPLEMENTED,
    STATUS_SUCCESS,
};

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
    crate::control::start_watchdog();
    STATUS_SUCCESS
}

/// SDR mode list for an EDID monitor: EDID-serial lookup → count-then-fill `IDDCX_MONITOR_MODE`.
pub unsafe extern "C" fn parse_monitor_description(
    p_in: *const iddcx::IDARG_IN_PARSEMONITORDESCRIPTION,
    p_out: *mut iddcx::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    // SAFETY: framework-provided in/out args, valid for the call.
    let in_args = unsafe { &*p_in };
    let out_args = unsafe { &mut *p_out };
    // SAFETY: the framework supplies a valid EDID buffer of `DataSize` bytes.
    let edid = unsafe {
        core::slice::from_raw_parts(
            in_args.MonitorDescription.pData.cast::<u8>(),
            in_args.MonitorDescription.DataSize as usize,
        )
    };
    let Ok(id) = crate::edid::Edid::get_serial(edid) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Some(modes) = crate::monitor::modes_for_id(id) else {
        return STATUS_NOT_FOUND;
    };
    let count = crate::monitor::flatten(&modes).count() as u32;
    out_args.MonitorModeBufferOutputCount = count;
    if in_args.MonitorModeBufferInputCount < count {
        // A zero input count is a count-only probe (success); a non-zero too-small buffer is an error.
        return if in_args.MonitorModeBufferInputCount > 0 {
            STATUS_BUFFER_TOO_SMALL
        } else {
            STATUS_SUCCESS
        };
    }
    // SAFETY: `pMonitorModes` points to >= `count` IDDCX_MONITOR_MODE entries (validated above).
    let out = unsafe { core::slice::from_raw_parts_mut(in_args.pMonitorModes, count as usize) };
    for (item, slot) in crate::monitor::flatten(&modes).zip(out.iter_mut()) {
        // SAFETY: building a C POD — the all-zero bit pattern is a valid uninitialized IDDCX_MONITOR_MODE;
        // the required `.Size` (+ origin / signal info) are set immediately below.
        let mut mode: iddcx::IDDCX_MONITOR_MODE = unsafe { core::mem::zeroed() };
        mode.Size = core::mem::size_of::<iddcx::IDDCX_MONITOR_MODE>() as u32;
        mode.Origin = iddcx::IDDCX_MONITOR_MODE_ORIGIN::IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;
        mode.MonitorVideoSignalInfo =
            crate::monitor::display_info(item.width, item.height, item.refresh_rate);
        *slot = mode;
    }
    out_args.PreferredMonitorModeIdx = 0;
    STATUS_SUCCESS
}

/// HDR (`*2`) mode list — writes `IDDCX_MONITOR_MODE2` (+BitsPerComponent). Mandatory under FP16. Mirrors
/// the v1 `parse_monitor_description` exactly (EDID-serial lookup → count-then-fill, same
/// BUFFER_TOO_SMALL/SUCCESS logic), but emits `IDDCX_MONITOR_MODE2` with the per-mode wire bit-depth so the
/// OS offers HDR10 modes. `IDARG_OUT_PARSEMONITORDESCRIPTION` is the SAME out struct as v1 (shared by the C
/// header); only the in-args / mode struct are the `*2` variants.
pub unsafe extern "C" fn parse_monitor_description2(
    p_in: *const iddcx::IDARG_IN_PARSEMONITORDESCRIPTION2,
    p_out: *mut iddcx::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    // SAFETY: framework-provided in/out args, valid for the call.
    let in_args = unsafe { &*p_in };
    let out_args = unsafe { &mut *p_out };
    // SAFETY: the framework supplies a valid EDID buffer of `DataSize` bytes.
    let edid = unsafe {
        core::slice::from_raw_parts(
            in_args.MonitorDescription.pData.cast::<u8>(),
            in_args.MonitorDescription.DataSize as usize,
        )
    };
    let Ok(id) = crate::edid::Edid::get_serial(edid) else {
        return STATUS_INVALID_PARAMETER;
    };
    let Some(modes) = crate::monitor::modes_for_id(id) else {
        return STATUS_NOT_FOUND;
    };
    let count = crate::monitor::flatten(&modes).count() as u32;
    out_args.MonitorModeBufferOutputCount = count;
    if in_args.MonitorModeBufferInputCount < count {
        // A zero input count is a count-only probe (success); a non-zero too-small buffer is an error.
        return if in_args.MonitorModeBufferInputCount > 0 {
            STATUS_BUFFER_TOO_SMALL
        } else {
            STATUS_SUCCESS
        };
    }
    // SAFETY: `pMonitorModes` points to >= `count` IDDCX_MONITOR_MODE2 entries (validated above).
    let out = unsafe { core::slice::from_raw_parts_mut(in_args.pMonitorModes, count as usize) };
    for (item, slot) in crate::monitor::flatten(&modes).zip(out.iter_mut()) {
        // SAFETY: building a C POD — the all-zero bit pattern is a valid uninitialized IDDCX_MONITOR_MODE2;
        // the required `.Size` (+ origin / signal info / bit depth) are set immediately below.
        let mut mode: iddcx::IDDCX_MONITOR_MODE2 = unsafe { core::mem::zeroed() };
        mode.Size = core::mem::size_of::<iddcx::IDDCX_MONITOR_MODE2>() as u32;
        mode.Origin = iddcx::IDDCX_MONITOR_MODE_ORIGIN::IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;
        mode.MonitorVideoSignalInfo =
            crate::monitor::display_info(item.width, item.height, item.refresh_rate);
        mode.BitsPerComponent = crate::monitor::wire_bits();
        *slot = mode;
    }
    out_args.PreferredMonitorModeIdx = 0;
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

/// SDR target (scan-out) modes: pointer-match the monitor → fill `IDDCX_TARGET_MODE`.
pub unsafe extern "C" fn monitor_query_modes(
    monitor: iddcx::IDDCX_MONITOR,
    p_in: *const iddcx::IDARG_IN_QUERYTARGETMODES,
    p_out: *mut iddcx::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    // SAFETY: framework-provided in/out args, valid for the call.
    let in_args = unsafe { &*p_in };
    let out_args = unsafe { &mut *p_out };
    let Some(modes) = crate::monitor::modes_for_object(monitor) else {
        return STATUS_NOT_FOUND;
    };
    let count = crate::monitor::flatten(&modes).count() as u32;
    out_args.TargetModeBufferOutputCount = count;
    if in_args.TargetModeBufferInputCount >= count {
        // SAFETY: `pTargetModes` points to >= `count` IDDCX_TARGET_MODE entries.
        let out = unsafe { core::slice::from_raw_parts_mut(in_args.pTargetModes, count as usize) };
        for (item, slot) in crate::monitor::flatten(&modes).zip(out.iter_mut()) {
            *slot = crate::monitor::target_mode(item.width, item.height, item.refresh_rate);
        }
    }
    STATUS_SUCCESS
}

/// HDR (`*2`) target modes — writes `IDDCX_TARGET_MODE2`. Mandatory under FP16. Mirrors the v1
/// `monitor_query_modes` exactly (pointer-match the monitor → count → fill), but emits `IDDCX_TARGET_MODE2`
/// (with per-mode wire bit-depth) via `monitor::target_mode2`. `IDARG_OUT_QUERYTARGETMODES` is the SAME out
/// struct as v1.
pub unsafe extern "C" fn monitor_query_modes2(
    monitor: iddcx::IDDCX_MONITOR,
    p_in: *const iddcx::IDARG_IN_QUERYTARGETMODES2,
    p_out: *mut iddcx::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    // SAFETY: framework-provided in/out args, valid for the call.
    let in_args = unsafe { &*p_in };
    let out_args = unsafe { &mut *p_out };
    let Some(modes) = crate::monitor::modes_for_object(monitor) else {
        return STATUS_NOT_FOUND;
    };
    let count = crate::monitor::flatten(&modes).count() as u32;
    out_args.TargetModeBufferOutputCount = count;
    if in_args.TargetModeBufferInputCount >= count {
        // SAFETY: `pTargetModes` points to >= `count` IDDCX_TARGET_MODE2 entries.
        let out = unsafe { core::slice::from_raw_parts_mut(in_args.pTargetModes, count as usize) };
        for (item, slot) in crate::monitor::flatten(&modes).zip(out.iter_mut()) {
            *slot = crate::monitor::target_mode2(item.width, item.height, item.refresh_rate);
        }
    }
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

/// A swap-chain was assigned to the monitor. STEP 5: spawn the `SwapChainProcessor` that drains it (so
/// the monitor is a usable display). Always returns `STATUS_SUCCESS` — on D3D-init failure we delete the
/// swap-chain so the OS makes a fresh one and re-assigns (the oracle pattern).
pub unsafe extern "C" fn assign_swap_chain(
    monitor: iddcx::IDDCX_MONITOR,
    p_in: *const iddcx::IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    // SAFETY: framework-provided in args, valid for the call.
    let in_args = unsafe { &*p_in };
    let swap_chain = in_args.hSwapChain;
    let render_adapter = in_args.RenderAdapterLuid;
    let new_frame_event = in_args.hNextSurfaceAvailable;

    // wdk-sys LUID → windows-crate LUID (identical { LowPart: u32, HighPart: i32 } layout). The render
    // adapter is the GPU the OS picked to render this virtual monitor; the pooled D3D device is keyed by
    // it (relevant on a hybrid iGPU+dGPU box).
    let luid = windows::Win32::Foundation::LUID {
        LowPart: render_adapter.LowPart,
        HighPart: render_adapter.HighPart,
    };
    dbglog!(
        "[pf-vd] assign_swap_chain: OS render adapter LUID = {:08x}:{:08x}",
        render_adapter.HighPart,
        render_adapter.LowPart
    );

    // FIRST drop any existing processor on this monitor (RAII-joins its worker), OUTSIDE the lock.
    drop(crate::monitor::take_swap_chain_processor(monitor));

    // The OS target id (stamped on the monitor at creation, after IddCxMonitorArrival) keys the
    // per-monitor objects STEP 6's host opens. 0 (default) if the monitor isn't found.
    let target_id = crate::monitor::target_id_for_object(monitor).unwrap_or(0);

    if let Some(device) = crate::direct_3d_device::pooled_device(luid) {
        let mut processor = crate::swap_chain_processor::SwapChainProcessor::new();
        // STEP 6: the publisher reports this render LUID into the host header so the host detects a
        // render-adapter mismatch (it created the ring textures on its own GPU). `luid` is the OS-picked
        // render adapter built above.
        processor.run(
            swap_chain,
            device,
            new_frame_event,
            target_id,
            luid.LowPart,
            luid.HighPart,
        );
        // Install on the monitor; drop any processor it replaced (a race lost above) OUTSIDE the lock.
        drop(crate::monitor::set_swap_chain_processor(monitor, processor));
    } else {
        // D3D init failed: delete the swap-chain so the OS generates a fresh one + retries.
        dbglog!(
            "[pf-vd] assign_swap_chain: pooled Direct3DDevice unavailable — deleting swap-chain for OS retry"
        );
        // SAFETY: `swap_chain` is the framework-provided IddCx swap-chain handle.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, swap_chain as WDFOBJECT);
        }
    }
    STATUS_SUCCESS
}

/// The monitor went inactive. STEP 5: drop the processor (RAII joins the worker thread, which deletes the
/// swap-chain object before returning).
pub unsafe extern "C" fn unassign_swap_chain(monitor: iddcx::IDDCX_MONITOR) -> NTSTATUS {
    // Take + drop OUTSIDE any lock (the take releases `MONITOR_MODES` before the join).
    let had = crate::monitor::take_swap_chain_processor(monitor);
    dbglog!(
        "[pf-vd] unassign_swap_chain — dropped live processor: {}",
        had.is_some()
    );
    drop(had);
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
