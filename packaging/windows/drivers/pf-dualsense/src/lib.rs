// punktfunk virtual DualSense / DualShock 4 — UMDF2 HID minidriver.
//
// A Rust port of the WDK `vhidmini2` UMDF2 sample, reconfigured to present a Sony DualSense
// (VID 054C / PID 0CE6) or DualShock 4 (device_type=1) using the inputtino report descriptor +
// feature blobs punktfunk already ships in `inject/{dualsense,dualshock4}.rs`. Games see a genuine
// HID PS controller; the host streams input in / reads output (rumble/lightbar/triggers) back.
//
// No WDF object contexts: this is a singleton virtual device, so per-device state lives in statics.
// The host channel is the **sealed pad channel** (design/gamepad-channel-sealing.md, proto v2): the
// whole handshake + all shared-memory access lives in `pf_umdf_util` (the audited unsafe layer), so
// this crate's channel/HID/IOCTL logic is 100% SAFE Rust. The only `unsafe` here is the unavoidable
// WDF setup FFI in DriverEntry/EvtDeviceAdd/the timer, each with a `// SAFETY:` proof.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]
// Every remaining `unsafe {}` (all WDF setup FFI) must carry a `// SAFETY:` proof.
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

use pf_driver_proto::gamepad::PadShm;
use pf_umdf_util::channel::{ChannelClient, ChannelConfig};
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
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000Du32 as NTSTATUS;

