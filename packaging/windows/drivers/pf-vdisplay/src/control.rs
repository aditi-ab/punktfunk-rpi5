//! The `pf-vdisplay-proto` control plane (`EvtIddCxDeviceIoControl`). The host opens the device interface
//! (`PF_VDISPLAY_INTERFACE_GUID`) and drives the low-frequency IOCTLs: GET_INFO (version handshake),
//! PING (watchdog keepalive), and — STEP 4 (next) — ADD/REMOVE/SET_RENDER_ADAPTER/CLEAR_ALL for virtual
//! monitors. Every path completes the `WDFREQUEST` exactly once (the `EVT_IDD_CX_DEVICE_IO_CONTROL` shape
//! returns `()`).

use core::sync::atomic::{AtomicU64, Ordering};

use pf_vdisplay_proto::control;
use wdk_iddcx::nt_success;
use wdk_sys::{call_unsafe_wdf_function_binding, NTSTATUS, WDFREQUEST};

use crate::{STATUS_NOT_FOUND, STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS};

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
        control::IOCTL_GET_INFO => unsafe { get_info(request) },
        control::IOCTL_PING => {
            WATCHDOG_PINGS.fetch_add(1, Ordering::Relaxed);
            complete(request, STATUS_SUCCESS);
        }
        // STEP 4 (next): ADD -> create_monitor, REMOVE, SET_RENDER_ADAPTER, CLEAR_ALL.
        control::IOCTL_ADD
        | control::IOCTL_REMOVE
        | control::IOCTL_SET_RENDER_ADAPTER
        | control::IOCTL_CLEAR_ALL => complete(request, STATUS_NOT_IMPLEMENTED),
        _ => complete(request, STATUS_NOT_FOUND),
    }
}

/// `IOCTL_GET_INFO`: write [`control::InfoReply`] (protocol version + watchdog timeout). The host asserts
/// `protocol_version == PROTOCOL_VERSION` and fails loudly on a mismatch.
///
/// # Safety
/// `request` is the framework `WDFREQUEST`.
unsafe fn get_info(request: WDFREQUEST) {
    let reply = control::InfoReply {
        protocol_version: pf_vdisplay_proto::PROTOCOL_VERSION,
        watchdog_timeout_s: WATCHDOG_TIMEOUT_S,
    };
    let mut buf: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `request` is valid; `buf`/`len` are out-params written by the framework.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            core::mem::size_of::<control::InfoReply>(),
            &mut buf,
            &mut len
        )
    };
    if !nt_success(st) || buf.is_null() {
        complete(request, st);
        return;
    }
    // SAFETY: `buf` has >= size_of::<InfoReply>() writable bytes (validated above); InfoReply is Pod.
    unsafe { buf.cast::<control::InfoReply>().write_unaligned(reply) };
    complete_info(request, STATUS_SUCCESS, core::mem::size_of::<control::InfoReply>());
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
