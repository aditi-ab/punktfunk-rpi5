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
use crate::inject::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

// /dev/uhid event ABI (linux/uhid.h) — identical to the DualSense backend's; see `super::dualsense`.
const UHID_PATH: &str = "/dev/uhid";
const UHID_DESTROY: u32 = 1;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;
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

/// The pairing reply for wire pad `pad`: [`DS4_FEATURE_PAIRING`] with the MAC's low octet offset
/// by the pad index — same per-pad-serial contract as the DualSense's
/// [`ds_pairing_reply`](super::dualsense_proto::ds_pairing_reply): the kernel adopts the MAC as
/// the HID uniq, and SDL/Steam dedup controllers by that serial.
fn ds4_pairing_reply(pad: u8) -> [u8; 16] {
    let mut r = [0u8; 16];
    r.copy_from_slice(DS4_FEATURE_PAIRING);
    r[1] = r[1].wrapping_add(pad); // MAC lives at bytes 1..7, LSB first
    r
}
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
    /// [`Ds4Feedback`] for pad `pad`. Call frequently — especially right after [`open`] so the
    /// init handshake completes.
    pub fn service(&mut self, pad: u8) -> Ds4Feedback {
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
                    let pairing = ds4_pairing_reply(pad);
                    let data: &[u8] = match ev[8] {
                        0x12 => &pairing,
                        0x02 => DS4_FEATURE_CALIBRATION,
                        0xA3 => DS4_FEATURE_FIRMWARE,
                        _ => &[],
                    };
                    let _ = self.reply_get_report(id, data);
                }
                UHID_SET_REPORT => {
                    // Ack (err=0) so a SET_REPORT writer doesn't block on the kernel's 5 s
                    // timeout; DS4 feedback arrives as OUTPUT reports (handled above).
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let _ = self.reply_set_report(id);
                }
                _ => {} // Start/Stop/Open/Close — ignore
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

    fn reply_set_report(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
        // uhid_set_report_reply_req: id u32 [4..8], err u16 [8..10].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err 0 (ack)
        self.fd
            .write_all(&ev)
            .context("write UHID_SET_REPORT_REPLY")?;
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

/// The DualShock-4-specific half of the shared stateful manager (see [`PadProto`]): UHID transport
/// open, the [`DsState`] mappers, and the kernel-handshake service pass. Lifecycle (slot table,
/// unplug sweep, heartbeat, dedup) lives in [`UhidManager`]; the lightbar dedup that used to be a
/// bespoke `last_led` vec (the kernel bundles the lightbar into every output report, incl.
/// rumble-only writes) now rides the shared `HidoutDedup` — identical semantics, `Led` compared
/// against the last-forwarded value and re-armed on create/unplug.
pub struct Ds4LinuxProto {
    /// Fallback policy for the Steam back grips a client may send (the DS4 has no back-button HID
    /// slot). `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop.
    remap: crate::inject::steam_remap::RemapConfig,
}

impl Default for Ds4LinuxProto {
    fn default() -> Ds4LinuxProto {
        Ds4LinuxProto {
            remap: crate::inject::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for Ds4LinuxProto {
    type Pad = DualShock4Pad;
    type State = DsState;
    const LABEL: &'static str = "DualShock 4";
    const DEVICE: &'static str = "DualShock 4";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DualShock4Pad> {
        let p = DualShock4Pad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual DualShock 4 created (UHID hid-playstation)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Merge buttons/sticks/triggers from the frame, preserving touch + motion + pad clicks (those
    /// arrive on the rich-input plane and must survive a button-only frame).
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        // Steam back grips have no DS4 slot — fold them onto standard buttons per the configured
        // policy (default drop) so they aren't silently lost.
        let buttons = crate::inject::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
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
        s
    }

    /// The shared DualSense-family mapping (dualsense_proto::DsState::apply_rich): Steam dual pads
    /// split the one touchpad left/right, pad clicks ride touch_click.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DualShock4Pad, st: &DsState) {
        let _ = pad.write_state(st);
    }

    /// Answer the kernel's init handshake (it blocks `hid-playstation` init until its GET_REPORTs
    /// are answered — call frequently) and parse a game's feedback: motor rumble on the universal
    /// 0xCA plane, the lightbar as a 0xCD `Led` event (a DS4 has no player LEDs / adaptive
    /// triggers).
    fn service(&self, pad: &mut DualShock4Pad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            rumble: fb.rumble,
            hidout: fb
                .led
                .map(|(r, g, b)| HidOutput::Led { pad: idx, r, g, b })
                .into_iter()
                .collect(),
            game_drove: None,
        }
    }
}

/// All virtual DualShock 4 pads of a session — the PS4 analog of
/// [`DualSenseManager`](super::dualsense::DualSenseManager), selected with `PUNKTFUNK_GAMEPAD=ps4`.
/// Like the DualSense, the shared [`UhidManager`] keeps each pad's full [`DsState`], re-emits the
/// merged report whenever buttons/sticks or touchpad/motion change, and heartbeats it through
/// input silence (a real DS4 streams report `0x01` continuously — `hid-playstation`/SDL treat a
/// multi-second gap as an unplug).
pub type DualShock4Manager = UhidManager<Ds4LinuxProto>;

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

    /// The pairing reply keeps the report id and differs across pads ONLY in the MAC low octet —
    /// distinct serials so SDL/Steam never dedup two virtual pads into one controller.
    #[test]
    fn pairing_reply_mac_is_per_pad() {
        assert_eq!(ds4_pairing_reply(0).as_slice(), DS4_FEATURE_PAIRING);
        let (a, b) = (ds4_pairing_reply(1), ds4_pairing_reply(2));
        assert_eq!(a[0], 0x12); // report id untouched
        assert_eq!(a[1], DS4_FEATURE_PAIRING[1].wrapping_add(1));
        assert_eq!(b[1], DS4_FEATURE_PAIRING[1].wrapping_add(2));
        assert_eq!(a[2..], b[2..]); // everything but the low octet identical
    }
}
