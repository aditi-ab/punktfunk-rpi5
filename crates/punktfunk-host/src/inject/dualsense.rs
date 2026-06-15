//! Virtual Sony DualSense via UHID — the rich-controller path (roadmap §5).
//!
//! Unlike the uinput X-Box-360 pad ([`super::gamepad`]), which only carries buttons + axes + a
//! rumble back-channel, a UHID device presents a *real* DualSense HID interface to the kernel:
//! `hid-playstation` binds it (matched by VID `054C`/PID `0CE6`) and exposes the full controller
//! — gamepad, motion sensors, touchpad, lightbar + player LEDs, and adaptive triggers — to games.
//! The host writes HID **input** reports (report `0x01`, our controller state) and reads HID
//! **output** reports (report `0x02`, a game's rumble/LED/trigger feedback) back, which it
//! forwards to the client as [`punktfunk_core::quic::HidOutput`].
//!
//! The report descriptor + field layout are the canonical inputtino ones (games-on-whales/
//! inputtino `src/uhid/include/uhid/ps5.hpp`), so `hid-playstation` binds the same as a USB pad.

use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

// /dev/uhid event ABI (linux/uhid.h). `struct uhid_event` is __packed__: a u32 `type` then a
// union whose largest member is uhid_create2_req (128+64+64 + 2+2 + 4*4 + rd_data[4096] = 4372).
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

