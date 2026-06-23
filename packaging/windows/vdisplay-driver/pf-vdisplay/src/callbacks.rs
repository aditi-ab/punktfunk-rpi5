use std::{
    mem::{self, MaybeUninit},
    ptr::NonNull,
};

use log::{error, info};
use wdf_umdf_sys::{
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1,
    DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1, __BindgenBitfieldUnit,
    DISPLAYCONFIG_2DREGION, DISPLAYCONFIG_RATIONAL, DISPLAYCONFIG_SCANLINE_ORDERING,
    DISPLAYCONFIG_TARGET_MODE, DISPLAYCONFIG_VIDEO_SIGNAL_INFO, IDARG_IN_ADAPTER_INIT_FINISHED,
    IDARG_IN_COMMITMODES, IDARG_IN_GETDEFAULTDESCRIPTIONMODES, IDARG_IN_PARSEMONITORDESCRIPTION,
    IDARG_IN_QUERYTARGETMODES, IDARG_IN_SETSWAPCHAIN, IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
    IDARG_OUT_PARSEMONITORDESCRIPTION, IDARG_OUT_QUERYTARGETMODES, IDDCX_ADAPTER__, IDDCX_PATH,
    IDDCX_MONITOR_MODE, IDDCX_MONITOR_MODE_ORIGIN, IDDCX_MONITOR__, IDDCX_TARGET_MODE, NTSTATUS,
    WDFDEVICE, WDF_POWER_DEVICE_STATE,
};
// IddCx 1.10 *2 DDIs (HDR-capable). For B1 we advertise SDR (8 bpc) so behaviour is unchanged; B2
// flips the bit depth + adapter flag to enable HDR.
use wdf_umdf_sys::{
    IDARG_IN_COMMITMODES2, IDARG_IN_PARSEMONITORDESCRIPTION2, IDARG_IN_QUERYTARGETMODES2,
    IDARG_IN_QUERYTARGET_INFO, IDARG_OUT_QUERYTARGET_INFO, IDDCX_BITS_PER_COMPONENT, IDDCX_MONITOR_MODE2,
    IDDCX_PATH2, IDDCX_TARGET_CAPS, IDDCX_TARGET_MODE2, IDDCX_WIRE_BITS_PER_COMPONENT,
};

use crate::{
    context::{DeviceContext, MonitorContext},
    edid::Edid,
    monitor::{AdapterObject, FlattenModes, ADAPTER, MONITOR_MODES},
};

pub extern "C-unwind" fn adapter_init_finished(
    adapter_object: *mut IDDCX_ADAPTER__,
    _p_in_args: *const IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    let Some(adapter_ptr) = NonNull::new(adapter_object) else {
        error!("Adapter ptr was null");
        return NTSTATUS::STATUS_INVALID_ADDRESS;
    };

    // store adapter object for the control plane to use
    if ADAPTER.set(AdapterObject(adapter_ptr)).is_err() {
        error!("Failed to set adapter");
        return NTSTATUS::STATUS_ADAPTER_HARDWARE_ERROR;
    }

    DeviceContext::finish_init();

    NTSTATUS::STATUS_SUCCESS
}

pub extern "C-unwind" fn device_d0_entry(
    device: WDFDEVICE,
    _previous_state: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    let status: NTSTATUS = unsafe {
        DeviceContext::get_mut(device.cast(), |context| {
            if let Err(e) = context.init_adapter() {
                error!("Failed to init adapter: {e:?}");
            }
        })
        .into()
    };

    if !status.is_success() {
        return status;
    }

    NTSTATUS::STATUS_SUCCESS
}

fn display_info(width: u32, height: u32, refresh_rate: u32) -> DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    let clock_rate = refresh_rate * (height + 4) * (height + 4) + 1000;

    DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
        pixelRate: u64::from(clock_rate),
        hSyncFreq: DISPLAYCONFIG_RATIONAL {
            Numerator: clock_rate,
            Denominator: height + 4,
        },
        vSyncFreq: DISPLAYCONFIG_RATIONAL {
            Numerator: clock_rate,
            Denominator: (height + 4) * (height + 4),
        },
        activeSize: DISPLAYCONFIG_2DREGION {
            cx: width,
            cy: height,
        },
        totalSize: DISPLAYCONFIG_2DREGION {
            cx: width + 4,
            cy: height + 4,
        },
        __bindgen_anon_1: DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1 {
            AdditionalSignalInfo: unsafe {
                mem::transmute::<
                    __BindgenBitfieldUnit<[u8; 4]>,
                    DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1,
                >(
                    DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                        255, 0, 0,
                    ),
                )
            },
        },
        scanLineOrdering:
            DISPLAYCONFIG_SCANLINE_ORDERING::DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE,
    }
}

