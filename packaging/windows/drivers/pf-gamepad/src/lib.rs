// punktfunk virtual DualSense / DualShock 4 / DualSense Edge — UMDF2 HID minidriver.
//
// A Rust port of the WDK `vhidmini2` UMDF2 sample, reconfigured to present a Sony DualSense
// (VID 054C / PID 0CE6), DualShock 4 (device_type=1), DualSense Edge (device_type=2), Steam Deck
// (device_type=3), Xbox Wireless Controller (device_type=4, VID 045E / PID 0B13), Xbox One S
// (device_type=5, 045E / 02FD) or Xbox Elite Wireless Controller Series 2 (device_type=6,
// 045E / 0B22) using the
// report descriptors + feature blobs punktfunk already ships in `inject/`. Games see a genuine
// HID PS controller; the host streams input in / reads output (rumble/lightbar/triggers) back.
//
// No WDF object contexts: this is a singleton virtual device, so per-device state lives in statics.
// The host channel is the **sealed pad channel** (design/gamepad-channel-sealing.md, proto v2): the
// whole handshake + all shared-memory access lives in `pf_umdf_util` (the audited unsafe layer), so
// this crate's channel/HID/IOCTL logic is 100% SAFE Rust. The only `unsafe` here is the unavoidable
// WDF setup FFI in DriverEntry/EvtDeviceAdd/the timer, each with a `// SAFETY:` proof.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]
// Every remaining `unsafe {}` (all WDF setup FFI) must carry a `// SAFETY:` proof.

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
/// DualSense Edge product id — served (same VID/version) when the host stamps device_type=2.
const DS_EDGE_PID: u16 = 0x0DF2;
/// The Steam Deck controller identity (Valve 28DE:1205), served when the host stamps
/// device_type=3. Started as the N4 spike (gamepad-new-types §6) answering "does Steam Input on
/// Windows promote a software-devnode HID Deck?"; it is now a shipping identity — every Steam Deck
/// CLIENT streaming to a Windows host declares it, and `steam_deck_windows` builds the pad.
const DECK_VID: u16 = 0x28DE;
const DECK_PID: u16 = 0x1205;
/// Steam Controller 2 ("Triton", 28DE:1302 wired), served when the host stamps device_type=7 —
/// same Valve VID as the Deck.
const TRITON_PID: u16 = 0x1302;
/// bcdDevice of the real wired Triton (Phase-0 bench capture). Unlike the Deck we do NOT borrow
/// `DS_VER` here: 0x0307 is the captured value, and the whole point of this identity is fidelity
/// to the capture.
const TRITON_VER: u16 = 0x0307;

