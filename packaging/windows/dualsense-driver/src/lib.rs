// punktfunk virtual DualSense — UMDF2 HID minidriver (M0 spike).
//
// A Rust port of the WDK `vhidmini2` UMDF2 sample, reconfigured to present a Sony DualSense
// (VID 054C / PID 0CE6) using the inputtino report descriptor + feature blobs punktfunk already
// ships in `inject/dualsense.rs`. Its purpose for M0(b) is to (1) enumerate as a genuine DualSense
// and (2) LOG every output report the game writes — the adaptive-trigger `0x02` gate.
//
// No WDF object contexts: this is a singleton virtual device, so per-device state lives in statics.
// All WDF calls go through `call_unsafe_wdf_function_binding!`; HID/WDF structs are hand-built.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, Ordering};

use wdk_sys::{
    call_unsafe_wdf_function_binding, windows::OutputDebugStringA, GUID, NTSTATUS, PCUNICODE_STRING,
    PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDFDEVICE, WDFDRIVER, WDFMEMORY, WDFQUEUE, WDFQUEUE__,
    WDFREQUEST, WDFTIMER, WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES, WDF_TIMER_CONFIG,
};

// ---- NTSTATUS values ----
const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_UNSUCCESSFUL: NTSTATUS = 0xC000_0001u32 as NTSTATUS;
const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002u32 as NTSTATUS;
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000Du32 as NTSTATUS;
const STATUS_INVALID_BUFFER_SIZE: NTSTATUS = 0xC000_0206u32 as NTSTATUS;

#[inline]
fn nt_success(s: NTSTATUS) -> bool {
    s >= 0
}

// ---- HID minidriver IOCTLs: CTL_CODE(FILE_DEVICE_KEYBOARD=0x0b, id, METHOD_NEITHER=3, ANY) ----
const fn hid_ctl(id: u32) -> u32 {
    (0x0000_000b << 16) | (id << 2) | 3
}
const IOCTL_HID_GET_DEVICE_DESCRIPTOR: u32 = hid_ctl(0);
const IOCTL_HID_GET_REPORT_DESCRIPTOR: u32 = hid_ctl(1);
const IOCTL_HID_READ_REPORT: u32 = hid_ctl(2);
const IOCTL_HID_WRITE_REPORT: u32 = hid_ctl(3);
const IOCTL_HID_GET_DEVICE_ATTRIBUTES: u32 = hid_ctl(9);
const IOCTL_UMDF_HID_SET_FEATURE: u32 = hid_ctl(20);
const IOCTL_UMDF_HID_GET_FEATURE: u32 = hid_ctl(21);
const IOCTL_UMDF_HID_SET_OUTPUT_REPORT: u32 = hid_ctl(22);
const IOCTL_UMDF_HID_GET_INPUT_REPORT: u32 = hid_ctl(23);

// ---- Host control channel: CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, fn, METHOD_BUFFERED=0, access) ----
const fn pfds_ctl(func: u32, access: u32) -> u32 {
    (0x0000_0022 << 16) | (access << 14) | (func << 2)
}
/// Host → driver: push the 64-byte `0x01` input report (FILE_WRITE_ACCESS).
const IOCTL_PFDS_SET_INPUT: u32 = pfds_ctl(0x800, 2);
/// Driver → host inverted-call: completed with a game's raw `0x02` output report (FILE_READ_ACCESS).
const IOCTL_PFDS_GET_OUTPUT: u32 = pfds_ctl(0x801, 1);

// ---- WDF enum values ----
const WdfIoQueueDispatchParallel: i32 = 2;
const WdfIoQueueDispatchManual: i32 = 3;
const WdfUseDefault: i32 = 2; // WDF_TRI_STATE
const WdfExecutionLevelInheritFromParent: i32 = 1; // WDF_EXECUTION_LEVEL
const WdfSynchronizationScopeInheritFromParent: i32 = 1; // WDF_SYNCHRONIZATION_SCOPE