use pf_umdf_util::nt_success;

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
const IOCTL_UMDF_HID_SET_FEATURE: u32 = hid_ctl(20);
const IOCTL_UMDF_HID_GET_FEATURE: u32 = hid_ctl(21);
const IOCTL_UMDF_HID_SET_OUTPUT_REPORT: u32 = hid_ctl(22);
const IOCTL_UMDF_HID_GET_INPUT_REPORT: u32 = hid_ctl(23);

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
/// DualShock 4 v2 product id — served (same VID/version) when the host stamps device_type=1.
const DS4_PID: u16 = 0x09CC;

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
static DS_FEATURE_CALIBRATION: [u8; 41] = [ // 0x05 motion calibration: 1 id + 40 data (descriptor declares feature 0x05 as 0x95 0x28 = 40)
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0xF4, 0x01, 0xF4, 0x01, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00,
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

// ---- DualShock 4 v2 assets (served when the host stamps device_type=1) ----
// Sony DualShock 4 v2 USB HID report descriptor (507 bytes), verbatim from inject/dualshock4.rs.
#[rustfmt::skip]
static DS4_RDESC: [u8; 507] = [
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31,
    0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95,
    0x04, 0x81, 0x02, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46,
    0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0E, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x0E, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x20, 0x75, 0x06, 0x95,
    0x01, 0x15, 0x00, 0x25, 0x7F, 0x81, 0x02, 0x05, 0x01, 0x09, 0x33, 0x09,
    0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02,
    0x06, 0x00, 0xFF, 0x09, 0x21, 0x95, 0x36, 0x81, 0x02, 0x85, 0x05, 0x09,
    0x22, 0x95, 0x1F, 0x91, 0x02, 0x85, 0x04, 0x09, 0x23, 0x95, 0x24, 0xB1,
    0x02, 0x85, 0x02, 0x09, 0x24, 0x95, 0x24, 0xB1, 0x02, 0x85, 0x08, 0x09,
    0x25, 0x95, 0x03, 0xB1, 0x02, 0x85, 0x10, 0x09, 0x26, 0x95, 0x04, 0xB1,
    0x02, 0x85, 0x11, 0x09, 0x27, 0x95, 0x02, 0xB1, 0x02, 0x85, 0x12, 0x06,
    0x02, 0xFF, 0x09, 0x21, 0x95, 0x0F, 0xB1, 0x02, 0x85, 0x13, 0x09, 0x22,
    0x95, 0x16, 0xB1, 0x02, 0x85, 0x14, 0x06, 0x05, 0xFF, 0x09, 0x20, 0x95,
    0x10, 0xB1, 0x02, 0x85, 0x15, 0x09, 0x21, 0x95, 0x2C, 0xB1, 0x02, 0x06,
    0x80, 0xFF, 0x85, 0x80, 0x09, 0x20, 0x95, 0x06, 0xB1, 0x02, 0x85, 0x81,
    0x09, 0x21, 0x95, 0x06, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x22, 0x95, 0x05,
    0xB1, 0x02, 0x85, 0x83, 0x09, 0x23, 0x95, 0x01, 0xB1, 0x02, 0x85, 0x84,
    0x09, 0x24, 0x95, 0x04, 0xB1, 0x02, 0x85, 0x85, 0x09, 0x25, 0x95, 0x06,
    0xB1, 0x02, 0x85, 0x86, 0x09, 0x26, 0x95, 0x06, 0xB1, 0x02, 0x85, 0x87,
    0x09, 0x27, 0x95, 0x23, 0xB1, 0x02, 0x85, 0x88, 0x09, 0x28, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0x89, 0x09, 0x29, 0x95, 0x02, 0xB1, 0x02, 0x85, 0x90,
    0x09, 0x30, 0x95, 0x05, 0xB1, 0x02, 0x85, 0x91, 0x09, 0x31, 0x95, 0x03,
    0xB1, 0x02, 0x85, 0x92, 0x09, 0x32, 0x95, 0x03, 0xB1, 0x02, 0x85, 0x93,
    0x09, 0x33, 0x95, 0x0C, 0xB1, 0x02, 0x85, 0x94, 0x09, 0x34, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xA0, 0x09, 0x40, 0x95, 0x06, 0xB1, 0x02, 0x85, 0xA1,
    0x09, 0x41, 0x95, 0x01, 0xB1, 0x02, 0x85, 0xA2, 0x09, 0x42, 0x95, 0x01,
    0xB1, 0x02, 0x85, 0xA3, 0x09, 0x43, 0x95, 0x30, 0xB1, 0x02, 0x85, 0xA4,
    0x09, 0x44, 0x95, 0x0D, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x47, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xF1, 0x09, 0x48, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2,
    0x09, 0x49, 0x95, 0x0F, 0xB1, 0x02, 0x85, 0xA7, 0x09, 0x4A, 0x95, 0x01,
    0xB1, 0x02, 0x85, 0xA8, 0x09, 0x4B, 0x95, 0x01, 0xB1, 0x02, 0x85, 0xA9,
    0x09, 0x4C, 0x95, 0x08, 0xB1, 0x02, 0x85, 0xAA, 0x09, 0x4E, 0x95, 0x01,
    0xB1, 0x02, 0x85, 0xAB, 0x09, 0x4F, 0x95, 0x39, 0xB1, 0x02, 0x85, 0xAC,
    0x09, 0x50, 0x95, 0x39, 0xB1, 0x02, 0x85, 0xAD, 0x09, 0x51, 0x95, 0x0B,
    0xB1, 0x02, 0x85, 0xAE, 0x09, 0x52, 0x95, 0x01, 0xB1, 0x02, 0x85, 0xAF,
    0x09, 0x53, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xB0, 0x09, 0x54, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xE0, 0x09, 0x57, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xB3,
    0x09, 0x55, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xB4, 0x09, 0x55, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xB5, 0x09, 0x56, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xD0,
    0x09, 0x58, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xD4, 0x09, 0x59, 0x95, 0x3F,
    0xB1, 0x02, 0xC0,
];
// DS4 feature reports games read during init (each array's first byte is the report id).
#[rustfmt::skip]
static DS4_FEATURE_PAIRING: [u8; 16] = [ // 0x12 pairing info (MAC at bytes 1..7)
    0x12, 0x01, 0x00, 0xEF, 0xBE, 0xAD, 0xDE, 0x08, 0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
static DS4_FEATURE_CALIBRATION: [u8; 37] = [ // 0x02 IMU calibration
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0xF0, 0xFF, 0x10, 0x00, 0xF0, 0xFF, 0x10,
    0x00, 0xF0, 0xFF, 0x20, 0x00, 0x20, 0x00, 0x00, 0x20, 0x00, 0xE0, 0x00, 0x20, 0x00, 0xE0, 0x00,
    0x20, 0x00, 0xE0, 0x00, 0x00,
];
#[rustfmt::skip]
static DS4_FEATURE_FIRMWARE: [u8; 49] = [ // 0xa3 firmware/build info
    0xA3, 0x41, 0x75, 0x67, 0x20, 0x20, 0x33, 0x20, 0x32, 0x30, 0x31, 0x33, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x30, 0x37, 0x3A, 0x30, 0x31, 0x3A, 0x31, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00,
];

// HID descriptor (9 bytes, packed): len, type=0x21, bcdHID=0x0100, country=0, numDesc=1, then
// {reportType=0x22, wReportLength}. DualSense = 273 (0x0111); DualShock 4 = 507 (0x01FB).
static HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x11, 0x01];
static DS4_HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0xFB, 0x01];

