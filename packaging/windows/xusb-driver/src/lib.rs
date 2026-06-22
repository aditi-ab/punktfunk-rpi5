// punktfunk virtual Xbox 360 XUSB companion — UMDF2 driver presenting the XUSB device interface so
// classic XInput (XInputGetState) reads the pad with no kernel bus driver (the HIDMaestro approach).
//
// xinput1_4.dll enumerates GUID_DEVINTERFACE_XUSB, opens the Nth instance (= player slot), and polls
// it with buffered IOCTLs. We register the interface and answer those IOCTLs from controller state the
// host publishes into a shared-memory section (`Global\pfxusb-shm-0`); a game's rumble (SET_STATE) is
// published back for the host to forward. Byte formats are the source-verified xusb22 wire layout
// (HIDMaestro driver/companion.c + nefarius/XInputHooker XUSB.h + ViGEm XUSB_REPORT).
//
// We answer the WAIT_* IOCTLs with STATUS_INVALID_DEVICE_REQUEST, which makes xinput1_4 fall back to
// synchronous GET_STATE polling — so no manual queue / timer is needed for classic XInput.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};
use wdk_sys::{
    call_unsafe_wdf_function_binding, windows::OutputDebugStringA, GUID, NTSTATUS, PCUNICODE_STRING,
    PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDFDEVICE, WDFDRIVER, WDFMEMORY, WDFQUEUE, WDFREQUEST,
    WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

// DEVICE_REGISTRY_PROPERTY: DevicePropertyLocationInformation (the const isn't re-exported at the
// wdk_sys root; the value is stable WDM).
const DEVICE_PROPERTY_LOCATION_INFORMATION: i32 = 10;

/// The pad index this device serves (which `pfxusb-shm-<index>` section to map). The host stamps it
/// into the device Location (`pszDeviceLocation`); the driver reads it in EvtDeviceAdd. With
/// `UmdfHostProcessSharing=ProcessSharingDisabled` (the INF) each pad gets its own WUDFHost, so this
/// static is per-pad — the basis for multi-pad.
static SHM_INDEX: AtomicU32 = AtomicU32::new(0);

// ---- NTSTATUS ----
const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = 0xC000_0010u32 as NTSTATUS;
const STATUS_INVALID_BUFFER_SIZE: NTSTATUS = 0xC000_0206u32 as NTSTATUS;

#[inline]
fn nt_success(s: NTSTATUS) -> bool {
    s >= 0
}

// GUID_DEVINTERFACE_XUSB {EC87F1E3-C13B-4100-B5F7-8B84D54260CB} — what xinput1_4 enumerates + opens.
const GUID_DEVINTERFACE_XUSB: GUID = GUID {
    Data1: 0xEC87_F1E3,
    Data2: 0xC13B,
    Data3: 0x4100,
    Data4: [0xB5, 0xF7, 0x8B, 0x84, 0xD5, 0x42, 0x60, 0xCB],
};

// ---- XUSB IOCTLs (METHOD_BUFFERED) ----
const IOCTL_XUSB_GET_INFORMATION: u32 = 0x8000_6000;
const IOCTL_XUSB_GET_CAPABILITIES: u32 = 0x8000_E004;
const IOCTL_XUSB_GET_LED_STATE: u32 = 0x8000_E008;
const IOCTL_XUSB_GET_STATE: u32 = 0x8000_E00C;
const IOCTL_XUSB_SET_STATE: u32 = 0x8000_A010;
const IOCTL_XUSB_WAIT_GUIDE_BUTTON: u32 = 0x8000_E014;
const IOCTL_XUSB_GET_BATTERY_INFORMATION: u32 = 0x8000_E018;
const IOCTL_XUSB_POWER_DOWN: u32 = 0x8000_A01C;
const IOCTL_XUSB_GET_XINPUT_MANAGEMENT_DRIVER: u32 = 0x8000_6380;
const IOCTL_XUSB_WAIT_FOR_INPUT: u32 = 0x8000_E3AC;
const IOCTL_XUSB_GET_INFORMATION_EX: u32 = 0x8000_E3FC;

// Xbox 360 wired identity (what GET_INFORMATION reports). 0x0103 unblocks SET_STATE (vibration).
const XUSB_VID: u16 = 0x045E;
const XUSB_PID: u16 = 0x028E;
const XUSB_VERSION: u16 = 0x0103;

// ---- WDF enum values ----
const WdfIoQueueDispatchParallel: i32 = 2;
const WdfUseDefault: i32 = 2; // WDF_TRI_STATE

// ---- shared-memory layout (host ↔ driver), must match the host's xbox_xusb_windows backend ----
// magic u32 @0 ("PFXU"); packet u32 @4 (host bumps on state change → dwPacketNumber); the XUSB_REPORT
// payload @8: wButtons u16 @8, bLeftTrigger @10, bRightTrigger @11, sThumbLX i16 @12, LY @14, RX @16,
// RY @18; rumble_seq u32 @24 (driver bumps on SET_STATE); rumble large @28, small @29.
const FILE_MAP_RW: u32 = 0x0002 | 0x0004;
const SHM_MAGIC: u32 = 0x5558_4650; // "PFXU" little-endian
const SHM_SIZE: usize = 64;

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
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("C:\\Users\\Public\\pfxusb-driver.log")
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
    log("[pf-xusb] DriverEntry");
    // SAFETY: zeroed config then Size + callback set.
    let mut config: WDF_DRIVER_CONFIG = unsafe { core::mem::zeroed() };
    config.Size = core::mem::size_of::<WDF_DRIVER_CONFIG>() as ULONG;
    config.EvtDriverDeviceAdd = Some(evt_device_add);
    // SAFETY: all pointers valid; provided by the loader.
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

/// Read the pad index the host stamped into the device Location (`pszDeviceLocation`), a NUL-terminated
/// UTF-16 decimal string. Defaults to 0 (single-pad) if absent.
fn query_shm_index(device: WDFDEVICE) -> u32 {
    let mut mem: WDFMEMORY = core::ptr::null_mut();
    // SAFETY: device valid; property = LocationInformation; pool ignored in UMDF; mem receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceAllocAndQueryProperty,
            device,
            DEVICE_PROPERTY_LOCATION_INFORMATION,
            0,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut mem
        )
    };
    if !nt_success(st) || mem.is_null() {
        return 0;
    }
    let mut len: usize = 0;
    // SAFETY: mem valid.
    let buf = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, mem, &mut len) }
        as *const u16;
    if buf.is_null() {
        return 0;
    }
    let mut idx: u32 = 0;
    let mut any = false;
    for i in 0..(len / 2).min(8) {
        // SAFETY: buf valid for len bytes; i < len/2.
        let c = unsafe { *buf.add(i) };
        if c == 0 {
            break;
        }
        if (0x30..=0x39).contains(&c) {
            idx = idx.wrapping_mul(10).wrapping_add((c - 0x30) as u32);
            any = true;
        }
    }
    if any {
        idx
    } else {
        0
    }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    log("[pf-xusb] EvtDeviceAdd");

    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: device_init valid; attributes null; device receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device
        )
    };
    if !nt_success(st) {
        dbglog!("[pf-xusb] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

    let idx = query_shm_index(device);
    SHM_INDEX.store(idx, Ordering::Relaxed);
    dbglog!("[pf-xusb] shm index = {idx}");

    // Register the XUSB device interface (no reference string) — what xinput1_4 enumerates + opens.
    // SAFETY: device valid; GUID static; null reference string.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &GUID_DEVINTERFACE_XUSB,
            core::ptr::null()
        )
    };
    if !nt_success(st) {
        dbglog!(
            "[pf-xusb] WdfDeviceCreateDeviceInterface failed 0x{:08x}",
            st as u32
        );
        return st;
    }

    // Default parallel queue: all the XUSB IOCTLs land here.
    // SAFETY: zeroed config then fields set; Size matches the struct.
    let mut qcfg: WDF_IO_QUEUE_CONFIG = unsafe { core::mem::zeroed() };
    qcfg.Size = core::mem::size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG;
    qcfg.DispatchType = WdfIoQueueDispatchParallel;
    qcfg.PowerManaged = WdfUseDefault;
    qcfg.DefaultQueue = 1;
    qcfg.EvtIoDeviceControl = Some(evt_io_device_control);
    qcfg.Settings.Parallel.NumberOfPresentedRequests = u32::MAX;
    let mut queue: WDFQUEUE = core::ptr::null_mut();
    // SAFETY: device + config valid; attributes null; queue receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &mut qcfg,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut queue
        )
    };
    if !nt_success(st) {
        dbglog!("[pf-xusb] WdfIoQueueCreate failed 0x{:08x}", st as u32);
        return st;
    }

    log("[pf-xusb] device ready (XUSB interface registered)");
    STATUS_SUCCESS
}