// ---- DualSense identity ----
const DS_VID: u16 = 0x054C;
const DS_PID: u16 = 0x0CE6;
const DS_VER: u16 = 0x0100;

// {7B2F8E4A-9C3D-4E1F-A6B5-1234567890AB} — the host↔driver control interface the punktfunk host
// opens (on the SwDeviceCreate'd device) to push input reports + pull a game's output reports.
const PFDS_CONTROL_GUID: GUID = GUID {
    Data1: 0x7b2f_8e4a,
    Data2: 0x9c3d,
    Data3: 0x4e1f,
    Data4: [0xa6, 0xb5, 0x12, 0x34, 0x56, 0x78, 0x90, 0xab],
};

// Sony DualSense USB HID report descriptor (273 bytes), verbatim from inputtino (== inject/dualsense.rs).
// NOTE: inject/dualsense.rs comments this as "232 bytes" — that comment is wrong; it is 273.
#[rustfmt::skip]
static DUALSENSE_RDESC: [u8; 273] = [
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x2F, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xB1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2F, 0xB1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xB1, 0x02, 0x85, 0x0A, 0x09, 0x25, 0x95, 0x1A, 0xB1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xB1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x2A, 0x95, 0x09, 0xB1, 0x02,
    0x85, 0x83, 0x09, 0x2B, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x84, 0x09, 0x2C, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x85, 0x09, 0x2D, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xA0, 0x09, 0x2E, 0x95, 0x01, 0xB1, 0x02,
    0x85, 0xE0, 0x09, 0x2F, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x30, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0xF1, 0x09, 0x31, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2, 0x09, 0x32, 0x95, 0x0F, 0xB1, 0x02,
    0x85, 0xF4, 0x09, 0x35, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF5, 0x09, 0x36, 0x95, 0x03, 0xB1, 0x02,
    0xC0,
];

// Feature reports hid-playstation / Steam read during init (each array's first byte is the report id).
#[rustfmt::skip]
static DS_FEATURE_CALIBRATION: [u8; 42] = [ // 0x05 motion calibration
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0xF4, 0x01, 0xF4, 0x01, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
static DS_FEATURE_PAIRING: [u8; 20] = [ // 0x09 pairing info (MAC at 1..7)
    0x09, 0x74, 0xE7, 0xD6, 0x3A, 0x53, 0x35, 0x08, 0x25, 0x00, 0x1E, 0x00, 0xEE, 0x74, 0xD0, 0xBC,
    0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
static DS_FEATURE_FIRMWARE: [u8; 64] = [ // 0x20 firmware info
    0x20, 0x4A, 0x75, 0x6E, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3A, 0x34,
    0x37, 0x3A, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xC1, 0xC8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x54, 0x01, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x0B, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// HID descriptor (9 bytes, packed): len, type=0x21, bcdHID=0x0100, country=0, numDesc=1,
// then {reportType=0x22, wReportLength=273 (0x0111)}.
static HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x11, 0x01];

// HID_DEVICE_ATTRIBUTES (32 bytes): Size(u32)=32, VendorID, ProductID, VersionNumber, Reserved[11].
fn hid_attrs() -> [u8; 32] {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&32u32.to_le_bytes());
    a[4..6].copy_from_slice(&DS_VID.to_le_bytes());
    a[6..8].copy_from_slice(&DS_PID.to_le_bytes());
    a[8..10].copy_from_slice(&DS_VER.to_le_bytes());
    a
}

// Neutral DualSense input report 0x01 (64 bytes): sticks centered (0x80), triggers 0, dpad neutral (8).
const NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01; // report id
    r[1] = 0x80; // LX
    r[2] = 0x80; // LY
    r[3] = 0x80; // RX
    r[4] = 0x80; // RY
    // r[5]=L2, r[6]=R2 = 0; r[7] = seq counter = 0
    r[8] = 0x08; // buttons[0]: low nibble = dpad hat (8 = neutral), high nibble = face buttons (0)
    r
};
fn neutral_report() -> [u8; 64] {
    NEUTRAL_REPORT
}