// ---- Xbox identities (device_type = 4 Wireless / 5 One S / 6 Elite Series 2) ----
//
// WHY THIS EXISTS (field 2026-08-09, `punktfunk-field-windows-pad-dead-0260`): the OTHER Windows
// Xbox backend — `pf-xusb` — registers ONLY `GUID_DEVINTERFACE_XUSB` and has no HID collection at
// all, so it is invisible to Steam's hidapi enumeration, to DirectInput, to `joy.cpl`, and to
// WGI/GameInput. Only classic `XInputGetState` via xinput1_4's interface walk ever sees it. A
// reporter spent two weeks on a dead controller for exactly that reason, and switching the client
// to DualSense — a REAL HID pad through this driver — fixed it instantly. This identity gives the
// Xbox pad the same HID footing the PlayStation ones have always had.
//
// ⚠️⚠️ **The VID/PID is a BLUETOOTH Xbox controller on purpose.** The wired ids the rest of the
// tree uses (`045E:028E` X-Box 360, `045E:02EA` Xbox One S USB) are vendor-class XUSB/GIP devices —
// they expose NO HID interface on real hardware, so a HID child claiming one is a device that has
// never existed and inbox promotion has nothing to match. The Xbox pads that genuinely ARE HID are
// the Bluetooth ones, which Windows binds through HIDCLASS.
const XBOX_VID: u16 = 0x045E;
/// Xbox Wireless Controller (Series X|S), Bluetooth — `device_type = 4`, the default Xbox identity.
/// Chosen over the Xbox One S BT id `0x02FD` because the host's OS floor is Windows 11 22H2, where
/// this is the current-generation identity (so glyphs read "Xbox Series") and SDL's mapping
/// database covers it.
///
/// ⭐ It is also the PID Microsoft's own `xinputhid.inf` allow-lists **twice** (once as a
/// `BTHLEDevice` stage-1 id, once as a plain `HID\…&IG_00` stage-2 id) — measured off `.173`,
/// 2026-08-09. That is not what promotes OUR pad (a software devnode matches no allow-list entry;
/// `pfGamepadXbox`'s `AddReg` writes what the matching sections would have written), but it is why
/// this stays the default of the three.
const XBOX_PID: u16 = 0x0B13;
/// Xbox One S controller over Bluetooth — `device_type = 5`.
///
/// ⚠️ **`02FD` appears in `xinputhid.inf` only as a `BTHENUM` (classic-BT bus) id — it has NO
/// stage-2 `HID\…&IG_00` model line.** That killed it as a "try another PID" lever for the
/// promotion work (handoff §4 B1). It does not block it as an IDENTITY, because our promotion
/// comes from the INF's own `AddReg` rather than from matching Microsoft's list — but if a future
/// Windows servicing update makes promotion depend on the allow-list again, this identity is the
/// one that loses it first. Worth re-measuring on glass before recommending it to anyone.
const XBOX_PID_ONE_S: u16 = 0x02FD;
/// Xbox Elite Wireless Controller Series 2 — `device_type = 6`. This is the pad
/// `tools/hid-descriptor-dump` captured on `.173` (`BTHLE\DEV_686CE647F191`, `REV_0521`), so it is
/// the one identity here whose real hardware we have measured directly.
const XBOX_PID_ELITE2: u16 = 0x0B22;
/// bcdDevice for every Xbox identity.
///
/// Deliberately ONE value rather than per-identity: the real Elite reports `REV_0521` (measured on
/// `.173`) but `create_swdevice` synthesizes the devnode's USB ids with a hardcoded `&REV_0100`
/// regardless, and SDL folds the version into its joystick GUID — so a version that disagrees with
/// the devnode buys nothing and risks missing a stock mapping. Revisit only with a measurement
/// that shows a consumer keying on it.
const XBOX_VER: u16 = 0x0407;

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
static DS_FEATURE_FIRMWARE: [u8; 64] = [ // 0x20 firmware info; bytes 44..46 = update version,
    // kept ABOVE Sony's real releases (0x0630 as of 2026-08) — an older value makes PlayStation
    // Accessories and libScePad titles demand a firmware update the virtual pad cannot take.
    // Mirrors inject/proto/dualsense_proto.rs DS_FEATURE_FIRMWARE; keep the two in sync.
    0x20, 0x4A, 0x75, 0x6E, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3A, 0x34,
    0x37, 0x3A, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xC1, 0xC8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x99, 0x09, 0x00, 0x00,
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
// 0x02 IMU calibration. A consumer (SDL's `SDL_hidapi_ps4`, or `hid-playstation` when this pad
// is read on Linux) DERIVES its motion scale from these words rather than assuming one: gyro
// resolution = (|pitch_plus| + |pitch_minus|) / (speed_plus + speed_minus) LSB per °/s, accel
// resolution = (acc_plus - acc_minus) / 2 LSB per g. So this blob is where the wire contract
// (20 LSB/°·s, 10000 LSB/g) is declared on the DS4 device type, and it must state exactly what
// the wire delivers — the pre-2026-08 values (±16 / speed 32 / ±8192) declared 0.5 LSB/°·s and
// 8192 LSB/g, i.e. every DS4 session read gyro 40× too fast and accel 1.22× hot.
// Mirrors inject/proto/dualshock4_proto.rs DS4_FEATURE_CALIBRATION; this WDK workspace can't
// depend on pf-inject, so pf-inject's `motion_contract` test parses THIS file and re-derives the
// units from it. Keep the two in sync.
#[rustfmt::skip]
static DS4_FEATURE_CALIBRATION: [u8; 37] = [
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0xF4, 0x01, 0xF4, 0x01, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0x00, 0x00,
];
#[rustfmt::skip]
static DS4_FEATURE_FIRMWARE: [u8; 49] = [ // 0xa3 firmware/build info
    0xA3, 0x41, 0x75, 0x67, 0x20, 0x20, 0x33, 0x20, 0x32, 0x30, 0x31, 0x33, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x30, 0x37, 0x3A, 0x30, 0x31, 0x3A, 0x31, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00,
];

// ---- DualSense Edge assets (served when the host stamps device_type=2) ----
// Sony DualSense Edge USB HID report descriptor (389 bytes), verbatim from
// inject/proto/dualsense_proto.rs (a real-device capture; see the provenance note there). Input
// report 0x01 is bit-identical to the plain DualSense — the Edge's Fn/back buttons ride reserved
// bits of buttons[2]; output report 0x02 grows to 63 bytes and 19 profile feature reports are added.
#[rustfmt::skip]
static DS_EDGE_RDESC: [u8; 389] = [
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x3F, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xB1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2F, 0xB1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xB1, 0x02, 0x85, 0x0A, 0x09, 0x25, 0x95, 0x1A, 0xB1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xB1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x2A, 0x95, 0x09, 0xB1, 0x02,
    0x85, 0x83, 0x09, 0x2B, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x84, 0x09, 0x2C, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x85, 0x09, 0x2D, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xA0, 0x09, 0x2E, 0x95, 0x01, 0xB1, 0x02,
    0x85, 0xE0, 0x09, 0x2F, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x30, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0xF1, 0x09, 0x31, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2, 0x09, 0x32, 0x95, 0x34, 0xB1, 0x02,
    0x85, 0xF4, 0x09, 0x35, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF5, 0x09, 0x36, 0x95, 0x03, 0xB1, 0x02,
    0x85, 0x60, 0x09, 0x41, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x61, 0x09, 0x42, 0xB1, 0x02, 0x85, 0x62,
    0x09, 0x43, 0xB1, 0x02, 0x85, 0x63, 0x09, 0x44, 0xB1, 0x02, 0x85, 0x64, 0x09, 0x45, 0xB1, 0x02,
    0x85, 0x65, 0x09, 0x46, 0xB1, 0x02, 0x85, 0x68, 0x09, 0x47, 0xB1, 0x02, 0x85, 0x70, 0x09, 0x48,
    0xB1, 0x02, 0x85, 0x71, 0x09, 0x49, 0xB1, 0x02, 0x85, 0x72, 0x09, 0x4A, 0xB1, 0x02, 0x85, 0x73,
    0x09, 0x4B, 0xB1, 0x02, 0x85, 0x74, 0x09, 0x4C, 0xB1, 0x02, 0x85, 0x75, 0x09, 0x4D, 0xB1, 0x02,
    0x85, 0x76, 0x09, 0x4E, 0xB1, 0x02, 0x85, 0x77, 0x09, 0x4F, 0xB1, 0x02, 0x85, 0x78, 0x09, 0x50,
    0xB1, 0x02, 0x85, 0x79, 0x09, 0x51, 0xB1, 0x02, 0x85, 0x7A, 0x09, 0x52, 0xB1, 0x02, 0x85, 0x7B,
    0x09, 0x53, 0xB1, 0x02, 0xC0,
];

// ---- N4-spike Steam Deck assets (served when the host stamps device_type=3) ----
// The Deck's captured CONTROLLER-interface report descriptor (38 bytes, interface 2 of a real
// 28DE:1205 — verbatim from inject/proto/steam_proto.rs RDESC_DECK_CTRL): one vendor-defined
// (page 0xFFFF) collection with a 64-byte input + 64-byte feature report.
#[rustfmt::skip]
static DECK_RDESC: [u8; 38] = [
    0x06, 0xff, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x09, 0x02, 0x09, 0x03, 0x15, 0x00, 0x26, 0xff, 0x00,
    0x75, 0x08, 0x95, 0x40, 0x81, 0x02, 0x09, 0x06, 0x09, 0x07, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
    0x08, 0x95, 0x40, 0xb1, 0x02, 0xc0,
];

// ---- Xbox assets (served when the host stamps device_type = 4, 5 or 6) ----
//
// ⭐⭐ **ONE DESCRIPTOR SERVES ALL THREE XBOX IDENTITIES, DELIBERATELY.** Xbox Wireless (4),
// Xbox One S (5) and Xbox Elite Series 2 (6) differ ONLY in VID/PID, product string and INF model
// line — in HID terms they are the same pad: same two 16-bit stick pairs, same trigger pair, same
// hat, same 15 buttons, same rumble output report. A report descriptor is the report SHAPE, not
// the identity; the identity is what SDL/Steam/Windows key their stock mappings off, and that
// travels in `hid_attrs`.
//
// This is load-bearing, not laziness. The ⚠️ block below is the record of what ONE hand-written
// descriptor has already cost: three separate bugs (no Feature report ⇒ the sealed channel never
// opened and the pad served neutral forever; no OUTPUT item ⇒ no rumble of any kind and dead
// host-side code; a layout that provably disagrees with the captured hardware). Two more
// hand-written descriptors would multiply that debt by three for no measured gain, and each would
// need its own capture, its own `wReportLength`, its own `xbox_proto` layout tests and its own
// on-glass verification. When a Linux-hidraw capture settles the real layout (handoff §3.3), it
// lands here ONCE and all three identities get it.
//
// A standards-clean Game Pad collection matching the Bluetooth Xbox layout: two 16-bit stick pairs,
// two 10-bit triggers on the Simulation page, a null-state hat, and 15 buttons. Report `0x01`,
// [`XBOX_INPUT_REPORT_LEN`] bytes on the wire including the id. `inject/proto/xbox_proto.rs` packs
// the matching bytes host-side; `xbox_proto`'s tests pin the two together.
//
// ⚠️⚠️⚠️ **PROVENANCE: this descriptor is CONSTRUCTED, not captured — unlike every sibling here
// (`DUALSENSE_RDESC` verbatim from inputtino, `DS4_RDESC` verbatim from `inject/dualshock4.rs`,
// `DECK_RDESC` captured off a real `28DE:1205`). It has never been compared against a real pad.**
// That matters more than usual: we claim a REAL Microsoft VID/PID, and SDL / Steam / Windows keep
// built-in mappings keyed off that VID/PID. If a consumer applies its stock `045E:0B13` mapping to a
// report laid out differently from the real device, every control silently lands on the wrong
// action — the same class of bug this whole change exists to kill.
//
// ⭐ **2026-08-09 — THE CAPTURE NOW EXISTS AND THIS BLOB DISAGREES WITH IT.** A real Xbox Elite
// Series 2 (`045E:0B22`, Bluetooth LE) was captured on `.173` with `tools/hid-descriptor-dump`; the
// dump, its provenance and the DualSense control that validates the tool are in
// `tools/hid-descriptor-dump/captures/`. Re-take it any time with `--vid 045E --pid 0B22`, and
// decode THIS array through the same decoder — no hardware needed — with:
//
//     hid-descriptor-dump --rust-source packaging/windows/drivers/pf-gamepad/src/lib.rs \
//                         --symbol XBOX_RDESC
//
// Four differences, and the ORDER one is the dangerous one:
//   * the real pad's game-controller report is **UNNUMBERED** (15 bytes of fields, no report id);
//     this one declares Report ID 1;
//   * it carries **ONE combined 16-bit `Z`** trigger axis at byte 8, not two Simulation-page axes;
//   * it declares **16 buttons at byte 10, BEFORE the hat** — this one puts 15 buttons AFTER it;
//   * neither has an OUTPUT collection, so the rumble gap is real on both.
//
// 🛑 **Do NOT simply paste the capture over this array.** Two blockers, recorded in
// `design/xbox-pad-windows-handoff.md` §3.3: (1) it is unverified whether Windows' view equals the
// pad's NATIVE report map — `xinputhid` filters that pad and the captured shape is the legacy
// DirectInput view, so cross-check on Linux hidraw first; (2) **the real descriptor has no Feature
// report, and we cannot ship without one** — `0x85` is the sealed channel's proof transport, and
// report ids are all-or-nothing, so declaring it forces a numbered input report the real pad does
// not have. Matching the hardware byte for byte and keeping the sealed channel as it stands are
// mutually exclusive; that needs a decision, not a paste. Whatever lands, re-run `xbox_proto`'s
// layout tests — they pin these offsets on the host side.
//
// ⚠️ The trailing vendor-defined Feature report `0x85` is NOT cosmetic and must not be trimmed as
// "unused": it is the CHANNEL PROOF transport (`ProofTransport::HidFeatureReport`). The captured
// PlayStation descriptors already declared `0x85`, which is why the proof "costs no descriptor
// change" there — but this descriptor is constructed, so it has to declare the report itself. Built
// without it the pad enumerates perfectly and then delivers NOTHING: hidclass rejects the host's
// `HidD_GetFeature` before the driver sees it, the host refuses to hand over the DATA section
// (measured on .173 2026-08-09 — WGI `RawGameController` saw `045E:0B13` with every axis pinned at
// 0.5000 and a timestamp frozen for 12 consecutive samples), and the pad serves only its neutral
// report forever. `0x3F` payload bytes so `FeatureReportByteLength` lands on 64, the buffer size
// `channel_proof::query` asks with; the proof itself needs 17.
#[rustfmt::skip]
static XBOX_RDESC: [u8; 223] = [
    0x05, 0x01,                    // Usage Page (Generic Desktop)
    0x09, 0x05,                    // Usage (Game Pad)
    0xA1, 0x01,                    // Collection (Application)
    0x85, 0x01,                    //   Report ID (1)
    0x09, 0x01,                    //   Usage (Pointer)
    0xA1, 0x00,                    //   Collection (Physical)
    0x09, 0x30,                    //     Usage (X)          — left stick X
    0x09, 0x31,                    //     Usage (Y)          — left stick Y
    0x15, 0x00,                    //     Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00,  //     Logical Maximum (65535)
    0x95, 0x02,                    //     Report Count (2)
    0x75, 0x10,                    //     Report Size (16)
    0x81, 0x02,                    //     Input (Data,Var,Abs)
    0xC0,                          //   End Collection
    // 🛑 THE RIGHT STICK IS `Z`/`Rz`, NOT `Rx`/`Ry`. This declared `Rx`/`Ry` until 2026-08-09 and
    // the right stick was DEAD: measured on `.173`, with every axis sweeping on its own phase,
    // `LX`/`LY`/`LT`/`RT` all reached XInput and `RX [0..0] RY [-1..-1]` never moved. Left and right
    // were declared identically here apart from these two usage bytes, so the usages are the whole
    // difference — `xinputhid`, which translates this collection into XUSB, maps `Z`/`Rz` to the
    // right stick and does not treat `Rx`/`Ry` as one. `DUALSENSE_RDESC` above (a real capture) uses
    // `Z`/`Rz` for its right stick too; the PS pads put the TRIGGERS on `Rx`/`Ry`, which is probably
    // where the original mistake came from.
    // ⚠️ This survived every bench measurement because the devtest only ever swept LS-X — the axis
    // that worked — so `RX [0..0]` read as "nothing is driving it". It was found on glass. The
    // devtest now sweeps all six axes on distinct phases so the harness can tell those two apart.
    0x09, 0x01,                    //   Usage (Pointer)
    0xA1, 0x00,                    //   Collection (Physical)
    0x09, 0x32,                    //     Usage (Z)          — right stick X
    0x09, 0x35,                    //     Usage (Rz)         — right stick Y
    0x15, 0x00,                    //     Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00,  //     Logical Maximum (65535)
    0x95, 0x02,                    //     Report Count (2)
    0x75, 0x10,                    //     Report Size (16)
    0x81, 0x02,                    //     Input (Data,Var,Abs)
    0xC0,                          //   End Collection
    0x05, 0x02,                    //   Usage Page (Simulation Controls)
    0x09, 0xC5,                    //   Usage (Brake)        — left trigger
    0x15, 0x00,                    //   Logical Minimum (0)
    0x26, 0xFF, 0x03,              //   Logical Maximum (1023)
    0x95, 0x01,                    //   Report Count (1)
    0x75, 0x10,                    //   Report Size (16)
    0x81, 0x02,                    //   Input (Data,Var,Abs)
    0x09, 0xC4,                    //   Usage (Accelerator)  — right trigger
    0x15, 0x00,                    //   Logical Minimum (0)
    0x26, 0xFF, 0x03,              //   Logical Maximum (1023)
    0x95, 0x01,                    //   Report Count (1)
    0x75, 0x10,                    //   Report Size (16)
    0x81, 0x02,                    //   Input (Data,Var,Abs)
    0x05, 0x01,                    //   Usage Page (Generic Desktop)
    0x09, 0x39,                    //   Usage (Hat switch)
    0x15, 0x01,                    //   Logical Minimum (1)
    0x25, 0x08,                    //   Logical Maximum (8)
    0x35, 0x00,                    //   Physical Minimum (0)
    0x46, 0x3B, 0x01,              //   Physical Maximum (315)
    0x65, 0x14,                    //   Unit (Eng Rot: Degrees)
    0x75, 0x04,                    //   Report Size (4)
    0x95, 0x01,                    //   Report Count (1)
    0x81, 0x42,                    //   Input (Data,Var,Abs,Null State)
    0x65, 0x00,                    //   Unit (None)
    0x75, 0x04,                    //   Report Size (4)
    0x95, 0x01,                    //   Report Count (1)
    0x81, 0x03,                    //   Input (Cnst,Var,Abs) — pad the hat byte
    0x05, 0x09,                    //   Usage Page (Button)
    0x19, 0x01,                    //   Usage Minimum (Button 1)
    0x29, 0x0F,                    //   Usage Maximum (Button 15)
    0x15, 0x00,                    //   Logical Minimum (0)
    0x25, 0x01,                    //   Logical Maximum (1)
    0x75, 0x01,                    //   Report Size (1)
    0x95, 0x0F,                    //   Report Count (15)
    0x81, 0x02,                    //   Input (Data,Var,Abs)
    0x75, 0x01,                    //   Report Size (1)
    0x95, 0x01,                    //   Report Count (1)
    0x81, 0x03,                    //   Input (Cnst,Var,Abs) — pad to a byte boundary
    // ---- Rumble OUTPUT report `0x03` (Physical Interface Device page) ----
    //
    // Without this the pad can receive NOTHING. hidclass routes an output report only if the
    // descriptor declares one, so with no `0x91` item `on_output_report` never fires,
    // `publish_output` never writes the ring, and `parse_xbox_output`
    // (`inject/windows/xbox_windows.rs`) is unreachable code — the whole host-side rumble plane is
    // already built and was simply never fed. That is why the HID Xbox pad had no rumble at all,
    // not merely no trigger rumble.
    //
    // ⚠️ PROVENANCE — HAND-WRITTEN, and it could not be otherwise. Every other output collection in
    // this file is a capture, and §3 of `design/xbox-pad-windows-handoff.md` insists on captures.
    // But the Elite capture taken for that work reports `OUTPUT items: 0` (Windows exposes no
    // literal report-descriptor bytes; hidapi reconstructs from `HidD_GetPreparsedData`, and that
    // reconstruction carries no output collection for this pad). So there was nothing to copy.
    // This block is the documented Xbox One S / Elite Bluetooth rumble report — PID-page
    // `Set Effect Report`, id `0x03`, 8 payload bytes — chosen because it is exactly the layout
    // `parse_xbox_output` and `design/trigger-rumble-plane.md` §2.1 already specify:
    //     [0x03][enable][left_trigger][right_trigger][left][right][duration][delay][loop]
    // with magnitudes 0..100 (hence `Logical Maximum (100)`, not 255).
    // **Replace it with a Linux hidraw capture when one can be taken** — that is the only route to
    // byte-exact truth here, and the enable-bit assignments for the two TRIGGER actuators remain
    // unverified (see trigger-rumble-plane.md WP0).
    //
    // Declared AFTER the final Input item and re-stating every global it uses, so it cannot
    // retroactively alter the 16-byte input layout `xbox_proto`'s tests pin.
    0x05, 0x0F,                    //   Usage Page (Physical Interface Device)
    0x09, 0x21,                    //   Usage (Set Effect Report)
    0x85, 0x03,                    //   Report ID (3)
    0xA1, 0x02,                    //   Collection (Logical)
    0x09, 0x97,                    //     Usage (DC Enable Actuators)
    0x15, 0x00,                    //     Logical Minimum (0)
    0x25, 0x01,                    //     Logical Maximum (1)
    0x75, 0x04,                    //     Report Size (4)
    0x95, 0x01,                    //     Report Count (1)
    0x91, 0x02,                    //     Output (Data,Var,Abs) — the enable mask, low nibble
    0x15, 0x00,                    //     Logical Minimum (0)
    0x25, 0x00,                    //     Logical Maximum (0)
    0x75, 0x04,                    //     Report Size (4)
    0x95, 0x01,                    //     Report Count (1)
    0x91, 0x03,                    //     Output (Cnst,Var,Abs) — pad the enable byte
    0x09, 0x70,                    //     Usage (Magnitude)
    0x15, 0x00,                    //     Logical Minimum (0)
    0x25, 0x64,                    //     Logical Maximum (100) — percent, NOT 255
    0x75, 0x08,                    //     Report Size (8)
    0x95, 0x04,                    //     Report Count (4) — LT, RT, left handle, right handle
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0x09, 0x50,                    //     Usage (Duration)
    0x66, 0x01, 0x10,              //     Unit (SI Linear: seconds)
    0x55, 0x0E,                    //     Unit Exponent (-2) — centiseconds
    0x15, 0x00,                    //     Logical Minimum (0)
    0x26, 0xFF, 0x00,              //     Logical Maximum (255)
    0x75, 0x08,                    //     Report Size (8)
    0x95, 0x01,                    //     Report Count (1)
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0x09, 0xA7,                    //     Usage (Start Delay) — same unit and range as Duration
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0x65, 0x00,                    //     Unit (None)
    0x55, 0x00,                    //     Unit Exponent (0)
    0x09, 0x7C,                    //     Usage (Loop Count)
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0xC0,                          //   End Collection
    // The channel-proof feature report — see the ⚠️ above. Declared last so it cannot disturb the
    // INPUT layout `xbox_proto` packs against: every global item here (Report Size/Count, Logical
    // Min/Max) is re-stated after the final Input item, so nothing above is retroactively changed.
    0x06, 0x00, 0xFF,              //   Usage Page (Vendor Defined 0xFF00)
    0x85, 0x85,                    //   Report ID (0x85)
    0x09, 0x2D,                    //   Usage (0x2D) — the id the PS descriptors use for it
    0x15, 0x00,                    //   Logical Minimum (0)
    0x26, 0xFF, 0x00,              //   Logical Maximum (255)
    0x75, 0x08,                    //   Report Size (8)
    0x95, 0x3F,                    //   Report Count (63) — 1 id + 63 = 64 = FeatureReportByteLength
    0xB1, 0x02,                    //   Feature (Data,Var,Abs)
    0xC0,                          // End Collection
];

/// Bytes the Xbox input report occupies on the wire, report id included — 1 id + 8 sticks +
/// 4 triggers + 1 hat + 2 buttons. hidclass sizes its READ_REPORT buffer from the descriptor, and
/// [`Request::copy_to_output`] REFUSES a source longer than that buffer (it does not truncate), so
/// the completion path must serve exactly this many bytes. See [`input_report_len`].
const XBOX_INPUT_REPORT_LEN: usize = 16;

// HID descriptor (9 bytes, packed): len, type=0x21, bcdHID=0x0100, country=0, numDesc=1, then
// {reportType=0x22, wReportLength}. DualSense = 273 (0x0111); DualShock 4 = 507 (0x01FB);
// DualSense Edge = 389 (0x0185).
static HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x11, 0x01];
static DS4_HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0xFB, 0x01];
static EDGE_HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x85, 0x01];
static DECK_HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x26, 0x00]; // 38 bytes
// Serves device_type 4, 5 AND 6 — one descriptor, three identities (see the XBOX_RDESC header).
static XBOX_HID_DESC: [u8; 9] = [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0xDF, 0x00]; // 223 bytes
// bcdHID 0x0111 (bytes 2-3) is the real capture's value — the other identities declare
// 0x0100; declared_len never reads it, this is deliberate identity fidelity.
static TRITON_HID_DESC: [u8; 9] = [0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x74, 0x01]; // 372 bytes

