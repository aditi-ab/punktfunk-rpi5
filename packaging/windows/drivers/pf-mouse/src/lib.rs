// punktfunk virtual HID mouse — UMDF2 HID minidriver (absolute pointer).
//
// Why it exists: with NO pointing device present (a headless streaming host — no dongle), win32k
// reports the cursor as absent (`SM_MOUSEPRESENT` = 0) and DWM never composites a cursor into the
// pf-vdisplay frame, so the streamed desktop has an invisible pointer even though `SendInput`
// moves it. This driver keeps a resident HID mouse devnode alive for the host service's lifetime,
// which makes Windows always consider a pointer present and draw the cursor — the industry-standard
// fix (what Sunshine/Parsec-class virtual-input drivers achieve). Injection stays `SendInput`;
// the report path below is exercised by `punktfunk-host vmouse-spike` (validation) and is the
// future higher-fidelity injection route.
//
// Structure is pf-gamepad minus the identity zoo: one fixed HID identity (PF:MO, an obviously
// virtual VID/PID no software matches on), one 8-byte input report (5 buttons + absolute 15-bit
// X/Y + wheel + AC-pan), no feature/output reports. The host channel is the **sealed pad channel**
// (design/gamepad-channel-sealing.md) verbatim — mailbox `Global\pfmouse-boot-<i>`, unnamed
// `pf_driver_proto::mouse::MouseShm` DATA section — so the whole handshake + shared-memory surface
// lives in `pf_umdf_util` (the audited unsafe layer) and this crate's logic is 100% SAFE Rust; the
// only `unsafe` here is the unavoidable WDF setup FFI, each with a `// SAFETY:` proof.
//
// Report delivery is EVENT-DRIVEN like a real mouse: the timer completes a pended READ_REPORT only
// when the host bumped `in_seq` — an idle section generates no HID traffic (a constant report
// stream would read as user activity to the OS: idle timers, display sleep).

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]
// Every remaining `unsafe {}` (all WDF setup FFI) must carry a `// SAFETY:` proof.
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use pf_driver_proto::gamepad::ChannelProof;
use pf_driver_proto::mouse::{
    MOUSE_PID, MOUSE_REPORT_ID, MOUSE_REPORT_LEN, MOUSE_VER, MOUSE_VID, MouseShm,
};
use pf_umdf_util::channel::{ChannelClient, ChannelConfig};
use pf_umdf_util::nt_success;
use pf_umdf_util::wdf::{self, Request};
use wdk_sys::{
    NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDF_DRIVER_CONFIG,
    WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
    WDF_TIMER_CONFIG, WDFDEVICE, WDFDRIVER, WDFQUEUE, WDFQUEUE__, WDFREQUEST, WDFTIMER,
    call_unsafe_wdf_function_binding, windows::OutputDebugStringA,
};

// ---- NTSTATUS values ----
const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002u32 as NTSTATUS;

// ---- HID minidriver IOCTLs: CTL_CODE(FILE_DEVICE_KEYBOARD=0x0b, id, METHOD_NEITHER=3, ANY) ----
const fn hid_ctl(id: u32) -> u32 {
    (0x0000_000b << 16) | (id << 2) | 3
}
const IOCTL_HID_GET_DEVICE_DESCRIPTOR: u32 = hid_ctl(0);
const IOCTL_HID_GET_REPORT_DESCRIPTOR: u32 = hid_ctl(1);
const IOCTL_HID_READ_REPORT: u32 = hid_ctl(2);
const IOCTL_HID_WRITE_REPORT: u32 = hid_ctl(3);
const IOCTL_HID_GET_DEVICE_ATTRIBUTES: u32 = hid_ctl(9);
const IOCTL_HID_GET_STRING: u32 = hid_ctl(4);
const IOCTL_UMDF_HID_SET_OUTPUT_REPORT: u32 = hid_ctl(22);
const IOCTL_UMDF_HID_GET_INPUT_REPORT: u32 = hid_ctl(23);