static MANUAL_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// Manual queue of pended host `IOCTL_PFDS_GET_OUTPUT` requests (inverted-call); completed with a
/// game's `0x02` output report as it arrives.
static OUTPUT_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// The latest input report the host pushed (report `0x01`); the timer + each SET_INPUT deliver it to
/// pended game READ_REPORTs. Defaults to neutral until the host connects.
static INPUT_REPORT: std::sync::Mutex<[u8; 64]> = std::sync::Mutex::new(NEUTRAL_REPORT);
/// One-shot logs so the control channel's first traffic is visible without per-frame spam.
static LOGGED_SET_INPUT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static LOGGED_GET_OUTPUT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

// ---- user-mode shared-memory IPC with the punktfunk host ----
// UMDF runs in WUDFHost.exe (user-mode) and hidclass blocks a control channel on the device stack
// (custom interface CreateFile → err 31; custom IOCTL on the HID handle → err 1) and UMDF has no
// control device, so the host channel is a named section the (privileged) host CREATES and the driver
// OPENS. Layout (256 B): magic u32 @0 ("PFDS"), input_seq u32 @4, input_report[64] @8,
// output_seq u32 @72, output_report[64] @76.
const FILE_MAP_RW: u32 = 0x0002 | 0x0004; // FILE_MAP_WRITE | FILE_MAP_READ
const SHM_MAGIC: u32 = 0x5046_4453; // "PFDS" little-endian
const SHM_SIZE: usize = 256;
static LOGGED_SHM: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// kernel32 file-mapping APIs (resolved via std's kernel32 import; UMDF permits file mapping).
unsafe extern "system" {
    fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
    fn MapViewOfFile(h: *mut c_void, access: u32, hi: u32, lo: u32, len: usize) -> *mut c_void;
    fn UnmapViewOfFile(addr: *const c_void) -> i32;
    fn CloseHandle(h: *mut c_void) -> i32;
}

