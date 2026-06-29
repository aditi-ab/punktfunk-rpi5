//! The `pf-driver-proto` control plane (`EvtIddCxDeviceIoControl`). The host opens the device interface
//! (`PF_VDISPLAY_INTERFACE_GUID`) and drives the low-frequency IOCTLs: GET_INFO (version handshake), PING
//! (watchdog keepalive), ADD/REMOVE/CLEAR_ALL (virtual monitors), and SET_RENDER_ADAPTER (next). Every
//! path completes the `WDFREQUEST` exactly once (the `EVT_IDD_CX_DEVICE_IO_CONTROL` shape returns `()`).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pf_driver_proto::control;
use wdk_iddcx::nt_success;
use wdk_sys::{NTSTATUS, WDFREQUEST, call_unsafe_wdf_function_binding};

use crate::{STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND, STATUS_SUCCESS};

/// The host must send an IOCTL within this window (it PINGs on a `timeout/3` timer) or the watchdog
/// treats it as gone and reaps every monitor. Reported to the host via [`control::IOCTL_GET_INFO`].
const WATCHDOG_TIMEOUT_S: u32 = 10;

/// Host-liveness counter — EVERY inbound IOCTL bumps it; [`start_watchdog`]'s thread samples it.
static WATCHDOG_PINGS: AtomicU64 = AtomicU64::new(0);
/// Spawns the watchdog thread exactly once (idempotent across re-entrant adapter inits).
static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);

/// Start the host-liveness watchdog (once, from `adapter_init_finished`).
///
/// Previously [`WATCHDOG_PINGS`] was bumped but NEVER sampled (no thread existed) — so a host that died
/// without a cooperative REMOVE (crash / `TerminateProcess`) left its virtual monitor + swap-chain
/// worker + pooled D3D device wedged in WUDFHost until the next host start's CLEAR_ALL, and a
/// not-restarted host left the orphan monitor in the desktop topology indefinitely
/// (`design/windows-host-rewrite.md` §2.8). This thread closes that: if no IOCTL arrives for
/// `WATCHDOG_TIMEOUT_S` while monitors exist, it departs them all.
///
/// (A WDF `EvtFileClose` on the control handle would be more immediate — the plan's preferred §3.4
/// option — but the polling watchdog matches the proven oracle and needs no IddCx file-object plumbing.)
pub fn start_watchdog() {
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let tick = Duration::from_secs(u64::from((WATCHDOG_TIMEOUT_S / 3).max(1)));
    let timeout = Duration::from_secs(u64::from(WATCHDOG_TIMEOUT_S));
    std::thread::spawn(move || {
        let mut last = WATCHDOG_PINGS.load(Ordering::Relaxed);
        let mut last_change = Instant::now();
        loop {
            std::thread::sleep(tick);
            let cur = WATCHDOG_PINGS.load(Ordering::Relaxed);
            if cur != last {
                last = cur;
                last_change = Instant::now();
                continue;
            }
            // No IOCTL since `last_change`. A live host PINGs every `timeout/3`, so this only trips once
            // the host is truly gone; only reap when there's something to reap.
            if last_change.elapsed() >= timeout && crate::monitor::has_monitors() {
                let n = crate::monitor::reap_orphaned(Duration::from_secs(3));
                if n > 0 {
                    dbglog!(
                        "[pf-vd] watchdog: no host IOCTL in {WATCHDOG_TIMEOUT_S}s — host gone, departed {n} monitor(s)"
                    );
                }
                last_change = Instant::now(); // don't re-reap every tick
            }
        }
    });
}

/// Dispatch one control IOCTL and complete the request.
///
/// # Safety
/// `request` is the framework-provided `WDFREQUEST` for an `EvtIddCxDeviceIoControl` call.
pub unsafe fn dispatch(request: WDFREQUEST, ioctl_code: u32) {
    // Every inbound IOCTL is host liveness (the host PINGs on a timer, plus ADD/REMOVE/GET_INFO/…) —
    // bump the watchdog at the top so it only fires once the host has gone truly silent. See
    // [`start_watchdog`].
    WATCHDOG_PINGS.fetch_add(1, Ordering::Relaxed);
    match ioctl_code {
        control::IOCTL_GET_INFO => {
            let reply = control::InfoReply {
                protocol_version: pf_driver_proto::PROTOCOL_VERSION,
                watchdog_timeout_s: WATCHDOG_TIMEOUT_S,
            };
            // SAFETY: `request` is the framework WDFREQUEST.
            unsafe { write_output_complete(request, &reply) };
        }
        control::IOCTL_PING => complete(request, STATUS_SUCCESS),
        // SAFETY: `request` is the framework WDFREQUEST.
        control::IOCTL_ADD => unsafe { add(request) },
        // SAFETY: `request` is the framework WDFREQUEST.
        control::IOCTL_REMOVE => unsafe { remove(request) },
        control::IOCTL_CLEAR_ALL => {
            crate::monitor::clear_all();
            complete(request, STATUS_SUCCESS);
        }
        // SAFETY: `request` is the framework WDFREQUEST.
        control::IOCTL_SET_RENDER_ADAPTER => unsafe { set_render_adapter(request) },
        _ => complete(request, STATUS_NOT_FOUND),
    }
}

/// Sanity bounds for a requested mode — generous (covers any real client) but rejects zero/absurd
/// values that would otherwise feed the EDID/mode math unchecked.
fn valid_mode(width: u32, height: u32, refresh_hz: u32) -> bool {
    (1..=16384).contains(&width)
        && (1..=16384).contains(&height)
        && (1..=1000).contains(&refresh_hz)
}

/// `IOCTL_SET_RENDER_ADAPTER`: pin the IddCx render adapter (hybrid-GPU IDD-push).
///
/// # Safety
/// `request` is the framework `WDFREQUEST`.
unsafe fn set_render_adapter(request: WDFREQUEST) {
    // SAFETY: `request` is the framework WDFREQUEST.
    let Some(req) = (unsafe { read_input::<control::SetRenderAdapterRequest>(request) }) else {
        complete(request, STATUS_INVALID_PARAMETER);
        return;
    };
    let st = crate::adapter::set_render_adapter(req.luid_low, req.luid_high);
    complete(request, st);
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
    if !valid_mode(req.width, req.height, req.refresh_hz) {
        complete(request, STATUS_INVALID_PARAMETER);
        return;
    }
    let Some((monitor_id, target_id, luid_low, luid_high)) = crate::monitor::create_monitor(
        req.session_id,
        req.width,
        req.height,
        req.refresh_hz,
        req.preferred_monitor_id,
    ) else {
        complete(request, STATUS_NOT_FOUND);
        return;
    };
    let reply = control::AddReply {
        adapter_luid_low: luid_low,
        adapter_luid_high: luid_high,
        target_id,
        resolved_monitor_id: monitor_id,
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
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            info as u64
        )
    };
}