// Each `wReportLength` above is a SECOND copy of a length that already exists as its descriptor's
// array size, and the two are edited in different places. Getting them out of step does not fail
// loudly — hidclass asks for `wReportLength` bytes and then parses whatever it got, so the pad
// either enumerates with a truncated descriptor or fails to enumerate at all, with nothing naming
// the cause. Assert the pairing at compile time instead; adding an item to a descriptor now cannot
// build until its length is updated too.
const fn declared_len(hid_desc: &[u8; 9]) -> usize {
    (hid_desc[7] as usize) | ((hid_desc[8] as usize) << 8)
}
const _: () = assert!(declared_len(&HID_DESC) == DUALSENSE_RDESC.len());
const _: () = assert!(declared_len(&DS4_HID_DESC) == DS4_RDESC.len());
const _: () = assert!(declared_len(&EDGE_HID_DESC) == DS_EDGE_RDESC.len());
const _: () = assert!(declared_len(&DECK_HID_DESC) == DECK_RDESC.len());
const _: () = assert!(declared_len(&XBOX_HID_DESC) == XBOX_RDESC.len());
const _: () = assert!(declared_len(&TRITON_HID_DESC) == pf_driver_proto::triton::RDESC.len());

// HID_DEVICE_ATTRIBUTES (32 bytes): Size(u32)=32, VendorID, ProductID, VersionNumber, Reserved[11].
// `devtype` selects the identity: PS family (same Sony VID/version), the N4-spike Deck, or one of
// the three Xbox pads (same Microsoft VID/version — only the PID differs, which is the entire
// difference between them; they share a report descriptor).
//
// ⚠️ THIS is where an Xbox identity is actually decided. Everything else in the Xbox path —
// descriptor, HID descriptor, report length, neutral report — is shared, so a new Xbox model is a
// PID here, a product string in `on_get_string`, an INF model line and nothing else.
fn hid_attrs(devtype: u8) -> [u8; 32] {
    let (vid, pid, ver) = match devtype {
        1 => (DS_VID, DS4_PID, DS_VER),
        2 => (DS_VID, DS_EDGE_PID, DS_VER),
        3 => (DECK_VID, DECK_PID, DS_VER),
        4 => (XBOX_VID, XBOX_PID, XBOX_VER),
        5 => (XBOX_VID, XBOX_PID_ONE_S, XBOX_VER),
        6 => (XBOX_VID, XBOX_PID_ELITE2, XBOX_VER),
        7 => (DECK_VID, TRITON_PID, TRITON_VER),
        _ => (DS_VID, DS_PID, DS_VER),
    };
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&32u32.to_le_bytes());
    a[4..6].copy_from_slice(&vid.to_le_bytes());
    a[6..8].copy_from_slice(&pid.to_le_bytes());
    a[8..10].copy_from_slice(&ver.to_le_bytes());
    a
}