fn log(s: &str) {
    if let Ok(c) = std::ffi::CString::new(s) {
        // SAFETY: c is a valid null-terminated string for the duration of the call.
        unsafe { OutputDebugStringA(c.as_ptr().cast()) };
    }
    // Also append to a world-writable file — DebugView can't capture the UMDF host's output
    // across session 0, so this is how we read driver-start diagnostics.
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("C:\\Users\\Public\\pfds-driver.log")
    {
        let _ = writeln!(f, "{s}");
    }
}
macro_rules! dbglog { ($($a:tt)*) => { log(&format!($($a)*)) } }

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    log("[pf-ds] DriverEntry");
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
    log("[pf-ds] EvtDeviceAdd");

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
        dbglog!("[pf-ds] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

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
        dbglog!("[pf-ds] default WdfIoQueueCreate failed 0x{:08x}", st as u32);
        return st;
    }

    // Manual queue: pended READ_REPORT requests are completed by the timer.
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
        dbglog!("[pf-ds] manual WdfIoQueueCreate failed 0x{:08x}", st as u32);
        return st;
    }
    MANUAL_QUEUE.store(manual_queue, Ordering::SeqCst);

    // Periodic timer (parent = manual queue) completes pended reads with the neutral report.
    // SAFETY: zeroed config then fields set.
    let mut tcfg: WDF_TIMER_CONFIG = unsafe { core::mem::zeroed() };
    tcfg.Size = core::mem::size_of::<WDF_TIMER_CONFIG>() as ULONG;
    tcfg.EvtTimerFunc = Some(evt_timer);
    tcfg.Period = 8; // ms
    tcfg.AutomaticSerialization = 1; // TRUE — UMDF requires a serialized timer (vhidmini2 pattern)
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
        dbglog!("[pf-ds] WdfTimerCreate failed 0x{:08x}", st as u32);
        return st;
    }
    // SAFETY: timer valid; -80000 == 8ms relative due time (100ns units, negative = relative).
    let _started = unsafe { call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -80000i64) };

    // Output queue: pended host GET_OUTPUT (inverted-call) requests, completed as games write 0x02.
    // SAFETY: zeroed config then fields set.
    let mut ocfg: WDF_IO_QUEUE_CONFIG = unsafe { core::mem::zeroed() };
    ocfg.Size = core::mem::size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG;
    ocfg.DispatchType = WdfIoQueueDispatchManual;
    ocfg.PowerManaged = WdfUseDefault;
    let mut output_queue: WDFQUEUE = core::ptr::null_mut();
    // SAFETY: device + config valid; attributes null; queue receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &mut ocfg,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut output_queue
        )
    };
    if !nt_success(st) {
        dbglog!("[pf-ds] output WdfIoQueueCreate failed 0x{:08x}", st as u32);
        return st;
    }
    OUTPUT_QUEUE.store(output_queue, Ordering::SeqCst);

    // Host↔driver control interface — the punktfunk host opens this to push input + pull output.
    // Non-fatal if it fails: the HID device still works for direct-app use, just not the host plane.
    // SAFETY: device valid; GUID is a valid static; the reference string is optional (null).
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &PFDS_CONTROL_GUID,
            core::ptr::null::<c_void>() as PCUNICODE_STRING
        )
    };
    if !nt_success(st) {
        dbglog!(
            "[pf-ds] WdfDeviceCreateDeviceInterface failed 0x{:08x}",
            st as u32
        );
    }

    log("[pf-ds] device ready (DualSense 054C:0CE6)");
    STATUS_SUCCESS
}

extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_len: usize,
    _input_len: usize,
    ioctl: ULONG,
) {
    let mut complete = true;
    // Skip the 8ms READ_REPORT cadence so the log stays readable during a game test;
    // the 0x02 OUTPUT report (the gate) and the descriptor handshake still log.
    if !matches!(
        ioctl,
        IOCTL_HID_READ_REPORT | IOCTL_PFDS_SET_INPUT | IOCTL_PFDS_GET_OUTPUT
    ) {
        dbglog!("[pf-ds] ioctl 0x{ioctl:08x} out={_output_len} in={_input_len}");
    }
    let status: NTSTATUS = match ioctl {
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => copy_to_output(request, &HID_DESC),
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => copy_to_output(request, &hid_attrs()),
        IOCTL_HID_GET_REPORT_DESCRIPTOR => copy_to_output(request, &DUALSENSE_RDESC),
        IOCTL_HID_READ_REPORT => {
            let mq: WDFQUEUE = MANUAL_QUEUE.load(Ordering::SeqCst);
            // SAFETY: request valid; mq is the manual queue created in EvtDeviceAdd.
            let st = unsafe {
                call_unsafe_wdf_function_binding!(WdfRequestForwardToIoQueue, request, mq)
            };
            if nt_success(st) {
                complete = false;
                STATUS_SUCCESS
            } else {
                st
            }
        }
        IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT => on_output_report(request, ioctl),
        IOCTL_UMDF_HID_SET_FEATURE => {
            log("[pf-ds] SET_FEATURE (stub ok)");
            STATUS_SUCCESS
        }
        IOCTL_UMDF_HID_GET_FEATURE => on_get_feature(request),
        IOCTL_UMDF_HID_GET_INPUT_REPORT => copy_to_output(request, &neutral_report()),
        // ---- host control channel ----
        IOCTL_PFDS_SET_INPUT => on_set_input(request),
        IOCTL_PFDS_GET_OUTPUT => {
            let oq: WDFQUEUE = OUTPUT_QUEUE.load(Ordering::SeqCst);
            // SAFETY: request valid; oq is the output manual queue created in EvtDeviceAdd.
            let st = unsafe {
                call_unsafe_wdf_function_binding!(WdfRequestForwardToIoQueue, request, oq)
            };
            if !LOGGED_GET_OUTPUT.swap(true, Ordering::Relaxed) {
                dbglog!(
                    "[pf-ds] control: first GET_OUTPUT posted (host pump up) st=0x{:08x}",
                    st as u32
                );
            }
            if nt_success(st) {
                complete = false;
                STATUS_SUCCESS
            } else {
                st
            }
        }
        _ => STATUS_NOT_IMPLEMENTED,
    };

    if !matches!(
        ioctl,
        IOCTL_HID_READ_REPORT | IOCTL_PFDS_SET_INPUT | IOCTL_PFDS_GET_OUTPUT
    ) {
        dbglog!("[pf-ds] ioctl 0x{ioctl:08x} -> 0x{:08x} complete={complete}", status as u32);
    }
    if complete {
        // SAFETY: request valid and not forwarded.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
    }
}