// Feature reports `hid-playstation` GET_REPORTs during init — without these replies it never
// finishes calibration and creates no input devices. Verbatim from inputtino (each array's
// first byte is the report id). The pairing report carries a fixed virtual MAC.
#[rustfmt::skip]
const DS_FEATURE_CALIBRATION: &[u8] = &[ // report 0x05 (motion calibration)
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0xF4, 0x01, 0xF4, 0x01, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
const DS_FEATURE_PAIRING: &[u8] = &[ // report 0x09 (pairing info: MAC at bytes 1..7)
    0x09, 0x74, 0xE7, 0xD6, 0x3A, 0x53, 0x35, 0x08, 0x25, 0x00, 0x1E, 0x00, 0xEE, 0x74, 0xD0, 0xBC,
    0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
const DS_FEATURE_FIRMWARE: &[u8] = &[ // report 0x20 (firmware info / build date)
    0x20, 0x4A, 0x75, 0x6E, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3A, 0x34,
    0x37, 0x3A, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xC1, 0xC8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x54, 0x01, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x0B, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Sony DualSense USB HID report descriptor (232 bytes), verbatim from inputtino — the exact
/// descriptor `hid-playstation` parses to bind a UHID device as a DualSense.
#[rustfmt::skip]
const DUALSENSE_RDESC: &[u8] = &[
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

const DS_VENDOR: u32 = 0x054C; // Sony Interactive Entertainment
const DS_PRODUCT: u32 = 0x0CE6; // DualSense Wireless Controller
/// USB input report `0x01` is 64 bytes total (report id + 63-byte body).
const DS_INPUT_REPORT_LEN: usize = 64;
/// The DualSense touchpad's reported resolution (the kernel exposes it as ABS_MT 0..1920/1080).
pub const DS_TOUCH_W: u16 = 1920;
pub const DS_TOUCH_H: u16 = 1080;

/// Bit positions inside the DualSense face/dpad button byte (`buttons[0]`, low nibble = hat).
mod btn0 {
    pub const SQUARE: u8 = 0x10;
    pub const CROSS: u8 = 0x20;
    pub const CIRCLE: u8 = 0x40;
    pub const TRIANGLE: u8 = 0x80;
}
/// `buttons[1]`: shoulders, triggers-as-buttons, create/options, stick clicks.
mod btn1 {
    pub const L1: u8 = 0x01;
    pub const R1: u8 = 0x02;
    pub const L2: u8 = 0x04;
    pub const R2: u8 = 0x08;
    pub const CREATE: u8 = 0x10; // "Share"
    pub const OPTIONS: u8 = 0x20;
    pub const L3: u8 = 0x40;
    pub const R3: u8 = 0x80;
}
/// `buttons[2]`: PS, touchpad click, mute (+ a rolling counter in the high bits).
mod btn2 {
    pub const PS: u8 = 0x01;
    pub const TOUCHPAD: u8 = 0x02;
    #[allow(dead_code)]
    pub const MUTE: u8 = 0x04;
}

/// One touchpad contact for the report.
#[derive(Clone, Copy, Default)]
pub struct Touch {
    pub active: bool,
    pub id: u8,
    pub x: u16, // 0..DS_TOUCH_W
    pub y: u16, // 0..DS_TOUCH_H
}

/// Full DualSense controller state to serialize into report `0x01`. Sticks/triggers are 8-bit
/// (`0x80` neutral for sticks, `0x00` released for triggers); `dpad` is the 8-way hat (`8` =
/// centered); `buttons[0..3]` are the packed DualSense button bytes; gyro/accel are raw i16.
#[derive(Clone, Copy, Default)]
pub struct DsState {
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
    pub l2: u8,
    pub r2: u8,
    pub dpad: u8, // 0..7 direction, 8 = neutral
    pub buttons: [u8; 4],
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
    pub touch: [Touch; 2],
}

impl DsState {
    /// A centered, nothing-pressed state (sticks 0x80, dpad neutral).
    pub fn neutral() -> DsState {
        DsState {
            lx: 0x80,
            ly: 0x80,
            rx: 0x80,
            ry: 0x80,
            dpad: 8,
            ..Default::default()
        }
    }

    /// Map a GameStream/XInput pad frame (button bitmask + i16 sticks + u8 triggers) into the
    /// DualSense report fields. Sticks are recentred to `0x80`; the Y axes are inverted (XInput
    /// `+y = up`, DualSense `0 = up`). Triggers double as the L2/R2 buttons when pressed. Touchpad
    /// + motion are filled separately from rich-input events.
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> DsState {
        use punktfunk_core::input::gamepad as gs;
        let to_u8 = |v: i16| (((v as i32) + 32768) >> 8) as u8;
        let on = |bit: u32| buttons & bit != 0;
        let mut s = DsState {
            lx: to_u8(lx),
            ly: 255 - to_u8(ly),
            rx: to_u8(rx),
            ry: 255 - to_u8(ry),
            l2: lt,
            r2: rt,
            ..DsState::neutral()
        };
        s.set_dpad(
            on(gs::BTN_DPAD_UP),
            on(gs::BTN_DPAD_DOWN),
            on(gs::BTN_DPAD_LEFT),
            on(gs::BTN_DPAD_RIGHT),
        );
        let mut b0 = 0;
        if on(gs::BTN_A) {
            b0 |= btn0::CROSS;
        }
        if on(gs::BTN_B) {
            b0 |= btn0::CIRCLE;
        }
        if on(gs::BTN_X) {
            b0 |= btn0::SQUARE;
        }
        if on(gs::BTN_Y) {
            b0 |= btn0::TRIANGLE;
        }
        s.buttons[0] = b0; // face buttons (high nibble); dpad merged in write_state
        let mut b1 = 0;
        if on(gs::BTN_LB) {
            b1 |= btn1::L1;
        }
        if on(gs::BTN_RB) {
            b1 |= btn1::R1;
        }
        if lt > 0 {
            b1 |= btn1::L2;
        }
        if rt > 0 {
            b1 |= btn1::R2;
        }
        if on(gs::BTN_BACK) {
            b1 |= btn1::CREATE;
        }
        if on(gs::BTN_START) {
            b1 |= btn1::OPTIONS;
        }
        if on(gs::BTN_LS_CLICK) {
            b1 |= btn1::L3;
        }
        if on(gs::BTN_RS_CLICK) {
            b1 |= btn1::R3;
        }
        s.buttons[1] = b1;
        if on(gs::BTN_GUIDE) {
            s.buttons[2] |= btn2::PS;
        }
        if on(gs::BTN_TOUCHPAD) {
            s.buttons[2] |= btn2::TOUCHPAD;
        }
        s
    }

    /// Set the dpad hat from the four GameStream dpad booleans (up/down/left/right).
    pub fn set_dpad(&mut self, up: bool, down: bool, left: bool, right: bool) {
        // DualSense hat: 0=N,1=NE,2=E,3=SE,4=S,5=SW,6=W,7=NW,8=neutral.
        self.dpad = match (up, right, down, left) {
            (true, false, false, false) => 0,
            (true, true, false, false) => 1,
            (false, true, false, false) => 2,
            (false, true, true, false) => 3,
            (false, false, true, false) => 4,
            (false, false, true, true) => 5,
            (false, false, false, true) => 6,
            (true, false, false, true) => 7,
            _ => 8,
        };
    }
}

/// Serialize a full input report `0x01` (pure — unit-testable without `/dev/uhid`). Field
/// offsets per the kernel's `struct dualsense_input_report`, this report's one consumer:
/// x..rz 0-5, seq 6, buttons[4] 7-10, reserved[4] 11-14, gyro[3] 15-20, accel[3] 21-26,
/// sensor_timestamp 27-30, reserved2 31, points[2] 32-39 (static_assert(sizeof == 63)).
/// The report id occupies r[0], so struct offset N = r[N + 1].
fn serialize_state(r: &mut [u8; DS_INPUT_REPORT_LEN], st: &DsState, seq: u8, ts: u32) {
    r[0] = 0x01; // report id; the struct fields follow (struct offset 0 == r[1])
    r[1] = st.lx;
    r[2] = st.ly;
    r[3] = st.rx;
    r[4] = st.ry;
    r[5] = st.l2;
    r[6] = st.r2;
    r[7] = seq; // seq_number (struct off 6)
    r[8] = (st.dpad & 0x0F) | (st.buttons[0] & 0xF0); // off 7: dpad + face buttons
    r[9] = st.buttons[1]; // off 8
    r[10] = st.buttons[2]; // off 9
    r[11] = st.buttons[3]; // off 10
    for (i, v) in st.gyro.iter().enumerate() {
        r[16 + i * 2..18 + i * 2].copy_from_slice(&v.to_le_bytes()); // gyro at struct off 15
    }
    for (i, v) in st.accel.iter().enumerate() {
        r[22 + i * 2..24 + i * 2].copy_from_slice(&v.to_le_bytes()); // accel at struct off 21
    }
    r[28..32].copy_from_slice(&ts.to_le_bytes()); // sensor_timestamp (struct off 27)
    pack_touch(&mut r[33..37], &st.touch[0]); // touch point 1 (struct off 32)
    pack_touch(&mut r[37..41], &st.touch[1]); // touch point 2
                                              // status byte (struct off 52 → r[53]) — hid-playstation reads battery here: low nibble =
                                              // capacity (×10+5 %), high nibble = charging state (0 = discharging). A virtual pad has no
                                              // real cell, so report "discharging, full" (0x0A → 100 %); leaving it 0 makes SteamOS / the
                                              // kernel see ~5 % and warn "low battery". (We don't forward the client pad's real charge yet.)
    r[53] = 0x0A;
}

fn pack_touch(dst: &mut [u8], t: &Touch) {
    // byte0: bit7 = NOT active (1 = no contact), bits0-6 = contact id.
    dst[0] = (t.id & 0x7F) | if t.active { 0 } else { 0x80 };
    // The kernel advertises ABS_MT ranges 0..=W-1 / 0..=H-1 — never emit the size itself.
    let (x, y) = (t.x.min(DS_TOUCH_W - 1), t.y.min(DS_TOUCH_H - 1));
    dst[1] = (x & 0xFF) as u8;
    dst[2] = (((x >> 8) & 0x0F) as u8) | (((y & 0x0F) as u8) << 4);
    dst[3] = ((y >> 4) & 0xFF) as u8;
}

/// What one [`DualSensePad::service`] pass extracted from the device's HID output reports.
/// Rich feedback (lightbar / player LEDs / adaptive triggers) rides the HID-output plane (0xCD);
/// motor rumble rides the universal rumble plane (0xCA) so non-DualSense clients still feel it.
#[derive(Default)]
pub struct DsFeedback {
    pub hidout: Vec<HidOutput>,
    /// `(low, high)` motor levels (0..=0xFFFF), if a report carried them.
    pub rumble: Option<(u16, u16)>,
}

/// A virtual DualSense backed by `/dev/uhid` (hand-rolled codec — no bindgen, mirroring the
/// uinput pad's style). Dropping it destroys the device (the kernel tears down the bound
/// `hid-playstation` interface).
pub struct DualSensePad {
    fd: File,
    seq: u8,
    ts: u32,
}

/// Copy a NUL-padded C string field into the event buffer.
fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]); // rest already zero (NUL-terminated)
}

impl DualSensePad {
    /// Create the UHID DualSense for pad `index` (used only to make the device name/uniq unique).
    pub fn open(index: u8) -> Result<DualSensePad> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the 60-punktfunk.rules uhid rule installed + are you in 'input'?)")
            })?;
        let mut ds = DualSensePad { fd, seq: 0, ts: 0 };
        ds.send_create2(index).context("UHID_CREATE2 DualSense")?;
        Ok(ds)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // union (uhid_create2_req) starts at byte 4.
        put_cstr(&mut ev, 4, 128, &format!("Punktfunk DualSense {index}")); // name[128]
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/dualsense/{index}")); // phys[64]
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-ds-{index}")); // uniq[64]
        ev[260..262].copy_from_slice(&(DUALSENSE_RDESC.len() as u16).to_ne_bytes()); // rd_size
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus
        ev[264..268].copy_from_slice(&DS_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&DS_PRODUCT.to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
        ev[280..280 + DUALSENSE_RDESC.len()].copy_from_slice(DUALSENSE_RDESC); // rd_data
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Serialize `st` into report `0x01` and write it to the kernel (UHID_INPUT2).
    pub fn write_state(&mut self, st: &DsState) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(1); // monotonic sensor timestamp is all the kernel needs
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.seq, self.ts);

        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes()); // input2.size
        ev[6..6 + r.len()].copy_from_slice(&r); // input2.data
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Service the device, non-blocking: answer the kernel's feature-report GET_REPORTs (calibration
    /// / pairing / firmware — required during `hid-playstation` init, or no input devices appear)
    /// and parse any HID OUTPUT reports (rumble / lightbar / player LEDs / adaptive triggers) into
    /// a [`DsFeedback`] for pad `pad`. Call frequently — especially right after [`open`] so the
    /// init handshake completes. The fd is `O_NONBLOCK`, so once drained `read` returns `WouldBlock`.
    pub fn service(&mut self, pad: u8) -> DsFeedback {
        let mut fb = DsFeedback::default();
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
                    parse_ds_output(pad, &ev[4..end], &mut fb);
                }
                UHID_GET_REPORT => {
                    // uhid_get_report_req: id u32 [4..8], rnum u8 [8].
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let data: &[u8] = match ev[8] {
                        0x05 => DS_FEATURE_CALIBRATION,
                        0x09 => DS_FEATURE_PAIRING,
                        0x20 => DS_FEATURE_FIRMWARE,
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

impl Drop for DualSensePad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// Parse a DualSense USB output report (`0x02`) into a [`DsFeedback`]. The byte layout below is
/// the USB DualSense common report; only the well-understood fields (motor rumble, lightbar RGB,
/// player LEDs) are surfaced — adaptive-trigger blocks are forwarded raw for the client.
///
/// Every field is gated on the report's valid-flags (`valid_flag0` at data[1], `valid_flag1`
/// at data[2]) — writers only set the bits for fields they mean to change (the kernel zeroes
/// the rest), so an ungated parse would turn every plain rumble write into a lightbar-off +
/// triggers-off broadcast.
fn parse_ds_output(pad: u8, data: &[u8], fb: &mut DsFeedback) {
    // data[0] is the report id (0x02). Be defensive about short reports.
    if data.first() != Some(&0x02) || data.len() < 48 {
        return;
    }
    let flag0 = data[1]; // BIT0 compat vibration, BIT1 haptics select, BIT2 R2, BIT3 L2
    let flag1 = data[2]; // BIT2 lightbar, BIT4 player indicators
                         // Motor rumble: high-frequency (small/right) motor at data[3], low-frequency (big/left) at
                         // data[4]. Scale 0..255 → 0..0xFFFF, same (low, high) convention as the uinput pad's mixer,
                         // and route to the universal rumble plane (0xCA).
    if flag0 & 0x03 != 0 {
        let high = (data[3] as u16) << 8;
        let low = (data[4] as u16) << 8;
        fb.rumble = Some((low, high));
    }
    // Lightbar RGB (USB common report: bytes 45..48). Player LEDs at byte 44.
    if flag1 & 0x04 != 0 {
        let (r, g, b) = (data[45], data[46], data[47]);
        fb.hidout.push(HidOutput::Led { pad, r, g, b });
    }
    if flag1 & 0x10 != 0 {
        fb.hidout.push(HidOutput::PlayerLeds {
            pad,
            bits: data[44] & 0x1F,
        });
    }
    // Adaptive-trigger parameter blocks, 11 bytes each: the RIGHT trigger comes FIRST in the
    // report (bytes 11..22), the left at 22..33 — per SDL's DS5EffectsState_t / inputtino's
    // ps5.hpp. Wire convention: which 0 = L2, 1 = R2.
    if data.len() >= 33 {
        if flag0 & 0x04 != 0 {
            fb.hidout.push(HidOutput::Trigger {
                pad,
                which: 1,
                effect: data[11..22].to_vec(),
            });
        }
        if flag0 & 0x08 != 0 {
            fb.hidout.push(HidOutput::Trigger {
                pad,
                which: 0,
                effect: data[22..33].to_vec(),
            });
        }
    }
}

/// All virtual DualSense pads of a session — the rich-controller analog of
/// [`GamepadManager`](super::gamepad::GamepadManager), selected with `PUNKTFUNK_GAMEPAD=dualsense`.
///
/// Unlike the uinput pad, a DualSense carries touchpad + motion, which arrive on a *separate*
/// rich-input plane ([`apply_rich`](Self::apply_rich)) from the button/stick frames
/// ([`handle`](Self::handle)). So the manager keeps each pad's full [`DsState`] and re-emits the
/// merged report whenever either source changes. [`pump`](Self::pump) services the kernel
/// handshake and routes a game's feedback back out: motor rumble on the universal plane, the rich
/// LED/player-LED/trigger feedback on the HID-output plane.
pub struct DualSenseManager {
    pads: Vec<Option<DualSensePad>>,
    /// Each pad's current full report — buttons/sticks merged with persisted touch + motion.
    state: Vec<DsState>,
    /// Last rumble forwarded per pad, so a report that only changes the LED doesn't re-send it.
    last_rumble: Vec<(u16, u16)>,
    /// When each pad last wrote an input report — drives [`DualSenseManager::heartbeat`], which
    /// re-emits the current state during input silence so the kernel never sees the device go quiet.
    last_write: Vec<Instant>,
    /// Pad creation failed (e.g. /dev/uhid permissions) — warn once, drop events.
    broken: bool,
}

impl Default for DualSenseManager {
    fn default() -> DualSenseManager {
        DualSenseManager::new()
    }
}

impl DualSenseManager {
    pub fn new() -> DualSenseManager {
        DualSenseManager {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            state: vec![DsState::neutral(); MAX_PADS],
            last_rumble: vec![(0, 0); MAX_PADS],
            last_write: vec![Instant::now(); MAX_PADS],
            broken: false,
        }
    }

    /// Handle one decoded controller event (create/destroy by mask, then merge button/stick state).
    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival (DualSense)");
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
                        tracing::info!(index = i, "controller unplugged (DualSense)");
                        *slot = None;
                        self.state[i] = DsState::neutral();
                        self.last_rumble[i] = (0, 0);
                    }
                }
                if f.active_mask & (1 << idx) == 0 {
                    return; // this event WAS the unplug
                }
                self.ensure(idx);
                // Merge buttons/sticks/triggers from the frame, preserving touch + motion (those
                // come on the rich-input plane and must survive a button-only frame).
                let prev = self.state[idx];
                let mut s = DsState::from_gamepad(
                    f.buttons,
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
                self.state[idx] = s;
                self.write(idx);
            }
        }
    }

    /// Apply one rich client→host event (touchpad contact / motion sample) to an existing pad,
    /// preserving its button/stick state. Rich events never create a pad (a controller must have
    /// arrived first); they're dropped if the pad isn't present.
    pub fn apply_rich(&mut self, rich: RichInput) {
        let idx = match rich {
            RichInput::Touchpad { pad, .. } | RichInput::Motion { pad, .. } => pad as usize,
        };
        if idx >= MAX_PADS || self.pads[idx].is_none() {
            return;
        }
        match rich {
            RichInput::Touchpad {
                finger,
                active,
                x,
                y,
                ..
            } => {
                // The DualSense touchpad carries two contacts; clamp to a valid slot and keep the
                // reported contact id consistent with it (the wire `finger` is untrusted).
                let slot = (finger as usize).min(1);
                let t = &mut self.state[idx].touch[slot];
                t.active = active;
                t.id = slot as u8;
                // Normalized 0..=65535 → the touchpad's coordinate range (0..=W-1 / 0..=H-1,
                // what the kernel advertises as the ABS_MT extents).
                t.x = ((x as u32 * (DS_TOUCH_W - 1) as u32) / u16::MAX as u32) as u16;
                t.y = ((y as u32 * (DS_TOUCH_H - 1) as u32) / u16::MAX as u32) as u16;
            }
            RichInput::Motion { gyro, accel, .. } => {
                self.state[idx].gyro = gyro;
                self.state[idx].accel = accel;
            }
        }
        self.write(idx);
    }

    fn write(&mut self, idx: usize) {
        let st = self.state[idx];
        if let Some(pad) = self.pads[idx].as_mut() {
            let _ = pad.write_state(&st);
        }
        // Reset the heartbeat timer on every write (real input or heartbeat), so an actively-used
        // pad emits no extra reports — the heartbeat only fills genuine input-silence gaps.
        self.last_write[idx] = Instant::now();
    }

    /// Re-emit each live pad's CURRENT report if it's been silent for `max_gap`. A real DualSense
    /// streams report `0x01` continuously (~250 Hz); the kernel `hid-playstation` driver / Proton /
    /// SDL treat a multi-second silence (a held-steady stick produces no wire events) as an
    /// unplugged controller — the "controller disconnected every few seconds" symptom. Re-sending
    /// the current state is idempotent (a stale-but-correct frame, never a phantom input);
    /// `write_state` bumps the report's seq + timestamp, so each is a fresh, well-formed report.
    pub fn heartbeat(&mut self, max_gap: Duration) {
        let now = Instant::now();
        for i in 0..self.pads.len() {
            if self.pads[i].is_some() && now.duration_since(self.last_write[i]) >= max_gap {
                self.write(i);
            }
        }
    }

    fn ensure(&mut self, idx: usize) {
        if idx >= MAX_PADS || self.pads[idx].is_some() || self.broken {
            return;
        }
        match DualSensePad::open(idx as u8) {
            Ok(p) => {
                tracing::info!(
                    index = idx,
                    "virtual DualSense created (UHID hid-playstation)"
                );
                self.pads[idx] = Some(p);
                self.state[idx] = DsState::neutral();
                self.last_rumble[idx] = (0, 0);
                self.last_write[idx] = Instant::now();
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual DualSense creation failed — controller input disabled");
                self.broken = true;
            }
        }
    }

    /// Service every pad: answer the kernel's init handshake and parse a game's feedback. `rumble`
    /// is invoked `(index, low, high)` only when the motor level *changes* (the universal 0xCA
    /// plane — both backends use it); `hidout` is invoked for each DualSense-only rich feedback
    /// event (lightbar / player LEDs / adaptive triggers — the 0xCD plane). Call frequently:
    /// the kernel blocks `hid-playstation` init until its GET_REPORTs are answered.
    pub fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16),
        mut hidout: impl FnMut(HidOutput),
    ) {
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_mut() else {
                continue;
            };
            let fb = pad.service(i as u8);
            if let Some(r) = fb.rumble {
                if self.last_rumble[i] != r {
                    self.last_rumble[i] = r;
                    rumble(i as u16, r.0, r.1);
                }
            }
            for h in fb.hidout {
                hidout(h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DualSense USB output report (`0x02`) with all valid-flags set parses into motor
    /// rumble (0xCA), lightbar, player LEDs, and both adaptive-trigger blocks (0xCD) — with
    /// the report's right-trigger-first layout mapped onto the wire's `which` (0 = L2).
    #[test]
    fn parse_output_report() {
        let mut data = vec![0u8; 48];
        data[0] = 0x02; // report id
        data[1] = 0x0F; // valid_flag0: vibration + haptics + R2 + L2 triggers
        data[2] = 0x14; // valid_flag1: lightbar + player indicators
        data[3] = 0x80; // right (high-freq) motor
        data[4] = 0x40; // left (low-freq) motor
        data[11] = 0x21; // right-trigger block mode byte (report bytes 11..22)
        data[22] = 0x26; // left-trigger block mode byte (report bytes 22..33)
        data[44] = 0x03; // player LEDs (low 5 bits)
        data[45] = 10; // R
        data[46] = 20; // G
        data[47] = 30; // B
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        // (low, high) = (left<<8, right<<8).
        assert_eq!(fb.rumble, Some((0x4000, 0x8000)));
        assert!(fb.hidout.contains(&HidOutput::Led {
            pad: 0,
            r: 10,
            g: 20,
            b: 30
        }));
        assert!(fb
            .hidout
            .contains(&HidOutput::PlayerLeds { pad: 0, bits: 3 }));
        // The report's FIRST block (bytes 11..22) is the RIGHT trigger → wire which = 1.
        let triggers: Vec<_> = fb
            .hidout
            .iter()
            .filter_map(|h| match h {
                HidOutput::Trigger { which, effect, .. } => Some((*which, effect[0])),
                _ => None,
            })
            .collect();
        assert_eq!(triggers, vec![(1, 0x21), (0, 0x26)]);
    }

    /// Writers set only the valid-flag bits for the fields they mean to change (the kernel
    /// zeroes the rest of the report) — a plain rumble write must NOT blank the lightbar /
    /// player LEDs / triggers, and an LED-only write must not stop the motors.
    #[test]
    fn parse_output_respects_valid_flags() {
        // Kernel-style rumble write: only the vibration flags set, everything else zero.
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0x03; // compatible vibration + haptics select
        data[3] = 0xFF;
        data[4] = 0xFF;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        assert_eq!(fb.rumble, Some((0xFF00, 0xFF00)));
        assert!(fb.hidout.is_empty(), "rumble write must not emit hidout");

        // Lightbar-only write: no rumble surfaced (would otherwise spam rumble-stops).
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[2] = 0x04; // lightbar control enable
        data[45] = 1;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        assert!(fb.rumble.is_none());
        assert_eq!(fb.hidout.len(), 1);
        assert!(matches!(fb.hidout[0], HidOutput::Led { r: 1, .. }));
    }

    /// The input report's sensor/touch bytes must land exactly where the kernel's
    /// `struct dualsense_input_report` reads them (gyro at struct offset 15, accel 21,
    /// timestamp 27, touch points 32 — report byte = struct offset + 1). A one-byte slip
    /// here turns client motion into noise and conjures phantom touch contacts.
    #[test]
    fn input_report_layout_matches_hid_playstation() {
        let mut st = DsState::neutral();
        st.gyro = [0x1122, 0x3344, 0x5566];
        st.accel = [0x778, 0x99A, 0xBBC];
        st.touch[0] = Touch {
            active: true,
            id: 5,
            x: 0x123,
            y: 0x356,
        };
        // touch[1] stays inactive — its NOT-active bit must be set.
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &st, 7, 0xAABBCCDD);
        assert_eq!(r[0], 0x01);
        assert_eq!(r[7], 7); // seq_number (struct off 6)
        assert_eq!(&r[16..22], &[0x22, 0x11, 0x44, 0x33, 0x66, 0x55]); // gyro LE
        assert_eq!(&r[22..28], &[0x78, 0x07, 0x9A, 0x09, 0xBC, 0x0B]); // accel LE
        assert_eq!(&r[28..32], &[0xDD, 0xCC, 0xBB, 0xAA]); // sensor_timestamp LE
                                                           // Touch point 1 at struct off 32 = r[33..37]: contact byte (active → bit7 clear),
                                                           // then 12-bit x / 12-bit y packed.
        assert_eq!(r[33], 5);
        assert_eq!(r[34], 0x23);
        assert_eq!(r[35], 0x61); // x_hi nibble 0x1 | (y & 0xF) << 4 (y=0x356 → 0x6 << 4)
        assert_eq!(r[36], 0x35); // y >> 4
        assert_eq!(r[37] & 0x80, 0x80); // touch point 2 inactive
                                        // status byte (struct off 52): discharging (high nibble 0) + full capacity (low nibble
                                        // 0xA → 100 %), so SteamOS/hid-playstation never reports a false "low battery".
        assert_eq!(r[53], 0x0A);
    }

    /// The wire touchpad-click bit (Moonlight's extended position) lands in `buttons[2]`.
    #[test]
    fn from_gamepad_maps_touchpad_click() {
        use punktfunk_core::input::gamepad as gs;
        let s = DsState::from_gamepad(gs::BTN_TOUCHPAD | gs::BTN_GUIDE, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], btn2::PS | btn2::TOUCHPAD);
        let s = DsState::from_gamepad(gs::BTN_A, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], 0);
    }

    /// A short / wrong-id report yields nothing.
    #[test]
    fn parse_output_rejects_garbage() {
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &[0x01, 0, 0], &mut fb); // wrong report id, too short
        assert!(fb.rumble.is_none());
        assert!(fb.hidout.is_empty());
    }
}