// ---- WDF enum values ----
const WdfIoQueueDispatchParallel: i32 = 2;
const WdfIoQueueDispatchManual: i32 = 3;
const WdfUseDefault: i32 = 2; // WDF_TRI_STATE
const WdfExecutionLevelInheritFromParent: i32 = 1; // WDF_EXECUTION_LEVEL
const WdfSynchronizationScopeInheritFromParent: i32 = 1; // WDF_SYNCHRONIZATION_SCOPE

// HID report descriptor (80 bytes): one application collection (Generic Desktop / Mouse), report
// id 0x01 — 5 buttons, ABSOLUTE 15-bit X/Y (logical 0..=32767), relative wheel + AC-pan. Absolute
// axes so a future report-driven injection maps 1:1 onto the desktop, and so the OS treats the
// device as a pointer that never "drifts"; presence (not fidelity) is this driver's job today.
#[rustfmt::skip]
static MOUSE_RDESC: [u8; 80] = [
    0x05, 0x01,        // Usage Page (Generic Desktop)
    0x09, 0x02,        // Usage (Mouse)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x01,        //   Report ID (1)
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x05, 0x09,        //     Usage Page (Button)
    0x19, 0x01,        //     Usage Minimum (1)
    0x29, 0x05,        //     Usage Maximum (5)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x05,        //     Report Count (5)
    0x81, 0x02,        //     Input (Data,Var,Abs) — buttons 1..5
    0x75, 0x03,        //     Report Size (3)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x03,        //     Input (Const) — pad
    0x05, 0x01,        //     Usage Page (Generic Desktop)
    0x09, 0x30,        //     Usage (X)
    0x09, 0x31,        //     Usage (Y)
    0x15, 0x00,        //     Logical Minimum (0)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x02,        //     Input (Data,Var,Abs) — absolute X/Y
    0x09, 0x38,        //     Usage (Wheel)
    0x15, 0x81,        //     Logical Minimum (-127)
    0x25, 0x7F,        //     Logical Maximum (127)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x06,        //     Input (Data,Var,Rel) — wheel
    0x05, 0x0C,        //     Usage Page (Consumer)
    0x0A, 0x38, 0x02,  //     Usage (AC Pan)
    0x15, 0x81,        //     Logical Minimum (-127)
    0x25, 0x7F,        //     Logical Maximum (127)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x06,        //     Input (Data,Var,Rel) — horizontal wheel
    0xC0,              //   End Collection
    0xC0,              // End Collection
];

// HID descriptor (9 bytes, packed): len, type=0x21, bcdHID=0x0100, country=0, numDesc=1, then
// {reportType=0x22, wReportLength = 80 (0x0050)}.
static HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x50, 0x00];

// HID_DEVICE_ATTRIBUTES (32 bytes): Size(u32)=32, VendorID, ProductID, VersionNumber, Reserved[11].
fn hid_attrs() -> [u8; 32] {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&32u32.to_le_bytes());
    a[4..6].copy_from_slice(&MOUSE_VID.to_le_bytes());
    a[6..8].copy_from_slice(&MOUSE_PID.to_le_bytes());
    a[8..10].copy_from_slice(&MOUSE_VER.to_le_bytes());
    a
}

/// A report that answers a client's GET_INPUT_REPORT query before the host published anything:
/// id + all-zero state. Never fed into the input stream (READ_REPORT completes only on a fresh
/// host publish), so it cannot warp the cursor to (0,0).
const NEUTRAL_REPORT: [u8; MOUSE_REPORT_LEN] = {
    let mut r = [0u8; MOUSE_REPORT_LEN];
    r[0] = MOUSE_REPORT_ID;
    r
};

static MANUAL_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// The latest host-published report (kept for GET_INPUT_REPORT queries).
static INPUT_REPORT: std::sync::Mutex<[u8; MOUSE_REPORT_LEN]> =
    std::sync::Mutex::new(NEUTRAL_REPORT);