// HID_DEVICE_ATTRIBUTES (32 bytes): Size(u32)=32, VendorID, ProductID, VersionNumber, Reserved[11].
// `ds4` selects the DualShock 4 product id (same VID/version).
fn hid_attrs(ds4: bool) -> [u8; 32] {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&32u32.to_le_bytes());
    a[4..6].copy_from_slice(&DS_VID.to_le_bytes());
    a[6..8].copy_from_slice(&(if ds4 { DS4_PID } else { DS_PID }).to_le_bytes());
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
// Neutral DualShock 4 input report 0x01: sticks centered (0x80); the dpad hat is in byte 5 (low
// nibble), so a neutral hat (8) lands there instead of byte 8.
const DS4_NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01; // report id
    r[1] = 0x80; // LX
    r[2] = 0x80; // LY
    r[3] = 0x80; // RX
    r[4] = 0x80; // RY
    r[5] = 0x08; // buttons[0]: low nibble = dpad hat (8 = neutral), high nibble = face buttons (0)
    r
};
fn neutral_report(ds4: bool) -> [u8; 64] {
    if ds4 {
        DS4_NEUTRAL_REPORT
    } else {
        NEUTRAL_REPORT
    }
}

static MANUAL_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// The latest input report the host pushed (report `0x01`) via shared memory; the timer delivers it
/// to pended game READ_REPORTs. Defaults to neutral until the host connects.
static INPUT_REPORT: std::sync::Mutex<[u8; 64]> = std::sync::Mutex::new(NEUTRAL_REPORT);

// ---- the sealed pad channel: layouts + offsets from pf_driver_proto (drift = compile error) ----
// UMDF runs in WUDFHost.exe (user-mode) and hidclass blocks a control channel on the device stack
// (custom interface CreateFile → err 31; custom IOCTL on the HID handle → err 1) and UMDF has no
// control device. So the DATA section (`PadShm`, 256 B — input report @8, output seq @72, output
// report @76, device_type @140, health marks @144/@148, pad_index @152) is UNNAMED and reached only
// through a handle the SYSTEM host duplicated into this WUDFHost, bootstrapped over the named mailbox
// `Global\pfds-boot-<index>`. The handshake + all shared-memory access live in `pf_umdf_util`.
const SHM_MAGIC: u32 = pf_driver_proto::gamepad::PAD_MAGIC; // "PFDS"
const SHM_SIZE: usize = core::mem::size_of::<PadShm>();
const GAMEPAD_PROTO_VERSION: u32 = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION;