pub extern "C-unwind" fn parse_monitor_description(
    p_in_args: *const IDARG_IN_PARSEMONITORDESCRIPTION,
    p_out_args: *mut IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    let in_args = unsafe { &*p_in_args };
    let out_args = unsafe { &mut *p_out_args };

    let Ok(monitors) = MONITOR_MODES.lock() else {
        error!("MONITOR_MODES mutex poisoned");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };

    let edid = unsafe {
        std::slice::from_raw_parts(
            in_args.MonitorDescription.pData as *const u8,
            in_args.MonitorDescription.DataSize as usize,
        )
    };

    let monitor_index = Edid::get_serial(edid);
    let Ok(monitor_index) = monitor_index else {
        error!(
            "We got an edid {} bytes long, but this is incorrect",
            edid.len()
        );
        return NTSTATUS::STATUS_INVALID_VIEW_SIZE;
    };

    let Some(monitor) = monitors.iter().find(|&m| m.data.id == monitor_index) else {
        error!("Failed to find monitor id {monitor_index}");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };

    let number_of_modes: u32 = monitor
        .data
        .modes
        .iter()
        .map(|m| u32::try_from(m.refresh_rates.len()).expect("Cannot use > u32::MAX refresh rates"))
        .sum();

    out_args.MonitorModeBufferOutputCount = number_of_modes;
    if in_args.MonitorModeBufferInputCount < number_of_modes {
        // Return success if there was no buffer, since the caller was only asking for a count of modes
        return if in_args.MonitorModeBufferInputCount > 0 {
            NTSTATUS::STATUS_BUFFER_TOO_SMALL
        } else {
            NTSTATUS::STATUS_SUCCESS
        };
    }

    let monitor_modes = unsafe {
        std::slice::from_raw_parts_mut(
            in_args
                .pMonitorModes
                .cast::<MaybeUninit<IDDCX_MONITOR_MODE>>(),
            number_of_modes as usize,
        )
    };

    for (mode, out_mode) in monitor.data.modes.flatten().zip(monitor_modes.iter_mut()) {
        out_mode.write(IDDCX_MONITOR_MODE {
            #[allow(clippy::cast_possible_truncation)]
            Size: mem::size_of::<IDDCX_MONITOR_MODE>() as u32,
            Origin: IDDCX_MONITOR_MODE_ORIGIN::IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR,
            MonitorVideoSignalInfo: display_info(mode.width, mode.height, mode.refresh_rate),
        });
    }

    // Set the preferred mode as represented in the EDID
    out_args.PreferredMonitorModeIdx = 0;

    NTSTATUS::STATUS_SUCCESS
}

pub extern "C-unwind" fn monitor_get_default_modes(
    _monitor_object: *mut IDDCX_MONITOR__,
    _p_in_args: *const IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    _p_out_args: *mut IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    info!("GET_DEFAULT_MODES called (we return NOT_IMPLEMENTED — only valid for a monitor with NO EDID)");
    NTSTATUS::STATUS_NOT_IMPLEMENTED
}

pub fn target_mode(width: u32, height: u32, refresh_rate: u32) -> IDDCX_TARGET_MODE {
    let total_size = DISPLAYCONFIG_2DREGION {
        cx: width,
        cy: height,
    };

    IDDCX_TARGET_MODE {
        #[allow(clippy::cast_possible_truncation)]
        Size: mem::size_of::<IDDCX_TARGET_MODE>() as u32,

        TargetVideoSignalInfo: DISPLAYCONFIG_TARGET_MODE {
            targetVideoSignalInfo: DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
                pixelRate: u64::from(refresh_rate) * u64::from(width) * u64::from(height),
                hSyncFreq: DISPLAYCONFIG_RATIONAL {
                    Numerator: refresh_rate * height,
                    Denominator: 1,
                },
                vSyncFreq: DISPLAYCONFIG_RATIONAL {
                    Numerator: refresh_rate,
                    Denominator: 1,
                },
                totalSize: total_size,
                activeSize: total_size,
                scanLineOrdering:
                    DISPLAYCONFIG_SCANLINE_ORDERING::DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE,
                __bindgen_anon_1: DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1 {
                    AdditionalSignalInfo: unsafe {
                        mem::transmute::<__BindgenBitfieldUnit<[u8; 4]>, DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1>(
                            DISPLAYCONFIG_VIDEO_SIGNAL_INFO__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                                255, 1, 0,
                            ),
                        )
                    },
                },
            },
        },

        ..Default::default()
    }
}

