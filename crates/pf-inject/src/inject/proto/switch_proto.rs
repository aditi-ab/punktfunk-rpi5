//! Nintendo Switch Pro Controller report codec and handshake replies for the
//! Linux UHID backend ([`super::switch_pro`]).
//!
//! Pinned to `drivers/hid/hid-nintendo.c`. Miss the probe and no input device appears.
//!
//! USB: output `0x80 <cmd>` → input `0x81 <cmd>` (`joycon_send_usb` matches those two
//! bytes). Subcommand `0x01` → `0x21` (≥ 49 bytes; we send 64); the driver matches only
//! the echoed id at byte 14. SPI `0x10` is served by address range so SDL's 18-byte /
//! 22-byte reads see the same factory blobs as the kernel's 9-byte ones. Input `0x30`
//! is buttons + packed sticks + three IMU frames.
//!
//! Face buttons are positional (wire south → report B). Wire motion is DualSense units
//! (20 LSB/°·s, 10000 LSB/g); the report is raw Pro units (14.247 LSB/°·s, 4096 LSB/g)
//! via the factory-calibration identity. Evidence: this module's tests and hid-nintendo.c.

use punktfunk_core::input::gamepad as gs;

pub const SWITCH_VENDOR: u32 = 0x057E; // Nintendo Co., Ltd
pub const SWITCH_PRODUCT: u32 = 0x2009; // Pro Controller

/// `JC_IMU_GYRO_RES_PER_DPS` in thousandths so 14.247 stays exact. Factory IMU cal is
/// the driver's identity default, so reports are consumed at this ratio.
const JC_IMU_GYRO_MILLI_RES_PER_DPS: i32 = 14_247;
/// `JC_IMU_ACCEL_RES_PER_G`. Same identity-cal path as gyro.
const JC_IMU_ACCEL_RES_PER_G: i32 = 4096;

/// Wired Pro Controller USB HID report descriptor (203 bytes). Report ids the driver
/// exchanges: in 0x30/0x21/0x81, out 0x01/0x10/0x80/0x82. Not the Bluetooth descriptor
/// (~170 bytes, different report set).
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
/// USB report size. The driver rejects `0x21` shorter than 49 bytes.
pub const SWITCH_REPORT_LEN: usize = 64;

/// 12-bit factory cal we advertise: driver maps `center ± range` → `∓/± 32767`.
pub const STICK_CENTER: u16 = 2048;
pub const STICK_RANGE: u16 = 1400;

/// Report byte 2: full + charging + wired (`0x91`). Suppresses low-battery warnings.
pub const BAT_CON_FULL_WIRED: u8 = 0x91;
/// Report byte 12. Zero here stops the driver's rumble queue (`joycon_ctlr_read_handler`).
pub const VIBRATOR_READY: u8 = 0x70;

// 24-bit LE button field (report bytes 3..6), `JC_BTN_*` in hid-nintendo.c.
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

/// Raw 12-bit sticks ([`STICK_CENTER`]-based) and raw IMU units for report `0x30` / `0x21`.
#[derive(Clone, Copy)]
pub struct SwitchState {
    pub buttons: u32,
    pub lx: u16,
    pub ly: u16,
    pub rx: u16,
    pub ry: u16,
    /// Raw gyro (~14.247 LSB/°·s) and accel (4096 LSB/g), driver axis order x/y/z.
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
}

impl SwitchState {
    /// Centered, unpressed, 1 g on +Z (pad at rest). Zero accel looks like free-fall.
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

    /// Positional face map (wire south → Switch B). Analog triggers become ZL/ZR.
    /// Fold paddles through [`super::steam_remap`] first — they have no Switch slot.
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

