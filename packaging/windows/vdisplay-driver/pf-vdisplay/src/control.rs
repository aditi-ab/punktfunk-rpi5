//! SudoVDA-compatible IOCTL control plane (`EVT_IDD_CX_DEVICE_IO_CONTROL`). The host's
//! `vdisplay/sudovda.rs` drives this unchanged: ADD a monitor at a requested mode → `{LUID, target_id}`,
//! REMOVE by GUID, PING the watchdog, GET_VERSION/GET_WATCHDOG, SET_RENDER_ADAPTER. Struct layouts are
//! byte-identical to `Common/Include/sudovda-ioctl.h`.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use log::{error, info};
use wdf_umdf::{
    IddCxAdapterSetRenderAdapter, IddCxMonitorDeparture, WdfRequestCompleteWithInformation,
    WdfRequestRetrieveInputBuffer, WdfRequestRetrieveOutputBuffer,
};
use wdf_umdf_sys::{IDARG_IN_ADAPTERSETRENDERADAPTER, LUID, NTSTATUS, WDFDEVICE, WDFREQUEST};

use crate::context::DeviceContext;
use crate::monitor::{
    default_modes, Mode, MonitorData, MonitorObject, ADAPTER, MONITOR_MODES, NEXT_ID,
    PREFERRED_RENDER_ADAPTER, PROTOCOL_VERSION, WATCHDOG_COUNTDOWN, WATCHDOG_TIMEOUT,
};

// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, func, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0).
const fn ctl(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}
const IOCTL_ADD: u32 = ctl(0x800);
const IOCTL_REMOVE: u32 = ctl(0x801);
const IOCTL_SET_RENDER_ADAPTER: u32 = ctl(0x802);
const IOCTL_GET_WATCHDOG: u32 = ctl(0x803);
const IOCTL_PING: u32 = ctl(0x888);
const IOCTL_GET_VERSION: u32 = ctl(0x8FF);

#[repr(C)]
struct AddParams {
    width: u32,
    height: u32,
    refresh: u32,
    guid: [u8; 16],
    device_name: [u8; 14],
    serial: [u8; 14],
}
#[repr(C)]
struct AddOut {
    luid_low: u32,
    luid_high: i32,
    target_id: u32,
}
#[repr(C)]
struct RemoveParams {
    guid: [u8; 16],
}
#[repr(C)]
struct SetRenderAdapterParams {
    luid_low: u32,
    luid_high: i32,
}
#[repr(C)]
struct WatchdogOut {
    timeout: u32,
    countdown: u32,
}

fn guid_key(b: &[u8; 16]) -> u128 {
    u128::from_le_bytes(*b)
}

/// SAFETY: `request` valid; returns a pointer to the request's input buffer of at least `min` bytes.
unsafe fn input_buf(request: WDFREQUEST, min: usize) -> Option<*const u8> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let mut len: usize = 0;
    let r = unsafe { WdfRequestRetrieveInputBuffer(request, min, &mut p, &mut len) };
    if r.is_err() || p.is_null() || len < min {
        return None;
    }
    Some(p.cast::<u8>())
}

/// SAFETY: `request` valid; returns a pointer to the request's output buffer of at least `min` bytes.
unsafe fn output_buf(request: WDFREQUEST, min: usize) -> Option<*mut u8> {
    let mut p: *mut c_void = std::ptr::null_mut();
    let mut len: usize = 0;
    let r = unsafe { WdfRequestRetrieveOutputBuffer(request, min, &mut p, &mut len) };
    if r.is_err() || p.is_null() || len < min {
        return None;
    }
    Some(p.cast::<u8>())
}