// PadShm field offsets (the driver reads input + device_type, writes output + health marks).
const OFF_INPUT: usize = core::mem::offset_of!(PadShm, input);
const OFF_OUT_SEQ: usize = core::mem::offset_of!(PadShm, out_seq);
const OFF_OUTPUT: usize = core::mem::offset_of!(PadShm, output);
const OFF_DEVICE_TYPE: usize = core::mem::offset_of!(PadShm, device_type);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(PadShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(PadShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(PadShm, pad_index);

/// The sealed-channel client (per-pad: `ProcessSharingDisabled` gives each pad its own WUDFHost, so
/// this static is per-pad). The handshake/adoption/validation state machine lives in `pf_umdf_util`.
static CHANNEL: ChannelClient = ChannelClient::new();
/// The last observed `device_type` (0 = DualSense, 1 = DualShock 4) — the neutral-report shape when
/// the channel detaches, and the fallback identity while unattached.
static LAST_DEVTYPE: AtomicU32 = AtomicU32::new(0);
/// device_type()'s bounded first-read wait fires at most once (see its docs).
static DEVTYPE_WAITED: AtomicBool = AtomicBool::new(false);

/// This pad's channel config (magic/size/pad_index offset + our logger).
fn channel_cfg() -> ChannelConfig {
    ChannelConfig {
        tag: "pf-ds",
        boot_name_prefix: "Global\\pfds-boot-",
        data_magic: SHM_MAGIC,
        data_size: SHM_SIZE,
        pad_index_off: OFF_PAD_INDEX,
        log,
    }
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

    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let shm_idx = unsafe { wdf::query_location_index(device) };
    CHANNEL.set_index(shm_idx);
    dbglog!("[pf-ds] shm index = {shm_idx}");

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
            "[pf-ds] default WdfIoQueueCreate failed 0x{:08x}",
            st as u32
        );
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
        dbglog!("[pf-ds] WdfTimerCreate failed 0x{:08x}", st as u32);
        return st;
    }
    // SAFETY: timer valid; -80000 == 8ms relative due time (100ns units, negative = relative).
    let _started = unsafe { call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -80000i64) };

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
    // SAFETY: `request` is the live request for THIS EvtIoDeviceControl invocation — exactly the
    // contract `Request::new` requires. Everything after is safe (the token owns completion).
    let request = unsafe { Request::new(request) };

    // Skip the 8ms READ_REPORT cadence so the log stays readable during a game test;
    // the 0x02 OUTPUT report (the gate) and the descriptor handshake still log.
    if ioctl != IOCTL_HID_READ_REPORT {
        dbglog!("[pf-ds] ioctl 0x{ioctl:08x} out={_output_len} in={_input_len}");
    }

    // READ_REPORT forwards to the manual queue (the timer completes it) — this CONSUMES the request
    // token, so it's handled apart from the status-and-complete paths below.
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
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => request.copy_to_output(if device_type() == 1 {
            &DS4_HID_DESC
        } else {
            &HID_DESC
        }),
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => request.copy_to_output(&hid_attrs(device_type() == 1)),
        IOCTL_HID_GET_REPORT_DESCRIPTOR => request.copy_to_output(if device_type() == 1 {
            &DS4_RDESC[..]
        } else {
            &DUALSENSE_RDESC[..]
        }),
        IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT => {
            on_output_report(&request, ioctl)
        }
        IOCTL_UMDF_HID_SET_FEATURE => {
            log("[pf-ds] SET_FEATURE (stub ok)");
            STATUS_SUCCESS
        }
        IOCTL_UMDF_HID_GET_FEATURE => on_get_feature(&request),
        IOCTL_UMDF_HID_GET_INPUT_REPORT => {
            request.copy_to_output(&neutral_report(device_type() == 1))
        }
        IOCTL_HID_GET_STRING => on_get_string(&request),
        _ => STATUS_NOT_IMPLEMENTED,
    };

    dbglog!("[pf-ds] ioctl 0x{ioctl:08x} -> 0x{:08x}", status as u32);
    request.complete(status);
}

