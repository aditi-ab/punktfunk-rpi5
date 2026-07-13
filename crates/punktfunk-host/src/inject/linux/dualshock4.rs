//! Virtual Sony DualShock 4 (PS4) via UHID — the PS4 sibling of the DualSense backend
//! ([`super::dualsense`]). A UHID device presents a *real* DualShock 4 HID interface to the kernel:
//! `hid-playstation` binds it (matched by VID `054C`/PID `09CC`, since Linux 6.2) and exposes the
//! full controller — gamepad, motion sensors, touchpad, lightbar, rumble — to games. We write HID
//! **input** reports (report `0x01`, our controller state) and read HID **output** reports (report
//! `0x05`, a game's rumble/lightbar feedback) back, forwarding them to the client.
//!
//! It carries everything the DualSense does *except* adaptive triggers, player LEDs and the mute
//! button (the DS4 hardware has none), so the only feedback it surfaces is motor rumble (universal
//! 0xCA plane) and the lightbar (HID-output 0xCD `Led`). The button/stick/dpad/touchpad mapping is
//! identical to the DualSense, so we reuse its pure [`DsState`] + [`DsState::from_gamepad`]; the
//! report codec (input `0x01` serializer, output `0x05` parser, touch dims) is the pure
//! [`super::dualshock4_proto`], shared with the Windows UMDF backend — this module is only the
//! `/dev/uhid` transport plus the report descriptor + feature-report handshake the kernel needs.

use super::dualsense_proto::DsState;
use super::dualshock4_proto::{
    parse_ds4_output, serialize_state, Ds4Feedback, DS4_INPUT_REPORT_LEN, DS4_PRODUCT, DS4_TOUCH_H,
    DS4_TOUCH_W, DS4_VENDOR,
};
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use crate::inject::pad_gate::PadGate;
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

// /dev/uhid event ABI (linux/uhid.h) — identical to the DualSense backend's; see `super::dualsense`.
const UHID_PATH: &str = "/dev/uhid";
const UHID_DESTROY: u32 = 1;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const UHID_EVENT_SIZE: usize = 4 + 4372; // type + union (create2)
const BUS_USB: u16 = 0x03;

// Feature reports `hid-playstation` GET_REPORTs during DS4 init. The PAIRING report (0x12) is
// MANDATORY — without a valid reply `dualshock4_create()` aborts and creates NO input devices; the
// kernel reads the 6-byte device MAC from bytes 1..7. CALIBRATION (0x02) and FIRMWARE (0xa3) are
// non-fatal (the kernel warns + falls back to identity IMU calibration), but we answer them for
// correct motion scaling. Each array's first byte is the report id (the kernel hard-checks it).
#[rustfmt::skip]
const DS4_FEATURE_PAIRING: &[u8] = &[ // report 0x12 (MAC at bytes 1..7, LE → DE:AD:BE:EF:00:01)
    0x12, 0x01, 0x00, 0xEF, 0xBE, 0xAD, 0xDE, 0x08, 0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
const DS4_FEATURE_CALIBRATION: &[u8] = &[ // report 0x02 (IMU calibration; all signed le16 words)
    0x02,
    0x00, 0x00, // gyro_pitch_bias  = 0
    0x00, 0x00, // gyro_yaw_bias    = 0
    0x00, 0x00, // gyro_roll_bias   = 0
    0x10, 0x00, // gyro_pitch_plus  = +16
    0xF0, 0xFF, // gyro_pitch_minus = -16
    0x10, 0x00, // gyro_yaw_plus    = +16
    0xF0, 0xFF, // gyro_yaw_minus   = -16
    0x10, 0x00, // gyro_roll_plus   = +16
    0xF0, 0xFF, // gyro_roll_minus  = -16
    0x20, 0x00, // gyro_speed_plus  = +32
    0x20, 0x00, // gyro_speed_minus = +32
    0x00, 0x20, // acc_x_plus  = +8192
    0x00, 0xE0, // acc_x_minus = -8192
    0x00, 0x20, // acc_y_plus  = +8192
    0x00, 0xE0, // acc_y_minus = -8192
    0x00, 0x20, // acc_z_plus  = +8192
    0x00, 0xE0, // acc_z_minus = -8192
    0x00, 0x00, // trailing pad (descriptor declares 36 data bytes)
];
#[rustfmt::skip]
const DS4_FEATURE_FIRMWARE: &[u8] = &[ // report 0xa3 (build date string + hw/fw versions; cosmetic)
    0xA3, 0x41, 0x75, 0x67, 0x20, 0x20, 0x33, 0x20, 0x32, 0x30, 0x31, 0x33, // "Aug  3 2013"
    0x00, 0x00, 0x00, 0x00, 0x00,
    0x30, 0x37, 0x3A, 0x30, 0x31, 0x3A, 0x31, 0x32, // "07:01:12"
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xA0, // hw_version = 0xA000 (buf[35])
    0x00, 0x00, 0x00, 0x00,
    0x00, 0x01, // fw_version = 0x0100 (buf[41])
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // trailing pad (buf[43..49]) → 49 bytes total
];

/// Sony DualShock 4 v2 USB HID report descriptor (507 bytes) — a verbatim real-device capture
/// (CUH-ZCT2E, `054C:09CC`). Declares input `0x01` (64 B), output `0x05` (32 B), and the feature
/// reports `0x02`/`0x12`/`0xa3` so the kernel's GET_REPORTs route. The kernel binds DS4 by VID/PID,
/// but HID core still needs these reports declared.
#[rustfmt::skip]
const DS4_RDESC: &[u8] = &[
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

/// Copy a NUL-padded C string field into the event buffer.
fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]); // rest already zero (NUL-terminated)
}

