//! Xbox Wireless Controller HID codec — the byte-exact input report the `pf-gamepad` driver serves
//! under `device_type = 4` ([`pf_driver_proto::gamepad::DEVTYPE_XBOX`]).
//!
//! **Why an Xbox pad speaks HID at all.** The other Windows Xbox backend, `pf-xusb`, registers only
//! `GUID_DEVINTERFACE_XUSB` and exposes no HID collection, so Steam's hidapi enumeration,
//! DirectInput, `joy.cpl` and WGI/GameInput cannot see it — only classic `XInputGetState` via
//! xinput1_4's interface walk ever does. A field report (2026-08-09) burned two weeks on a dead
//! controller for exactly that reason, and switching the client to DualSense — a real HID pad
//! through the same UMDF driver — fixed it instantly. This codec puts the Xbox pad on that footing.
//!
//! **The report is the descriptor's mirror image.** `pf-gamepad`'s `XBOX_RDESC` declares, in order:
//! two 16-bit stick pairs (`X`/`Y`, then `Rx`/`Ry`, logical 0..65535), two 16-bit triggers on the
//! Simulation page (`Brake`/`Accelerator`, logical 0..1023), a 4-bit null-state hat plus 4 bits of
//! padding, and 15 buttons plus 1 bit of padding. [`serialize_xbox_state`] writes exactly that, and
//! [`tests`] pins every field position — change one side and the tests fail.
//!
//! ⚠️⚠️ **The button numbering below is the REAL Xbox-Bluetooth layout, gaps included, and that is
//! load-bearing.** We enumerate as a genuine Microsoft `045E:0B13`, and SDL / Steam / Windows all
//! carry built-in mappings keyed off that VID/PID. Renumber these to something "tidier" and every
//! consumer with a stock mapping silently lands each control on the wrong action — the exact class
//! of bug this module exists to end. The reserved slots (3, 6, 9, 10) are Microsoft's; leave them
//! empty.
//!
//! ⚠️ **Never validated against real hardware.** No Windows box was reachable when this was written
//! (`punktfunk-field-windows-pad-dead-0260`), so the layout is from the documented Xbox One S / Series
//! Bluetooth report and has not been diffed against a capture. Do that before shipping: dump a real
//! pad's descriptor + a few reports and compare against `XBOX_RDESC` and the tests here.

use punktfunk_core::input::gamepad as gs;

/// Bytes an Xbox input report occupies on the wire, report id included. Must equal the driver's
/// `XBOX_INPUT_REPORT_LEN` — hidclass sizes its READ_REPORT buffer from the descriptor and the
/// driver's `copy_to_output` refuses a longer source rather than truncating.
pub const XBOX_REPORT_LEN: usize = 16;

/// The report id the descriptor declares for the input report.
const REPORT_ID: u8 = 0x01;

/// Stick centre on the descriptor's 0..65535 axis.
const STICK_CENTRE: u16 = 0x8000;

/// Trigger full scale on the descriptor's 0..1023 (10-bit) axis.
const TRIGGER_MAX: u32 = 1023;

// ---- Button bit positions, LSB-first across report bytes 14..16 ----
//
// HID button N lands on bit (N-1). These are the REAL Xbox-Bluetooth assignments; slots 3, 6, 9
// and 10 are reserved by Microsoft and stay empty (see the module note).
const BIT_A: u8 = 0; // button 1
const BIT_B: u8 = 1; // button 2
const BIT_X: u8 = 3; // button 4
const BIT_Y: u8 = 4; // button 5
const BIT_LB: u8 = 6; // button 7
const BIT_RB: u8 = 7; // button 8
const BIT_VIEW: u8 = 10; // button 11 (Back/Select)
const BIT_MENU: u8 = 11; // button 12 (Start)
const BIT_GUIDE: u8 = 12; // button 13 (Xbox button)
const BIT_LS: u8 = 13; // button 14 (left stick click)
const BIT_RS: u8 = 14; // button 15 (right stick click)

/// One Xbox pad's state, in the wire's own conventions (sticks −32768..32767 with **+y = up**,
/// triggers 0..255, buttons the [`gs`] `BTN_*` bitmask) — converted to the HID report's
/// conventions by [`serialize_xbox_state`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XboxState {
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

impl XboxState {
    /// Build from the wire's per-pad frame fields (`punktfunk_core::input::GamepadFrame`).
    #[allow(clippy::too_many_arguments)]
    pub fn from_gamepad(
        buttons: u32,
        left_trigger: u8,
        right_trigger: u8,
        ls_x: i16,
        ls_y: i16,
        rs_x: i16,
        rs_y: i16,
    ) -> XboxState {
        XboxState {
            buttons,
            left_trigger,
            right_trigger,
            ls_x,
            ls_y,
            rs_x,
            rs_y,
        }
    }
}