// The 0x02 gate: a game writing an output report (rumble / lightbar / ADAPTIVE TRIGGERS). Per the
// UMDF marshalling convention the report data is the *input* buffer and the report id is carried in
// the *output* buffer length. We log it, then publish it to the DATA section for the host.
fn on_output_report(request: &Request, ioctl: ULONG) -> NTSTATUS {
    let (bytes, inlen) = match request.input_bytes(64) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let report_id = request.output_buffer_len() as u32; // report id, UMDF convention

    let mut hex = String::new();
    for b in bytes.iter().take(48) {
        hex.push_str(&format!("{b:02x} "));
    }
    let kind = if ioctl == IOCTL_HID_WRITE_REPORT {
        "WRITE_REPORT"
    } else {
        "SET_OUTPUT_REPORT"
    };
    dbglog!("[pf-ds] *** OUTPUT {kind} reportId={report_id} len={inlen} data: {hex}");

    // Publish the game's 0x02 output report to the sealed DATA section for the host (rumble /
    // lightbar / player-LEDs / adaptive triggers), then bump the host-polled output seq.
    if !bytes.is_empty()
        && let Some(view) = CHANNEL.data()
    {
        view.write_bytes(OFF_OUTPUT, &bytes);
        let seq = view.read_u32(OFF_OUT_SEQ).wrapping_add(1);
        view.write_u32(OFF_OUT_SEQ, seq);
    }

    request.set_information(inlen as u64);
    STATUS_SUCCESS
}

// GET_FEATURE: report id from the input buffer; reply with the matching DualSense/DualShock 4 blob.
fn on_get_feature(request: &Request) -> NTSTATUS {
    let (bytes, _) = match request.input_bytes(1) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let Some(&report_id) = bytes.first() else {
        return STATUS_INVALID_PARAMETER;
    };
    // DualSense uses feature ids 0x05/0x09/0x20; DualShock 4 uses 0x02/0x12/0xa3.
    let blob: &[u8] = match (device_type() == 1, report_id) {
        (false, 0x05) => &DS_FEATURE_CALIBRATION,
        (false, 0x09) => &DS_FEATURE_PAIRING,
        (false, 0x20) => &DS_FEATURE_FIRMWARE,
        (true, 0x02) => &DS4_FEATURE_CALIBRATION,
        (true, 0x12) => &DS4_FEATURE_PAIRING,
        (true, 0xA3) => &DS4_FEATURE_FIRMWARE,
        (_, other) => {
            dbglog!("[pf-ds] GET_FEATURE unknown report id 0x{other:02x}");
            return STATUS_INVALID_PARAMETER;
        }
    };
    request.copy_to_output(blob)
}

// IOCTL_HID_GET_STRING: the input is a ULONG whose low word is the string id and whose high word is
// the language id. Reply with the requested device string as a NUL-terminated UTF-16 buffer. Native
// PS5 / Steam code reads these (HidD_GetProductString / HidD_GetSerialNumberString — the serial is one
// way they tell USB from BT). Observed live: Windows polls ids 0x0E/0x0F/0x10 (lang 0x0409)
// cyclically — the manufacturer/product/serial slots — NOT the 0/1/2 HID_STRING_ID_* constants; both.
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
    let ds4 = device_type() == 1;
    dbglog!("[pf-ds] GET_STRING id=0x{string_id:04x} (raw 0x{id_val:08x}) ds4={ds4}");
    let s: &str = match string_id {
        0 | 0x000e => {
            if ds4 {
                "Sony Computer Entertainment"
            } else {
                "Sony Interactive Entertainment"
            }
        }
        2 | 0x0010 => {
            if ds4 {
                "DEADBEEF0001"
            } else {
                "35533AD6E774"
            }
        }
        _ => {
            if ds4 {
                "Wireless Controller"
            } else {
                "DualSense Wireless Controller"
            }
        }
    };
    let mut wide: Vec<u8> = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16() {
        wide.extend_from_slice(&u.to_le_bytes());
    }
    wide.extend_from_slice(&[0, 0]); // NUL terminator (UTF-16)
    request.copy_to_output(&wide)
}

