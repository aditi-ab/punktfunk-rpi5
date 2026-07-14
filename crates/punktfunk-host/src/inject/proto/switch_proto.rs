//! Transport-independent Nintendo Switch Pro Controller contract — the report codec + canned
//! handshake replies the Linux UHID backend ([`super::switch_pro`]) drives `hid-nintendo` with.
//!
//! Everything here is pinned against the kernel driver source (drivers/hid/hid-nintendo.c —
//! the ONE consumer of these bytes; a virtual pad must answer its probe exactly or no input
//! devices appear):
//!
//! - **USB handshake**: 2-byte output reports `0x80 <cmd>` (handshake / baudrate / no-timeout),
//!   each ACKed with an input report `0x81 <cmd>` (`joycon_send_usb` matches only those two
//!   bytes).
//! - **Subcommands**: output report `0x01` (packet counter + 8 rumble bytes + subcommand id +
//!   args), ACKed with input report `0x21` — a 12-byte input-state header, then ack byte /
//!   echoed subcommand id / payload. The driver matches ONLY the echoed id (byte 14) and
//!   requires ≥ 49 bytes; real hardware sends 64.
//! - **SPI flash reads** (subcommand `0x10`): the driver reads the user-calibration magics
//!   (absent here → `0xFF 0xFF`, so it takes the factory path), the factory stick calibrations
//!   (9-byte packed 12-bit triples — max/center/min order DIFFERS left vs right), and the
//!   24-byte factory IMU calibration. We serve blobs chosen so the math is clean: sticks
//!   centered at [`STICK_CENTER`] ± [`STICK_RANGE`], IMU offsets 0 with the driver's default
//!   scales (accel 16384, gyro 13371) so raw units pass through 1:1.
//! - **Input report `0x30`**: 3 button bytes (bit layout per `JC_BTN_*`), two packed 12-bit
//!   stick triples, battery/connection, and 3 IMU sample frames (accel then gyro, i16 LE).
//! - **Rumble**: 4 encoded bytes per side in every `0x01`/`0x10` output; we decode the
//!   amplitude through the driver's own `joycon_rumble_amplitudes` table (inverted) back to the
//!   0..=0xFFFF wire magnitudes it was scaled from (left = strong/low, right = weak/high).
//!
//! Wire-mapping subtleties (see the plan doc, gamepad-new-types §4):
//! - **Positional swap.** Wire `BTN_A` is the SOUTH button (GameStream convention); on a Switch
//!   pad SOUTH is `B`. `from_gamepad` maps wire-south → the report's B bit (and X/Y likewise),
//!   so the physical-position ↔ glyph relationship stays correct end-to-end.
//! - **Units.** Wire motion is DualSense-convention (20 LSB/°·s, 10000 LSB/g); the report wants
//!   real-Pro-Controller raw units (≈14.247 LSB/°·s per `JC_IMU_GYRO_RES_PER_DPS`, 4096 LSB/g
//!   per `JC_IMU_ACCEL_RES_PER_G`), which our calibration blobs make the driver consume 1:1.

use punktfunk_core::input::gamepad as gs;

pub const SWITCH_VENDOR: u32 = 0x057E; // Nintendo Co., Ltd
pub const SWITCH_PRODUCT: u32 = 0x2009; // Pro Controller