// Copy `src` into the request's output memory and set the completed byte count.
fn copy_to_output(request: WDFREQUEST, src: &[u8]) -> NTSTATUS {
    let mut mem: WDFMEMORY = core::ptr::null_mut();
    // SAFETY: request valid; mem receives the memory handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestRetrieveOutputMemory, request, &mut mem)
    };
    if !nt_success(st) {
        return st;
    }
    let mut outlen: usize = 0;
    // SAFETY: mem valid; outlen receives the buffer size.
    let _ = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut outlen) };
    if outlen < src.len() {
        return STATUS_INVALID_BUFFER_SIZE;
    }
    // SAFETY: mem valid; src is a valid buffer of src.len() bytes.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCopyFromBuffer,
            mem,
            0usize,
            src.as_ptr() as *mut c_void,
            src.len()
        )
    };
    if !nt_success(st) {
        return st;
    }
    // SAFETY: request valid.
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestSetInformation, request, src.len() as u64) };
    STATUS_SUCCESS
}

// The 0x02 gate: a game writing an output report (rumble / lightbar / ADAPTIVE TRIGGERS). Per the
// UMDF marshalling convention the report data is the *input* buffer and the report id is carried in
// the *output* buffer length. We log it.
fn on_output_report(request: WDFREQUEST, ioctl: ULONG) -> NTSTATUS {
    let mut inmem: WDFMEMORY = core::ptr::null_mut();
    // SAFETY: request valid.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestRetrieveInputMemory, request, &mut inmem)
    };
    if !nt_success(st) {
        return st;
    }
    let mut inlen: usize = 0;
    // SAFETY: inmem valid.
    let inbuf =
        unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, inmem, &mut inlen) }
            as *const u8;

    // report id from output-buffer length (UMDF convention).
    let mut report_id: u32 = 0;
    let mut outmem: WDFMEMORY = core::ptr::null_mut();
    // SAFETY: request valid; output memory is optional here.
    if nt_success(unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestRetrieveOutputMemory, request, &mut outmem)
    }) {
        let mut outlen: usize = 0;
        // SAFETY: outmem valid.
        let _ = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, outmem, &mut outlen) };
        report_id = outlen as u32;
    }

    let n = inlen.min(48);
    let mut hex = String::new();
    if !inbuf.is_null() {
        // SAFETY: inbuf valid for inlen bytes; we read at most n.
        let bytes = unsafe { core::slice::from_raw_parts(inbuf, n) };
        for b in bytes {
            hex.push_str(&format!("{b:02x} "));
        }
    }
    let kind = if ioctl == IOCTL_HID_WRITE_REPORT {
        "WRITE_REPORT"
    } else {
        "SET_OUTPUT_REPORT"
    };
    dbglog!(
        "[pf-ds] *** OUTPUT {kind} reportId={report_id} len={inlen} data: {hex}"
    );

    // Forward the raw report to a pended host GET_OUTPUT request so the punktfunk host can relay
    // rumble / lightbar / player-LEDs / adaptive-trigger feedback to the client.
    if !inbuf.is_null() && inlen > 0 {
        // SAFETY: inbuf valid for inlen bytes; cap the copy at 64.
        let report = unsafe { core::slice::from_raw_parts(inbuf, inlen.min(64)) };
        deliver_output(report);
    }

    // Publish to shared memory for the host — the real feedback channel (the IOCTL path above is
    // inert under hidclass). output_report @76, output_seq @72.
    if !inbuf.is_null() && inlen > 0 {
        let n = inlen.min(64);
        with_shm(|view| {
            // SAFETY: view is a mapped 256-byte section; write the report then bump the host-polled seq.
            unsafe {
                core::ptr::copy_nonoverlapping(inbuf, view.add(76), n);
                let seqp = view.add(72) as *mut u32;
                let seq = core::ptr::read_unaligned(seqp).wrapping_add(1);
                core::ptr::write_unaligned(seqp, seq);
            }
        });
    }

    // SAFETY: request valid.
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestSetInformation, request, inlen as u64) };
    STATUS_SUCCESS
}