/// The host's device-type selector from the sealed DATA section (`device_type` @140): 0 = DualSense
/// (default), 1 = DualShock 4. Read fresh on each enumeration query — cheap. If the channel hasn't
/// attached when hidclass first asks (the host stamps the section + eager-delivers before
/// `SwDeviceCreate` returns, but the handshake can be a few ms behind), pump the channel briefly —
/// ONCE — for the delivery: a DS4 pad must not enumerate with the default DualSense identity because
/// of a lost race. After that one bounded wait, fall back to the last observed type.
fn device_type() -> u8 {
    if let Some(view) = CHANNEL.data() {
        let t = view.read_u8(OFF_DEVICE_TYPE);
        LAST_DEVTYPE.store(t as u32, Ordering::Relaxed);
        return t;
    }
    if !DEVTYPE_WAITED.swap(true, Ordering::SeqCst) {
        let cfg = channel_cfg();
        for _ in 0..100 {
            if let Some(view) = CHANNEL.pump(&cfg) {
                let t = view.read_u8(OFF_DEVICE_TYPE);
                LAST_DEVTYPE.store(t as u32, Ordering::Relaxed);
                return t;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        dbglog!(
            "[pf-ds] device_type: sealed channel not attached within 1s — defaulting to the last observed identity"
        );
    }
    LAST_DEVTYPE.load(Ordering::Relaxed) as u8
}

extern "C" fn evt_timer(timer: WDFTIMER) {
    // One sealed-channel tick: publish our pid / adopt a delivery / detect host-gone, then pull the
    // latest host input report from the attached DATA section (all safe, via pf_umdf_util).
    match CHANNEL.pump(&channel_cfg()) {
        Some(view) => {
            let mut buf = [0u8; 64];
            view.read_bytes(OFF_INPUT, &mut buf);
            if buf[0] == 0x01
                && let Ok(mut g) = INPUT_REPORT.lock()
            {
                *g = buf;
            }
            // Health marks the host watches: driver_proto (attach signal, idempotent) and
            // driver_heartbeat (+1 per ~8 ms tick = liveness). Lets the host tell "driver bound
            // and alive" apart from "driver package missing/failed to bind".
            view.write_u32(OFF_DRIVER_PROTO, GAMEPAD_PROTO_VERSION);
            let hb = view.read_u32(OFF_DRIVER_HEARTBEAT).wrapping_add(1);
            view.write_u32(OFF_DRIVER_HEARTBEAT, hb);
        }
        None => {
            // Host gone (mailbox name vanished) or channel not attached yet: feed games the neutral
            // report instead of a frozen last state (matters for the persistent out-of-band devnode,
            // which outlives host sessions).
            if let Ok(mut g) = INPUT_REPORT.lock() {
                *g = neutral_report(LAST_DEVTYPE.load(Ordering::Relaxed) == 1);
            }
        }
    }

    // Complete the next pended READ_REPORT with the current input report (safe queue/request API).
    // SAFETY: the timer's parent object is the manual queue (set in EvtDeviceAdd); the framework
    // guarantees a live handle here.
    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfTimerGetParentObject, timer) } as WDFQUEUE;
    // SAFETY: `queue` is that live manual queue — the exact contract `retrieve_next_request` needs.
    if let Some(request) = unsafe { wdf::retrieve_next_request(queue) } {
        let report = INPUT_REPORT.lock().map(|g| *g).unwrap_or(NEUTRAL_REPORT);
        let st = request.copy_to_output(&report);
        request.complete(st);
    }
}