/// Nintendo Switch Pro Controller **USB** HID report descriptor (203 bytes) — a verbatim
/// real-device capture (usbhid-dump off a wired Pro Controller; three independent public
/// captures agree byte-for-byte: mzyy94's usbhid-dump, ToadKing's full USB capture, and
/// spacemeowx2's annotated dump). Declares exactly the report ids `hid-nintendo` exchanges
/// wired (inputs 0x30/0x21/0x81, outputs 0x01/0x10/0x80/0x82); the driver reads raw events,
/// so the descriptor only has to `hid_parse()` — but this is what real hardware presents.
/// NOT the Bluetooth descriptor (that one is ~170 bytes with a different report set).
#[rustfmt::skip]
pub const PROCON_RDESC: &[u8] = &[
    0x05, 0x01, 0x15, 0x00, 0x09, 0x04, 0xA1, 0x01, 0x85, 0x30, 0x05, 0x01, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x0A, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0A, 0x55, 0x00, 0x65, 0x00, 0x81, 0x02,
    0x05, 0x09, 0x19, 0x0B, 0x29, 0x0E, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04, 0x81, 0x02,
    0x75, 0x01, 0x95, 0x02, 0x81, 0x03, 0x0B, 0x01, 0x00, 0x01, 0x00, 0xA1, 0x00, 0x0B, 0x30, 0x00,
    0x01, 0x00, 0x0B, 0x31, 0x00, 0x01, 0x00, 0x0B, 0x32, 0x00, 0x01, 0x00, 0x0B, 0x35, 0x00, 0x01,
    0x00, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x04, 0x81, 0x02, 0xC0, 0x0B,
    0x39, 0x00, 0x01, 0x00, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75,
    0x04, 0x95, 0x01, 0x81, 0x02, 0x05, 0x09, 0x19, 0x0F, 0x29, 0x12, 0x15, 0x00, 0x25, 0x01, 0x75,
    0x01, 0x95, 0x04, 0x81, 0x02, 0x75, 0x08, 0x95, 0x34, 0x81, 0x03, 0x06, 0x00, 0xFF, 0x85, 0x21,
    0x09, 0x01, 0x75, 0x08, 0x95, 0x3F, 0x81, 0x03, 0x85, 0x81, 0x09, 0x02, 0x75, 0x08, 0x95, 0x3F,
    0x81, 0x03, 0x85, 0x01, 0x09, 0x03, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, 0x85, 0x10, 0x09, 0x04,
    0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, 0x85, 0x80, 0x09, 0x05, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83,
    0x85, 0x82, 0x09, 0x06, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, 0xC0,
];
/// Every input report we emit is the full USB size (the driver requires ≥ 49 for `0x21`).
pub const SWITCH_REPORT_LEN: usize = 64;

/// Stick raw center + full-deflection range of OUR virtual pad's calibration (12-bit axis).
/// The factory blobs below advertise exactly this, so the driver maps
/// `center ± range → ∓/± 32767` — one clean linear scale from the wire values.
pub const STICK_CENTER: u16 = 2048;
pub const STICK_RANGE: u16 = 1400;

/// `battery and connection info` byte (report byte 2): high 3 bits = level (4 = full),
/// BIT(4) = charging, BIT(0) = host powered — "full + charging + wired", so no low-battery
/// warnings ever.
pub const BAT_CON_FULL_WIRED: u8 = 0x91;
/// `vibrator_report` (report byte 12): must be non-zero or the driver stops pumping its rumble
/// queue (`joycon_ctlr_read_handler` gates on it). Real hardware sends 0x70-ish.
pub const VIBRATOR_READY: u8 = 0x70;

// Button bits of the 24-bit little-endian button field (report bytes 3..6), per the kernel's
// JC_BTN_* defines.
pub mod btn {
    pub const Y: u32 = 1 << 0;
    pub const X: u32 = 1 << 1;
    pub const B: u32 = 1 << 2;
    pub const A: u32 = 1 << 3;
    pub const R: u32 = 1 << 6;
    pub const ZR: u32 = 1 << 7;
    pub const MINUS: u32 = 1 << 8;
    pub const PLUS: u32 = 1 << 9;
    pub const RSTICK: u32 = 1 << 10;
    pub const LSTICK: u32 = 1 << 11;
    pub const HOME: u32 = 1 << 12;
    pub const CAPTURE: u32 = 1 << 13;
    pub const DOWN: u32 = 1 << 16;
    pub const UP: u32 = 1 << 17;
    pub const RIGHT: u32 = 1 << 18;
    pub const LEFT: u32 = 1 << 19;
    pub const L: u32 = 1 << 22;
    pub const ZL: u32 = 1 << 23;
}

/// Full Pro Controller state serialized into report `0x30` (and the `0x21` reply headers).
/// Sticks are the RAW 12-bit values ([`STICK_CENTER`]-centered); motion is raw IMU units.
#[derive(Clone, Copy)]
pub struct SwitchState {
    /// 24-bit `JC_BTN_*` field.
    pub buttons: u32,
    pub lx: u16,
    pub ly: u16,
    pub rx: u16,
    pub ry: u16,
    /// Raw gyro (≈14.247 LSB/°·s) and accel (4096 LSB/g), driver axis order x/y/z.
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
}