pub extern "C-unwind" fn monitor_query_modes(
    monitor_object: *mut IDDCX_MONITOR__,
    p_in_args: *const IDARG_IN_QUERYTARGETMODES,
    p_out_args: *mut IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    // find out which monitor this belongs too

    let Ok(monitors) = MONITOR_MODES.lock() else {
        error!("MONITOR_MODES mutex poisoned");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };

    // we have stored the monitor object per id, so we should be able to compare pointers
    let Some(monitor) = monitors
        .iter()
        .find(|&m| m.object.is_some_and(|p| p.as_ptr() == monitor_object))
    else {
        error!("Failed to find monitor object in cache for {monitor_object:?}");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };

    let number_of_modes = monitor
        .data
        .modes
        .iter()
        .map(|m| u32::try_from(m.refresh_rates.len()).expect("Cannot use > u32::MAX modes"))
        .sum();

    // Create a set of modes supported for frame processing and scan-out. These are typically not based on the
    // monitor's descriptor and instead are based on the static processing capability of the device. The OS will
    // report the available set of modes for a given output as the intersection of monitor modes with target modes.

    let out_args = unsafe { &mut *p_out_args };
    out_args.TargetModeBufferOutputCount = number_of_modes;

    let in_args = unsafe { &*p_in_args };

    if in_args.TargetModeBufferInputCount >= number_of_modes {
        let out_target_modes = unsafe {
            std::slice::from_raw_parts_mut(
                in_args
                    .pTargetModes
                    .cast::<MaybeUninit<IDDCX_TARGET_MODE>>(),
                number_of_modes as usize,
            )
        };

        for (mode, out_target) in monitor
            .data
            .modes
            .flatten()
            .zip(out_target_modes.iter_mut())
        {
            let target_mode = target_mode(mode.width, mode.height, mode.refresh_rate);

            out_target.write(target_mode);
        }
    }

    NTSTATUS::STATUS_SUCCESS
}

pub extern "C-unwind" fn adapter_commit_modes(
    _adapter_object: *mut IDDCX_ADAPTER__,
    p_in_args: *const IDARG_IN_COMMITMODES,
) -> NTSTATUS {
    // DIAGNOSTIC: does the OS commit an ACTIVE path for our monitor? IDDCX_PATH_FLAGS_ACTIVE = 2. If
    // no active path is ever committed, the OS never calls ASSIGN_SWAPCHAIN (the bug we're chasing).
    let in_args = unsafe { &*p_in_args };
    info!("COMMIT_MODES: path_count={}", in_args.PathCount);
    for i in 0..in_args.PathCount {
        let path: &IDDCX_PATH = unsafe { &*in_args.pPaths.add(i as usize) };
        let active = (path.Flags.0 & 2) != 0;
        info!(
            "  path[{i}] monitor={:p} flags=0x{:x} active={active}",
            path.MonitorObject, path.Flags.0
        );
    }
    NTSTATUS::STATUS_SUCCESS
}

pub extern "C-unwind" fn assign_swap_chain(
    monitor_object: *mut IDDCX_MONITOR__,
    p_in_args: *const IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    let p_in_args = unsafe { &*p_in_args };

    unsafe {
        MonitorContext::get_mut(monitor_object.cast(), |context| {
            context.assign_swap_chain(
                p_in_args.hSwapChain,
                p_in_args.RenderAdapterLuid,
                p_in_args.hNextSurfaceAvailable,
            );
        })
        .into()
    }
}

pub extern "C-unwind" fn unassign_swap_chain(monitor_object: *mut IDDCX_MONITOR__) -> NTSTATUS {
    info!("swap-chain unassigned (monitor inactive)");
    unsafe {
        MonitorContext::get_mut(monitor_object.cast(), |context| {
            context.unassign_swap_chain();
        })
        .into()
    }
}

// ===== IddCx 1.10 *2 DDIs (HDR-capable path) ============================================
// These mirror the 1.x callbacks above but advertise per-mode wire bit-depth. B1 reports SDR (8 bpc);
// B2 bumps `wire_bits()` to add 10 bpc + sets CAN_PROCESS_FP16 to actually enable HDR.