/// A virtual DualShock 4 backed by `/dev/uhid` (hand-rolled codec mirroring the DualSense pad's).
/// Dropping it destroys the device (the kernel tears down the bound `hid-playstation` interface).
pub struct DualShock4Pad {
    fd: File,
    counter: u8,
    ts: u16,
}

impl DualShock4Pad {
    /// Create the UHID DualShock 4 for pad `index` (used only to make the device name/uniq unique).
    pub fn open(index: u8) -> Result<DualShock4Pad> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the 60-punktfunk.rules uhid rule installed + are you in 'input'?)")
            })?;
        let mut ds = DualShock4Pad {
            fd,
            counter: 0,
            ts: 0,
        };
        ds.send_create2(index).context("UHID_CREATE2 DualShock4")?;
        Ok(ds)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // union (uhid_create2_req) starts at byte 4.
        put_cstr(&mut ev, 4, 128, &format!("Punktfunk DualShock 4 {index}")); // name[128]
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/dualshock4/{index}")); // phys[64]

        // A unique uniq[64] keeps the sysfs nodes tidy when several pads coexist (the kernel's
        // duplicate-device check itself keys off the per-pad MAC in the pairing feature report).
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-ds4-{index}")); // uniq[64]
        ev[260..262].copy_from_slice(&(DS4_RDESC.len() as u16).to_ne_bytes()); // rd_size
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus
        ev[264..268].copy_from_slice(&(DS4_VENDOR as u32).to_ne_bytes());
        ev[268..272].copy_from_slice(&(DS4_PRODUCT as u32).to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
        ev[280..280 + DS4_RDESC.len()].copy_from_slice(DS4_RDESC); // rd_data
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Serialize `st` into report `0x01` and write it to the kernel (UHID_INPUT2).
    pub fn write_state(&mut self, st: &DsState) -> Result<()> {
        self.counter = self.counter.wrapping_add(1);
        self.ts = self.ts.wrapping_add(188); // ~1ms in the DS4's 5.33µs sensor-clock units
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.counter, self.ts);

        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes()); // input2.size
        ev[6..6 + r.len()].copy_from_slice(&r); // input2.data
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Service the device, non-blocking: answer the kernel's feature-report GET_REPORTs (pairing /
    /// calibration / firmware — the pairing reply is required during `hid-playstation` init, or no
    /// input devices appear) and parse any HID OUTPUT reports (rumble / lightbar) into a
    /// [`Ds4Feedback`]. Call frequently — especially right after [`open`] so the init handshake
    /// completes.
    pub fn service(&mut self) -> Ds4Feedback {
        let mut fb = Ds4Feedback::default();
        let mut ev = [0u8; UHID_EVENT_SIZE];
        while let Ok(n) = self.fd.read(&mut ev) {
            if n < UHID_EVENT_SIZE {
                break;
            }
            match u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]) {
                UHID_OUTPUT => {
                    // uhid_output_req: data[4096] at [4..4100], size u16 at [4100..4102].
                    let size = u16::from_ne_bytes([ev[4100], ev[4101]]) as usize;
                    let end = 4 + size.min(HID_MAX_DESCRIPTOR_SIZE);
                    parse_ds4_output(&ev[4..end], &mut fb);
                }
                UHID_GET_REPORT => {
                    // uhid_get_report_req: id u32 [4..8], rnum u8 [8].
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let data: &[u8] = match ev[8] {
                        0x12 => DS4_FEATURE_PAIRING,
                        0x02 => DS4_FEATURE_CALIBRATION,
                        0xA3 => DS4_FEATURE_FIRMWARE,
                        _ => &[],
                    };
                    let _ = self.reply_get_report(id, data);
                }
                _ => {} // Start/Stop/Open/Close/SetReport — ignore
            }
        }
        fb
    }

    fn reply_get_report(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        // uhid_get_report_reply_req: id u32 [4..8], err u16 [8..10], size u16 [10..12], data [12..].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        let err: u16 = if data.is_empty() { 5 } else { 0 }; // EIO if we don't know the report
        ev[8..10].copy_from_slice(&err.to_ne_bytes());
        ev[10..12].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[12..12 + data.len()].copy_from_slice(data);
        self.fd
            .write_all(&ev)
            .context("write UHID_GET_REPORT_REPLY")?;
        Ok(())
    }
}

