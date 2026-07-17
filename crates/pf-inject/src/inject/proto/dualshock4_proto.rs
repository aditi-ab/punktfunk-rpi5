//! Transport-independent DualShock 4 HID contract — the pure report codec shared by the Windows
//! UMDF-driver backend ([`super::dualshock4_windows`]) and the Linux UHID backend
//! ([`super::dualshock4`]).
//!
//! The PS4 sibling of [`super::dualsense_proto`]: the pure report codec with no transport. The DS4
//! reuses the DualSense [`DsState`] controller model + its `GameStream`/XInput mapper
//! ([`DsState::from_gamepad`]) — only the report *byte layout*, the touchpad resolution, and the
//! feedback report differ. The Linux backend writes report `0x01` to `/dev/uhid` and reads `0x05` via
//! `UHID_OUTPUT`; the Windows backend pushes `0x01` to the UMDF driver and pulls `0x05` back over its
//! shared-memory channel — both build/parse the exact same bytes here.
//!
//! Field offsets are the canonical real-DS4-USB layout the kernel `struct
//! dualshock4_input_report_usb` / `_output_report_common` parse.

use super::dualsense_proto::{DsState, Touch};

/// DualShock 4 v2 USB identity (Sony Interactive Entertainment / CUH-ZCT2).
pub const DS4_VENDOR: u16 = 0x054C;
pub const DS4_PRODUCT: u16 = 0x09CC;
/// USB input report `0x01` is 64 bytes total (report id + 63-byte body).
pub const DS4_INPUT_REPORT_LEN: usize = 64;
/// The DualShock 4 touchpad resolution the kernel advertises (ABS_MT 0..1919 / 0..941). Narrower
/// than the DualSense's 1920×1080.
pub const DS4_TOUCH_W: u16 = 1920;
pub const DS4_TOUCH_H: u16 = 942;

/// Pack one touchpad contact into the DS4's 4-byte point (same bit layout as the DualSense's:
/// byte0 bit7 = NOT-active, bits0-6 = id; 12-bit X then 12-bit Y).
fn pack_touch(dst: &mut [u8], t: &Touch) {
    dst[0] = (t.id & 0x7F) | if t.active { 0 } else { 0x80 };
    // Never emit the extent itself — the kernel advertises 0..=W-1 / 0..=H-1.
    let (x, y) = (t.x.min(DS4_TOUCH_W - 1), t.y.min(DS4_TOUCH_H - 1));
    dst[1] = (x & 0xFF) as u8;
    dst[2] = (((x >> 8) & 0x0F) as u8) | (((y & 0x0F) as u8) << 4);
    dst[3] = ((y >> 4) & 0xFF) as u8;
}