/// Wire bit-depth advertised per mode. B2: advertise BOTH 8 and 10 bpc RGB so the OS offers HDR10
/// modes (the bitfield: 8 = 0x2, 10 = 0x4).
fn wire_bits() -> IDDCX_WIRE_BITS_PER_COMPONENT {
    let rgb = IDDCX_BITS_PER_COMPONENT(
        IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_8.0
            | IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_10.0,
    );
    IDDCX_WIRE_BITS_PER_COMPONENT {
        Rgb: rgb,
        YCbCr444: IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
        YCbCr422: IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
        YCbCr420: IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
    }
}

/// 1.10 variant of [`parse_monitor_description`] — writes `IDDCX_MONITOR_MODE2` (adds bit-depth).
pub extern "C-unwind" fn parse_monitor_description2(
    p_in_args: *const IDARG_IN_PARSEMONITORDESCRIPTION2,
    p_out_args: *mut IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    let in_args = unsafe { &*p_in_args };
    let out_args = unsafe { &mut *p_out_args };

    let Ok(monitors) = MONITOR_MODES.lock() else {
        error!("MONITOR_MODES mutex poisoned");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };

    let edid = unsafe {
        std::slice::from_raw_parts(
            in_args.MonitorDescription.pData as *const u8,
            in_args.MonitorDescription.DataSize as usize,
        )
    };
    let Ok(monitor_index) = Edid::get_serial(edid) else {
        error!("bad edid ({} bytes)", edid.len());
        return NTSTATUS::STATUS_INVALID_VIEW_SIZE;
    };
    let Some(monitor) = monitors.iter().find(|&m| m.data.id == monitor_index) else {
        error!("Failed to find monitor id {monitor_index}");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };

    let number_of_modes: u32 = monitor
        .data
        .modes
        .iter()
        .map(|m| u32::try_from(m.refresh_rates.len()).expect("Cannot use > u32::MAX refresh rates"))
        .sum();

    out_args.MonitorModeBufferOutputCount = number_of_modes;
    if in_args.MonitorModeBufferInputCount < number_of_modes {
        return if in_args.MonitorModeBufferInputCount > 0 {
            NTSTATUS::STATUS_BUFFER_TOO_SMALL
        } else {
            NTSTATUS::STATUS_SUCCESS
        };
    }

    let monitor_modes = unsafe {
        std::slice::from_raw_parts_mut(
            in_args.pMonitorModes.cast::<MaybeUninit<IDDCX_MONITOR_MODE2>>(),
            number_of_modes as usize,
        )
    };
    for (mode, out_mode) in monitor.data.modes.flatten().zip(monitor_modes.iter_mut()) {
        out_mode.write(IDDCX_MONITOR_MODE2 {
            #[allow(clippy::cast_possible_truncation)]
            Size: mem::size_of::<IDDCX_MONITOR_MODE2>() as u32,
            Origin: IDDCX_MONITOR_MODE_ORIGIN::IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR,
            MonitorVideoSignalInfo: display_info(mode.width, mode.height, mode.refresh_rate),
            BitsPerComponent: wire_bits(),
        });
    }
    out_args.PreferredMonitorModeIdx = 0;
    NTSTATUS::STATUS_SUCCESS
}

fn target_mode2(width: u32, height: u32, refresh_rate: u32) -> IDDCX_TARGET_MODE2 {
    let m1 = target_mode(width, height, refresh_rate);
    IDDCX_TARGET_MODE2 {
        #[allow(clippy::cast_possible_truncation)]
        Size: mem::size_of::<IDDCX_TARGET_MODE2>() as u32,
        TargetVideoSignalInfo: m1.TargetVideoSignalInfo,
        BitsPerComponent: wire_bits(),
        ..Default::default()
    }
}