/// The last `in_seq` a READ_REPORT was completed for — the event-driven gate. NOT advanced when no
/// read is pended (the next tick retries), so a publish is never dropped while a reader exists.
static DELIVERED_SEQ: AtomicU32 = AtomicU32::new(0);

// ---- the sealed host channel: layouts + offsets from pf_driver_proto (drift = compile error) ----
const SHM_MAGIC: u32 = pf_driver_proto::mouse::MOUSE_MAGIC; // "PFMO"
const SHM_SIZE: usize = core::mem::size_of::<MouseShm>();
const GAMEPAD_PROTO_VERSION: u32 = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION;

// MouseShm field offsets (the driver reads report + in_seq, writes the health marks).
const OFF_IN_SEQ: usize = core::mem::offset_of!(MouseShm, in_seq);
const OFF_REPORT: usize = core::mem::offset_of!(MouseShm, report);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(MouseShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(MouseShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(MouseShm, pad_index);

/// The sealed-channel client (`ProcessSharingDisabled` gives the mouse its own WUDFHost, so this
/// static is per-device). The handshake/adoption/validation state machine lives in `pf_umdf_util`.
static CHANNEL: ChannelClient = ChannelClient::new();

/// This device's channel config (magic/size/index offset + our logger).
fn channel_cfg() -> ChannelConfig {
    ChannelConfig {
        tag: "pf-mouse",
        boot_name_prefix: "Global\\pfmouse-boot-",
        data_magic: SHM_MAGIC,
        data_size: SHM_SIZE,
        min_data_size: SHM_SIZE, // layout never grew — no fallback size
        pad_index_off: OFF_PAD_INDEX,
        log,
    }
}

/// Whether the bring-up file log is enabled (resolved once). OPT-IN — debug builds, or the
/// `PFMOUSE_DEBUG_LOG` (system-wide) env var — the same policy as the pad drivers (audit §4.4):
/// a RELEASE driver never writes it. DebugView can't see the UMDF host across session 0, so the
/// file stays the bring-up diagnostic when enabled.
fn file_log_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(debug_assertions) || std::env::var_os("PFMOUSE_DEBUG_LOG").is_some())
}

/// Process-lifetime append handle to the bring-up log, opened ONCE and shared via a `Mutex`
/// (pf-vdisplay's pattern) — no per-line open/close.
fn file_appender() -> Option<&'static std::sync::Mutex<std::fs::File>> {
    use std::sync::OnceLock;
    static APPENDER: OnceLock<Option<std::sync::Mutex<std::fs::File>>> = OnceLock::new();
    APPENDER
        .get_or_init(|| {
            if !file_log_enabled() {
                return None;
            }
            // Write to the WUDFHost's own (LocalService) temp dir — NOT world-writable/readable
            // `C:\Users\Public`, where any local reader gets the diagnostics and a non-admin can
            // pre-create the path as a hard link to redirect this LocalService appender's writes
            // (security-review 2026-07-17; pf-xusb/pf-gamepad/pf-vdisplay were moved then, this
            // driver was missed — security-review 2026-07-28). Opt-in/debug only.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::env::temp_dir().join("pfmouse-driver.log"))
                .ok()
                .map(std::sync::Mutex::new)
        })
        .as_ref()
}