/// Bytes to hand a pended `IOCTL_HID_READ_REPORT`, per identity.
///
/// The PlayStation/Deck identities all declare 64-byte input reports, which is why the report slot
/// and [`INPUT_REPORT`] are 64 bytes wide and the completion path could hand the whole buffer over
/// unconditionally. The Xbox identity declares a [`XBOX_INPUT_REPORT_LEN`]-byte report, and
/// [`Request::copy_to_output`] returns `STATUS_INVALID_BUFFER_SIZE` when the source is LONGER than
/// the caller's buffer rather than truncating — so handing hidclass 64 bytes for a 16-byte report
/// fails every single read and the pad looks dead.
///
/// Returns 64 for every pre-existing identity, so this is provably a no-op for them. All three
/// Xbox identities share one descriptor, hence one report length. The Triton identity (7) gets
/// 54 — its LARGEST declared input report (0x42, id byte included), the length hidclass sizes a
/// natural `HidD_GetInputReport` buffer from. The `evt_timer` serve path never consults this
/// function for the Triton (it trims each served report to
/// `pf_driver_proto::triton::input_len(id)` per id), so the ONLY consumer this arm affects is the
/// `IOCTL_UMDF_HID_GET_INPUT_REPORT` arm, which serves
/// `neutral_report(dt)[..input_report_len(dt)]` — with the 64 default it handed a 64-byte source
/// to that natural 54-byte buffer, and `copy_to_output` refuses source > buffer
/// (`STATUS_INVALID_BUFFER_SIZE`) rather than truncating, failing every such GET.
fn input_report_len(devtype: u8) -> usize {
    match devtype {
        4..=6 => XBOX_INPUT_REPORT_LEN,
        // = `triton::input_len(0x42)`, the largest input the 372-byte descriptor declares.
        7 => 54,
        _ => 64,
    }
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
// Neutral Steam Deck input frame (unnumbered): header [0x01, 0x00, ID_CONTROLLER_DECK_STATE=0x09,
// payload-len 0x3C], everything released.
const DECK_NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01;
    r[2] = 0x09;
    r[3] = 0x3C;
    r
};
// Neutral Xbox input report 0x01: both sticks centred (0x8000 on a 0..65535 axis), triggers 0,
// hat 0 (the descriptor's NULL state — the logical range starts at 1), no buttons held. Only the
// first [`XBOX_INPUT_REPORT_LEN`] bytes are ever served; the rest of the 64-byte slot stays zero so
// the shared [`INPUT_REPORT`] type is unchanged.
const XBOX_NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01; // report id
    r[2] = 0x80; // LX = 0x8000 (little-endian)
    r[3] = 0xFF; // LY = 0x7FFF — the Y axes are INVERTED (+y is up on the wire, down in HID),
    r[4] = 0x7F; //   and mirroring an even-sized range centres one unit low. See `xbox_proto`.
    r[6] = 0x80; // RX = 0x8000
    r[7] = 0xFF; // RY = 0x7FFF
    r[8] = 0x7F;
    r
};
// Neutral wired-Triton 0x42 state report: id + an all-zero payload — the same canned shape the
// host's `neutral_triton_report` (triton_windows.rs) seeds the section with. `static`, not
// `const` like its siblings, so the timer's completion path can serve
// `&TRITON_NEUTRAL_REPORT[..54]` as a `'static` slice (a const would borrow a temporary).
static TRITON_NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x42; // ID_CONTROLLER_STATE, the wired Triton's input state report
    r
};
fn neutral_report(devtype: u8) -> [u8; 64] {
    match devtype {
        1 => DS4_NEUTRAL_REPORT,
        3 => DECK_NEUTRAL_REPORT,
        // Wireless / One S / Elite Series 2 — one report shape, three identities.
        4..=6 => XBOX_NEUTRAL_REPORT,
        7 => TRITON_NEUTRAL_REPORT,
        _ => NEUTRAL_REPORT, // DualSense and Edge share the report 0x01 shape
    }
}

static MANUAL_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// The latest input report the host pushed (report `0x01`) via shared memory; the timer delivers it
/// to pended game READ_REPORTs. Defaults to neutral until the host connects.
static INPUT_REPORT: std::sync::Mutex<[u8; 64]> = std::sync::Mutex::new(NEUTRAL_REPORT);
/// Whether [`INPUT_REPORT`] holds a value no pended READ_REPORT has been completed with yet. Set
/// only when the latch actually CHANGES, cleared only when a request is actually completed, so a
/// tick that finds no read pended leaves the report undelivered rather than losing it. Consulted
/// by the Triton identity alone — see the delivery gate in [`evt_timer`].
static INPUT_DIRTY: AtomicBool = AtomicBool::new(true);

// ---- the sealed pad channel: layouts + offsets from pf_driver_proto (drift = compile error) ----
// UMDF runs in WUDFHost.exe (user-mode) and hidclass blocks a control channel on the device stack
// (custom interface CreateFile → err 31; custom IOCTL on the HID handle → err 1) and UMDF has no
// control device. So the DATA section (`PadShm` — input report @8, output seq @72, output
// report @76, device_type @140, health marks @144/@148, pad_index @152, output-report ring
// @156..) is UNNAMED and reached only
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
// v2.1/v2.2 output-report ring (see PadShm docs in pf_driver_proto).
const OFF_OUT_RING_VER: usize = core::mem::offset_of!(PadShm, out_ring_ver);
const OFF_RING_HEAD: usize = core::mem::offset_of!(PadShm, ring_head);
const OFF_OUT_RING_LEN: usize = core::mem::offset_of!(PadShm, out_ring_len);
const OFF_OUT_RING: usize = core::mem::offset_of!(PadShm, out_ring);
const OFF_INPUT_GEN: usize = core::mem::offset_of!(PadShm, input_gen);

/// How many timer ticks separate two runs of the channel/health housekeeping. The tick itself is
/// [`TIMER_PERIOD_MS`]; the pump, the `driver_proto` stamp and the heartbeat keep their historical
/// ~8 ms cadence so nothing that watches them changes rate — only the input path got faster.
const PUMP_EVERY_N_TICKS: u32 = 4;
/// Timer period. Was 8 ms, which — with one pended READ_REPORT completed per tick — capped what a
/// game could observe at ~125 Hz and added up to 8 ms of latency, while clients stream motion at
/// ~250 Hz. 2 ms is about a real DualShock 4's Bluetooth cadence and leaves headroom above the
/// client rate; the extra ticks only do the cheap half (read the input slot, complete one pended
/// read), see [`PUMP_EVERY_N_TICKS`].
const TIMER_PERIOD_MS: u32 = 2;

/// Read the host's input report out of the section under the v2.3 seqlock, so a report caught
/// mid-copy is retried instead of handed to a game.
///
/// The host takes `input_gen` odd before writing the 64 bytes and even after, so an odd sample or
/// a changed one means the read straddled a write. One retry: the host publishes in microseconds
/// and this runs on a 2 ms timer, so a second collision is not a thing that happens, and if it did,
/// re-serving the previous whole report beats serving a torn one.
///
/// Against a pre-v2.3 host the field is never written, so it reads 0 — constant and even — and
/// this accepts on the first pass, exactly as the driver behaved before the seqlock existed.
/// `false` means "no whole report available"; the caller keeps what it had.
fn read_input_report(view: &pf_umdf_util::section::MappedView, buf: &mut [u8; 64]) -> bool {
    for _ in 0..2 {
        let before = view.load_u32(OFF_INPUT_GEN, Ordering::Acquire);
        if !before.is_multiple_of(2) {
            continue; // a write is in flight right now
        }
        view.read_bytes(OFF_INPUT, buf);
        // Acquire: the body reads above must not sink below this sample of the generation.
        if view.load_u32(OFF_INPUT_GEN, Ordering::Acquire) == before {
            return true;
        }
    }
    false
}
const OUT_SLOT_SIZE: usize = core::mem::size_of::<pf_driver_proto::gamepad::OutSlot>();
const OUT_RING_LEN: u32 = pf_driver_proto::gamepad::OUT_RING_LEN;
const OUT_RING_LEN_V22: u32 = pf_driver_proto::gamepad::OUT_RING_LEN_V22;

/// The output-ring length this side's slot math uses against the attached section — the driver's
/// half of the v2.2 negotiation (PadShm docs): the host's `out_ring_ver` stamp declares what it
/// can drain, the mapped length proves the slots exist in OUR view, and the shorter understanding
/// wins. `0` = no ring (pre-v2.1 host, or a fallback-size map too small to hold one) — legacy
/// latest-slot only. Constant per attachment (both inputs are fixed once the view exists).
fn ring_len(view: &pf_umdf_util::section::MappedView) -> u32 {
    if view.read_u32(OFF_OUT_RING_VER) == 0
        || view.mapped_len() < pf_driver_proto::gamepad::PAD_SHM_V21_SIZE
    {
        return 0;
    }
    if view.read_u32(OFF_OUT_RING_VER) >= 2
        && view.mapped_len() >= pf_driver_proto::gamepad::PAD_SHM_SIZE
    {
        OUT_RING_LEN_V22
    } else {
        OUT_RING_LEN
    }
}

/// Publish one game output report to the host: the legacy latest-report slot + `out_seq` bump
/// (every host generation reads this), and — when the host stamped `out_ring_ver` (it created the
/// ring region) and our view maps it — the lossless report ring: slot bytes first, then the
/// [`ring_len`] echo (the v2.2 length negotiation — a pre-v2.2 host never reads it), then the
/// `ring_head` bump with `Release` so the host's Acquire load can never observe the bump without
/// the slot bytes and the length that indexed them. The ring is what stops a rumble-STOP report
/// from being coalesced away by a following LED/trigger report inside one host poll window (the
/// confirmed stuck-rumble path).
///
/// `feature` ORs [`pf_driver_proto::triton::OUT_FEATURE_BIT`] (bit 31) into the ring slot's len —
/// the Triton identity's FEATURE/OUTPUT kind tag, stripped back out by the host's `drain_tagged`.
/// Only the ring carries the tag; the legacy latest-slot has no length field to tag, which is fine
/// because the one consumer that needs the split (triton_windows) always drains the ring. Every
/// pre-Triton call site passes `false` (the Deck host expects untagged frames), so plain lengths
/// are bit-identical to before the parameter existed.
fn publish_output(view: &pf_umdf_util::section::MappedView, bytes: &[u8], feature: bool) {
    // Serialized: the whole publish is a read-modify-write (read the cursor, write the slot it
    // names, then advance it) and the framework dispatches output callbacks in PARALLEL, so two
    // can be inside this at once. Unsynchronized, both read the same `ring_head`, both write the
    // SAME slot — tearing one report's bytes across the other's — and both store head+1, so the
    // cursor advances once for two reports and the host sees a single torn entry.
    //
    // An atomic `fetch_add` on the head does not fix it. That hands each writer a distinct slot,
    // but it advances the cursor BEFORE the slot bytes exist, so the host can read a slot that is
    // still being filled — trading a torn slot for a torn slot the host is invited to read. Making
    // the head-advance mean "the slot below is complete" is exactly what the lock buys.
    //
    // Poison-tolerant on purpose. Poison is sticky, so the repo's usual `if let Ok(g) = lock()`
    // would skip the publish for the REST OF THE PROCESS after a single panic elsewhere — silently
    // ending game output. Recovering the guard is safe here: the protected state is bytes in a
    // shared section, not an invariant a panic could have broken.
    let _publish = RING_PUBLISH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    view.write_bytes(OFF_OUTPUT, bytes);
    let seq = view.read_u32(OFF_OUT_SEQ).wrapping_add(1);
    // Release, not a plain write: the host loads `out_seq` with Acquire specifically to order its
    // copy of the report bytes after it (`dualsense_windows.rs`, "Acquire pairs with the driver's
    // publish-then-bump store order"). An Acquire load pairs with a Release store and nothing
    // else, so as a plain write this promised the host an ordering it never actually established —
    // on a weakly-ordered core (ARM64) the fresh seq could arrive ahead of the bytes it announces.
    view.store_u32(OFF_OUT_SEQ, seq, Ordering::Release);
    let len = ring_len(view);
    if len != 0 {
        let head = view.read_u32(OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % len) as usize * OUT_SLOT_SIZE;
        let n = bytes.len().min(64);
        let tag = if feature {
            pf_driver_proto::triton::OUT_FEATURE_BIT
        } else {
            0
        };
        view.write_u32(slot, n as u32 | tag);
        view.write_bytes(slot + 4, &bytes[..n]);
        view.write_u32(OFF_OUT_RING_LEN, len);
        view.store_u32(OFF_RING_HEAD, head.wrapping_add(1), Ordering::Release);
    }
}