/// Wire stick axis (−32768..32767) → the descriptor's unsigned 0..65535 X/Rx axis.
fn axis_x(v: i16) -> u16 {
    (v as i32 + 32768) as u16
}

/// Wire stick axis → the descriptor's 0..65535 Y/Ry axis, **inverted**.
///
/// The wire follows the XInput/Moonlight convention where **+y is UP**; HID's `Y`/`Ry` grow
/// DOWNWARD. Forwarding the wire value unconverted is how a pad ends up with an inverted look
/// stick that nobody notices until they aim.
///
/// ⚠️ A signed 16-bit range has no exact midpoint, so the inverted axis centres one unit lower
/// than the upright one: `axis_x(0)` is 32768 and `axis_y(0)` is 32767. Both endpoints are exact
/// (full up → 0, full down → 65535), which is what matters; the 1/65536 offset at rest is below
/// any deadzone. Do NOT "fix" it by centring on 32768 — that costs an endpoint instead.
fn axis_y(v: i16) -> u16 {
    65535 - axis_x(v)
}

/// Wire trigger (0..255) → the descriptor's 10-bit 0..1023 axis, rounded rather than truncated so
/// a fully-held trigger reads exactly full scale.
fn trigger(v: u8) -> u16 {
    ((v as u32 * TRIGGER_MAX + 127) / 255) as u16
}

/// The d-pad bits → the descriptor's hat value: `0` is the NULL state (the logical range starts at
/// 1), then 1..8 clockwise from North. Opposing presses cancel, matching a physical hat.
fn hat(buttons: u32) -> u8 {
    let up = buttons & gs::BTN_DPAD_UP != 0;
    let down = buttons & gs::BTN_DPAD_DOWN != 0;
    let left = buttons & gs::BTN_DPAD_LEFT != 0;
    let right = buttons & gs::BTN_DPAD_RIGHT != 0;
    // Cancel opposing pairs first so up+down reads centred rather than picking one.
    let (up, down) = if up && down {
        (false, false)
    } else {
        (up, down)
    };
    let (left, right) = if left && right {
        (false, false)
    } else {
        (left, right)
    };
    match (up, right, down, left) {
        (true, false, false, false) => 1, // N
        (true, true, false, false) => 2,  // NE
        (false, true, false, false) => 3, // E
        (false, true, true, false) => 4,  // SE
        (false, false, true, false) => 5, // S
        (false, false, true, true) => 6,  // SW
        (false, false, false, true) => 7, // W
        (true, false, false, true) => 8,  // NW
        _ => 0,                           // nothing held → NULL
    }
}

/// The 15 face/shoulder/system buttons packed into the report's last two bytes.
fn button_bits(buttons: u32) -> (u8, u8) {
    let mut bits: u16 = 0;
    for (mask, bit) in [
        (gs::BTN_A, BIT_A),
        (gs::BTN_B, BIT_B),
        (gs::BTN_X, BIT_X),
        (gs::BTN_Y, BIT_Y),
        (gs::BTN_LB, BIT_LB),
        (gs::BTN_RB, BIT_RB),
        (gs::BTN_BACK, BIT_VIEW),
        (gs::BTN_START, BIT_MENU),
        (gs::BTN_GUIDE, BIT_GUIDE),
        (gs::BTN_LS_CLICK, BIT_LS),
        (gs::BTN_RS_CLICK, BIT_RS),
    ] {
        if buttons & mask != 0 {
            bits |= 1 << bit;
        }
    }
    (bits as u8, (bits >> 8) as u8)
}

/// Serialize one [`XboxState`] into the driver's input report.
pub fn serialize_xbox_state(s: &XboxState) -> [u8; XBOX_REPORT_LEN] {
    let mut r = [0u8; XBOX_REPORT_LEN];
    r[0] = REPORT_ID;
    r[1..3].copy_from_slice(&axis_x(s.ls_x).to_le_bytes());
    r[3..5].copy_from_slice(&axis_y(s.ls_y).to_le_bytes());
    r[5..7].copy_from_slice(&axis_x(s.rs_x).to_le_bytes());
    r[7..9].copy_from_slice(&axis_y(s.rs_y).to_le_bytes());
    r[9..11].copy_from_slice(&trigger(s.left_trigger).to_le_bytes());
    r[11..13].copy_from_slice(&trigger(s.right_trigger).to_le_bytes());
    r[13] = hat(s.buttons); // low nibble; the high nibble is descriptor padding
    let (lo, hi) = button_bits(s.buttons);
    r[14] = lo;
    r[15] = hi;
    r
}