/// `EVT_IDD_CX_DEVICE_IO_CONTROL` — IddCx redirects device IOCTLs here. Signature matches SudoVDA's
/// `SudoVDAIoDeviceControl(Device, Request, OutputBufferLength, InputBufferLength, IoControlCode)`.
pub extern "C-unwind" fn device_io_control(
    device: WDFDEVICE,
    request: WDFREQUEST,
    output_len: usize,
    input_len: usize,
    ioctl_code: u32,
) {
    // Reset the watchdog on any IOCTL except the watchdog query (the host PINGs to keep alive).
    if ioctl_code != IOCTL_GET_WATCHDOG {
        WATCHDOG_COUNTDOWN.store(WATCHDOG_TIMEOUT.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    let mut bytes: usize = 0;
    // SAFETY: dispatch reads/writes the request buffers it validated; `device` is the IddCx device.
    let status = unsafe {
        match ioctl_code {
            IOCTL_ADD => do_add(device, request, input_len, output_len, &mut bytes),
            IOCTL_REMOVE => do_remove(request, input_len),
            IOCTL_SET_RENDER_ADAPTER => do_set_render_adapter(request, input_len),
            IOCTL_GET_WATCHDOG => do_get_watchdog(request, output_len, &mut bytes),
            IOCTL_PING => NTSTATUS::STATUS_SUCCESS,
            IOCTL_GET_VERSION => do_get_version(request, output_len, &mut bytes),
            _ => NTSTATUS::STATUS_INVALID_DEVICE_REQUEST,
        }
    };

    // SAFETY: completing the request we were handed.
    let _ = unsafe { WdfRequestCompleteWithInformation(request, status, bytes as u64) };
}

unsafe fn do_add(
    device: WDFDEVICE,
    request: WDFREQUEST,
    input_len: usize,
    output_len: usize,
    bytes: &mut usize,
) -> NTSTATUS {
    if input_len < size_of::<AddParams>() || output_len < size_of::<AddOut>() {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    }
    let (Some(pin), Some(pout)) = (
        unsafe { input_buf(request, size_of::<AddParams>()) },
        unsafe { output_buf(request, size_of::<AddOut>()) },
    ) else {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    };
    let params = unsafe { &*pin.cast::<AddParams>() };
    let guid = guid_key(&params.guid);

    // Dedup: an existing GUID returns its LUID + target id (the host may re-ADD on reconnect).
    {
        let lock = MONITOR_MODES.lock().unwrap();
        if let Some(mon) = lock.iter().find(|m| m.guid == guid) {
            let out = AddOut {
                luid_low: mon.adapter_luid_low,
                luid_high: mon.adapter_luid_high,
                target_id: mon.target_id,
            };
            unsafe { pout.cast::<AddOut>().write_unaligned(out) };
            *bytes = size_of::<AddOut>();
            return NTSTATUS::STATUS_SUCCESS;
        }
    }

    if params.width == 0 || params.height == 0 || params.refresh == 0 {
        return NTSTATUS::STATUS_INVALID_PARAMETER;
    }

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    // Requested mode first (preferred), then fallbacks.
    let mut modes = vec![Mode {
        width: params.width,
        height: params.height,
        refresh_rates: vec![params.refresh],
    }];
    modes.extend(default_modes());
    MONITOR_MODES.lock().unwrap().push(MonitorObject {
        object: None,
        data: MonitorData { id, modes },
        guid,
        target_id: 0,
        adapter_luid_low: 0,
        adapter_luid_high: 0,
    });

    // Create the IddCx monitor via the device context (captures target id + LUID into the entry).
    let created = unsafe {
        DeviceContext::get_mut(device.cast(), |ctx| {
            if let Err(e) = ctx.create_monitor(id) {
                error!("ADD: create_monitor failed: {e:?}");
            }
        })
    };

    let lock = MONITOR_MODES.lock().unwrap();
    let mon = lock.iter().find(|m| m.data.id == id);
    if created.is_err() || mon.map_or(true, |m| m.object.is_none()) {
        drop(lock);
        MONITOR_MODES.lock().unwrap().retain(|m| m.data.id != id);
        error!("ADD: monitor {id} failed to arrive");
        return NTSTATUS::STATUS_UNSUCCESSFUL;
    }
    let mon = mon.unwrap();
    let out = AddOut {
        luid_low: mon.adapter_luid_low,
        luid_high: mon.adapter_luid_high,
        target_id: mon.target_id,
    };
    unsafe { pout.cast::<AddOut>().write_unaligned(out) };
    *bytes = size_of::<AddOut>();
    info!(
        "ADD {}x{}@{} -> target_id={} luid={:08x}:{:08x}",
        params.width, params.height, params.refresh, mon.target_id, mon.adapter_luid_high, mon.adapter_luid_low
    );
    NTSTATUS::STATUS_SUCCESS
}

unsafe fn do_remove(request: WDFREQUEST, input_len: usize) -> NTSTATUS {
    if input_len < size_of::<RemoveParams>() {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    }
    let Some(pin) = (unsafe { input_buf(request, size_of::<RemoveParams>()) }) else {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    };
    let params = unsafe { &*pin.cast::<RemoveParams>() };
    let guid = guid_key(&params.guid);

    let mut lock = MONITOR_MODES.lock().unwrap();
    if let Some(pos) = lock.iter().position(|m| m.guid == guid) {
        let mon = lock.remove(pos);
        if let Some(obj) = mon.object {
            if let Err(e) = unsafe { IddCxMonitorDeparture(obj.as_ptr()) } {
                error!("REMOVE: departure failed: {e:?}");
            }
        }
        info!("REMOVE target_id={}", mon.target_id);
        NTSTATUS::STATUS_SUCCESS
    } else {
        NTSTATUS::STATUS_NOT_FOUND
    }
}

unsafe fn do_set_render_adapter(request: WDFREQUEST, input_len: usize) -> NTSTATUS {
    if input_len < size_of::<SetRenderAdapterParams>() {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    }
    let Some(pin) = (unsafe { input_buf(request, size_of::<SetRenderAdapterParams>()) }) else {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    };
    let params = unsafe { &*pin.cast::<SetRenderAdapterParams>() };
    PREFERRED_RENDER_ADAPTER.store(
        ((params.luid_high as u32 as u64) << 32) | u64::from(params.luid_low),
        Ordering::Relaxed,
    );
    if let Some(adapter) = ADAPTER.get() {
        let in_args = IDARG_IN_ADAPTERSETRENDERADAPTER {
            PreferredRenderAdapter: LUID {
                LowPart: params.luid_low,
                HighPart: params.luid_high,
            },
        };
        if let Err(e) = unsafe { IddCxAdapterSetRenderAdapter(adapter.0.as_ptr(), &in_args) } {
            error!("SET_RENDER_ADAPTER failed: {e:?}");
        }
    }
    NTSTATUS::STATUS_SUCCESS
}

unsafe fn do_get_watchdog(request: WDFREQUEST, output_len: usize, bytes: &mut usize) -> NTSTATUS {
    if output_len < size_of::<WatchdogOut>() {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    }
    let Some(pout) = (unsafe { output_buf(request, size_of::<WatchdogOut>()) }) else {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    };
    let out = WatchdogOut {
        timeout: WATCHDOG_TIMEOUT.load(Ordering::Relaxed),
        countdown: WATCHDOG_COUNTDOWN.load(Ordering::Relaxed),
    };
    unsafe { pout.cast::<WatchdogOut>().write_unaligned(out) };
    *bytes = size_of::<WatchdogOut>();
    NTSTATUS::STATUS_SUCCESS
}

unsafe fn do_get_version(request: WDFREQUEST, output_len: usize, bytes: &mut usize) -> NTSTATUS {
    if output_len < PROTOCOL_VERSION.len() {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    }
    let Some(pout) = (unsafe { output_buf(request, PROTOCOL_VERSION.len()) }) else {
        return NTSTATUS::STATUS_BUFFER_TOO_SMALL;
    };
    unsafe { std::ptr::copy_nonoverlapping(PROTOCOL_VERSION.as_ptr(), pout, PROTOCOL_VERSION.len()) };
    *bytes = PROTOCOL_VERSION.len();
    NTSTATUS::STATUS_SUCCESS
}

/// Tear down every monitor (watchdog expiry — the host is gone). Mirrors SudoVDA's DisconnectAllMonitors.
fn disconnect_all_monitors() {
    let mut lock = MONITOR_MODES.lock().unwrap();
    if lock.is_empty() {
        return;
    }
    for mon in lock.drain(..) {
        if let Some(obj) = mon.object {
            // SAFETY: `obj` is a live IddCx monitor object.
            if let Err(e) = unsafe { IddCxMonitorDeparture(obj.as_ptr()) } {
                error!("watchdog: monitor departure failed: {e:?}");
            }
        }
    }
}

/// Start the watchdog thread (once). The host reads the timeout via GET_WATCHDOG and PINGs every
/// timeout/3; if it stops, the countdown reaches 0 and every monitor is torn down — so a crashed/gone
/// host never leaves a phantom display. Mirrors SudoVDA's RunWatchdog.
pub fn start_watchdog() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let timeout = WATCHDOG_TIMEOUT.load(Ordering::Relaxed);
    if timeout == 0 {
        return;
    }
    WATCHDOG_COUNTDOWN.store(timeout, Ordering::Relaxed);
    thread::spawn(|| loop {
        thread::sleep(Duration::from_secs(1));
        // Nothing to guard while there are no monitors.
        if MONITOR_MODES.lock().unwrap().is_empty() {
            continue;
        }
        let prev = WATCHDOG_COUNTDOWN.load(Ordering::Relaxed);
        if prev == 0 {
            continue;
        }
        // Decrement without clobbering a concurrent IOCTL reset (CAS).
        if WATCHDOG_COUNTDOWN
            .compare_exchange(prev, prev - 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
            && prev - 1 == 0
        {
            error!("watchdog expired (host stopped pinging) — tearing down all monitors");
            disconnect_all_monitors();
        }
    });
}