/// 1.10 variant of [`monitor_query_modes`] — writes `IDDCX_TARGET_MODE2`.
pub extern "C-unwind" fn monitor_query_modes2(
    monitor_object: *mut IDDCX_MONITOR__,
    p_in_args: *const IDARG_IN_QUERYTARGETMODES2,
    p_out_args: *mut IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    let Ok(monitors) = MONITOR_MODES.lock() else {
        error!("MONITOR_MODES mutex poisoned");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };
    let Some(monitor) = monitors
        .iter()
        .find(|&m| m.object.is_some_and(|p| p.as_ptr() == monitor_object))
    else {
        error!("Failed to find monitor object in cache for {monitor_object:?}");
        return NTSTATUS::STATUS_DRIVER_INTERNAL_ERROR;
    };

    let number_of_modes = monitor
        .data
        .modes
        .iter()
        .map(|m| u32::try_from(m.refresh_rates.len()).expect("Cannot use > u32::MAX modes"))
        .sum();

    let out_args = unsafe { &mut *p_out_args };
    out_args.TargetModeBufferOutputCount = number_of_modes;

    let in_args = unsafe { &*p_in_args };
    if in_args.TargetModeBufferInputCount >= number_of_modes {
        let out_target_modes = unsafe {
            std::slice::from_raw_parts_mut(
                in_args.pTargetModes.cast::<MaybeUninit<IDDCX_TARGET_MODE2>>(),
                number_of_modes as usize,
            )
        };
        for (mode, out_target) in monitor.data.modes.flatten().zip(out_target_modes.iter_mut()) {
            out_target.write(target_mode2(mode.width, mode.height, mode.refresh_rate));
        }
    }
    NTSTATUS::STATUS_SUCCESS
}

/// 1.10 variant of [`adapter_commit_modes`] — `IDDCX_PATH2` carries the committed wire format.
pub extern "C-unwind" fn adapter_commit_modes2(
    _adapter_object: *mut IDDCX_ADAPTER__,
    p_in_args: *const IDARG_IN_COMMITMODES2,
) -> NTSTATUS {
    let in_args = unsafe { &*p_in_args };
    info!("COMMIT_MODES2: path_count={}", in_args.PathCount);
    for i in 0..in_args.PathCount {
        let path: &IDDCX_PATH2 = unsafe { &*in_args.pPaths.add(i as usize) };
        let active = (path.Flags.0 & 2) != 0;
        info!(
            "  path2[{i}] monitor={:p} flags=0x{:x} active={active} colorspace={} rgb_bpc=0x{:x}",
            path.MonitorObject,
            path.Flags.0,
            path.WireFormatInfo.ColorSpace.0,
            path.WireFormatInfo.BitsPerComponent.Rgb.0
        );
    }
    NTSTATUS::STATUS_SUCCESS
}

/// 1.10 NEW: per-target capabilities. B2 reports `HIGH_COLOR_SPACE` so the OS enables HDR10 (transfer
/// curve + wide gamut) on this target.
pub extern "C-unwind" fn query_target_info(
    _adapter_object: *mut IDDCX_ADAPTER__,
    _p_in_args: *mut IDARG_IN_QUERYTARGET_INFO,
    p_out_args: *mut IDARG_OUT_QUERYTARGET_INFO,
) -> NTSTATUS {
    let out_args = unsafe { &mut *p_out_args };
    out_args.TargetCaps = IDDCX_TARGET_CAPS::IDDCX_TARGET_CAPS_HIGH_COLOR_SPACE;
    out_args.DitheringSupport = IDDCX_WIRE_BITS_PER_COMPONENT::default();
    NTSTATUS::STATUS_SUCCESS
}

/// 1.10 NEW (HDR): the OS hands us the default HDR10 static metadata for the monitor. B2 accepts it
/// (the host/client own the final HDR metadata for the stream); B3 will forward it to the host for the
/// HEVC mastering-display SEI. Stub keeps the OS's HDR setup happy.
pub extern "C-unwind" fn set_default_hdr_metadata(
    _monitor_object: *mut IDDCX_MONITOR__,
    _p_in_args: *const wdf_umdf_sys::IDARG_IN_MONITOR_SET_DEFAULT_HDR_METADATA,
) -> NTSTATUS {
    NTSTATUS::STATUS_SUCCESS
}

/// 1.10 HDR: the OS hands us the gamma ramp (a 3x4 colour-space matrix in HDR mode). We do NOT apply it
/// server-side — the host streams the scRGB FP16 and the CLIENT's display applies its own transform —
/// so we accept it. Wiring this is OBLIGATED once CAN_PROCESS_FP16 is set; without it the OS rejects
/// the adapter at init (`IddCxAdapterInitAsync` → "Failed to get adapter").
pub extern "C-unwind" fn set_gamma_ramp(
    _monitor_object: *mut IDDCX_MONITOR__,
    _p_in_args: *const wdf_umdf_sys::IDARG_IN_SET_GAMMARAMP,
) -> NTSTATUS {
    NTSTATUS::STATUS_SUCCESS
}