/// Serializes [`publish_output`] against itself — see the note there for why an atomic cursor is
/// not enough. Uncontended in the common case: one output report at a time is the norm, and the
/// critical section is a few dozen bytes of memcpy into an already-mapped view.
static RING_PUBLISH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The sealed-channel client (per-pad: `ProcessSharingDisabled` gives each pad its own WUDFHost, so
/// this static is per-pad). The handshake/adoption/validation state machine lives in `pf_umdf_util`.
static CHANNEL: ChannelClient = ChannelClient::new();
/// The last observed `device_type` (0 = DualSense, 1 = DualShock 4, 2 = DualSense Edge,
/// 3 = Steam Deck, 4 = Xbox Wireless, 5 = Xbox One S, 6 = Xbox Elite Series 2,
/// 7 = Steam Controller 2 ("Triton")) — the neutral-report shape when the channel detaches,
/// and the fallback identity while unattached.
static LAST_DEVTYPE: AtomicU32 = AtomicU32::new(0);
/// The identity resolved from the devnode's PnP hardware ids at `EvtDeviceAdd` ([`devtype_from_hwids`]);
/// `u32::MAX` = not resolved. See [`device_type`] for why this exists.
static PNP_DEVTYPE: AtomicU32 = AtomicU32::new(u32::MAX);
/// Timer ticks since load — picks the [`PUMP_EVERY_N_TICKS`] ticks that also do the channel
/// handshake and health marks. Wrapping is fine: only its residue matters.
static TICK: AtomicU32 = AtomicU32::new(0);

/// Map a devnode's hardware-id list (lowercase, `;`-separated — see
/// [`wdf::query_hardware_ids`](pf_umdf_util::wdf::query_hardware_ids)) to the `device_type` the host
/// stamps into the section. The host picks one `pf_*` id per identity and lists it FIRST (it is the
/// INF binding contract, pinned by `dualsense_windows::drain_tests::hwid_matches_inf`), so the two
/// can never disagree.
///
/// Order matters: `pf_dualsense` is a prefix of `pf_dualsenseedge`, so the Edge is tested first.
/// (No Xbox token is a prefix of another — `pf_xboxwireless` / `pf_xboxones` / `pf_xboxelite`
/// diverge at the 8th character — but `hwid_devtype_table_matches_the_driver` re-checks that for
/// every pair rather than trusting this note.)
fn devtype_from_hwids(ids: &str) -> Option<u8> {
    for (token, devtype) in [
        ("pf_xboxwireless", 4u8),
        ("pf_xboxones", 5),
        ("pf_xboxelite", 6),
        ("pf_triton", 7),
        ("pf_steamdeck", 3),
        ("pf_dualsenseedge", 2),
        ("pf_dualshock4", 1),
        ("pf_dualsense", 0),
    ] {
        if ids.contains(token) {
            return Some(devtype);
        }
    }
    None
}

/// This pad's channel config (magic/size/pad_index offset + our logger).
fn channel_cfg() -> ChannelConfig {
    ChannelConfig {
        tag: "pf-gamepad",
        boot_name_prefix: "Global\\pfds-boot-",
        data_magic: SHM_MAGIC,
        data_size: SHM_SIZE,
        // The v2.1 layout grew by tail extension (the output-report ring); against an old host's
        // 256-byte section the full-size map still succeeds (sections are page-granular), but if
        // it is ever refused, mapping the legacy size keeps the pad alive with the ring disabled.
        min_data_size: pf_driver_proto::gamepad::PAD_SHM_LEGACY_SIZE,
        pad_index_off: OFF_PAD_INDEX,
        log,
    }
}

/// The wire pad index the host stamped into the sealed section (0 while the channel hasn't
/// attached yet). Keys every per-pad identity surface: the Deck unit id + serial, the PS
/// identities' pairing MAC (feature 0x09/0x12) and USB serial string — SDL/Steam dedup
/// controllers by serial, so two virtual pads must never share one (identical serials make a
/// second pad read as the FIRST one re-appearing over another transport, and it is merged).
fn pad_index() -> u8 {
    (CHANNEL
        .data()
        .map(|v| v.read_u32(OFF_PAD_INDEX))
        .unwrap_or(0)
        & 0xFF) as u8
}

/// Whether the world-writable bring-up file log is enabled (resolved once). OPT-IN — debug builds,
/// or the `PFGAMEPAD_DEBUG_LOG` (system-wide) env var — the same treatment pf-vdisplay got in audit
/// §4.4: a RELEASE driver never writes the Public file (info-leak/DoS surface), and the per-report
/// OUTPUT hex dumps stop being a sustained disk-write path during gameplay. DebugView can't see the
/// UMDF host across session 0, so the file stays the bring-up diagnostic when enabled.
fn file_log_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(debug_assertions) || std::env::var_os("PFGAMEPAD_DEBUG_LOG").is_some())
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
            // WUDFHost's own (LocalService) temp dir — NOT world-writable/readable `C:\Users\Public`,
            // where the OUTPUT/feature-report hex dumps could leak per-pad identity/serial material to
            // any local reader (security-review 2026-07-17). Opt-in/debug only.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::env::temp_dir().join("pf_gamepad-driver.log"))
                .ok()
                .map(std::sync::Mutex::new)
        })
        .as_ref()
}