impl Drop for DualShock4Pad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// All virtual DualShock 4 pads of a session — the PS4 analog of
/// [`DualSenseManager`](super::dualsense::DualSenseManager), selected with `PUNKTFUNK_GAMEPAD=ps4`.
/// Like the DualSense it keeps each pad's full [`DsState`] and re-emits the merged report whenever
/// buttons/sticks ([`handle`](Self::handle)) or touchpad/motion ([`apply_rich`](Self::apply_rich))
/// change. [`pump`](Self::pump) services the kernel handshake and routes a game's feedback back:
/// motor rumble on the universal plane, the lightbar on the HID-output plane.
pub struct DualShock4Manager {
    pads: Vec<Option<DualShock4Pad>>,
    /// Each pad's current full report — buttons/sticks merged with persisted touch + motion.
    state: Vec<DsState>,
    /// Last rumble forwarded per pad, so a report that only changes the lightbar doesn't re-send it.
    last_rumble: Vec<(u16, u16)>,
    /// Last lightbar RGB forwarded per pad — the kernel bundles the lightbar into every output
    /// report (incl. rumble-only writes), so dedup here to avoid flooding the HID-output plane.
    last_led: Vec<Option<(u8, u8, u8)>>,
    /// When each pad last wrote an input report — drives [`heartbeat`](Self::heartbeat).
    last_write: Vec<Instant>,
    /// Create-retry gate: a transient `/dev/uhid` failure backs off and retries instead of
    /// permanently disabling every pad for the session.
    gate: PadGate,
    /// Fallback policy for the Steam back grips a client may send (the DS4 has no back-button HID
    /// slot). `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop.
    remap: crate::inject::steam_remap::RemapConfig,
}

impl Default for DualShock4Manager {
    fn default() -> DualShock4Manager {
        DualShock4Manager::new()
    }
}

impl DualShock4Manager {
    pub fn new() -> DualShock4Manager {
        DualShock4Manager {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            state: vec![DsState::neutral(); MAX_PADS],
            last_rumble: vec![(0, 0); MAX_PADS],
            last_led: vec![None; MAX_PADS],
            last_write: vec![Instant::now(); MAX_PADS],
            gate: PadGate::new(),
            remap: crate::inject::steam_remap::RemapConfig::from_env(),
        }
    }