/// Serialize a full DS4 input report `0x01` (pure — unit-testable without a transport). Field offsets
/// per the kernel's `struct dualshock4_input_report_usb` { report_id; common; num_touch; touch[3];
/// rsvd[3] } where `common` = { x,y,rx,ry; buttons[3]; z,rz; sensor_ts le16; temp; gyro[3] le16;
/// accel[3] le16; rsvd[5]; status[2]; rsvd }. The report id is byte 0, so a `common` field at struct
/// offset N sits at report byte N+1.
pub fn serialize_state(r: &mut [u8; DS4_INPUT_REPORT_LEN], st: &DsState, counter: u8, ts: u16) {
    r[0] = 0x01; // report id
    r[1] = st.lx;
    r[2] = st.ly;
    r[3] = st.rx;
    r[4] = st.ry;
    r[5] = (st.dpad & 0x0F) | (st.buttons[0] & 0xF0); // dpad hat (low) + face buttons (high)
    r[6] = st.buttons[1]; // L1/R1, L2/R2 digital, Share/Options, L3/R3
    r[7] = (st.buttons2_with_click() & 0x03) | ((counter & 0x3F) << 2); // PS + touchpad-click (incl. rich pad clicks) + report counter
    r[8] = st.l2; // L2 analog (z)
    r[9] = st.r2; // R2 analog (rz)
    r[10..12].copy_from_slice(&ts.to_le_bytes()); // sensor_timestamp (struct off 9)
                                                  // r[12] temperature stays 0
    for (i, v) in st.gyro.iter().enumerate() {
        r[13 + i * 2..15 + i * 2].copy_from_slice(&v.to_le_bytes()); // gyro at struct off 12
    }
    for (i, v) in st.accel.iter().enumerate() {
        r[19 + i * 2..21 + i * 2].copy_from_slice(&v.to_le_bytes()); // accel at struct off 18
    }
    // r[25..30] reserved2.
    // status[0] (struct off 29 → r[30]): bit4 = cable/wired, low nibble = battery capacity. Report
    // wired + full (0x1B) so SteamOS / the kernel never warn "low battery" on a virtual pad.
    r[30] = 0x10 | 0x0B;
    // r[31] status[1] = 0 (no headphone/mic), r[32] reserved3 = 0.
    r[33] = 1; // num_touch_reports: one frame carrying the two contacts (a real DS4 always sends one)
    r[34] = ts as u8; // touch_reports[0].timestamp
    pack_touch(&mut r[35..39], &st.touch[0]); // touch point 0
    pack_touch(&mut r[39..43], &st.touch[1]); // touch point 1
                                              // remaining touch frames (r[43..61]) + reserved (r[61..64]) stay zero
}

/// What one feedback pass extracted from the device's HID output reports. Rumble rides the universal
/// 0xCA plane; the lightbar rides the HID-output 0xCD plane as a `Led` event (DS4 has no player LEDs
/// or adaptive triggers, so those never appear).
#[derive(Default)]
pub struct Ds4Feedback {
    /// `(low, high)` motor levels (0..=0xFF00), if a report carried them.
    pub rumble: Option<(u16, u16)>,
    /// Lightbar RGB, if the report carried it (deduped by the manager).
    pub led: Option<(u8, u8, u8)>,
    /// The driver's output-report ring overflowed this poll — pending reports were DISCARDED and
    /// feedback state is unknown; the [`UhidManager`](crate::uhid_manager) must resync (silence +
    /// re-armed dedups). Set by the backend's section drain, never by the parser.
    pub resync: bool,
}