// Host → driver: store the pushed `0x01` input report and deliver it to a pending game READ_REPORT.
fn on_set_input(request: WDFREQUEST) -> NTSTATUS {
    let mut inmem: WDFMEMORY = core::ptr::null_mut();
    // SAFETY: request valid.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestRetrieveInputMemory, request, &mut inmem)
    };
    if !nt_success(st) {
        return st;
    }
    let mut inlen: usize = 0;
    // SAFETY: inmem valid.
    let inbuf = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, inmem, &mut inlen) }
        as *const u8;
    if inbuf.is_null() || inlen == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let n = inlen.min(64);
    if let Ok(mut guard) = INPUT_REPORT.lock() {
        // SAFETY: inbuf valid for inlen >= n bytes.
        let src = unsafe { core::slice::from_raw_parts(inbuf, n) };
        guard[..n].copy_from_slice(src);
    }
    if !LOGGED_SET_INPUT.swap(true, Ordering::Relaxed) {
        dbglog!("[pf-ds] control: first SET_INPUT ({inlen} bytes) — host input plane up");
    }
    complete_one_read();
    STATUS_SUCCESS
}

// Pull one pended game READ_REPORT and complete it with the current input report.
fn complete_one_read() {
    let queue: WDFQUEUE = MANUAL_QUEUE.load(Ordering::SeqCst);
    if queue.is_null() {
        return;
    }
    let mut request: WDFREQUEST = core::ptr::null_mut();
    // SAFETY: queue valid; request receives the next pended request if any.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoQueueRetrieveNextRequest, queue, &mut request)
    };
    if nt_success(st) {
        let report = INPUT_REPORT.lock().map(|g| *g).unwrap_or(NEUTRAL_REPORT);
        let s = copy_to_output(request, &report);
        // SAFETY: request valid and dequeued.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, s) };
    }
}

// Deliver a game's raw `0x02` output report to a pended host GET_OUTPUT request (if one is posted).
fn deliver_output(data: &[u8]) {
    let oq: WDFQUEUE = OUTPUT_QUEUE.load(Ordering::SeqCst);
    if oq.is_null() {
        return;
    }
    let mut request: WDFREQUEST = core::ptr::null_mut();
    // SAFETY: oq valid; request receives the next pended request if any.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoQueueRetrieveNextRequest, oq, &mut request)
    };
    if nt_success(st) {
        let s = copy_to_output(request, data);
        // SAFETY: request valid and dequeued.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, s) };
    }
}