fn log(s: &str) {
    // Gated as a whole on [`file_log_enabled`]: `OutputDebugStringA` used to fire unconditionally
    // — a syscall + CString alloc per logged event in a RELEASE driver, on paths that run per
    // IOCTL. Debug builds and the env-var opt-in keep the full debug-string + file tee.
    if !file_log_enabled() {
        return;
    }
    if let Ok(c) = std::ffi::CString::new(s) {
        // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
        unsafe { OutputDebugStringA(c.as_ptr().cast()) };
    }
    use std::io::Write;
    if let Some(m) = file_appender()
        && let Ok(mut f) = m.lock()
    {
        let _ = writeln!(f, "{s}");
    }
}
// The `file_log_enabled()` pre-check skips the `format!` alloc too when logging is off.
macro_rules! dbglog { ($($a:tt)*) => { if file_log_enabled() { log(&format!($($a)*)) } } }

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    log("[pf-mouse] DriverEntry");
    // SAFETY: zeroed WDF_DRIVER_CONFIG is a valid all-null config; we then set Size + the callback.
    let mut config: WDF_DRIVER_CONFIG = unsafe { core::mem::zeroed() };
    config.Size = core::mem::size_of::<WDF_DRIVER_CONFIG>() as ULONG;
    config.EvtDriverDeviceAdd = Some(evt_device_add);

    // SAFETY: all pointers valid; driver/registry_path provided by the loader.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>()
        )
    }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    log("[pf-mouse] EvtDeviceAdd");

    // Mark as a filter (HID minidriver sits below mshidumdf.sys).
    // SAFETY: device_init is provided by the framework and non-null.
    unsafe { call_unsafe_wdf_function_binding!(WdfFdoInitSetFilter, device_init) };

    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: device_init valid; attributes allowed null; device receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device
        )
    };
    if !nt_success(st) {
        dbglog!("[pf-mouse] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let shm_idx = unsafe { wdf::query_location_index(device) };
    CHANNEL.set_index(shm_idx);
    dbglog!("[pf-mouse] shm index = {shm_idx}");

    // Default parallel queue handling all IOCTLs.
    // SAFETY: zeroed config then fields set; Size matches the struct.
    let mut qcfg: WDF_IO_QUEUE_CONFIG = unsafe { core::mem::zeroed() };
    qcfg.Size = core::mem::size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG;
    qcfg.DispatchType = WdfIoQueueDispatchParallel;
    qcfg.PowerManaged = WdfUseDefault;
    qcfg.DefaultQueue = 1;
    qcfg.EvtIoDeviceControl = Some(evt_io_device_control);
    // WDF_IO_QUEUE_CONFIG_INIT sets this to (ULONG)-1 (unlimited); mem::zeroed left it 0,
    // which on a parallel queue means present ZERO requests → EvtIoDeviceControl never fires.
    qcfg.Settings.Parallel.NumberOfPresentedRequests = u32::MAX;
    let mut default_queue: WDFQUEUE = core::ptr::null_mut();
    // SAFETY: device + config valid; attributes null; queue receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &mut qcfg,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut default_queue
        )
    };
    if !nt_success(st) {
        dbglog!(
            "[pf-mouse] default WdfIoQueueCreate failed 0x{:08x}",
            st as u32
        );
        return st;
    }

    // Manual queue: pended READ_REPORT requests are completed by the timer on fresh host input.
    // SAFETY: zeroed config then fields set.
    let mut mcfg: WDF_IO_QUEUE_CONFIG = unsafe { core::mem::zeroed() };
    mcfg.Size = core::mem::size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG;
    mcfg.DispatchType = WdfIoQueueDispatchManual;
    mcfg.PowerManaged = WdfUseDefault;
    let mut manual_queue: WDFQUEUE = core::ptr::null_mut();
    // SAFETY: device + config valid; attributes null; queue receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &mut mcfg,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut manual_queue
        )
    };
    if !nt_success(st) {
        dbglog!(
            "[pf-mouse] manual WdfIoQueueCreate failed 0x{:08x}",
            st as u32
        );
        return st;
    }
    MANUAL_QUEUE.store(manual_queue, Ordering::SeqCst);

    // Periodic timer (parent = manual queue): sealed-channel pump + health marks + event-driven
    // READ_REPORT completion. 8 ms — the proven pf-gamepad cadence; the mouse is presence-first
    // (SendInput injects), so a 125 Hz ceiling on the validation/report path is fine.
    // SAFETY: zeroed config then fields set.
    let mut tcfg: WDF_TIMER_CONFIG = unsafe { core::mem::zeroed() };
    tcfg.Size = core::mem::size_of::<WDF_TIMER_CONFIG>() as ULONG;
    tcfg.EvtTimerFunc = Some(evt_timer);
    tcfg.Period = 8; // ms
    tcfg.AutomaticSerialization = 1; // TRUE — UMDF requires a serialized timer (vhidmini2 pattern)
    // SAFETY: a zeroed WDF_OBJECT_ATTRIBUTES is a valid all-null attributes struct; we set Size + the
    // fields we use below.
    let mut tattr: WDF_OBJECT_ATTRIBUTES = unsafe { core::mem::zeroed() };
    tattr.Size = core::mem::size_of::<WDF_OBJECT_ATTRIBUTES>() as ULONG;
    tattr.ParentObject = manual_queue.cast();
    // mem::zeroed leaves these at 0 (Invalid) → set them like WDF_OBJECT_ATTRIBUTES_INIT
    // (matches the working vhidmini2 UMDF timer setup; avoids 0xc0200209 / 0xc00000bb).
    tattr.ExecutionLevel = WdfExecutionLevelInheritFromParent;
    tattr.SynchronizationScope = WdfSynchronizationScopeInheritFromParent;
    let mut timer: WDFTIMER = core::ptr::null_mut();
    // SAFETY: config + attributes valid; timer receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfTimerCreate, &mut tcfg, &mut tattr, &mut timer)
    };
    if !nt_success(st) {
        dbglog!("[pf-mouse] WdfTimerCreate failed 0x{:08x}", st as u32);
        return st;
    }
    // SAFETY: timer valid; -80000 == 8ms relative due time (100ns units, negative = relative).
    let _started = unsafe { call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -80000i64) };

    log("[pf-mouse] device ready (HID mouse 5046:4D4F)");
    STATUS_SUCCESS
}

extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_len: usize,
    _input_len: usize,
    ioctl: ULONG,
) {
    // SAFETY: `request` is the live request for THIS EvtIoDeviceControl invocation — exactly the
    // contract `Request::new` requires. Everything after is safe (the token owns completion).
    let request = unsafe { Request::new(request) };

    // Skip the READ_REPORT cadence so the log stays readable; the descriptor handshake still logs.
    if ioctl != IOCTL_HID_READ_REPORT {
        dbglog!("[pf-mouse] ioctl 0x{ioctl:08x} out={_output_len} in={_input_len}");
    }

    // READ_REPORT forwards to the manual queue (the timer completes it on fresh input) — this
    // CONSUMES the request token, so it's handled apart from the status-and-complete paths below.
    if ioctl == IOCTL_HID_READ_REPORT {
        let mq: WDFQUEUE = MANUAL_QUEUE.load(Ordering::SeqCst);
        // SAFETY: `mq` is the manual queue created in EvtDeviceAdd (a live WDFQUEUE of this device).
        match unsafe { request.forward_to_queue(mq) } {
            Ok(()) => {}                        // framework owns it now (completed by the timer)
            Err((req, st)) => req.complete(st), // forward failed → complete with the error
        }
        return;
    }

    let status: NTSTATUS = match ioctl {
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => request.copy_to_output(&HID_DESC),
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => request.copy_to_output(&hid_attrs()),
        IOCTL_HID_GET_REPORT_DESCRIPTOR => request.copy_to_output(&MOUSE_RDESC),
        IOCTL_UMDF_HID_GET_INPUT_REPORT => {
            let report = INPUT_REPORT.lock().map(|g| *g).unwrap_or(NEUTRAL_REPORT);
            request.copy_to_output(&report)
        }
        // No output reports are declared; ack a stray write instead of failing the sender.
        IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT => STATUS_SUCCESS,
        IOCTL_HID_GET_STRING => on_get_string(&request),
        // The channel proof (see `pf_umdf_util::hid`): the host asks THIS devnode which process
        // serves it, and duplicates the DATA section into the answer — so it never has to trust the
        // LocalService-writable bootstrap mailbox to name its target.
        _ => STATUS_NOT_IMPLEMENTED,
    };

    dbglog!("[pf-mouse] ioctl 0x{ioctl:08x} -> 0x{:08x}", status as u32);
    request.complete(status);
}