// Open + map the host's shared section and run `f` against the mapped base if magic is valid, then
// unmap. Re-mapped per access (the host may recreate the section across restarts).
fn with_shm<F: FnOnce(*mut u8)>(f: F) {
    let name: Vec<u16> = format!("Global\\pfxusb-shm-{}", SHM_INDEX.load(Ordering::Relaxed))
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: name is a valid NUL-terminated UTF-16 string.
    let h = unsafe { OpenFileMappingW(FILE_MAP_RW, 0, name.as_ptr()) };
    if h.is_null() {
        return;
    }
    // SAFETY: h is a valid mapping handle; map the whole section; the view keeps it alive.
    let view = unsafe { MapViewOfFile(h, FILE_MAP_RW, 0, 0, SHM_SIZE) } as *mut u8;
    unsafe { CloseHandle(h) };
    if view.is_null() {
        return;
    }
    // SAFETY: view points at >= 4 mapped bytes.
    let magic = unsafe { core::ptr::read_unaligned(view as *const u32) };
    if magic == SHM_MAGIC {
        f(view);
    }
    // SAFETY: view came from MapViewOfFile.
    unsafe { UnmapViewOfFile(view as *const c_void) };
}

/// The current controller state from shared memory (zeros / neutral if the host hasn't connected).
/// Returns `(dwPacketNumber, wButtons, lt, rt, lx, ly, rx, ry)`.
fn read_state() -> (u32, u16, u8, u8, i16, i16, i16, i16) {
    let mut out = (0u32, 0u16, 0u8, 0u8, 0i16, 0i16, 0i16, 0i16);
    with_shm(|v| {
        // SAFETY: v points at a mapped SHM_SIZE section with valid magic.
        unsafe {
            out.0 = core::ptr::read_unaligned(v.add(4) as *const u32);
            out.1 = core::ptr::read_unaligned(v.add(8) as *const u16);
            out.2 = *v.add(10);
            out.3 = *v.add(11);
            out.4 = core::ptr::read_unaligned(v.add(12) as *const i16);
            out.5 = core::ptr::read_unaligned(v.add(14) as *const i16);
            out.6 = core::ptr::read_unaligned(v.add(16) as *const i16);
            out.7 = core::ptr::read_unaligned(v.add(18) as *const i16);
        }
    });
    out
}