// GET_FEATURE: report id from the input buffer; reply with the matching DualSense feature blob.
fn on_get_feature(request: WDFREQUEST) -> NTSTATUS {
    let mut inmem: WDFMEMORY = core::ptr::null_mut();
    // SAFETY: request valid.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestRetrieveInputMemory, request, &mut inmem)
    };
    if !nt_success(st) {
        return st;
    }
    let mut inlen: usize = 0;
    // SAFETY: inmem valid.
    let inbuf =
        unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, inmem, &mut inlen) }
            as *const u8;
    if inbuf.is_null() || inlen < 1 {
        return STATUS_INVALID_PARAMETER;
    }
    // SAFETY: inbuf valid for >=1 byte.
    let report_id = unsafe { *inbuf };
    let blob: &[u8] = match report_id {
        0x05 => &DS_FEATURE_CALIBRATION,
        0x09 => &DS_FEATURE_PAIRING,
        0x20 => &DS_FEATURE_FIRMWARE,
        other => {
            dbglog!("[pf-ds] GET_FEATURE unknown report id 0x{other:02x}");
            return STATUS_INVALID_PARAMETER;
        }
    };
    copy_to_output(request, blob)
}

// Open + map the host's shared-memory section (Global\pfds-shm-0) and run `f` against the mapped base
// if it exists with a valid magic, then unmap. NOT cached: re-mapped per access so the driver always
// sees the current section (UMDF groups all devices in one WUDFHost, and the host may recreate the
// section across restarts — a cached view would go stale). ~125 maps/s from the timer = negligible.
fn with_shm<F: FnOnce(*mut u8)>(f: F) {
    let name: Vec<u16> = "Global\\pfds-shm-0"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: name is a valid NUL-terminated UTF-16 string.
    let h = unsafe { OpenFileMappingW(FILE_MAP_RW, 0, name.as_ptr()) };
    if h.is_null() {
        return;
    }
    // SAFETY: h is a valid mapping handle; map the whole section. The view keeps the section alive,
    // so the handle can be closed right away.
    let view = unsafe { MapViewOfFile(h, FILE_MAP_RW, 0, 0, SHM_SIZE) } as *mut u8;
    unsafe { CloseHandle(h) };
    if view.is_null() {
        return;
    }
    // SAFETY: view points at >= 4 mapped bytes.
    let magic = unsafe { core::ptr::read_unaligned(view as *const u32) };
    if magic == SHM_MAGIC {
        if !LOGGED_SHM.swap(true, Ordering::Relaxed) {
            dbglog!("[pf-ds] control: shared memory mapped (Global\\pfds-shm-0)");
        }
        f(view);
    }
    // SAFETY: view came from MapViewOfFile.
    unsafe { UnmapViewOfFile(view as *const c_void) };
}

extern "C" fn evt_timer(timer: WDFTIMER) {
    // Pull the latest host input report from shared memory (if the host has connected).
    with_shm(|view| {
        let mut buf = [0u8; 64];
        // SAFETY: view points at a mapped 256-byte section; input lives at offset 8..72.
        unsafe { core::ptr::copy_nonoverlapping(view.add(8), buf.as_mut_ptr(), 64) };
        if buf[0] == 0x01 {
            if let Ok(mut g) = INPUT_REPORT.lock() {
                *g = buf;
            }
        }
    });
    // SAFETY: timer valid; parent is the manual queue.
    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfTimerGetParentObject, timer) } as WDFQUEUE;
    let mut request: WDFREQUEST = core::ptr::null_mut();
    // SAFETY: queue valid; request receives the next pended request if any.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoQueueRetrieveNextRequest, queue, &mut request)
    };
    if nt_success(st) {
        let report = INPUT_REPORT.lock().map(|g| *g).unwrap_or(NEUTRAL_REPORT);
        let s = copy_to_output(request, &report);
        // SAFETY: request valid and dequeued.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, s) };
    }
    let _ = STATUS_UNSUCCESSFUL; // keep the const referenced
}