impl SwitchState {
    /// Centered sticks, nothing pressed, flat at rest (1 g on +Z — a pad lying on the desk, so
    /// SDL/games don't see a free-falling controller).
    pub fn neutral() -> SwitchState {
        SwitchState {
            buttons: 0,
            lx: STICK_CENTER,
            ly: STICK_CENTER,
            rx: STICK_CENTER,
            ry: STICK_CENTER,
            gyro: [0; 3],
            accel: [0, 0, 4096],
        }
    }

    /// Map a GameStream/XInput pad frame into Pro Controller state. Face buttons are mapped
    /// **positionally** (wire A = south → Switch B, etc. — see the module doc); triggers are
    /// digital on a Pro Controller, so any analog pull presses ZL/ZR. The wire paddles have no
    /// Switch slot — fold them via [`super::steam_remap`] BEFORE calling this (like the
    /// DualSense-family backends do).
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> SwitchState {
        let on = |bit: u32| buttons & bit != 0;
        let mut b = 0u32;
        // Positional: wire south/east/west/north → the Switch button at that position.
        if on(gs::BTN_A) {
            b |= btn::B; // south
        }
        if on(gs::BTN_B) {
            b |= btn::A; // east
        }
        if on(gs::BTN_X) {
            b |= btn::Y; // west
        }
        if on(gs::BTN_Y) {
            b |= btn::X; // north
        }
        if on(gs::BTN_LB) {
            b |= btn::L;
        }
        if on(gs::BTN_RB) {
            b |= btn::R;
        }
        if lt > 0 {
            b |= btn::ZL;
        }
        if rt > 0 {
            b |= btn::ZR;
        }
        if on(gs::BTN_BACK) {
            b |= btn::MINUS;
        }
        if on(gs::BTN_START) {
            b |= btn::PLUS;
        }
        if on(gs::BTN_LS_CLICK) {
            b |= btn::LSTICK;
        }
        if on(gs::BTN_RS_CLICK) {
            b |= btn::RSTICK;
        }
        if on(gs::BTN_GUIDE) {
            b |= btn::HOME;
        }
        if on(gs::BTN_MISC1) {
            b |= btn::CAPTURE;
        }
        if on(gs::BTN_DPAD_UP) {
            b |= btn::UP;
        }
        if on(gs::BTN_DPAD_DOWN) {
            b |= btn::DOWN;
        }
        if on(gs::BTN_DPAD_LEFT) {
            b |= btn::LEFT;
        }
        if on(gs::BTN_DPAD_RIGHT) {
            b |= btn::RIGHT;
        }
        SwitchState {
            buttons: b,
            lx: stick_raw(lx),
            ly: stick_raw(ly),
            rx: stick_raw(rx),
            ry: stick_raw(ry),
            ..SwitchState::neutral()
        }
    }

    /// Apply a wire motion sample (DualSense-convention units) as raw IMU values. No axis flip:
    /// both conventions are x-toward-triggers / z-up for a Pro Controller held like a DualSense,
    /// and the driver applies no negation for the Pro (only the right Joy-Con negates).
    pub fn apply_motion(&mut self, gyro: [i16; 3], accel: [i16; 3]) {
        // gyro: wire 20 LSB/°·s → raw 14.247 LSB/°·s; accel: wire 10000 LSB/g → raw 4096 LSB/g.
        self.gyro = gyro.map(|v| ((v as i32 * 14247) / 20000) as i16);
        self.accel = accel.map(|v| ((v as i32 * 4096) / 10000) as i16);
    }
}

/// Wire stick value (i16, +32767 = right/up) → raw 12-bit axis. The driver Y-negates BOTH the
/// wire's and evdev's conventions away: it computes `evdev_y = -scale(raw_y)`, and evdev's
/// gamepad convention is negative-up — so wire +y (up) maps to raw above-center, exactly like x.
pub fn stick_raw(v: i16) -> u16 {
    let raw = STICK_CENTER as i32 + (v as i32 * STICK_RANGE as i32) / 32767;
    raw.clamp(0, 0xFFF) as u16
}

/// Pack two 12-bit values into the 3-byte stick / calibration wire form
/// (`hid_field_extract` little-endian bitfield order).
pub fn pack12(a: u16, b: u16) -> [u8; 3] {
    [
        (a & 0xFF) as u8,
        ((a >> 8) & 0x0F) as u8 | ((b & 0x0F) << 4) as u8,
        ((b >> 4) & 0xFF) as u8,
    ]
}