fn log(s: &str) {
    // Gated as a whole on [`file_log_enabled`] (the pf-xusb/pf-mouse treatment): `OutputDebugStringA`
    // used to fire unconditionally — a syscall + CString alloc per logged event in a RELEASE driver,
    // on per-IOCTL paths (the OUTPUT hex dumps during rumble, the cyclic GET_STRING polls). Debug
    // builds and the env-var opt-in keep the full debug-string + file tee.
    if !file_log_enabled() {
        return;
    }
    if let Ok(c) = std::ffi::CString::new(s) {
        // SAFETY: c is a valid null-terminated string for the duration of the call.
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
    log("[pf-gamepad] DriverEntry");
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
    log("[pf-gamepad] EvtDeviceAdd");

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
        dbglog!("[pf-gamepad] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let shm_idx = unsafe { wdf::query_location_index(device) };
    CHANNEL.set_index(shm_idx);
    dbglog!("[pf-gamepad] shm index = {shm_idx}");

    // Settle WHICH controller we are before hidclass asks (see `device_type`): the PnP hardware ids
    // are the only identity available this early, and every descriptor/attribute answer depends on it.
    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let hwids = unsafe { wdf::query_hardware_ids(device) };
    match devtype_from_hwids(&hwids) {
        Some(t) => {
            PNP_DEVTYPE.store(t as u32, Ordering::Relaxed);
            LAST_DEVTYPE.store(t as u32, Ordering::Relaxed);
            dbglog!("[pf-gamepad] identity from PnP hardware ids: device_type={t} ({hwids})");
        }
        // No pf_* id: an unexpected devnode (or a property query that failed). Keep the historical
        // behaviour — wait for the channel, then fall back to DualSense.
        None => dbglog!(
            "[pf-gamepad] no pf_* hardware id in ({hwids}) — identity deferred to the channel"
        ),
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
        dbglog!(
            "[pf-gamepad] default WdfIoQueueCreate failed 0x{:08x}",
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
        dbglog!(
            "[pf-gamepad] manual WdfIoQueueCreate failed 0x{:08x}",
            st as u32
        );
        return st;
    }
    MANUAL_QUEUE.store(manual_queue, Ordering::SeqCst);

    // Periodic timer (parent = manual queue) completes pended reads with the neutral report.
    // SAFETY: zeroed config then fields set.
    let mut tcfg: WDF_TIMER_CONFIG = unsafe { core::mem::zeroed() };
    tcfg.Size = core::mem::size_of::<WDF_TIMER_CONFIG>() as ULONG;
    tcfg.EvtTimerFunc = Some(evt_timer);
    tcfg.Period = TIMER_PERIOD_MS;
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
        dbglog!("[pf-gamepad] WdfTimerCreate failed 0x{:08x}", st as u32);
        return st;
    }
    let due = -(TIMER_PERIOD_MS as i64) * 10_000;
    // SAFETY: timer valid; the due time is TIMER_PERIOD_MS in 100 ns units, negative = relative.
    let _started = unsafe { call_unsafe_wdf_function_binding!(WdfTimerStart, timer, due) };

    log("[pf-gamepad] device ready");
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
        dbglog!("[pf-gamepad] ioctl 0x{ioctl:08x} out={_output_len} in={_input_len}");
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
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => request.copy_to_output(match device_type() {
            1 => &DS4_HID_DESC,
            2 => &EDGE_HID_DESC,
            3 => &DECK_HID_DESC,
            4..=6 => &XBOX_HID_DESC,
            7 => &TRITON_HID_DESC,
            _ => &HID_DESC,
        }),
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => request.copy_to_output(&hid_attrs(device_type())),
        // The three Xbox identities share ONE report descriptor on purpose — see the XBOX_RDESC
        // header. Only `hid_attrs` (VID/PID) and `on_get_string` (product string) tell them apart.
        IOCTL_HID_GET_REPORT_DESCRIPTOR => request.copy_to_output(match device_type() {
            1 => &DS4_RDESC[..],
            2 => &DS_EDGE_RDESC[..],
            3 => &DECK_RDESC[..],
            4..=6 => &XBOX_RDESC[..],
            // The Triton's captured 372-byte descriptor lives in the shared proto crate — the
            // host and the pf-inject layout tests read the SAME bytes (drift = test failure).
            7 => &pf_driver_proto::triton::RDESC[..],
            _ => &DUALSENSE_RDESC[..],
        }),
        IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT => {
            on_output_report(&request, ioctl)
        }
        IOCTL_UMDF_HID_SET_FEATURE => on_set_feature(&request),
        IOCTL_UMDF_HID_GET_FEATURE => on_get_feature(&request),
        // Sliced to the identity's declared report length for the same reason the timer's
        // completion is (see `input_report_len`): a source longer than the caller's buffer is
        // refused outright, not truncated. Serves the CURRENT latch, not neutral — a reader that
        // opens mid-session (Steam restarting during a held input) queries the true state instead
        // of a fabricated all-zeros one. Does NOT touch `INPUT_DIRTY`: this is an on-demand
        // query, and consuming the dirty flag here would starve the interrupt pipeline of a
        // report it still owes. Before any host publish the latch is the neutral default anyway.
        IOCTL_UMDF_HID_GET_INPUT_REPORT => {
            let dt = device_type();
            let report = INPUT_REPORT.lock().map(|g| *g).unwrap_or(NEUTRAL_REPORT);
            let served: &[u8] = if dt == pf_driver_proto::gamepad::DEVTYPE_TRITON {
                // Same per-id trim as the timer's completion: Triton input reports are
                // variable-length and id-first; an undeclared latched id falls back to neutral.
                match pf_driver_proto::triton::input_len(report[0]) {
                    Some(len) => &report[..len],
                    None => &TRITON_NEUTRAL_REPORT[..54],
                }
            } else {
                &report[..input_report_len(dt)]
            };
            request.copy_to_output(served)
        }
        IOCTL_HID_GET_STRING => on_get_string(&request),
        // The channel proof (see `pf_umdf_util::hid`): the host asks THIS devnode which process
        // serves it, and duplicates the DATA section into the answer — so it never has to trust the
        // LocalService-writable bootstrap mailbox to name its target.
        _ => STATUS_NOT_IMPLEMENTED,
    };

    dbglog!(
        "[pf-gamepad] ioctl 0x{ioctl:08x} -> 0x{:08x}",
        status as u32
    );
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
    dbglog!("[pf-gamepad] *** OUTPUT {kind} reportId={report_id} len={inlen} data: {hex}");

    // Publish the game's 0x02 output report to the sealed DATA section for the host (rumble /
    // lightbar / player-LEDs / adaptive triggers): legacy slot + seq, plus the v2.1 ring.
    // Triton OUTPUT reports (0x80.. haptics) flow through here too, untagged = OUTPUT kind; the
    // largest declared ones (0x87/0x88/0x89, 1 id + 63 payload = 64) exactly fit the 64-byte
    // ring slot, so nothing is ever truncated.
    if !bytes.is_empty()
        && let Some(view) = CHANNEL.data()
    {
        publish_output(view, &bytes, false);
    }

    request.set_information(inlen as u64);
    STATUS_SUCCESS
}

/// Deck identity: the last SET_FEATURE payload (the Steam command byte + args, minus the
/// report-id prefix). Steam's Deck contract is command-in-SET_FEATURE → answer-in-GET_FEATURE
/// on the one unnumbered feature report; the PS identities ignore this (their SET_FEATUREs are
/// fire-and-forget) — acking them is all they need.
static LAST_SET_FEATURE: std::sync::Mutex<[u8; 64]> = std::sync::Mutex::new([0; 64]);

/// Triton identity: the last SET_FEATURE frame, WHOLE (id-first, exactly as marshalled) plus its
/// true length — `pf_driver_proto::triton::feature_reply` wants the frame as SET, and the host's
/// drain replays the same bytes verbatim. Separate from [`LAST_SET_FEATURE`] because that latch
/// strips a leading `0x00` (the Deck's unnumbered-report marshalling), which would mangle a
/// numbered Triton frame. Per-pad like every static here: `ProcessSharingDisabled` gives each pad
/// its own WUDFHost (see [`INPUT_REPORT`]).
static TRITON_LAST_SET: std::sync::Mutex<([u8; 64], usize)> = std::sync::Mutex::new(([0; 64], 0));

/// Whether a latched Triton SET_FEATURE frame is the host's channel-proof command — the SAME
/// two-byte [`pf_driver_proto::gamepad::DECK_PROOF_CMD`] the Deck identity answers, riding the
/// same SET→GET feature contract. The `[0x00, cmd, …]` shape is the Deck/UNNUMBERED-report
/// marshalling of `channel_proof::ask_feature`; a numbered collection like this one never sees
/// it — hidclass rejects a feature buffer whose byte 0 is not a declared nonzero report id — so
/// the host's numbered leg frames the proof id-first, `[0x01, cmd, …]`. The driver accepts the
/// bare, `0x00`- and `0x01`-prefixed shapes alike to cover every sender rather than pinning one
/// marshalling (mirroring `triton::feature_reply`'s tolerance).
fn triton_proof_requested(frame: &[u8]) -> bool {
    let body = match frame {
        [0x00 | 0x01, rest @ ..] => rest,
        d => d,
    };
    body.starts_with(&pf_driver_proto::gamepad::DECK_PROOF_CMD)
}

// SET_FEATURE: ack (the PS identities' contract), latch the payload for the Deck's/Triton's
// GET_FEATURE answer, and — the Deck + Triton feedback paths — publish Steam's commands to the
// host. Per the UMDF marshalling convention the report data is the input buffer.
fn on_set_feature(request: &Request) -> NTSTATUS {
    if let Ok((bytes, _)) = request.input_bytes(64) {
        if device_type() == pf_driver_proto::gamepad::DEVTYPE_TRITON {
            // Latch the WHOLE id-first frame (see TRITON_LAST_SET), then republish it to the
            // host FEATURE-tagged — Steam's SET_REPORT features (lizard-off / IMU-enable /
            // settings) must reach the physical pad, and the tag is how the host's
            // `drain_tagged` tells them from interrupt OUTPUT reports.
            let n = bytes.len().min(64);
            if let Ok(mut g) = TRITON_LAST_SET.lock() {
                g.0.fill(0);
                g.0[..n].copy_from_slice(&bytes[..n]);
                g.1 = n;
            }
            if triton_proof_requested(&bytes[..n]) {
                // The channel-proof exchange is host↔driver plumbing; the client must never
                // see it — latched for the GET answer, NOT republished.
            } else if let Some(view) = CHANNEL.data() {
                publish_output(view, &bytes[..n], true);
            }
        } else {
            // The wire carries [report-id 0, cmd, …] for the unnumbered Steam report; store the
            // command-first view. (PS set-features carry their own report id first — harmless.)
            let src: &[u8] = if bytes.first() == Some(&0x00) && bytes.len() > 1 {
                &bytes[1..]
            } else {
                &bytes
            };
            if let Ok(mut g) = LAST_SET_FEATURE.lock() {
                g.fill(0);
                let n = src.len().min(64);
                g[..n].copy_from_slice(&src[..n]);
            }
            // Deck feedback: Steam drives rumble (0xEB) and trackpad haptic pulses (0x8F) via
            // SET_FEATURE on the unnumbered report — the PS identities get theirs as OUTPUT
            // reports instead. Publish them to the host through the same output slot + seq the
            // output path uses, re-prefixed with the report-id 0 byte so the host's
            // `parse_steam_output` sees the exact wire shape the Linux UHID path delivers.
            // Untagged: the Deck host expects plain frames.
            if device_type() == 3
                && matches!(src.first(), Some(&0xEB) | Some(&0x8F))
                && let Some(view) = CHANNEL.data()
            {
                let mut out = [0u8; 64];
                let n = src.len().min(63);
                out[1..1 + n].copy_from_slice(&src[..n]);
                publish_output(view, &out, false);
            }
        }
    }
    dbglog!("[pf-gamepad] SET_FEATURE (acked, latched for GET)");
    STATUS_SUCCESS
}

/// Deck identity: build the GET_FEATURE reply from the latched SET_FEATURE command — the
/// 0x83 GET_ATTRIBUTES 9-attribute blob (unit id keyed per pad) or the 0xAE unit serial, both
/// captured from a physical Deck (see inject/proto/steam_proto.rs feature_reply, the source of
/// truth this mirrors). Anything else echoes the latched command.
fn deck_feature_reply() -> [u8; 64] {
    let last = LAST_SET_FEATURE.lock().map(|g| *g).unwrap_or([0u8; 64]);
    // Per-pad unit id "PF" + the pad index the host stamped into the section — matches
    // steam_proto::deck_unit_id / deck_serial, so two virtual Decks never collide in Steam's eyes.
    let unit_id: u32 = 0x5046_0000 | pad_index() as u32;
    // Steam validates the unit serial's PREFIX before accepting it: a "PF"-leading serial is
    // REJECTED ("Invalid or missing unit serial number …") and Steam then substitutes a hash and
    // MANGLES the displayed name ("Steam Deck Controllerggg"). An 'F'-leading serial passes, so we
    // keep our PunktFunk marker one slot in ("FVPF") — still distinct enough for the Linux side's
    // physical-Deck self-detection while satisfying Steam's format check. (This, not the build-time
    // attributes below, is what un-mangles the name — verified by A/B on .173.)
    let unit_serial = format!("FVPF{unit_id:08X}");
    let unit_serial = unit_serial.as_bytes();
    let mut r = [0u8; 64];
    // The CHANNEL PROOF, Deck flavour: the Deck's ONE feature report is unnumbered and Steam drives
    // it as command→response, so the proof rides that same contract instead of a new report id (no
    // descriptor change). Two command bytes, so a Steam command we haven't catalogued cannot collide.
    if last.starts_with(&pf_driver_proto::gamepad::DECK_PROOF_CMD) {
        return proof_reply();
    }
    match last[0] {
        0x83 => {
            // GET_ATTRIBUTES_VALUES: [0x83, 0x2d, then 9x (attr-id, value u32-LE)].
            r[0] = 0x83;
            r[1] = 0x2D;
            // Attribute semantics per SDL's controller_constants.h: 0x04 = FIRMWARE_BUILD_TIME
            // and 0x0A = BOOTLOADER_BUILD_TIME are unix timestamps that must look like real build
            // dates (the old unit-id-derived junk here was cosmetic; the name mangling was the
            // serial prefix). Uniqueness rides the serial.
            let attrs: [(u8, u32); 9] = [
                (0x01, 0x1205),      // ATTRIB_PRODUCT_ID
                (0x02, 0),           // ATTRIB_CAPABILITIES
                (0x0A, 0x6408_9000), // ATTRIB_BOOTLOADER_BUILD_TIME (2023-03-08)
                (0x04, 0x66A8_C000), // ATTRIB_FIRMWARE_BUILD_TIME (2024-07-30)
                (0x09, 0x2E),        // ATTRIB_BOARD_REVISION (captured)
                (0x0B, 0x0FA0),      // ATTRIB_CONNECTION_INTERVAL_IN_US (4 ms)
                (0x0D, 0),
                (0x0C, 0),
                (0x0E, 0),
            ];
            let mut o = 2;
            for (id, val) in attrs {
                r[o] = id;
                r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                o += 5;
            }
        }
        0xAE => {
            // GET_STRING_ATTRIBUTE: [0xAE, len, attr, ascii…]. Steam requests two strings: attr
            // 0x00 = ATTRIB_STR_BOARD_SERIAL (the PCB serial) and 0x01 = ATTRIB_STR_UNIT_SERIAL.
            // Echo the exact attr requested (last[2]) — the unit serial is the one that matters:
            // getting its format right (FVPF…, see above) is what un-mangles the displayed name.
            // Steam ALSO validates the PCB serial against a Valve-internal format we don't have a
            // real capture of; it logs "Deck Controller PCB Serial# invalid" for ANY value we send
            // (including an empty one — verified on .173), but that line is BENIGN: unlike a bad
            // unit serial, it does not mangle the name, change the handle, or block promotion. So we
            // serve the unit serial for both attrs and accept the log.
            r[0] = 0xAE;
            r[1] = unit_serial.len() as u8;
            r[2] = last[2];
            r[3..3 + unit_serial.len()].copy_from_slice(unit_serial);
        }
        _ => r.copy_from_slice(&last),
    }
    r
}

/// The channel-proof GET_FEATURE answer both command-driven identities (Deck + Triton) serve:
/// `[DECK_PROOF_CMD, ChannelProof(16 bytes), zeros…]`.
///
/// ⚠️ Security-load-bearing input: the proof carries `CHANNEL.index()` — the pad index this driver
/// read from its OWN devnode Location at `EvtDeviceAdd` — and NOT [`pad_index`], which reads the
/// section. The host cross-checks the proof's index against the pad it is about to deliver
/// PRECISELY because it does not yet trust any section; a section-derived index would let a forged
/// delivery vouch for itself. Do not "simplify" the two into one.
fn proof_reply() -> [u8; 64] {
    let proof = pf_driver_proto::gamepad::ChannelProof::new(CHANNEL.index(), std::process::id());
    let mut r = [0u8; 64];
    r[..2].copy_from_slice(&pf_driver_proto::gamepad::DECK_PROOF_CMD);
    r[2..18].copy_from_slice(&proof.to_bytes());
    r
}

// GET_FEATURE: report id from the input buffer; reply with the matching DualSense/DualShock 4 blob
// (the Deck identity instead answers the latched Steam command — its one feature report is
// unnumbered; the Triton identity answers its latched command through the shared
// `triton::feature_reply` machine).
fn on_get_feature(request: &Request) -> NTSTATUS {
    if device_type() == pf_driver_proto::gamepad::DEVTYPE_TRITON {
        let (last, len) = TRITON_LAST_SET.lock().map(|g| *g).unwrap_or(([0u8; 64], 0));
        let is_proof = triton_proof_requested(&last[..len]);
        let mut reply = if is_proof {
            proof_reply()
        } else {
            // The query dance (0x83 attributes / 0xAE string / 0xF2 firmware) + echo fallback —
            // and feature report 2 rides the SAME machine (mirror semantics, no special table).
            let mut serial = [0u8; 13];
            pf_driver_proto::triton::serial(pad_index(), &mut serial);
            pf_driver_proto::triton::feature_reply(
                &last[..len],
                // `triton::serial` writes 13 ASCII bytes, so the conversion is infallible.
                core::str::from_utf8(&serial).unwrap_or(""),
                pf_driver_proto::triton::unit_id(pad_index()),
            )
        };
        // A real pad echoes the feature id it was asked for, and this collection declares TWO
        // (0x01/0x02) while `triton::feature_reply` stamps every answer 0x01 — so a GET of
        // declared report 0x02 came back stamped 0x01. Read the requested id the way the PS arm
        // below does (input-buffer byte 0) and stamp it over the reply when it names a different
        // nonzero report. The proof reply is exempt: it is host↔driver plumbing framed
        // `[DECK_PROOF_CMD, proof…]`, and the host matches that command prefix — an id stamp
        // would destroy it.
        if !is_proof
            && let Ok((req, _)) = request.input_bytes(1)
            && let Some(&id) = req.first()
            && id != 0
            && id != reply[0]
        {
            reply[0] = id;
        }
        // The UMDF request's output-buffer length is authoritative: Steam asks with wLength 64
        // AND 65 (Phase-0 bench log — the 63-byte declared reports marshal as either), so serve
        // min(buffer_len, 64) zero-padded bytes and complete with that count.
        let n = request.output_buffer_len().min(64);
        return request.copy_to_output(&reply[..n]);
    }
    if device_type() == 3 {
        return request.copy_to_output(&deck_feature_reply());
    }
    let (bytes, _) = match request.input_bytes(1) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let Some(&report_id) = bytes.first() else {
        return STATUS_INVALID_PARAMETER;
    };
    // The CHANNEL PROOF (security-review 2026-07-28): tell the host which process serves this
    // devnode, so it never has to trust the LocalService-writable bootstrap mailbox to name its
    // duplication target. `0x85` is already declared as a Feature report in all three captured
    // descriptors and was previously answered with STATUS_INVALID_PARAMETER, so this costs NO
    // report-descriptor change — the identity Steam and SDL fingerprint is untouched. Derived only
    // from our own devnode Location + pid: nothing a caller supplies feeds into it.
    if report_id == pf_driver_proto::gamepad::HID_FEATURE_REPORT_CHANNEL_PROOF {
        let len = request.output_buffer_len();
        return match pf_driver_proto::gamepad::ChannelProof::new(
            CHANNEL.index(),
            std::process::id(),
        )
        .to_feature_report(report_id, len)
        {
            Some(rep) => request.copy_to_output(&rep),
            None => STATUS_INVALID_PARAMETER, // caller's buffer can't hold id + proof
        };
    }
    // DualSense + Edge use feature ids 0x05/0x09/0x20 (same blobs — SDL forces enhanced-rumble
    // for the Edge PID regardless of the firmware version at 0x20[44..46]); DualShock 4 uses
    // 0x02/0x12/0xa3.
    // The pairing replies are per-pad: the MAC (bytes 1..7, LSB first) low octet carries the pad
    // index (see `pad_index` — SDL/Steam dedup controllers by this serial), agreeing with the
    // GET_STRING serial in `on_get_string`. The Edge lands on its GET_STRING base (0x75 = DS
    // base + 1) so its feature MAC and USB serial string agree too.
    let devtype = device_type();
    let mut ds_pairing = DS_FEATURE_PAIRING;
    ds_pairing[1] = ds_pairing[1]
        .wrapping_add(u8::from(devtype == 2))
        .wrapping_add(pad_index());
    let mut ds4_pairing = DS4_FEATURE_PAIRING;
    ds4_pairing[1] = ds4_pairing[1].wrapping_add(pad_index());
    let blob: &[u8] = match (devtype, report_id) {
        (0 | 2, 0x05) => &DS_FEATURE_CALIBRATION,
        (0 | 2, 0x09) => &ds_pairing,
        (0 | 2, 0x20) => &DS_FEATURE_FIRMWARE,
        (1, 0x02) => &DS4_FEATURE_CALIBRATION,
        (1, 0x12) => &ds4_pairing,
        (1, 0xA3) => &DS4_FEATURE_FIRMWARE,
        (_, other) => {
            dbglog!("[pf-gamepad] GET_FEATURE unknown report id 0x{other:02x}");
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
    let devtype = device_type();
    dbglog!("[pf-gamepad] GET_STRING id=0x{string_id:04x} (raw 0x{id_val:08x}) devtype={devtype}");
    let s: String = match string_id {
        0 | 0x000e => match devtype {
            1 => "Sony Computer Entertainment".into(),
            3 | 7 => "Valve Software".into(),
            4..=6 => "Microsoft".into(),
            _ => "Sony Interactive Entertainment".into(),
        },
        // Per-pad serials (see `pad_index`): SDL reads this via HidD_GetSerialNumberString and
        // Steam dedups controllers by it. The PS strings are the pairing MAC MSB-first, so the
        // low octet — the LAST two hex chars — carries the pad index, agreeing with the patched
        // feature 0x09/0x12 replies in `on_get_feature`. The Deck serial must agree with
        // deck_feature_reply's 0xAE answer (Steam reads both).
        2 | 0x0010 => match devtype {
            1 => format!("DEADBEEF00{:02X}", 0x01u8.wrapping_add(pad_index())),
            2 => format!("35533AD6E7{:02X}", 0x75u8.wrapping_add(pad_index())),
            3 => format!("FVPF{:08X}", 0x5046_0000u32 | pad_index() as u32),
            // Xbox pads report a Bluetooth MAC-shaped serial; the low octet carries the pad index
            // so Steam dedups multiple forwarded pads, exactly like the PS identities above. Each
            // Xbox identity gets its OWN base octet (0x10 / 0x30 / 0x50) rather than sharing one:
            // a mixed session can present a Wireless pad and an Elite at once, and two identities
            // whose serials differ only by pad index are one off-by-one away from colliding — the
            // failure being Steam silently treating two live pads as one device.
            4 => format!("F4B0FC2A6C{:02X}", 0x10u8.wrapping_add(pad_index())),
            5 => format!("F4B0FC2A6C{:02X}", 0x30u8.wrapping_add(pad_index())),
            6 => format!("F4B0FC2A6C{:02X}", 0x50u8.wrapping_add(pad_index())),
            // The Triton serial comes from the shared proto helper (13 ASCII bytes,
            // "FVPF1302<idx>D03") so it always agrees with the query dance's 0xAE / firmware
            // replies in `triton::feature_reply` — Steam reads both.
            7 => {
                let mut s = [0u8; 13];
                pf_driver_proto::triton::serial(pad_index(), &mut s);
                String::from_utf8_lossy(&s).into_owned()
            }
            _ => format!("35533AD6E7{:02X}", 0x74u8.wrapping_add(pad_index())),
        },
        _ => match devtype {
            1 => "Wireless Controller".into(),
            2 => "DualSense Edge Wireless Controller".into(),
            3 => "Steam Deck Controller".into(),
            // ⚠️ 4 and 5 share a product string ON PURPOSE — a real Xbox Wireless Controller
            // (Series X|S, `0B13`) and a real Xbox One S pad (`02FD`) BOTH report exactly
            // "Xbox Wireless Controller" over Bluetooth. The PID is what tells them apart, and
            // that is what SDL/Steam/Windows key their stock mappings off. Do not "fix" this by
            // inventing a distinguishing string; it would make the One S identity a device that
            // has never existed. (The INF's Device Manager descriptions DO differ — that string
            // is ours, not the pad's.)
            4 | 5 => "Xbox Wireless Controller".into(),
            6 => "Xbox Elite Wireless Controller Series 2".into(),
            7 => "Steam Controller".into(),
            _ => "DualSense Wireless Controller".into(),
        },
    };
    let mut wide: Vec<u8> = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16() {
        wide.extend_from_slice(&u.to_le_bytes());
    }
    wide.extend_from_slice(&[0, 0]); // NUL terminator (UTF-16)
    request.copy_to_output(&wide)
}

/// The device-type selector: 0 = DualSense, 1 = DualShock 4, 2 = DualSense Edge, 3 = Steam Deck,
/// 4 = Xbox Wireless Controller, 5 = Xbox One S, 6 = Xbox Elite Wireless Controller Series 2,
/// 7 = Steam Controller 2 ("Triton"). Read fresh on each enumeration query — cheap.
///
/// ⚠️ **The sealed section cannot answer the enumeration queries.** hidclass asks for
/// `GET_DEVICE_DESCRIPTOR` / `GET_REPORT_DESCRIPTOR` / `GET_DEVICE_ATTRIBUTES` while it STARTS the
/// device; the host can only deliver the DATA section over the HID device interface
/// (`ProofTransport::HidFeatureReport`), which does not exist until those very queries are answered.
/// So the channel is *structurally* unavailable here, not merely racing — the 1 s wait below always
/// timed out, and every non-DualSense identity silently enumerated with the DualSense VID/PID **and
/// the DualSense report descriptor**. For the Deck that meant Windows parsed the 64-byte
/// `ID_CONTROLLER_DECK_STATE` frame as DualSense report `0x01`: `LX = report[1] = 0x00` (stick hard
/// left), `LY = report[2] = 0x09` (hard up) and a d-pad hat of 0 (UP held) — the "stuck stick/button"
/// a Steam Deck client saw on a Windows host.
///
/// [`PNP_DEVTYPE`] closes it: the devnode's hardware ids carry the identity and are readable at
/// `EvtDeviceAdd`, before anything is asked. The section stays authoritative once attached (same
/// host wrote both). NO in-dispatch wait remains: the old 1 s bounded pump loop could only run
/// for a devnode whose ids matched nothing, where its own premise above guarantees the timeout —
/// it burned a second of a WUDFHost dispatch thread mid-enumeration and then fell back to
/// [`LAST_DEVTYPE`] anyway (which the 8 ms timer keeps fresh whenever the section is attached).
fn device_type() -> u8 {
    if let Some(view) = CHANNEL.data() {
        let t = view.read_u8(OFF_DEVICE_TYPE);
        LAST_DEVTYPE.store(t as u32, Ordering::Relaxed);
        return t;
    }
    let pnp = PNP_DEVTYPE.load(Ordering::Relaxed);
    if pnp != u32::MAX {
        return pnp as u8;
    }
    LAST_DEVTYPE.load(Ordering::Relaxed) as u8
}

extern "C" fn evt_timer(timer: WDFTIMER) {
    // Two cadences on one timer. EVERY tick ([`TIMER_PERIOD_MS`]) does the cheap input half —
    // read the section's report slot, complete one pended READ_REPORT — because that pair is what
    // bounds the rate a game can observe, and at the old 8 ms it halved a 250 Hz motion stream.
    // The channel handshake and the health marks stay on their historical ~8 ms
    // ([`PUMP_EVERY_N_TICKS`]): they cost more, nothing about them wants to be faster, and the
    // heartbeat's documented "+1 per ~8 ms tick" is what the host reads as liveness.
    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    let housekeeping = tick.is_multiple_of(PUMP_EVERY_N_TICKS);
    let view = if housekeeping {
        // Publish our pid / adopt a delivery / detect host-gone.
        CHANNEL.pump(&channel_cfg())
    } else {
        CHANNEL.data()
    };
    match view {
        Some(view) => {
            let mut buf = [0u8; 64];
            // A torn read is dropped rather than served: `read_input_report` returns false only
            // when it caught the host mid-publish, and the previous whole report stays in place.
            // ⚠️ ORDER IS LOAD-BEARING: the report-id check runs AFTER `read_input_report` filled
            // `buf` (short-circuit). Hoisted before the read it would test a zeroed buffer, fail
            // for every identity, and every pad would serve neutral forever — indistinguishable
            // from a Steam-claim failure at the bench.
            if read_input_report(view, &mut buf)
                && (if device_type() == pf_driver_proto::gamepad::DEVTYPE_TRITON {
                    // Triton reports are id-first (0x42 state, 0x43 battery, …). Undeclared ids
                    // (0x47 BLE timestamp) are dropped — hidclass refuses ids the descriptor
                    // doesn't declare.
                    pf_driver_proto::triton::input_len(buf[0]).is_some()
                } else {
                    buf[0] == 0x01
                })
                && let Ok(mut g) = INPUT_REPORT.lock()
            {
                // Compare before storing: the dirty flag must mean "new state", not "the host
                // published again". An unchanged republish carries nothing a game can act on, and
                // treating it as fresh would put the Triton path back on the host's publish rate.
                if *g != buf {
                    *g = buf;
                    INPUT_DIRTY.store(true, Ordering::Relaxed);
                }
            }
            if housekeeping {
                // Keep the fallback identity fresh: `device_type()`'s last resort (channel
                // detached, no PnP match) reads LAST_DEVTYPE, and this tick is the one place that
                // always sees the attached section.
                LAST_DEVTYPE.store(view.read_u8(OFF_DEVICE_TYPE) as u32, Ordering::Relaxed);
                // Health marks the host watches: driver_proto (attach signal, idempotent) and
                // driver_heartbeat (+1 per ~8 ms = liveness). Lets the host tell "driver bound and
                // alive" apart from "driver package missing/failed to bind".
                view.write_u32(OFF_DRIVER_PROTO, GAMEPAD_PROTO_VERSION);
                let hb = view.read_u32(OFF_DRIVER_HEARTBEAT).wrapping_add(1);
                view.write_u32(OFF_DRIVER_HEARTBEAT, hb);
            }
        }
        None => {
            // Host gone (mailbox name vanished) or channel not attached yet: feed games the neutral
            // report instead of a frozen last state (matters for the persistent out-of-band devnode,
            // which outlives host sessions).
            if let Ok(mut g) = INPUT_REPORT.lock() {
                let neutral = neutral_report(LAST_DEVTYPE.load(Ordering::Relaxed) as u8);
                if *g != neutral {
                    *g = neutral;
                    INPUT_DIRTY.store(true, Ordering::Relaxed);
                }
            }
        }
    }

    // Triton delivery is EVENT-DRIVEN; every other identity keeps the every-tick cadence.
    //
    // The others carry typed frames a client streams at ~250 Hz, which a 2 ms tick undersamples
    // nothing of. Triton carries the physical pad's own BLE reports instead, and iOS floors the
    // connection interval at ~15 ms (~66 Hz) — so re-serving the latch every tick handed Steam
    // ~7 identical reports and then one holding a full 15 ms of trackpad travel. A delta that
    // large across a 2 ms inter-report gap reads as a flick ~7x faster than the finger made it,
    // which is where the runaway trackpad momentum came from (bench 2, 2026-08-23). Real hardware
    // NAKs the interrupt IN when it has nothing new; leaving the READ_REPORT pended is this
    // stack's equivalent, so Steam sees one report per real report, spaced as the pad spaced them.
    //
    // No idle re-serve floor: the pad streams state reports continuously — ~66 Hz over BLE with
    // the seq byte advancing even at rest (600-frame capture, 2026-06-08) — so total silence
    // means link loss, not idleness, and neutral-on-detach rides this same dirty path (the
    // detach branch above latches neutral, which IS a change). A time-based re-serve would only
    // ever fire across a stream stall, where re-serving the latch resets the reader's
    // arrival-time reference and the recovery report's delta lands on a compressed window —
    // the momentum bug's exact shape.
    let dt = device_type();
    if dt == pf_driver_proto::gamepad::DEVTYPE_TRITON && !INPUT_DIRTY.load(Ordering::Relaxed) {
        return;
    }

    // Complete the next pended READ_REPORT with the current input report (safe queue/request API).
    // SAFETY: the timer's parent object is the manual queue (set in EvtDeviceAdd); the framework
    // guarantees a live handle here.
    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfTimerGetParentObject, timer) } as WDFQUEUE;
    // SAFETY: `queue` is that live manual queue — the exact contract `retrieve_next_request` needs.
    if let Some(request) = unsafe { wdf::retrieve_next_request(queue) } {
        let report = INPUT_REPORT.lock().map(|g| *g).unwrap_or(NEUTRAL_REPORT);
        // Serve exactly what this identity's descriptor declares — `copy_to_output` REFUSES a
        // source longer than hidclass's buffer instead of truncating, so a 64-byte hand-over for
        // the Xbox pad's 16-byte report would fail every read and the pad would look dead.
        // A retrieved request is ALWAYS completed on every path below: `Request` has no Drop
        // impl and `complete(self, …)` consumes it, so a dequeued-but-uncompleted READ_REPORT
        // would leak.
        // Cleared HERE and not at the gate above: a tick that finds nothing pended must leave the
        // report undelivered, not drop it on the floor.
        INPUT_DIRTY.store(false, Ordering::Relaxed);
        let served: &[u8] = if dt == pf_driver_proto::gamepad::DEVTYPE_TRITON {
            // Per-id trim: Triton input reports are variable-length and id-first. hidclass's
            // READ buffer is 54 bytes (0x42, the largest declared input), so every served
            // length fits; a latched id the descriptor doesn't declare falls back to neutral.
            match pf_driver_proto::triton::input_len(report[0]) {
                Some(len) => &report[..len],
                None => &TRITON_NEUTRAL_REPORT[..54],
            }
        } else {
            &report[..input_report_len(dt)]
        };
        let st = request.copy_to_output(served);
        request.complete(st);
    }
}
