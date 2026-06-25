//! The `pf-vdisplay-proto` control plane (`EvtIddCxDeviceIoControl`). The host opens the device interface
//! (`PF_VDISPLAY_INTERFACE_GUID`) and drives the low-frequency IOCTLs: GET_INFO (version handshake), PING
//! (watchdog keepalive), ADD/REMOVE/CLEAR_ALL (virtual monitors), and SET_RENDER_ADAPTER (next). Every
//! path completes the `WDFREQUEST` exactly once (the `EVT_IDD_CX_DEVICE_IO_CONTROL` shape returns `()`).

use core::sync::atomic::{AtomicU64, Ordering};

use pf_vdisplay_proto::control;
use wdk_iddcx::nt_success;
use wdk_sys::{call_unsafe_wdf_function_binding, NTSTATUS, WDFREQUEST};

use crate::{STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND, STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS};

/// The host must PING within this window or the watchdog reaps all monitors (STEP 4: the watchdog thread).
const WATCHDOG_TIMEOUT_S: u32 = 10;

/// Keepalive counter — PING bumps it; STEP 4's watchdog thread samples it to detect a gone host.
static WATCHDOG_PINGS: AtomicU64 = AtomicU64::new(0);

/// Dispatch one control IOCTL and complete the request.
///
/// # Safety
/// `request` is the framework-provided `WDFREQUEST` for an `EvtIddCxDeviceIoControl` call.
pub unsafe fn dispatch(request: WDFREQUEST, ioctl_code: u32) {
    match ioctl_code {
        control::IOCTL_GET_INFO => {
            let reply = control::InfoReply {
                protocol_version: pf_vdisplay_proto::PROTOCOL_VERSION,
                watchdog_timeout_s: WATCHDOG_TIMEOUT_S,
            };
            // SAFETY: `request` is the framework WDFREQUEST.
            unsafe { write_output_complete(request, &reply) };
        }
        control::IOCTL_PING => {
            WATCHDOG_PINGS.fetch_add(1, Ordering::Relaxed);
            complete(request, STATUS_SUCCESS);
        }
        // SAFETY: `request` is the framework WDFREQUEST.
        control::IOCTL_ADD => unsafe { add(request) },
        // SAFETY: `request` is the framework WDFREQUEST.
        control::IOCTL_REMOVE => unsafe { remove(request) },
        control::IOCTL_CLEAR_ALL => {
            crate::monitor::clear_all();
            complete(request, STATUS_SUCCESS);
        }
        // SET_RENDER_ADAPTER (hybrid-GPU render pin): STEP 4 (next).
        control::IOCTL_SET_RENDER_ADAPTER => complete(request, STATUS_NOT_IMPLEMENTED),
        _ => complete(request, STATUS_NOT_FOUND),
    }
}

/// `IOCTL_ADD`: create a virtual monitor at the requested mode → reply with the OS target id + LUID.
///
/// # Safety
/// `request` is the framework `WDFREQUEST`.
unsafe fn add(request: WDFREQUEST) {
    // SAFETY: `request` is the framework WDFREQUEST.
    let Some(req) = (unsafe { read_input::<control::AddRequest>(request) }) else {
        complete(request, STATUS_INVALID_PARAMETER);
        return;
    };
    let Some((target_id, luid_low, luid_high)) =
        crate::monitor::create_monitor(req.session_id, req.width, req.height, req.refresh_hz)
    else {
        complete(request, STATUS_NOT_FOUND);
        return;
    };
    let reply = control::AddReply {
        adapter_luid_low: luid_low,
        adapter_luid_high: luid_high,
        target_id,
        _reserved: 0,
    };
    // SAFETY: `request` is the framework WDFREQUEST.
    unsafe { write_output_complete(request, &reply) };
}

/// `IOCTL_REMOVE`: depart + drop the monitor for the given session id.
///
/// # Safety
/// `request` is the framework `WDFREQUEST`.
unsafe fn remove(request: WDFREQUEST) {
    // SAFETY: `request` is the framework WDFREQUEST.
    let Some(req) = (unsafe { read_input::<control::RemoveRequest>(request) }) else {
        complete(request, STATUS_INVALID_PARAMETER);
        return;
    };
    crate::monitor::remove_monitor(req.session_id);
    complete(request, STATUS_SUCCESS);
}

/// Read a `Copy`/`Pod` input struct from the request's input buffer (None if too small / unavailable).
///
/// # Safety
/// `request` is the framework `WDFREQUEST`.
unsafe fn read_input<T: Copy>(request: WDFREQUEST) -> Option<T> {
    let mut buf: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `request` valid; `buf`/`len` are out-params written by the framework.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveInputBuffer,
            request,
            core::mem::size_of::<T>(),
            &mut buf,
            &mut len
        )
    };
    if !nt_success(st) || buf.is_null() || len < core::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: `buf` has >= size_of::<T>() bytes; T is a Pod control struct.
    Some(unsafe { buf.cast::<T>().read_unaligned() })
}

/// Write a `Copy`/`Pod` reply to the request's output buffer + complete with its byte count.
///
/// # Safety
/// `request` is the framework `WDFREQUEST`.
unsafe fn write_output_complete<T: Copy>(request: WDFREQUEST, value: &T) {
    let mut buf: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `request` valid; `buf`/`len` are out-params written by the framework.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            core::mem::size_of::<T>(),
            &mut buf,
            &mut len
        )
    };
    if !nt_success(st) || buf.is_null() {
        complete(request, st);
        return;
    }
    // SAFETY: `buf` has >= size_of::<T>() writable bytes; T is a Pod control struct.
    unsafe { buf.cast::<T>().write_unaligned(*value) };
    complete_info(request, STATUS_SUCCESS, core::mem::size_of::<T>());
}

/// Complete a request with just a status (no output).
fn complete(request: WDFREQUEST, status: NTSTATUS) {
    // SAFETY: completing hands the framework `WDFREQUEST` back to the OS.
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
}

/// Complete a request with a status + the number of output bytes written.
fn complete_info(request: WDFREQUEST, status: NTSTATUS, info: usize) {
    // SAFETY: completing hands the framework `WDFREQUEST` back to the OS.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestCompleteWithInformation, request, status, info as u64)
    };
}