/// Parse a DualShock 4 USB output report (`0x05`) into a [`Ds4Feedback`]. Layout per the kernel
/// `struct dualshock4_output_report_common`: valid_flag0 (bit0 motor, bit1 LED, bit2 blink) at [1],
/// valid_flag1 [2], reserved [3], motor_right (weak/small) [4], motor_left (strong/large) [5],
/// lightbar R/G/B [6..9], blink on/off [9..11]. Gated on the valid-flags so a rumble-only write
/// doesn't masquerade as a lightbar change.
pub fn parse_ds4_output(data: &[u8], fb: &mut Ds4Feedback) {
    if data.first() != Some(&0x05) || data.len() < 11 {
        return; // not the USB output report (BT 0x11 is shifted) / too short
    }
    let flag0 = data[1];
    if flag0 & 0x01 != 0 {
        // motor_left (strong/large/low-freq) at [5], motor_right (weak/small/high-freq) at [4];
        // scale 0..255 → 0..0xFF00, same (low, high) convention as the other backends.
        let low = (data[5] as u16) << 8;
        let high = (data[4] as u16) << 8;
        fb.rumble = Some((low, high));
    }
    if flag0 & 0x02 != 0 {
        fb.led = Some((data[6], data[7], data[8]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Report 0x01 places sticks/buttons/triggers/motion/touch at the kernel's DS4 offsets.
    #[test]
    fn serialize_offsets() {
        use punktfunk_core::input::gamepad as gs;
        let mut st = DsState::from_gamepad(
            gs::BTN_A | gs::BTN_DPAD_UP | gs::BTN_LB,
            16384, // lx (right)
            0,
            0,
            -32768, // ry (down) — inverted to 0xFF
            200,    // L2
            0,
        );
        st.gyro = [0x0102, 0x0304, 0x0506];
        st.accel = [0x1112, 0x1314, 0x1516];
        st.touch[0] = Touch {
            active: true,
            id: 0,
            x: 100,
            y: 200,
        };
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, &st, 0, 0);
        assert_eq!(r[0], 0x01); // report id
        assert_eq!(r[8], 200); // L2 analog at byte 8 (not the DualSense's byte 5)
        assert_eq!(r[5] & 0x0F, 0); // dpad hat = N (up)
        assert_eq!(r[5] & 0x20, 0x20); // Cross (A) face bit
        assert_eq!(r[6] & 0x01, 0x01); // L1
                                       // gyro le16 at 13..19, accel le16 at 19..25.
        assert_eq!(&r[13..19], &[0x02, 0x01, 0x04, 0x03, 0x06, 0x05]);
        assert_eq!(&r[19..25], &[0x12, 0x11, 0x14, 0x13, 0x16, 0x15]);
        assert_eq!(r[33], 1); // one touch frame
        assert_eq!(r[35] & 0x80, 0); // contact 0 active (bit7 clear)
        assert_eq!(r[35] & 0x7F, 0); // contact id 0
        assert_eq!(r[30] & 0x10, 0x10); // cable/wired bit set

        // A rich-plane pad click (`touch_click`, no BTN_TOUCHPAD in the frame) rides the
        // touchpad-click bit at byte 7 bit 1 via `buttons2_with_click` — the Linux backend used to
        // serialize raw `buttons[2]` here and drop it.
        assert_eq!(r[7] & 0x02, 0); // no click yet
        st.touch_click[0] = true;
        serialize_state(&mut r, &st, 0, 0);
        assert_eq!(r[7] & 0x02, 0x02);
    }

    /// A DS4 USB output report (`0x05`) with motor + LED flags parses into rumble (0xCA) and a
    /// lightbar `Led` (0xCD); a rumble-only report (no LED flag) leaves the lightbar untouched.
    #[test]
    fn parse_output_rumble_and_lightbar() {
        let mut report = [0u8; 32];
        report[0] = 0x05;
        report[1] = 0x01 | 0x02; // MOTOR | LED
        report[4] = 0x40; // motor_right (weak/high)
        report[5] = 0x80; // motor_left (strong/low)
        report[6] = 0x11; // R
        report[7] = 0x22; // G
        report[8] = 0x33; // B
        let mut fb = Ds4Feedback::default();
        parse_ds4_output(&report, &mut fb);
        assert_eq!(fb.rumble, Some((0x8000, 0x4000))); // (low=strong, high=weak)
        assert_eq!(fb.led, Some((0x11, 0x22, 0x33)));

        let mut motor_only = [0u8; 32];
        motor_only[0] = 0x05;
        motor_only[1] = 0x01; // MOTOR only
        motor_only[5] = 0x10;
        let mut fb2 = Ds4Feedback::default();
        parse_ds4_output(&motor_only, &mut fb2);
        assert!(fb2.rumble.is_some());
        assert_eq!(fb2.led, None); // lightbar not asserted → no spurious change

        // LED-only write: rumble not asserted → stays `None` (this is what `rumble_drove` keys
        // on — an LED stream must not read as rumble activity), and an explicit flagged zero
        // parses as `Some((0, 0))`, never as absence.
        let mut led_only = [0u8; 32];
        led_only[0] = 0x05;
        led_only[1] = 0x02; // LED only
        led_only[6] = 0x11;
        let mut fb3 = Ds4Feedback::default();
        parse_ds4_output(&led_only, &mut fb3);
        assert!(fb3.rumble.is_none());
        let mut stop = [0u8; 32];
        stop[0] = 0x05;
        stop[1] = 0x01; // MOTOR flag, motors zero
        let mut fb4 = Ds4Feedback::default();
        parse_ds4_output(&stop, &mut fb4);
        assert_eq!(fb4.rumble, Some((0, 0)));
    }
}