    /// Handle one decoded controller event (create/destroy by mask, then merge button/stick state).
    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival (DualShock 4)");
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Unplugs: drop any allocated pad whose mask bit cleared, resetting its state.
                for (i, slot) in self.pads.iter_mut().enumerate() {
                    if slot.is_some() && f.active_mask & (1 << i) == 0 {
                        tracing::info!(index = i, "controller unplugged (DualShock 4)");
                        *slot = None;
                        self.state[i] = DsState::neutral();
                        self.last_rumble[i] = (0, 0);
                        self.last_led[i] = None;
                    }
                }
                if f.active_mask & (1 << idx) == 0 {
                    return; // this event WAS the unplug
                }
                self.ensure(idx);
                // Merge buttons/sticks/triggers, preserving touch + motion (those arrive on the
                // rich-input plane and must survive a button-only frame).
                let prev = self.state[idx];
                // Steam back grips have no DS4 slot — fold them onto standard buttons per the
                // configured policy (default drop) so they aren't silently lost.
                let buttons =
                    crate::inject::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
                let mut s = DsState::from_gamepad(
                    buttons,
                    f.ls_x,
                    f.ls_y,
                    f.rs_x,
                    f.rs_y,
                    f.left_trigger,
                    f.right_trigger,
                );
                s.touch = prev.touch;
                s.gyro = prev.gyro;
                s.accel = prev.accel;
                s.touch_click = prev.touch_click;
                self.state[idx] = s;
                self.write(idx);
            }
        }
    }

    /// Apply one rich client→host event (touchpad contact / motion sample) to an existing pad,
    /// preserving its button/stick state. Rich events never create a pad; they're dropped if the
    /// pad isn't present.
    pub fn apply_rich(&mut self, rich: RichInput) {
        let idx = match rich {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. } => pad as usize,
        };
        if idx >= MAX_PADS || self.pads[idx].is_none() {
            return;
        }
        // The shared DualSense-family mapping (dualsense_proto::DsState::apply_rich): Steam
        // dual pads split the one touchpad left/right, pad clicks ride touch_click.
        self.state[idx].apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
        self.write(idx);
    }

    fn write(&mut self, idx: usize) {
        let st = self.state[idx];
        if let Some(pad) = self.pads[idx].as_mut() {
            let _ = pad.write_state(&st);
        }
        self.last_write[idx] = Instant::now();
    }

    /// Re-emit each live pad's CURRENT report if it's been silent for `max_gap` — a real DS4 streams
    /// report `0x01` continuously, and `hid-playstation` / SDL treat a multi-second silence (a
    /// held-steady stick) as an unplugged controller. Idempotent (a stale-but-correct frame);
    /// `write_state` bumps the counter + timestamp so each is a fresh, well-formed report.
    pub fn heartbeat(&mut self, max_gap: Duration) {
        let now = Instant::now();
        for i in 0..self.pads.len() {
            if self.pads[i].is_some() && now.duration_since(self.last_write[i]) >= max_gap {
                self.write(i);
            }
        }
    }

    fn ensure(&mut self, idx: usize) {
        if idx >= MAX_PADS || self.pads[idx].is_some() || !self.gate.allow(Instant::now()) {
            return;
        }
        match DualShock4Pad::open(idx as u8) {
            Ok(p) => {
                tracing::info!(
                    index = idx,
                    "virtual DualShock 4 created (UHID hid-playstation)"
                );
                self.pads[idx] = Some(p);
                self.state[idx] = DsState::neutral();
                self.last_rumble[idx] = (0, 0);
                self.last_led[idx] = None;
                self.last_write[idx] = Instant::now();
                self.gate.on_success();
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual DualShock 4 creation failed — retrying with backoff");
                self.gate.on_failure(Instant::now());
            }
        }
    }

    /// Service every pad: answer the kernel's init handshake and parse a game's feedback. `rumble`
    /// is invoked `(index, low, high)` only when the motor level *changes* (universal 0xCA plane);
    /// `hidout` carries the lightbar (0xCD `Led`), deduped. Call frequently — the kernel blocks
    /// `hid-playstation` init until its GET_REPORTs are answered.
    pub fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16),
        mut hidout: impl FnMut(HidOutput),
    ) {
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_mut() else {
                continue;
            };
            let fb = pad.service();
            if let Some(r) = fb.rumble {
                if self.last_rumble[i] != r {
                    self.last_rumble[i] = r;
                    rumble(i as u16, r.0, r.1);
                }
            }
            if let Some(rgb) = fb.led {
                if self.last_led[i] != Some(rgb) {
                    self.last_led[i] = Some(rgb);
                    hidout(HidOutput::Led {
                        pad: i as u8,
                        r: rgb.0,
                        g: rgb.1,
                        b: rgb.2,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The report 0x01 serializer + output 0x05 parser are covered in `dualshock4_proto` (the codec
    // is shared with the Windows backend); only the UHID-transport-specific pieces are tested here.

    /// Feature-report arrays carry the right report id + length the kernel expects.
    #[test]
    fn feature_report_shapes() {
        assert_eq!(DS4_FEATURE_PAIRING.len(), 16);
        assert_eq!(DS4_FEATURE_PAIRING[0], 0x12);
        assert_eq!(DS4_FEATURE_CALIBRATION.len(), 37);
        assert_eq!(DS4_FEATURE_CALIBRATION[0], 0x02);
        assert_eq!(DS4_FEATURE_FIRMWARE.len(), 49);
        assert_eq!(DS4_FEATURE_FIRMWARE[0], 0xA3);
    }
}