    /// Zero gyro only. Gravity stays. True iff the sample changed (`PadProto::neutralize_gyro`).
    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    /// Motion only — this pad has no touchpad (`PadProto::clear_rich`).
    pub fn clear_rich(&mut self) {
        let fresh = SwitchState::neutral();
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    /// DualSense-convention sample → raw IMU. No axis flip: the Pro path does not negate.
    pub fn apply_motion(&mut self, gyro: [i16; 3], accel: [i16; 3]) {
        let gyro_den = 1000 * gs::MOTION_GYRO_LSB_PER_DEG_S;
        self.gyro = gyro.map(|v| ((v as i32 * JC_IMU_GYRO_MILLI_RES_PER_DPS) / gyro_den) as i16);
        self.accel = accel
            .map(|v| ((v as i32 * JC_IMU_ACCEL_RES_PER_G) / gs::MOTION_ACCEL_LSB_PER_G) as i16);
    }
}

/// Wire i16 (+ = right/up) → 12-bit raw. Driver Y-negates both conventions, so +y
/// is above-center, same as x.
pub fn stick_raw(v: i16) -> u16 {
    let raw = STICK_CENTER as i32 + (v as i32 * STICK_RANGE as i32) / 32767;
    raw.clamp(0, 0xFFF) as u16
}

/// Two 12-bit values in `hid_field_extract` little-endian bitfield order.
pub fn pack12(a: u16, b: u16) -> [u8; 3] {
    [
        (a & 0xFF) as u8,
        ((a >> 8) & 0x0F) as u8 | ((b & 0x0F) << 4) as u8,
        ((b >> 4) & 0xFF) as u8,
    ]
}

/// Shared 13-byte header for `0x30` and every `0x21` reply.
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

/// Report `0x30`: header + 3 IMU frames (accel then gyro, i16 LE). The same sample
/// is repeated; we do not sample per 5 ms sub-frame.
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

/// `0x81 <cmd>` ACK. `joycon_send_usb` matches those two bytes only.
pub fn build_usb_ack(cmd: u8) -> [u8; SWITCH_REPORT_LEN] {
    let mut r = [0u8; SWITCH_REPORT_LEN];
    r[0] = 0x81;
    r[1] = cmd;
    r
}

/// `0x21` reply. Driver matches echoed id (byte 14) only; ack MSB-set like hardware.
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

/// Subcommand `0x02`: FW 4.33, type `0x03` (Pro), MAC used as the input `uniq`.
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

/// Nintendo OUI + pad index. Device-info MAC; the driver keys `uniq` off it.
pub fn switch_mac(index: u8) -> [u8; 6] {
    [0x7C, 0xBB, 0x8A, 0xDF, 0x00, index]
}

/// Modelled SPI flash as `(start, bytes)`. Anything else reads as zero.
///
/// `0x6020` IMU: offsets 0, accel scale 16384, gyro scale 13371 (driver identity).
/// Stick cal: [`STICK_CENTER`] ± [`STICK_RANGE`]. Left = max ++ center ++ min;
/// right = center ++ min ++ max (`joycon_read_stick_calibration`). User magics
/// at `0x8010`/`0x801B`/`0x8026` are not `0xB2 0xA1`, so consumers take factory.
fn flash_blocks() -> [(u32, Vec<u8>); 6] {
    let cal_pair = pack12(STICK_RANGE, STICK_RANGE);
    let center_pair = pack12(STICK_CENTER, STICK_CENTER);
    let mut imu = Vec::with_capacity(24);
    imu.extend_from_slice(&[0u8; 6]);
    for _ in 0..3 {
        imu.extend_from_slice(&16384u16.to_le_bytes()); // accel scale (driver default)
    }
    imu.extend_from_slice(&[0u8; 6]);
    for _ in 0..3 {
        imu.extend_from_slice(&13371u16.to_le_bytes()); // gyro scale (driver default)
    }
    [
        (0x6020, imu),
        (0x603D, [cal_pair, center_pair, cal_pair].concat()),
        (0x6046, [center_pair, cal_pair, cal_pair].concat()),
        (0x8010, vec![0xFF, 0xFF]),
        (0x801B, vec![0xFF, 0xFF]),
        (0x8026, vec![0xFF, 0xFF]),
    ]
}

/// SPI `0x10` reply: echoed LE addr + len + `len` bytes at `addr`.
///
/// Serve by range, never by exact `(addr, len)`. hid-nintendo reads two 9-byte
/// stick blocks; SDL reads 18 bytes at `0x603D` and 22 at `0x8010`. Exact-pair
/// matching zero-fills Steam and pins both sticks to a corner.
pub fn spi_flash_read(addr: u32, len: u8) -> Vec<u8> {
    let mut data = vec![0u8; len as usize];
    for (start, bytes) in flash_blocks() {
        for (i, slot) in data.iter_mut().enumerate() {
            let a = addr.saturating_add(i as u32);
            if let Some(b) = a.checked_sub(start).and_then(|o| bytes.get(o as usize)) {
                *slot = *b;
            }
        }
    }
    let mut payload = Vec::with_capacity(5 + data.len());
    payload.extend_from_slice(&addr.to_le_bytes());
    payload.push(len);
    payload.extend_from_slice(&data);
    payload
}

pub enum SwitchOutput {
    /// `0x80 <cmd>` — reply with [`build_usb_ack`].
    UsbCmd(u8),
    /// `0x01` — reply with `0x21`.
    Subcmd {
        id: u8,
        args: Vec<u8>,
        rumble: (u16, u16),
    },
    /// `0x10` rumble-only — no reply.
    Rumble((u16, u16)),
}

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

/// `joycon_rumble_amplitudes` amplitude column, indexed by `amp_high / 2`.
/// Last entry is `joycon_max_rumble_amp` (1003).
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

/// Invert one side: even bits of byte 1 are the table index × 2
/// (`data[1] = freq_high_lo + amp.high`; freq is bit 0 only).
fn side_amplitude(side: &[u8]) -> u16 {
    let idx = ((side[1] & 0xFE) / 2) as usize;
    let amp = RUMBLE_AMPS[idx.min(RUMBLE_AMPS.len() - 1)] as u32;
    // Driver: amp = magnitude * 1003 / 65535 — invert, saturating at full scale.
    ((amp * 65535) / 1003).min(65535) as u16
}

/// 8 rumble bytes → (low, high). Left = strong/low, right = weak/high (`joycon_play_effect`).
pub fn decode_rumble(bytes: &[u8]) -> (u16, u16) {
    if bytes.len() < 8 {
        return (0, 0);
    }
    (side_amplitude(&bytes[..4]), side_amplitude(&bytes[4..8]))
}

/// `(flash << 4) | on` → wire bits. A flashing LED counts as on.
pub fn player_leds_bits(arg: u8) -> u8 {
    (arg & 0x0F) | (arg >> 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire south/east/west/north → Switch B/A/Y/X (`JC_BTN_*` for the rest).
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

    /// Full deflection → `center ± range`. Driver Y-negation restores evdev negative-up.
    #[test]
    fn stick_scaling() {
        assert_eq!(stick_raw(0), STICK_CENTER);
        assert_eq!(stick_raw(32767), STICK_CENTER + STICK_RANGE);
        assert_eq!(stick_raw(-32767), STICK_CENTER - STICK_RANGE);
        assert!(stick_raw(i16::MIN) <= 0xFFF);
    }

    /// A at bit 0, B at bit 12 (`hid_field_extract` LE bitfield).
    #[test]
    fn pack12_layout() {
        assert_eq!(pack12(0x578, 0x578), [0x78, 0x85, 0x57]); // 1400/1400 (the cal pair)
        assert_eq!(pack12(0x800, 0x800), [0x00, 0x08, 0x80]); // 2048/2048 (the center pair)
        let p = pack12(0xABC, 0x123);
        let a = p[0] as u16 | ((p[1] as u16 & 0xF) << 8);
        let b = ((p[1] as u16) >> 4) | ((p[2] as u16) << 4);
        assert_eq!((a, b), (0xABC, 0x123));
    }

    /// `struct joycon_input_report` + `joycon_imu_data`: header, packed sticks, 3 IMU frames.
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
        assert_eq!(&r[13..15], &(-1i16).to_le_bytes());
        assert_eq!(&r[15..17], &0x3344u16.to_le_bytes());
        assert_eq!(&r[19..21], &0x1122u16.to_le_bytes());
        assert_eq!(&r[13..25], &r[25..37]);
        assert_eq!(&r[13..25], &r[37..49]);
    }

    /// ≥ 49 bytes, ack at 13, echoed id at 14 (the only byte the driver matches).
    #[test]
    fn subcmd_reply_layout() {
        let st = SwitchState::neutral();
        let r = build_subcmd_reply(&st, 3, 0x90, 0x10, &[0xAA, 0xBB]);
        assert_eq!(r.len(), SWITCH_REPORT_LEN);
        assert_eq!(r[0], 0x21);
        assert_eq!(r[13], 0x90);
        assert_eq!(r[14], 0x10);
        assert_eq!(&r[15..17], &[0xAA, 0xBB]);
        let a = build_usb_ack(0x02);
        assert_eq!((a[0], a[1]), (0x81, 0x02));
    }

    /// User magics absent; stick min < center < max in per-side byte order; reply echoes addr+len.
    #[test]
    fn spi_blobs_valid() {
        for addr in [0x8010u32, 0x801B, 0x8026] {
            let p = spi_flash_read(addr, 2);
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
        let l = spi_flash_read(0x603D, 9);
        let (data, hdr) = (&l[5..], &l[..5]);
        assert_eq!(hdr, &[0x3D, 0x60, 0, 0, 9]);
        let (max_above, _) = unpack(&data[0..3]);
        let (center, _) = unpack(&data[3..6]);
        let (min_below, _) = unpack(&data[6..9]);
        assert_eq!(center, STICK_CENTER);
        assert!(center - min_below < center && center < center + max_above);
        // Right: center ++ min-below ++ max-above.
        let r = spi_flash_read(0x6046, 9);
        let (rc, _) = unpack(&r[5..8]);
        assert_eq!(rc, STICK_CENTER);
        let imu = spi_flash_read(0x6020, 24);
        let d = &imu[5..];
        assert_eq!(&d[0..6], &[0; 6]);
        assert_eq!(&d[6..8], &16384u16.to_le_bytes());
        assert_eq!(&d[12..18], &[0; 6]);
        assert_eq!(&d[18..20], &13371u16.to_le_bytes());
        let gap = spi_flash_read(0x6050, 12);
        assert_eq!(&gap[..5], &[0x50, 0x60, 0, 0, 12]);
        assert_eq!(&gap[5..], &[0u8; 12]);
    }

    /// SDL reads 18 factory bytes at `0x603D` and 22 user bytes at `0x8010` — shapes
    /// hid-nintendo never asks. Exact `(addr, len)` matching zero-fills those reads.
    #[test]
    fn spi_serves_sdl_read_shapes() {
        let f = spi_flash_read(0x603D, 18);
        assert_eq!(&f[..5], &[0x3D, 0x60, 0, 0, 18]);
        assert_eq!(&f[5..14], &spi_flash_read(0x603D, 9)[5..]);
        assert_eq!(&f[14..], &spi_flash_read(0x6046, 9)[5..]);
        let cal = &f[5..];
        let cx = (((cal[4] as u16) << 8) & 0xF00) | cal[3] as u16;
        let cy = ((cal[5] as u16) << 4) | ((cal[4] as u16) >> 4);
        assert_eq!((cx, cy), (STICK_CENTER, STICK_CENTER));
        let u = spi_flash_read(0x8010, 22);
        assert_eq!(&u[..5], &[0x10, 0x80, 0, 0, 22]);
        let user = &u[5..];
        assert_eq!(user.len(), 22);
        assert_eq!(&user[0..2], &[0xFF, 0xFF]); // left magic  @ 0x8010
        assert_eq!(&user[11..13], &[0xFF, 0xFF]); // right magic @ 0x801B
    }

    /// Wire 20 LSB/°·s, 10000 LSB/g → raw 14.247 LSB/°·s, 4096 LSB/g.
    #[test]
    fn motion_units() {
        let mut st = SwitchState::neutral();
        // 100 °/s = wire 2000 → raw ≈ 1424; 1 g = wire 10000 → raw 4096.
        st.apply_motion([2000, 0, -2000], [10000, -10000, 0]);
        assert_eq!(st.gyro, [1424, 0, -1424]);
        assert_eq!(st.accel, [4096, -4096, 0]);
    }

    /// Neutral → 0; max amp → 65535; left = low/strong, right = high/weak.
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

    #[test]
    fn parse_output_shapes() {
        assert!(matches!(
            parse_output(&[0x80, 0x02]),
            Some(SwitchOutput::UsbCmd(0x02))
        ));
        let mut sub = vec![0x01, 0x05];
        sub.extend_from_slice(&[0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40]);
        sub.push(0x10);
        sub.extend_from_slice(&[0x3D, 0x60, 0x00, 0x00, 0x09]);
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

    /// Solid and flashing nibbles both count as lit.
    #[test]
    fn player_lights() {
        assert_eq!(player_leds_bits(0x01), 0b0001);
        assert_eq!(player_leds_bits(0x10), 0b0001); // flashing LED 1
        assert_eq!(player_leds_bits(0x23), 0b0011 | 0b0010);
    }

    #[test]
    fn device_info_shape() {
        let mac = switch_mac(3);
        let p = device_info_payload(&mac);
        assert_eq!(p[2], 0x03);
        assert_eq!(&p[4..10], &mac);
        assert_eq!(mac[5], 3);
    }
}