/// Write the shared 13-byte input-state header (report id .. `vibrator_report`) that both the
/// `0x30` stream and every `0x21` subcommand reply carry.
fn write_header(r: &mut [u8; SWITCH_REPORT_LEN], id: u8, st: &SwitchState, timer: u8) {
    r[0] = id;
    r[1] = timer;
    r[2] = BAT_CON_FULL_WIRED;
    r[3] = (st.buttons & 0xFF) as u8;
    r[4] = ((st.buttons >> 8) & 0xFF) as u8;
    r[5] = ((st.buttons >> 16) & 0xFF) as u8;
    r[6..9].copy_from_slice(&pack12(st.lx, st.ly));
    r[9..12].copy_from_slice(&pack12(st.rx, st.ry));
    r[12] = VIBRATOR_READY;
}

/// Serialize the full/standard input report `0x30`: state header + 3 IMU sample frames
/// (accel x/y/z then gyro x/y/z, i16 LE — `struct joycon_imu_data`). We repeat the current
/// sample across all three 5 ms sub-frames (we sample per report, not per sub-frame).
pub fn serialize_report_0x30(st: &SwitchState, timer: u8) -> [u8; SWITCH_REPORT_LEN] {
    let mut r = [0u8; SWITCH_REPORT_LEN];
    write_header(&mut r, 0x30, st, timer);
    for frame in 0..3 {
        let off = 13 + frame * 12;
        for (i, v) in st.accel.iter().enumerate() {
            r[off + i * 2..off + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in st.gyro.iter().enumerate() {
            r[off + 6 + i * 2..off + 6 + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
    r
}

/// Build the `0x81 <cmd>` input report acknowledging a USB `0x80 <cmd>` command
/// (`joycon_send_usb` matches exactly those two bytes).
pub fn build_usb_ack(cmd: u8) -> [u8; SWITCH_REPORT_LEN] {
    let mut r = [0u8; SWITCH_REPORT_LEN];
    r[0] = 0x81;
    r[1] = cmd;
    r
}

/// Build a `0x21` subcommand reply: state header, then ack / echoed subcommand id / payload.
/// The driver matches on the echoed id only; the MSB-set ack byte mirrors real hardware
/// (`0x80` plain ack, `0x80 | data-type` when a payload follows).
pub fn build_subcmd_reply(
    st: &SwitchState,
    timer: u8,
    ack: u8,
    subcmd: u8,
    payload: &[u8],
) -> [u8; SWITCH_REPORT_LEN] {
    let mut r = [0u8; SWITCH_REPORT_LEN];
    write_header(&mut r, 0x21, st, timer);
    r[13] = ack;
    r[14] = subcmd;
    let n = payload.len().min(SWITCH_REPORT_LEN - 15);
    r[15..15 + n].copy_from_slice(&payload[..n]);
    r
}

/// The device-info payload (subcommand `0x02`): firmware 4.33, type `0x03` = **Pro Controller**
/// (`ctlr_type` — the value that selects the Pro button/stick/IMU paths), `0x02`, the 6-byte
/// MAC (parsed into `ctlr->mac_addr`, printed + used as the input devices' `uniq`), `0x01`,
/// and `0x01` = "colors in SPI" (not read by the driver).
pub fn device_info_payload(mac: &[u8; 6]) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[0] = 0x04;
    p[1] = 0x21;
    p[2] = 0x03; // JOYCON_CTLR_TYPE_PRO
    p[3] = 0x02;
    p[4..10].copy_from_slice(mac);
    p[10] = 0x01;
    p[11] = 0x01;
    p
}

/// A stable per-pad virtual MAC (Nintendo OUI + our index) — the driver requires one from
/// device info and keys the input devices' `uniq` off it.
pub fn switch_mac(index: u8) -> [u8; 6] {
    [0x7C, 0xBB, 0x8A, 0xDF, 0x00, index]
}

/// The canned SPI-flash contents (subcommand `0x10`): reply payload = echoed LE address +
/// echoed length + the flash bytes. `None` for an unmapped range (the caller then replies with
/// zeroes — the driver falls back to defaults rather than aborting).
///
/// Served ranges:
/// - `0x8010`/`0x801B`/`0x8026` (user-cal magics, 2 B): NOT `0xB2 0xA1` → user cal absent, the
///   driver takes the factory path.
/// - `0x603D`/`0x6046` (factory stick cal, 9 B): [`STICK_CENTER`] ± [`STICK_RANGE`] on every
///   axis. **Byte order differs**: left = max-above ++ center ++ min-below; right = center ++
///   min-below ++ max-above (`joycon_read_stick_calibration`).
/// - `0x6020` (factory IMU cal, 24 B): offsets 0, accel scale 16384, gyro scale 13371 — the
///   driver's own defaults, making its per-sample math the identity (accel) / ×1000 (gyro).
pub fn spi_flash_read(addr: u32, len: u8) -> Option<Vec<u8>> {
    let cal_pair = pack12(STICK_RANGE, STICK_RANGE);
    let center_pair = pack12(STICK_CENTER, STICK_CENTER);
    let data: Vec<u8> = match (addr, len) {
        (0x8010 | 0x801B | 0x8026, 2) => vec![0xFF, 0xFF],
        (0x603D, 9) => [cal_pair, center_pair, cal_pair].concat(),
        (0x6046, 9) => [center_pair, cal_pair, cal_pair].concat(),
        (0x6020, 24) => {
            let mut v = Vec::with_capacity(24);
            v.extend_from_slice(&[0u8; 6]); // accel offsets = 0
            for _ in 0..3 {
                v.extend_from_slice(&16384u16.to_le_bytes()); // accel scale (driver default)
            }
            v.extend_from_slice(&[0u8; 6]); // gyro offsets = 0
            for _ in 0..3 {
                v.extend_from_slice(&13371u16.to_le_bytes()); // gyro scale (driver default)
            }
            v
        }
        _ => return None,
    };
    let mut payload = Vec::with_capacity(5 + data.len());
    payload.extend_from_slice(&addr.to_le_bytes());
    payload.push(len);
    payload.extend_from_slice(&data);
    Some(payload)
}

/// One decoded host-bound output report from the driver.
pub enum SwitchOutput {
    /// `0x80 <cmd>` USB command — answer with [`build_usb_ack`].
    UsbCmd(u8),
    /// `0x01` subcommand (with its rumble bytes) — answer with a `0x21` reply.
    Subcmd {
        id: u8,
        /// Subcommand argument bytes (report bytes 11..).
        args: Vec<u8>,
        /// Decoded rumble `(low, high)` magnitudes.
        rumble: (u16, u16),
    },
    /// `0x10` rumble-only report — no reply expected.
    Rumble((u16, u16)),
}

/// Parse one output report from the driver. Returns `None` for anything unrecognized/short.
pub fn parse_output(data: &[u8]) -> Option<SwitchOutput> {
    match *data.first()? {
        0x80 => Some(SwitchOutput::UsbCmd(*data.get(1)?)),
        0x01 if data.len() >= 11 => Some(SwitchOutput::Subcmd {
            id: data[10],
            args: data.get(11..).map(|s| s.to_vec()).unwrap_or_default(),
            rumble: decode_rumble(&data[2..10]),
        }),
        0x10 if data.len() >= 10 => Some(SwitchOutput::Rumble(decode_rumble(&data[2..10]))),
        _ => None,
    }
}

/// The driver's `joycon_rumble_amplitudes` table, amplitude column only, indexed by
/// `amp_high / 2` (the encoded high-band amplitude byte is always even). Copied verbatim from
/// hid-nintendo.c; last entry = `joycon_max_rumble_amp` (1003).
#[rustfmt::skip]
const RUMBLE_AMPS: [u16; 101] = [
       0,   10,   12,   14,   17,   20,   24,   28,   33,   40,
      47,   56,   67,   80,   95,  112,  117,  123,  128,  134,
     140,  146,  152,  159,  166,  173,  181,  189,  198,  206,
     215,  225,  230,  235,  240,  245,  251,  256,  262,  268,
     273,  279,  286,  292,  298,  305,  311,  318,  325,  332,
     340,  347,  355,  362,  370,  378,  387,  395,  404,  413,
     422,  431,  440,  450,  460,  470,  480,  491,  501,  512,
     524,  535,  547,  559,  571,  584,  596,  609,  623,  636,
     650,  665,  679,  694,  709,  725,  741,  757,  773,  790,
     808,  825,  843,  862,  881,  900,  920,  940,  960,  981,
    1003,
];

/// Invert the driver's per-side rumble encoding back to the 0..=0xFFFF magnitude it scaled
/// from: byte1's even bits carry the amplitude-table index × 2 (`data[1] = freq_high_lo +
/// amp.high`, where the freq contribution is only ever bit 0).
fn side_amplitude(side: &[u8]) -> u16 {
    let idx = ((side[1] & 0xFE) / 2) as usize;
    let amp = RUMBLE_AMPS[idx.min(RUMBLE_AMPS.len() - 1)] as u32;
    // Driver: amp = magnitude * 1003 / 65535 — invert, saturating at full scale.
    ((amp * 65535) / 1003).min(65535) as u16
}

/// Decode the 8 rumble bytes (left side = strong → wire `low`, right side = weak → wire
/// `high`, per `joycon_play_effect`).
pub fn decode_rumble(bytes: &[u8]) -> (u16, u16) {
    if bytes.len() < 8 {
        return (0, 0);
    }
    (side_amplitude(&bytes[..4]), side_amplitude(&bytes[4..8]))
}

/// Decode a player-lights subcommand payload (`(flash << 4) | on`, one bit per LED) into the
/// wire `PlayerLeds` bits: a flashing LED counts as on.
pub fn player_leds_bits(arg: u8) -> u8 {
    (arg & 0x0F) | (arg >> 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The positional swap, pinned: wire south/east/west/north land on the Switch B/A/Y/X bits
    /// (the driver then maps them back to BTN_SOUTH/EAST/WEST/NORTH — position-correct
    /// end-to-end), and the rest of the buttons land on their JC_BTN_* bits.
    #[test]
    fn positional_swap_and_button_bits() {
        let st = SwitchState::from_gamepad(gs::BTN_A, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::B);
        let st = SwitchState::from_gamepad(gs::BTN_B, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::A);
        let st = SwitchState::from_gamepad(gs::BTN_X, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::Y);
        let st = SwitchState::from_gamepad(gs::BTN_Y, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::X);
        // Shoulders / sticks / meta / dpad / triggers-as-digital.
        let st = SwitchState::from_gamepad(
            gs::BTN_LB | gs::BTN_RB | gs::BTN_BACK | gs::BTN_START | gs::BTN_GUIDE | gs::BTN_MISC1,
            0,
            0,
            0,
            0,
            255,
            1,
        );
        assert_eq!(
            st.buttons,
            btn::L | btn::R | btn::MINUS | btn::PLUS | btn::HOME | btn::CAPTURE | btn::ZL | btn::ZR
        );
        let st = SwitchState::from_gamepad(gs::BTN_DPAD_UP | gs::BTN_DPAD_LEFT, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::UP | btn::LEFT);
    }

    /// Sticks: wire full deflection → center ± range on the raw 12-bit axis, both axes the same
    /// direction (the driver's own Y negation restores evdev's negative-up).
    #[test]
    fn stick_scaling() {
        assert_eq!(stick_raw(0), STICK_CENTER);
        assert_eq!(stick_raw(32767), STICK_CENTER + STICK_RANGE);
        assert_eq!(stick_raw(-32767), STICK_CENTER - STICK_RANGE);
        // Extreme min doesn't underflow past the 12-bit range.
        assert!(stick_raw(i16::MIN) <= 0xFFF);
    }

    /// The 3-byte 12-bit packing matches `hid_field_extract`'s little-endian bitfield order:
    /// value A at bit 0, value B at bit 12.
    #[test]
    fn pack12_layout() {
        assert_eq!(pack12(0x578, 0x578), [0x78, 0x85, 0x57]); // 1400/1400 (the cal pair)
        assert_eq!(pack12(0x800, 0x800), [0x00, 0x08, 0x80]); // 2048/2048 (the center pair)
        // Extract back: a = b0 | (b1 & 0xF) << 8; b = (b1 >> 4) | b2 << 4.
        let p = pack12(0xABC, 0x123);
        let a = p[0] as u16 | ((p[1] as u16 & 0xF) << 8);
        let b = ((p[1] as u16) >> 4) | ((p[2] as u16) << 4);
        assert_eq!((a, b), (0xABC, 0x123));
    }

    /// Report 0x30 layout, pinned against `struct joycon_input_report` + `joycon_imu_data`:
    /// header bytes, packed sticks, and the 3 × 12-byte IMU frames (accel then gyro, LE).
    #[test]
    fn report_0x30_layout() {
        let mut st = SwitchState::neutral();
        st.buttons = btn::B | btn::MINUS | btn::ZL;
        st.gyro = [0x1122, -2, 3];
        st.accel = [-1, 0x3344, 5];
        let r = serialize_report_0x30(&st, 7);
        assert_eq!(r[0], 0x30);
        assert_eq!(r[1], 7);
        assert_eq!(r[2], BAT_CON_FULL_WIRED);
        assert_eq!(r[3], 0x04); // B = bit 2
        assert_eq!(r[4], 0x01); // MINUS = bit 8
        assert_eq!(r[5], 0x80); // ZL = bit 23
        assert_eq!(&r[6..9], &pack12(STICK_CENTER, STICK_CENTER));
        assert_eq!(&r[9..12], &pack12(STICK_CENTER, STICK_CENTER));
        assert_eq!(r[12], VIBRATOR_READY);
        // Frame 0 at byte 13: accel x/y/z then gyro x/y/z, i16 LE.
        assert_eq!(&r[13..15], &(-1i16).to_le_bytes());
        assert_eq!(&r[15..17], &0x3344u16.to_le_bytes());
        assert_eq!(&r[19..21], &0x1122u16.to_le_bytes());
        // Frames repeat identically at +12 and +24.
        assert_eq!(&r[13..25], &r[25..37]);
        assert_eq!(&r[13..25], &r[37..49]);
    }

    /// Subcommand replies: ≥ 49 bytes (we send 64), ack at byte 13, echoed id at byte 14 (the
    /// ONLY byte the driver's matcher checks), payload from byte 15.
    #[test]
    fn subcmd_reply_layout() {
        let st = SwitchState::neutral();
        let r = build_subcmd_reply(&st, 3, 0x90, 0x10, &[0xAA, 0xBB]);
        assert_eq!(r.len(), SWITCH_REPORT_LEN);
        assert_eq!(r[0], 0x21);
        assert_eq!(r[13], 0x90);
        assert_eq!(r[14], 0x10);
        assert_eq!(&r[15..17], &[0xAA, 0xBB]);
        // USB ack: exactly the two bytes joycon_send_usb matches.
        let a = build_usb_ack(0x02);
        assert_eq!((a[0], a[1]), (0x81, 0x02));
    }

    /// SPI blobs: magics read as ABSENT (≠ B2 A1); the stick blobs put center strictly between
    /// min and max on both axes in the driver's per-side byte order; the reply echoes addr+len.
    #[test]
    fn spi_blobs_valid() {
        for addr in [0x8010u32, 0x801B, 0x8026] {
            let p = spi_flash_read(addr, 2).unwrap();
            assert_eq!(&p[..4], &addr.to_le_bytes());
            assert_eq!(p[4], 2);
            assert!(!(p[5] == 0xB2 && p[6] == 0xA1));
        }
        let unpack = |b: &[u8]| -> (u16, u16) {
            let a = b[0] as u16 | ((b[1] as u16 & 0xF) << 8);
            let y = ((b[1] as u16) >> 4) | ((b[2] as u16) << 4);
            (a, y)
        };
        // Left: max-above ++ center ++ min-below.
        let l = spi_flash_read(0x603D, 9).unwrap();
        let (data, hdr) = (&l[5..], &l[..5]);
        assert_eq!(hdr, &[0x3D, 0x60, 0, 0, 9]);
        let (max_above, _) = unpack(&data[0..3]);
        let (center, _) = unpack(&data[3..6]);
        let (min_below, _) = unpack(&data[6..9]);
        assert_eq!(center, STICK_CENTER);
        assert!(center - min_below < center && center < center + max_above);
        // Right: center ++ min-below ++ max-above.
        let r = spi_flash_read(0x6046, 9).unwrap();
        let (rc, _) = unpack(&r[5..8]);
        assert_eq!(rc, STICK_CENTER);
        // IMU: offsets 0, driver-default scales — the identity calibration.
        let imu = spi_flash_read(0x6020, 24).unwrap();
        let d = &imu[5..];
        assert_eq!(&d[0..6], &[0; 6]);
        assert_eq!(&d[6..8], &16384u16.to_le_bytes());
        assert_eq!(&d[12..18], &[0; 6]);
        assert_eq!(&d[18..20], &13371u16.to_le_bytes());
        // Unmapped range → None.
        assert!(spi_flash_read(0x6050, 12).is_none());
    }

    /// Motion unit conversion: wire (20 LSB/°·s, 10000 LSB/g) → raw (14.247 LSB/°·s, 4096 LSB/g).
    #[test]
    fn motion_units() {
        let mut st = SwitchState::neutral();
        // 100 °/s = wire 2000 → raw ≈ 1424; 1 g = wire 10000 → raw 4096.
        st.apply_motion([2000, 0, -2000], [10000, -10000, 0]);
        assert_eq!(st.gyro, [1424, 0, -1424]);
        assert_eq!(st.accel, [4096, -4096, 0]);
    }

    /// Rumble decode inverts the driver's encoder: a neutral packet decodes to silence; the
    /// max-amplitude packet decodes to full scale; left = low/strong, right = high/weak.
    #[test]
    fn rumble_decode() {
        // Neutral per the driver's tables: freq defaults + amp 0.
        let neutral = [0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40];
        assert_eq!(decode_rumble(&neutral), (0, 0));
        // Max amp (0xC8 → index 100 → 1003 → 65535) on the LEFT only → (low=full, high=0).
        let left_max = [0x00, 0xC8, 0x40, 0x72, 0x00, 0x01, 0x40, 0x40];
        assert_eq!(decode_rumble(&left_max), (65535, 0));
        // Mid-table on the right: amp_high 0x20 → index 16 → 117 → 117*65535/1003 = 7644.
        let right_mid = [0x00, 0x01, 0x40, 0x40, 0x00, 0x20, 0x48, 0x40];
        assert_eq!(decode_rumble(&right_mid), (0, 7644));
        // The freq bit riding data[1] bit0 must not disturb the amplitude index.
        let with_freq_bit = [0x00, 0x21, 0x48, 0x40, 0x00, 0x01, 0x40, 0x40];
        assert_eq!(decode_rumble(&with_freq_bit).0, 7644);
        // Short slice → silence, not a panic.
        assert_eq!(decode_rumble(&[0x10; 4]), (0, 0));
    }

    /// Output-report parse: the three shapes the driver sends.
    #[test]
    fn parse_output_shapes() {
        assert!(matches!(
            parse_output(&[0x80, 0x02]),
            Some(SwitchOutput::UsbCmd(0x02))
        ));
        let mut sub = vec![0x01, 0x05];
        sub.extend_from_slice(&[0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40]);
        sub.push(0x10); // subcmd id
        sub.extend_from_slice(&[0x3D, 0x60, 0x00, 0x00, 0x09]); // SPI addr+len args
        match parse_output(&sub) {
            Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                assert_eq!(id, 0x10);
                assert_eq!(&args[..5], &[0x3D, 0x60, 0x00, 0x00, 0x09]);
                assert_eq!(rumble, (0, 0));
            }
            _ => panic!("expected subcmd"),
        }
        let mut rum = vec![0x10, 0x06];
        rum.extend_from_slice(&[0x00, 0xC8, 0x40, 0x72, 0x00, 0x01, 0x40, 0x40]);
        assert!(matches!(
            parse_output(&rum),
            Some(SwitchOutput::Rumble((65535, 0)))
        ));
        assert!(parse_output(&[0x21]).is_none());
        assert!(parse_output(&[]).is_none());
    }

    /// Player lights: solid + flashing nibbles both count as lit.
    #[test]
    fn player_lights() {
        assert_eq!(player_leds_bits(0x01), 0b0001);
        assert_eq!(player_leds_bits(0x10), 0b0001); // flashing LED 1
        assert_eq!(player_leds_bits(0x23), 0b0011 | 0b0010);
    }

    /// Device info: type byte 0x03 (Pro Controller) at payload[2], MAC at [4..10].
    #[test]
    fn device_info_shape() {
        let mac = switch_mac(3);
        let p = device_info_payload(&mac);
        assert_eq!(p[2], 0x03);
        assert_eq!(&p[4..10], &mac);
        assert_eq!(mac[5], 3);
    }
}