/// Publish a game's rumble (from SET_STATE) into shared memory for the host to forward.
fn publish_rumble(large: u8, small: u8) {
    with_shm(|v| {
        // SAFETY: v points at a mapped SHM_SIZE section; rumble_seq @24, large @28, small @29.
        unsafe {
            *v.add(28) = large;
            *v.add(29) = small;
            let seqp = v.add(24) as *mut u32;
            let seq = core::ptr::read_unaligned(seqp).wrapping_add(1);
            core::ptr::write_unaligned(seqp, seq);
        }
    });
}

// Build the 29-byte GET_STATE buffer (the layout xinput1_4 parses).
fn build_get_state() -> [u8; 29] {
    let (packet, buttons, lt, rt, lx, ly, rx, ry) = read_state();
    let mut s = [0u8; 29];
    s[0..2].copy_from_slice(&XUSB_VERSION.to_le_bytes());
    s[2] = 0x01; // device count
    s[5..9].copy_from_slice(&packet.to_le_bytes());
    s[0x0B..0x0D].copy_from_slice(&buttons.to_le_bytes());
    s[0x0D] = lt;
    s[0x0E] = rt;
    s[0x0F..0x11].copy_from_slice(&lx.to_le_bytes());
    s[0x11..0x13].copy_from_slice(&ly.to_le_bytes());
    s[0x13..0x15].copy_from_slice(&rx.to_le_bytes());
    s[0x15..0x17].copy_from_slice(&ry.to_le_bytes());
    s
}

// GET_INFORMATION: 12 bytes — version, device count, VID/PID. Marks the slot connected.
fn build_information() -> [u8; 12] {
    let mut info = [0u8; 12];
    info[0..2].copy_from_slice(&XUSB_VERSION.to_le_bytes());
    info[2] = 0x01; // one device/port
    info[8..10].copy_from_slice(&XUSB_VID.to_le_bytes());
    info[10..12].copy_from_slice(&XUSB_PID.to_le_bytes());
    info
}

// GET_CAPABILITIES V1 (24 bytes): Type=0x03 SubType=0x01 (gamepad), button/stick masks, motor max
// = 0xFFFF (advertise rumble). The V2 (36-byte) form prepends a 16-byte header when WGI asks for 36.
#[rustfmt::skip]
const CAPS_V1: [u8; 24] = [
    0x03, 0x01, 0x00, 0x01, 0xFF, 0xF7, 0xFF, 0xFF,
    0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF,
];