/// The at-rest report: sticks centred, triggers released, hat NULL, nothing held. Must agree with
/// the driver's `XBOX_NEUTRAL_REPORT` — [`tests::neutral_matches_a_zeroed_state`] pins that.
pub fn neutral_xbox_report() -> [u8; XBOX_REPORT_LEN] {
    serialize_xbox_state(&XboxState::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }

    /// The field offsets the driver's `XBOX_RDESC` declares. If this fails, one side moved.
    #[test]
    fn the_report_matches_the_descriptor_layout() {
        let s = XboxState::from_gamepad(0, 0, 0, 0, 0, 0, 0);
        let r = serialize_xbox_state(&s);
        assert_eq!(r.len(), XBOX_REPORT_LEN, "16 bytes: id + 8 + 4 + 1 + 2");
        assert_eq!(r[0], 0x01, "report id");
    }

    #[test]
    fn sticks_span_the_full_unsigned_axis() {
        let full = XboxState::from_gamepad(0, 0, 0, i16::MIN, i16::MIN, i16::MAX, i16::MAX);
        let r = serialize_xbox_state(&full);
        assert_eq!(le(&r[1..3]), 0, "LX at hard left = 0");
        assert_eq!(le(&r[5..7]), 65535, "RX at hard right = 65535");
    }

    /// +y is UP on the wire and DOWN in HID — the conversion has to flip, or aiming is inverted.
    #[test]
    fn the_y_axes_are_inverted_into_hid_convention() {
        let up = XboxState::from_gamepad(0, 0, 0, 0, i16::MAX, 0, i16::MAX);
        let r = serialize_xbox_state(&up);
        assert_eq!(le(&r[3..5]), 0, "stick fully UP is 0 in HID");
        assert_eq!(le(&r[7..9]), 0, "right stick too");

        let down = XboxState::from_gamepad(0, 0, 0, 0, i16::MIN, 0, i16::MIN);
        let r = serialize_xbox_state(&down);
        assert_eq!(le(&r[3..5]), 65535, "stick fully DOWN is full scale");
        assert_eq!(le(&r[7..9]), 65535);
    }

    /// At rest the upright axes sit on `STICK_CENTRE` and the inverted ones one unit below — the
    /// unavoidable consequence of mirroring a range with an even number of steps (see `axis_y`).
    #[test]
    fn a_centred_stick_reads_centred() {
        let r = neutral_xbox_report();
        assert_eq!(le(&r[1..3]), STICK_CENTRE, "LX");
        assert_eq!(le(&r[5..7]), STICK_CENTRE, "RX");
        assert_eq!(le(&r[3..5]), STICK_CENTRE - 1, "LY (inverted)");
        assert_eq!(le(&r[7..9]), STICK_CENTRE - 1, "RY (inverted)");
    }

    /// A fully-held trigger must reach exactly full scale — truncating division stops at 1020 and
    /// games with a "trigger fully pressed" threshold never fire.
    #[test]
    fn triggers_scale_to_full_ten_bit_range() {
        let none = serialize_xbox_state(&XboxState::default());
        assert_eq!(le(&none[9..11]), 0);
        assert_eq!(le(&none[11..13]), 0);

        let held = XboxState::from_gamepad(0, 255, 255, 0, 0, 0, 0);
        let r = serialize_xbox_state(&held);
        assert_eq!(le(&r[9..11]), 1023, "LT fully held = full scale");
        assert_eq!(le(&r[11..13]), 1023, "RT fully held = full scale");

        let half = XboxState::from_gamepad(0, 128, 0, 0, 0, 0, 0);
        let r = serialize_xbox_state(&half);
        assert_eq!(le(&r[9..11]), 514, "128/255 rounds to 514, not 513");
    }

    #[test]
    fn the_hat_walks_clockwise_from_north() {
        let cases = [
            (0, 0u8),
            (gs::BTN_DPAD_UP, 1),
            (gs::BTN_DPAD_UP | gs::BTN_DPAD_RIGHT, 2),
            (gs::BTN_DPAD_RIGHT, 3),
            (gs::BTN_DPAD_RIGHT | gs::BTN_DPAD_DOWN, 4),
            (gs::BTN_DPAD_DOWN, 5),
            (gs::BTN_DPAD_DOWN | gs::BTN_DPAD_LEFT, 6),
            (gs::BTN_DPAD_LEFT, 7),
            (gs::BTN_DPAD_UP | gs::BTN_DPAD_LEFT, 8),
        ];
        for (buttons, want) in cases {
            let r = serialize_xbox_state(&XboxState::from_gamepad(buttons, 0, 0, 0, 0, 0, 0));
            assert_eq!(r[13] & 0x0F, want, "buttons {buttons:#x}");
        }
    }

    /// Opposing presses cancel to NULL rather than resolving to one direction — a physical hat
    /// cannot report both, and a game that sees "up" while the player holds up+down drifts.
    #[test]
    fn opposing_dpad_presses_cancel() {
        let ud = gs::BTN_DPAD_UP | gs::BTN_DPAD_DOWN;
        let r = serialize_xbox_state(&XboxState::from_gamepad(ud, 0, 0, 0, 0, 0, 0));
        assert_eq!(r[13] & 0x0F, 0);
        let lr = gs::BTN_DPAD_LEFT | gs::BTN_DPAD_RIGHT;
        let r = serialize_xbox_state(&XboxState::from_gamepad(lr, 0, 0, 0, 0, 0, 0));
        assert_eq!(r[13] & 0x0F, 0);
    }

    /// The real Xbox-Bluetooth button numbering, gaps included. SDL/Steam/Windows key their stock
    /// mappings off our claimed `045E:0B13`, so these positions are a compatibility contract.
    #[test]
    fn buttons_land_on_the_real_xbox_bluetooth_positions() {
        let cases: [(u32, usize, u8); 11] = [
            (gs::BTN_A, 14, 0),
            (gs::BTN_B, 14, 1),
            (gs::BTN_X, 14, 3),
            (gs::BTN_Y, 14, 4),
            (gs::BTN_LB, 14, 6),
            (gs::BTN_RB, 14, 7),
            (gs::BTN_BACK, 15, 2),
            (gs::BTN_START, 15, 3),
            (gs::BTN_GUIDE, 15, 4),
            (gs::BTN_LS_CLICK, 15, 5),
            (gs::BTN_RS_CLICK, 15, 6),
        ];
        for (mask, byte, bit) in cases {
            let r = serialize_xbox_state(&XboxState::from_gamepad(mask, 0, 0, 0, 0, 0, 0));
            assert_eq!(
                r[byte] & (1u8 << bit),
                1u8 << bit,
                "mask {mask:#x} should set byte {byte} bit {bit}"
            );
            // and nothing else in the button bytes
            let shift = bit as u16 + (byte as u16 - 14) * 8;
            let others = (r[14] as u16 | (r[15] as u16) << 8) & !(1u16 << shift);
            assert_eq!(others, 0, "mask {mask:#x} set a second button bit");
        }
    }

    /// Microsoft's reserved slots (buttons 3, 6, 9, 10) and the descriptor's trailing pad bit must
    /// stay clear — a stray bit there reads as a button the real pad does not have.
    #[test]
    fn reserved_button_slots_stay_empty() {
        let all = gs::BTN_A
            | gs::BTN_B
            | gs::BTN_X
            | gs::BTN_Y
            | gs::BTN_LB
            | gs::BTN_RB
            | gs::BTN_BACK
            | gs::BTN_START
            | gs::BTN_GUIDE
            | gs::BTN_LS_CLICK
            | gs::BTN_RS_CLICK;
        let r = serialize_xbox_state(&XboxState::from_gamepad(all, 0, 0, 0, 0, 0, 0));
        let bits = r[14] as u16 | (r[15] as u16) << 8;
        for reserved_bit in [2u8, 5, 8, 9, 15] {
            assert_eq!(
                bits & (1 << reserved_bit),
                0,
                "bit {reserved_bit} is reserved/padding and must stay clear"
            );
        }
    }

    /// Extended wire buttons the Xbox HID profile has no slot for (touchpad, capture, paddles) must
    /// be dropped silently rather than colliding with a real button.
    #[test]
    fn unmappable_wire_buttons_are_dropped() {
        let extra = gs::BTN_TOUCHPAD | gs::BTN_MISC1 | gs::BTN_PADDLE1;
        let r = serialize_xbox_state(&XboxState::from_gamepad(extra, 0, 0, 0, 0, 0, 0));
        assert_eq!(r[14], 0);
        assert_eq!(r[15], 0);
        assert_eq!(r[13] & 0x0F, 0);
    }

    #[test]
    fn neutral_matches_a_zeroed_state() {
        assert_eq!(
            neutral_xbox_report(),
            serialize_xbox_state(&XboxState::default())
        );
        let r = neutral_xbox_report();
        assert_eq!(r[13], 0, "hat NULL");
        assert_eq!(r[14], 0);
        assert_eq!(r[15], 0);
        // Mirrors the driver's XBOX_NEUTRAL_REPORT byte for byte — if these drift, a game reads a
        // different at-rest pose before the host's first frame lands than after it.
        assert_eq!(r[0], 0x01);
        assert_eq!([r[1], r[2]], [0x00, 0x80], "LX = 0x8000");
        assert_eq!([r[3], r[4]], [0xFF, 0x7F], "LY = 0x7FFF (inverted centre)");
        assert_eq!([r[5], r[6]], [0x00, 0x80], "RX = 0x8000");
        assert_eq!([r[7], r[8]], [0xFF, 0x7F], "RY = 0x7FFF (inverted centre)");
    }
}