// IOCTL_HID_GET_STRING: the input is a ULONG whose low word is the string id and whose high word
// is the language id. Windows polls ids 0x0E/0x0F/0x10 (manufacturer/product/serial) as well as
// the 0/1/2 HID_STRING_ID_* constants — serve both (the pf-gamepad finding).
fn on_get_string(request: &Request) -> NTSTATUS {
    let (bytes, _) = match request.input_bytes(4) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let id_val: u32 = if bytes.len() >= 4 {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        0
    };
    let string_id = id_val & 0xFFFF;
    let s: String = match string_id {
        0 | 0x000E => "Punktfunk".into(),
        // (2) The SERIAL carries the channel proof — the one transport measured to reach a UMDF HID
        // minidriver from user mode (`HidD_GetSerialNumberString`, zero-access handle, verified on
        // .173). Safe HERE and only here: nothing reads the virtual mouse's serial, whereas the pads'
        // serials are what SDL and Steam dedup controllers on. The old value was the inert
        // "PFMOUSE00"; the proof text is just as inert and does the security work.
        2 | 0x0010 => ChannelProof::new(CHANNEL.index(), std::process::id()).to_hid_string(),
        _ => "Punktfunk Virtual Mouse".into(),
    };
    let mut wide: Vec<u8> = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16() {
        wide.extend_from_slice(&u.to_le_bytes());
    }
    wide.extend_from_slice(&[0, 0]); // NUL terminator (UTF-16)
    request.copy_to_output(&wide)
}

extern "C" fn evt_timer(timer: WDFTIMER) {
    // One sealed-channel tick: publish our pid / adopt a delivery / detect host-gone (all safe,
    // via pf_umdf_util), then stamp the health marks the host watches.
    let Some(view) = CHANNEL.pump(&channel_cfg()) else {
        return; // host gone or not attached — nothing to deliver, nothing to mark
    };
    view.write_u32(OFF_DRIVER_PROTO, GAMEPAD_PROTO_VERSION);
    let hb = view.read_u32(OFF_DRIVER_HEARTBEAT).wrapping_add(1);
    view.write_u32(OFF_DRIVER_HEARTBEAT, hb);

    // Event-driven delivery: only when the host published a NEW report (in_seq advanced) does a
    // pended READ_REPORT complete. Acquire pairs with the host's Release bump, so the report bytes
    // read below are the ones that seq published. If no read is pended right now, DELIVERED_SEQ is
    // NOT advanced — the next tick retries while hidclass re-pends its reader.
    let seq = view.load_u32(OFF_IN_SEQ, Ordering::Acquire);
    if seq == 0 || seq == DELIVERED_SEQ.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY-free queue access: the timer's parent object is the manual queue (set in
    // EvtDeviceAdd); the framework guarantees a live handle here.
    // SAFETY: see above — WdfTimerGetParentObject on the framework-provided live timer.
    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfTimerGetParentObject, timer) } as WDFQUEUE;
    // SAFETY: `queue` is that live manual queue — the exact contract `retrieve_next_request` needs.
    let Some(request) = (unsafe { wdf::retrieve_next_request(queue) }) else {
        return; // no reader pended — retry next tick (seq stays undelivered)
    };
    let mut report = [0u8; MOUSE_REPORT_LEN];
    view.read_bytes(OFF_REPORT, &mut report);
    DELIVERED_SEQ.store(seq, Ordering::Relaxed);
    if report[0] == MOUSE_REPORT_ID {
        if let Ok(mut g) = INPUT_REPORT.lock() {
            *g = report;
        }
        let st = request.copy_to_output(&report);
        request.complete(st);
    } else {
        // A malformed publish (host bug / torn first write): don't feed hidclass garbage — repend
        // by completing nothing this tick. The request was already retrieved, so complete it with
        // the last good report instead of dropping it on the floor.
        let report = INPUT_REPORT.lock().map(|g| *g).unwrap_or(NEUTRAL_REPORT);
        let st = request.copy_to_output(&report);
        request.complete(st);
    }
}