fn build_caps_v2() -> [u8; 36] {
    let mut c = [0u8; 36];
    c[0..6].copy_from_slice(&[0x03, 0x01, 0x01, 0x01, 0x0C, 0x00]);
    c[6..8].copy_from_slice(&XUSB_VID.to_le_bytes());
    c[8..10].copy_from_slice(&XUSB_PID.to_le_bytes());
    c[10..16].copy_from_slice(&[0x10, 0x01, 0x00, 0xFA, 0x34, 0x22]);
    c[16..36].copy_from_slice(&CAPS_V1[4..24]); // the XINPUT_CAPABILITIES struct body
    c
}

extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    output_len: usize,
    input_len: usize,
    ioctl: ULONG,
) {
    let status: NTSTATUS = match ioctl {
        IOCTL_XUSB_GET_INFORMATION => copy_to_output(request, &build_information()),
        IOCTL_XUSB_GET_INFORMATION_EX => {
            let mut ex = [0u8; 64];
            ex[0..2].copy_from_slice(&XUSB_VERSION.to_le_bytes());
            ex[2] = 0x01;
            ex[3] = 0x01;
            ex[8..10].copy_from_slice(&XUSB_VID.to_le_bytes());
            ex[10..12].copy_from_slice(&XUSB_PID.to_le_bytes());
            let n = output_len.min(64);
            copy_to_output(request, &ex[..n])
        }
        IOCTL_XUSB_GET_CAPABILITIES => {
            if output_len >= 36 {
                copy_to_output(request, &build_caps_v2())
            } else {
                copy_to_output(request, &CAPS_V1)
            }
        }
        IOCTL_XUSB_GET_STATE => copy_to_output(request, &build_get_state()),
        IOCTL_XUSB_GET_LED_STATE => copy_to_output(request, &[0x00, 0x00, 0x06]),
        IOCTL_XUSB_GET_BATTERY_INFORMATION => {
            copy_to_output(request, &[0x00, 0x01, 0x03, 0x00])
        }
        IOCTL_XUSB_SET_STATE => on_set_state(request),
        IOCTL_XUSB_POWER_DOWN | IOCTL_XUSB_GET_XINPUT_MANAGEMENT_DRIVER => STATUS_SUCCESS,
        // Decline the async waits → xinput1_4 falls back to synchronous GET_STATE polling.
        IOCTL_XUSB_WAIT_GUIDE_BUTTON | IOCTL_XUSB_WAIT_FOR_INPUT => STATUS_INVALID_DEVICE_REQUEST,
        other => {
            dbglog!("[pf-xusb] unhandled IOCTL 0x{other:08x} in={input_len} out={output_len}");
            STATUS_INVALID_DEVICE_REQUEST
        }
    };
    // SAFETY: request valid and not forwarded.
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
}

// SET_STATE: the rumble packet. Classic xusb22 layout is small; the motor bytes sit near the end.
// We publish a best-effort (large = byte 3, small = byte 4 for the 5-byte form) and log the raw bytes
// so the exact offsets can be confirmed against a real pad.
fn on_set_state(request: WDFREQUEST) -> NTSTATUS {
    let mut inmem: WDFMEMORY = core::ptr::null_mut();
    // SAFETY: request valid.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestRetrieveInputMemory, request, &mut inmem)
    };
    if nt_success(st) {
        let mut len: usize = 0;
        // SAFETY: inmem valid.
        let p = unsafe { call_unsafe_wdf_function_binding!(WdfMemoryGetBuffer, inmem, &mut len) }
            as *const u8;
        if !p.is_null() && len >= 2 {
            let n = len.min(8);
            // SAFETY: p valid for len bytes; read at most n.
            let bytes = unsafe { core::slice::from_raw_parts(p, n) };
            let mut hex = String::new();
            for b in bytes {
                hex.push_str(&format!("{b:02x} "));
            }
            dbglog!("[pf-xusb] SET_STATE len={len} data: {hex}");
            // Observed 5-byte form {00, led, largeMotor, smallMotor, subcmd}: subcmd 0x02 = rumble
            // (large/low-freq at [2], small/high-freq at [3]); 0x01 = player-LED set (ignored).
            // 4-byte = raw XINPUT_VIBRATION → the two motor hi bytes.
            if len >= 5 && bytes[4] == 0x02 {
                publish_rumble(bytes[2], bytes[3]);
            } else if len == 4 {
                publish_rumble(bytes[1], bytes[3]);
            }
        }
    }
    STATUS_SUCCESS
}

// Copy `src` into the request's (buffered) output buffer and set the completed byte count.
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
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestSetInformation, request, src.len() as u64)
    };
    STATUS_SUCCESS
}
