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

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

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
    /// Set from a touchpad-press rich event (no equivalent on the GameStream xpad).
    #[allow(dead_code)]
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

fn pack_touch(dst: &mut [u8], t: &Touch) {
    // byte0: bit7 = NOT active (1 = no contact), bits0-6 = contact id.
    dst[0] = (t.id & 0x7F) | if t.active { 0 } else { 0x80 };
    let (x, y) = (t.x.min(DS_TOUCH_W), t.y.min(DS_TOUCH_H));
    dst[1] = (x & 0xFF) as u8;
    dst[2] = (((x >> 8) & 0x0F) as u8) | (((y & 0x0F) as u8) << 4);
    dst[3] = ((y >> 4) & 0xFF) as u8;
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
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        r[0] = 0x01; // report id; the struct fields follow (struct offset 0 == r[1])
        r[1] = st.lx;
        r[2] = st.ly;
        r[3] = st.rx;
        r[4] = st.ry;
        r[5] = st.l2;
        r[6] = st.r2;
        self.seq = self.seq.wrapping_add(1);
        r[7] = self.seq; // seq_number (struct off 6)
        r[8] = (st.dpad & 0x0F) | (st.buttons[0] & 0xF0); // off 7: dpad + face buttons
        r[9] = st.buttons[1]; // off 8
        r[10] = st.buttons[2]; // off 9
        r[11] = st.buttons[3]; // off 10
        for (i, v) in st.gyro.iter().enumerate() {
            r[15 + i * 2..17 + i * 2].copy_from_slice(&v.to_le_bytes()); // gyro at struct off 14
        }
        for (i, v) in st.accel.iter().enumerate() {
            r[21 + i * 2..23 + i * 2].copy_from_slice(&v.to_le_bytes()); // accel at struct off 20
        }
        self.ts = self.ts.wrapping_add(1); // monotonic sensor timestamp is all the kernel needs
        r[27..31].copy_from_slice(&self.ts.to_le_bytes()); // sensor_timestamp (struct off 26)
        pack_touch(&mut r[34..38], &st.touch[0]); // touch point 1 (struct off 33)
        pack_touch(&mut r[38..42], &st.touch[1]); // touch point 2

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
    /// [`HidOutput`] events for pad `pad`. Call frequently — especially right after [`open`] so the
    /// init handshake completes. The fd is `O_NONBLOCK`, so once drained `read` returns `WouldBlock`.
    pub fn service(&mut self, pad: u8) -> Vec<punktfunk_core::quic::HidOutput> {
        let mut out = Vec::new();
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
                    parse_ds_output(pad, &ev[4..end], &mut out);
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
        out
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

/// Parse a DualSense USB output report (`0x02`) into [`HidOutput`] events. The byte layout below
/// is the USB DualSense common report; only the well-understood fields (motor rumble, lightbar
/// RGB, player LEDs) are surfaced — adaptive-trigger blocks are forwarded raw for the client.
fn parse_ds_output(pad: u8, data: &[u8], out: &mut Vec<punktfunk_core::quic::HidOutput>) {
    use punktfunk_core::quic::HidOutput;
    // data[0] is the report id (0x02). Be defensive about short reports.
    if data.first() != Some(&0x02) || data.len() < 48 {
        return;
    }
    // Lightbar RGB (USB common report: bytes 45..48). Player LEDs at byte 44.
    let (r, g, b) = (data[45], data[46], data[47]);
    out.push(HidOutput::Led { pad, r, g, b });
    out.push(HidOutput::PlayerLeds {
        pad,
        bits: data[44] & 0x1F,
    });
    // Adaptive-trigger parameter blocks: L2 at bytes 11..22, R2 at 22..33 (11 bytes each).
    if data.len() >= 33 {
        out.push(HidOutput::Trigger {
            pad,
            which: 0,
            effect: data[11..22].to_vec(),
        });
        out.push(HidOutput::Trigger {
            pad,
            which: 1,
            effect: data[22..33].to_vec(),
        });
    }
}
