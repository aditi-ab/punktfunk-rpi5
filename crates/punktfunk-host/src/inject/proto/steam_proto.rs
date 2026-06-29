//! Transport-independent Steam Controller / Steam Deck HID contract — the Steam analogue of
//! [`super::dualsense_proto`]. The report descriptor, the command/feature IDs, the byte-exact
//! Deck input-report serializer, the `XInput`/rich-input → state mappers, and the rumble-feedback
//! parser. Pure logic, shared by the Linux UHID backend and (later) a Windows UMDF backend.
//!
//! **Layout source of truth:** the kernel `drivers/hid/hid-steam.c` `steam_do_deck_input_event`
//! (+ `steam_do_deck_sensors_event`) — every offset/bit/sign below is transcribed verbatim from
//! it and on-box-validated against kernel 7.0 (see `design/steam-controller-deck-support.md`).
//! M0 proved the device binds + parses; M1 (here) makes the serializer byte-exact.
//!
//! Three load-bearing details the DualSense path does NOT have:
//!   * **report id 0 / unnumbered**: input reports are the raw 64 bytes starting `[0x01,0x00,0x09]`
//!     (no report-id prefix); FEATURE get/set reports DO carry a leading `0x00` report-id byte
//!     (`steam_send_report` does `memcpy(buf+1, cmd, …)`, `steam_recv_report` strips `buf[0]`).
//!   * **`gamepad_mode` gate**: `steam_do_deck_input_event` early-returns when
//!     `!gamepad_mode && lizard_mode` (the module param, default on). `gamepad_mode` starts false
//!     and TOGGLES when [`btn::STEAM_MENU_RIGHT`] (`b9.6`, the mode-switch) is held ~450 ms while
//!     no hidraw client is open. The backend enters gamepad mode at session start (pulse that bit,
//!     or load `hid_steam lizard_mode=0`) — see the backend, not this module.
//!   * **the `UHID_SET_REPORT` handshake** must be answered (DualSense omits it).
#![allow(dead_code)] // Some of the full model is consumed only once the M2 backend + M3 wire land.

use punktfunk_core::input::gamepad as gs;
use punktfunk_core::quic::RichInput;

/// Valve. `hid-steam` matches purely by VID/PID over `BUS_USB`.
pub const STEAM_VENDOR: u32 = 0x28DE;
/// Steam Deck built-in controller (same PID on LCD + OLED).
pub const STEAMDECK_PRODUCT: u32 = 0x1205;
/// Classic Steam Controller, wired (report id 1 / `ID_CONTROLLER_STATE`; a later model).
pub const STEAMCTRL_WIRED_PRODUCT: u32 = 0x1102;

/// The Steam HID state/command report is a fixed 64-byte, **unnumbered** (report-id-0) frame.
pub const STEAM_REPORT_LEN: usize = 64;

// Command IDs (drivers/hid/hid-steam.c), confirmed against the kernel source.
pub const ID_CLEAR_DIGITAL_MAPPINGS: u8 = 0x81;
pub const ID_GET_ATTRIBUTES_VALUES: u8 = 0x83;
pub const ID_SET_SETTINGS_VALUES: u8 = 0x87;
pub const ID_LOAD_DEFAULT_SETTINGS: u8 = 0x8E;
pub const ID_GET_DEVICE_INFO: u8 = 0xA1;
pub const ID_GET_STRING_ATTRIBUTE: u8 = 0xAE;
pub const ATTRIB_STR_UNIT_SERIAL: u8 = 0x01;
/// Host→client feedback: `steam_haptic_rumble` emits report `[0xEB, 9, …]` (FF_RUMBLE → trackpad
/// actuators / Deck motors). The Deck's rumble path; the classic SC also has `0x8F` pad pulses.
pub const ID_TRIGGER_RUMBLE_CMD: u8 = 0xEB;
pub const ID_TRIGGER_HAPTIC_PULSE: u8 = 0x8F;
/// Input report message types: SC = `ID_CONTROLLER_STATE`, Deck = `ID_CONTROLLER_DECK_STATE`.
pub const ID_CONTROLLER_STATE: u8 = 0x01;
pub const ID_CONTROLLER_DECK_STATE: u8 = 0x09;

/// Which Steam device identity to present. M1 implements the Deck fully; the classic Controller
/// (dual trackpads, report id 1, trackpad-only haptics) is a later identity behind the same path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteamModel {
    Deck,
    Controller,
}

impl SteamModel {
    pub fn product(self) -> u32 {
        match self {
            SteamModel::Deck => STEAMDECK_PRODUCT,
            SteamModel::Controller => STEAMCTRL_WIRED_PRODUCT,
        }
    }
}

/// Minimal vendor-defined HID report descriptor: one application collection with a 64-byte input
/// report and a 64-byte feature report, both UNNUMBERED (report id 0). `hid-steam` is a raw-event
/// driver, so the field layout is cosmetic — but `steam_probe` requires `hid_parse` to succeed AND
/// a non-empty FEATURE report list (`steam_is_valve_interface`), so the feature item is mandatory.
#[rustfmt::skip]
pub const STEAMDECK_RDESC: &[u8] = &[
    0x06, 0x00, 0xFF, // Usage Page (Vendor-Defined 0xFF00)
    0x09, 0x01,       // Usage (0x01)
    0xA1, 0x01,       // Collection (Application)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x00, //   Logical Maximum (255)
    0x75, 0x08,       //   Report Size (8 bits)
    0x95, 0x40,       //   Report Count (64)
    0x09, 0x01,       //   Usage (0x01)
    0x81, 0x02,       //   Input (Data,Var,Abs)    — the 64-byte state report
    0x09, 0x01,       //   Usage (0x01)
    0x95, 0x40,       //   Report Count (64)
    0xB1, 0x02,       //   Feature (Data,Var,Abs)  — makes steam_is_valve_interface() true
    0xC0,             // End Collection
];

/// Deck button bits, indexed in the `u64` packed across report bytes 8..16 — bit `(byte-8)*8 + bit`,
/// transcribed verbatim from `steam_do_deck_input_event` (bytes 12 + 15 carry no buttons). Naming
/// follows the physical Deck control; the trailing comment is the kernel `BTN_*` it maps to.
pub mod btn {
    // byte 8
    pub const RT_FULL: u64 = 1 << 0; // BTN_TR2  — right trigger fully pressed
    pub const LT_FULL: u64 = 1 << 1; // BTN_TL2  — left trigger fully pressed
    pub const RB: u64 = 1 << 2; //      BTN_TR   — right shoulder
    pub const LB: u64 = 1 << 3; //      BTN_TL   — left shoulder
    pub const Y: u64 = 1 << 4;
    pub const B: u64 = 1 << 5;
    pub const X: u64 = 1 << 6;
    pub const A: u64 = 1 << 7;
    // byte 9
    pub const DPAD_UP: u64 = 1 << 8;
    pub const DPAD_RIGHT: u64 = 1 << 9;
    pub const DPAD_LEFT: u64 = 1 << 10;
    pub const DPAD_DOWN: u64 = 1 << 11;
    pub const VIEW: u64 = 1 << 12; //   BTN_SELECT — "menu left" (View / Back)
    pub const STEAM: u64 = 1 << 13; //  BTN_MODE   — Steam logo button
    pub const MENU: u64 = 1 << 14; //   BTN_START  — "menu right" (Start / Options)
    pub const L5: u64 = 1 << 15; //     BTN_GRIPL2 — left BOTTOM back grip
                                 // byte 10
    pub const R5: u64 = 1 << 16; //     BTN_GRIPR2 — right BOTTOM back grip
    pub const LPAD_CLICK: u64 = 1 << 17; // BTN_THUMB  — left pad pressed (click)
    pub const RPAD_CLICK: u64 = 1 << 18; // BTN_THUMB2 — right pad pressed (click)
    pub const LPAD_TOUCH: u64 = 1 << 19; // gates ABS_HAT0 (left pad coords)
    pub const RPAD_TOUCH: u64 = 1 << 20; // gates ABS_HAT1 (right pad coords)
    pub const L3: u64 = 1 << 22; //     BTN_THUMBL — left joystick click
                                 // byte 11
    pub const R3: u64 = 1 << 26; //     BTN_THUMBR — right joystick click
                                 // byte 13
    pub const L4: u64 = 1 << 41; //     BTN_GRIPL  — left TOP back grip
    pub const R4: u64 = 1 << 42; //     BTN_GRIPR  — right TOP back grip
    pub const LJOY_TOUCH: u64 = 1 << 46;
    pub const RJOY_TOUCH: u64 = 1 << 47;
    // byte 14
    pub const QAM: u64 = 1 << 50; //    BTN_BASE   — quick-access (…) button
    /// `b9.6` doubles as the mode-switch: held ~450 ms (no hidraw client) it toggles `gamepad_mode`.
    pub const STEAM_MENU_RIGHT: u64 = MENU;
}

/// Full virtual Steam Deck controller state. All analog fields are stored as the RAW little-endian
/// report values the kernel reads (so [`serialize_deck_state`] is a pure memcpy); the kernel applies
/// its own sign conventions on top (`ABS_Y = -raw`, etc.) — see [`SteamState::from_gamepad`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SteamState {
    /// Packed button bits (see [`btn`]); occupies report bytes 8..16.
    pub buttons: u64,
    /// Left / right joystick, raw s16 (report 48/50/52/54). The kernel negates the Y axes.
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    /// Left / right analog trigger, raw u16 (report 44/46 → ABS_HAT2Y/X).
    pub lt: u16,
    pub rt: u16,
    /// Left / right trackpad position, raw s16, centred 0 (report 16/18/20/22). Only surfaced by
    /// the kernel while the matching `*PAD_TOUCH` button bit is set.
    pub lpad_x: i16,
    pub lpad_y: i16,
    pub rpad_x: i16,
    pub rpad_y: i16,
    pub lpad_pressure: u16,
    pub rpad_pressure: u16,
    /// IMU, raw s16. `accel`/`gyro` are `[X, Y, Z]`; the kernel maps them to ABS_X/Z/Y + ABS_RX/RZ/RY
    /// (with Z/RZ negated) on the separate sensors evdev.
    pub accel: [i16; 3],
    pub gyro: [i16; 3],
}

impl SteamState {
    pub fn neutral() -> SteamState {
        SteamState::default()
    }

    /// Set/clear a button (or group) by its [`btn`] mask.
    pub fn press(&mut self, mask: u64, down: bool) {
        if down {
            self.buttons |= mask;
        } else {
            self.buttons &= !mask;
        }
    }

    /// Map an `XInput`/GameStream pad frame (button bitmask + i16 sticks + u8 triggers) into the Deck
    /// state. Sticks pass through (the kernel negates Y, which yields the conventional direction —
    /// validated on-box); triggers scale u8 0..255 → u16 0..32640 and set the full-pull bit when
    /// pressed. Trackpad + motion + the back grips arrive separately ([`apply_rich`], the M3 wire).
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> SteamState {
        let on = |bit: u32| buttons & bit != 0;
        let mut s = SteamState {
            lx,
            ly,
            rx,
            ry,
            lt: (lt as u16) * 128,
            rt: (rt as u16) * 128,
            ..SteamState::neutral()
        };
        let mut b = 0u64;
        let set = |b: &mut u64, on: bool, m: u64| {
            if on {
                *b |= m;
            }
        };
        set(&mut b, on(gs::BTN_A), btn::A);
        set(&mut b, on(gs::BTN_B), btn::B);
        set(&mut b, on(gs::BTN_X), btn::X);
        set(&mut b, on(gs::BTN_Y), btn::Y);
        set(&mut b, on(gs::BTN_LB), btn::LB);
        set(&mut b, on(gs::BTN_RB), btn::RB);
        set(&mut b, lt > 0, btn::LT_FULL);
        set(&mut b, rt > 0, btn::RT_FULL);
        set(&mut b, on(gs::BTN_BACK), btn::VIEW);
        set(&mut b, on(gs::BTN_START), btn::MENU);
        set(&mut b, on(gs::BTN_GUIDE), btn::STEAM);
        set(&mut b, on(gs::BTN_LS_CLICK), btn::L3);
        set(&mut b, on(gs::BTN_RS_CLICK), btn::R3);
        set(&mut b, on(gs::BTN_DPAD_UP), btn::DPAD_UP);
        set(&mut b, on(gs::BTN_DPAD_DOWN), btn::DPAD_DOWN);
        set(&mut b, on(gs::BTN_DPAD_LEFT), btn::DPAD_LEFT);
        set(&mut b, on(gs::BTN_DPAD_RIGHT), btn::DPAD_RIGHT);
        // The DualSense touchpad-click wire bit maps to the Deck's RIGHT pad click (the pad that
        // stands in for the DualSense touchpad — see apply_rich).
        set(&mut b, on(gs::BTN_TOUCHPAD), btn::RPAD_CLICK);
        s.buttons = b;
        s
    }

    /// Apply one rich client→host event into this state, preserving everything else. The single-pad
    /// wire [`RichInput::Touchpad`] maps to the **right** trackpad (the Deck pad analogous to the
    /// DualSense touchpad); the left pad arrives via the M3 `TouchpadEx` surface. [`RichInput::Motion`]
    /// passes gyro/accel straight through (raw i16; cross-device unit scaling is M3).
    pub fn apply_rich(&mut self, rich: RichInput) {
        match rich {
            RichInput::Touchpad { active, x, y, .. } => {
                self.press(btn::RPAD_TOUCH, active);
                // Normalized 0..=65535 (centre 32768) → the pad's centred s16 range.
                self.rpad_x = ((x as i32) - 32768) as i16;
                self.rpad_y = ((y as i32) - 32768) as i16;
            }
            RichInput::Motion { gyro, accel, .. } => {
                self.gyro = gyro;
                self.accel = accel;
            }
        }
    }
}

/// Serialize the full Deck input report (`ID_CONTROLLER_DECK_STATE`) into the 64-byte unnumbered
/// frame `hid-steam` parses. Pure + byte-exact against `steam_do_deck_input_event`; the report-id
/// constant is `data[0]=0x01` (NOT a HID report id — this report is unnumbered).
pub fn serialize_deck_state(r: &mut [u8; STEAM_REPORT_LEN], st: &SteamState, seq: u32) {
    r.fill(0);
    r[0] = 0x01;
    r[1] = 0x00;
    r[2] = ID_CONTROLLER_DECK_STATE;
    r[3] = 0x3C; // payload length; the kernel ignores it
    r[4..8].copy_from_slice(&seq.to_le_bytes());
    r[8..16].copy_from_slice(&st.buttons.to_le_bytes()); // bytes 8..16 (12+15 stay 0)
    r[16..18].copy_from_slice(&st.lpad_x.to_le_bytes());
    r[18..20].copy_from_slice(&st.lpad_y.to_le_bytes());
    r[20..22].copy_from_slice(&st.rpad_x.to_le_bytes());
    r[22..24].copy_from_slice(&st.rpad_y.to_le_bytes());
    r[24..26].copy_from_slice(&st.accel[0].to_le_bytes()); // accel X → IMU ABS_X
    r[26..28].copy_from_slice(&st.accel[1].to_le_bytes()); // accel Y → IMU ABS_Z (kernel negates)
    r[28..30].copy_from_slice(&st.accel[2].to_le_bytes()); // accel Z → IMU ABS_Y
    r[30..32].copy_from_slice(&st.gyro[0].to_le_bytes()); //  gyro X  → IMU ABS_RX
    r[32..34].copy_from_slice(&st.gyro[1].to_le_bytes()); //  gyro Y  → IMU ABS_RZ (kernel negates)
    r[34..36].copy_from_slice(&st.gyro[2].to_le_bytes()); //  gyro Z  → IMU ABS_RY
                                                          // 36..44 quaternion — left 0 (optional; the kernel does not surface it)
    r[44..46].copy_from_slice(&st.lt.to_le_bytes()); // left trigger  → ABS_HAT2Y
    r[46..48].copy_from_slice(&st.rt.to_le_bytes()); // right trigger → ABS_HAT2X
    r[48..50].copy_from_slice(&st.lx.to_le_bytes()); // left joystick X  → ABS_X
    r[50..52].copy_from_slice(&st.ly.to_le_bytes()); // left joystick Y  → ABS_Y (kernel negates)
    r[52..54].copy_from_slice(&st.rx.to_le_bytes()); // right joystick X → ABS_RX
    r[54..56].copy_from_slice(&st.ry.to_le_bytes()); // right joystick Y → ABS_RY (kernel negates)
    r[56..58].copy_from_slice(&st.lpad_pressure.to_le_bytes());
    r[58..60].copy_from_slice(&st.rpad_pressure.to_le_bytes());
}

/// Build the `steam_get_serial` GET_REPORT reply. The Steam feature path is report-id-0 with a
/// leading report-id byte the kernel strips (`steam_recv_report` does `memcpy(data, buf+1, …)`), so
/// the wire is `[0x00, 0xAE, len, 0x01, ascii…]`; the kernel then validates `reply[0]==0xAE`,
/// `1<=reply[1]<=21`, `reply[2]==0x01`. Non-fatal (a bad reply → the `"XXXXXXXXXX"` fallback).
pub fn serial_reply(serial: &str) -> [u8; STEAM_REPORT_LEN] {
    let mut buf = [0u8; STEAM_REPORT_LEN];
    let bytes = serial.as_bytes();
    let len = bytes.len().clamp(1, 21);
    buf[0] = 0x00; // report id 0 — stripped by steam_recv_report
    buf[1] = ID_GET_STRING_ATTRIBUTE;
    buf[2] = len as u8;
    buf[3] = ATTRIB_STR_UNIT_SERIAL;
    buf[4..4 + len].copy_from_slice(&bytes[..len]);
    buf
}

/// One service pass's extracted feedback. Rumble rides the universal 0xCA plane (so any client
/// feels it); the classic SC's trackpad-pulse haptics (`0x8F`) are a later, model-specific add.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct SteamFeedback {
    /// `(low, high)` motor levels (left/strong, right/weak), if a rumble report carried them.
    pub rumble: Option<(u16, u16)>,
}

/// Parse a feature/output report the kernel wrote to our device. The Steam feedback path is a
/// FEATURE `SET_REPORT` whose wire data is `[0x00 report-id, cmd, len, …]`; `cmd == 0xEB`
/// (`steam_haptic_rumble`) carries `[…, 0, intensity(2), left_speed(2), right_speed(2), gains(2)]`.
/// We surface `(left_speed, right_speed)` as `(low, high)` for the 0xCA rumble plane.
pub fn parse_steam_output(data: &[u8]) -> SteamFeedback {
    let mut fb = SteamFeedback::default();
    // data[0] is the stripped report-id byte (0); the command id follows.
    if data.len() >= 10 && data[1] == ID_TRIGGER_RUMBLE_CMD {
        let le = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
        let left = le(6); // left_speed  (report[5..7])  → low / strong motor
        let right = le(8); // right_speed (report[7..9]) → high / weak motor
        fb.rumble = Some((left, right));
    }
    fb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_declares_input_and_feature_reports() {
        assert!(
            STEAMDECK_RDESC.contains(&0xB1),
            "missing Feature main item — steam_is_valve_interface() would fail"
        );
        assert!(STEAMDECK_RDESC.contains(&0x81), "missing Input main item");
        assert_eq!(
            *STEAMDECK_RDESC.last().unwrap(),
            0xC0,
            "unterminated collection"
        );
    }

    /// Every analog field lands at the exact offset `steam_do_deck_input_event` reads, the header is
    /// what `steam_raw_event` requires, and the buttons pack into bytes 8..16 (12+15 zero). A
    /// one-byte slip here turns the whole controller into noise.
    #[test]
    fn serialize_is_byte_exact() {
        let mut st = SteamState::neutral();
        st.buttons = btn::A | btn::L4 | btn::R5 | btn::QAM;
        st.lx = 0x1122;
        st.ly = 0x3344;
        st.rx = 0x5566;
        st.ry = 0x778;
        st.lt = 0xABCD;
        st.rt = 0xEF01;
        st.lpad_x = 0x0A0B;
        st.lpad_y = 0x0C0D;
        st.rpad_x = 0x0E0F;
        st.rpad_y = 0x1011;
        st.accel = [0x0102, 0x0304, 0x0506];
        st.gyro = [0x0708, 0x090A, 0x0B0C];
        st.lpad_pressure = 0x1314;
        st.rpad_pressure = 0x1516;
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, &st, 0xAABB_CCDD);
        assert_eq!(&r[0..4], &[0x01, 0x00, 0x09, 0x3C]);
        assert_eq!(&r[4..8], &[0xDD, 0xCC, 0xBB, 0xAA]); // seq LE
                                                         // buttons: A=bit7 (byte8), L4=bit41 (byte13.1), R5=bit16 (byte10.0), QAM=bit50 (byte14.2).
        assert_eq!(r[8], 0x80); // A
        assert_eq!(r[10], 0x01); // R5
        assert_eq!(r[12], 0x00); // unused button byte
        assert_eq!(r[13], 0x02); // L4 (bit 1)
        assert_eq!(r[14], 0x04); // QAM (bit 2)
        assert_eq!(r[15], 0x00); // unused button byte
        assert_eq!(&r[16..18], &0x0A0Bi16.to_le_bytes()); // lpad X
        assert_eq!(&r[20..22], &0x0E0Fi16.to_le_bytes()); // rpad X
        assert_eq!(&r[24..26], &0x0102i16.to_le_bytes()); // accel X
        assert_eq!(&r[26..28], &0x0304i16.to_le_bytes()); // accel Y
        assert_eq!(&r[28..30], &0x0506i16.to_le_bytes()); // accel Z
        assert_eq!(&r[30..32], &0x0708i16.to_le_bytes()); // gyro X
        assert_eq!(&r[44..46], &0xABCDu16.to_le_bytes()); // left trigger
        assert_eq!(&r[46..48], &0xEF01u16.to_le_bytes()); // right trigger
        assert_eq!(&r[48..50], &0x1122i16.to_le_bytes()); // left joy X
        assert_eq!(&r[50..52], &0x3344i16.to_le_bytes()); // left joy Y
        assert_eq!(&r[52..54], &0x5566i16.to_le_bytes()); // right joy X
        assert_eq!(&r[56..58], &0x1314u16.to_le_bytes()); // left pad pressure
        assert_eq!(&r[58..60], &0x1516u16.to_le_bytes()); // right pad pressure
    }

    /// `from_gamepad` sets the right Deck bits + scales triggers, and a touched flag is merged when
    /// a trackpad contact arrives via `apply_rich`.
    #[test]
    fn from_gamepad_and_rich_mapping() {
        let s = SteamState::from_gamepad(
            gs::BTN_A | gs::BTN_START | gs::BTN_GUIDE | gs::BTN_LB,
            1000,
            -2000,
            0,
            0,
            255,
            0,
        );
        assert_ne!(s.buttons & btn::A, 0);
        assert_ne!(s.buttons & btn::MENU, 0);
        assert_ne!(s.buttons & btn::STEAM, 0);
        assert_ne!(s.buttons & btn::LB, 0);
        assert_ne!(s.buttons & btn::LT_FULL, 0); // lt=255 → full-pull bit
        assert_eq!(s.lt, 255 * 128);
        assert_eq!(s.lx, 1000);
        assert_eq!(s.ly, -2000);

        let mut s = SteamState::neutral();
        s.apply_rich(RichInput::Touchpad {
            pad: 0,
            finger: 0,
            active: true,
            x: 65535,
            y: 0,
        });
        assert_ne!(s.buttons & btn::RPAD_TOUCH, 0);
        assert_eq!(s.rpad_x, 32767); // 65535-32768
        assert_eq!(s.rpad_y, -32768); // 0-32768
        s.apply_rich(RichInput::Motion {
            pad: 0,
            gyro: [1, 2, 3],
            accel: [4, 5, 6],
        });
        assert_eq!(s.gyro, [1, 2, 3]);
        assert_eq!(s.accel, [4, 5, 6]);
    }

    /// The serial reply carries the leading report-id byte the kernel strips, so the *stripped*
    /// view (`reply[1..]`) is what `steam_get_serial` validates: `[0xAE, len, 0x01, ascii…]`.
    #[test]
    fn serial_reply_has_stripped_prefix() {
        let r = serial_reply("PUNKTFUNK01");
        assert_eq!(r[0], 0x00); // report id, stripped by steam_recv_report
        assert_eq!(r[1], ID_GET_STRING_ATTRIBUTE); // becomes reply[0] after strip
        assert!((1..=21).contains(&r[2]));
        assert_eq!(r[3], ATTRIB_STR_UNIT_SERIAL);
        assert_eq!(&r[4..4 + r[2] as usize], b"PUNKTFUNK01");
    }

    /// A `0xEB` rumble feature report parses to `(left_speed, right_speed)`; other commands don't.
    #[test]
    fn parse_rumble_feedback() {
        // [report-id 0, 0xEB, len 9, 0, intensity(2), left(2), right(2), gains(2)]
        let mut d = vec![0u8; 12];
        d[1] = ID_TRIGGER_RUMBLE_CMD;
        d[2] = 9;
        d[6..8].copy_from_slice(&0x8000u16.to_le_bytes()); // left_speed
        d[8..10].copy_from_slice(&0x4000u16.to_le_bytes()); // right_speed
        assert_eq!(parse_steam_output(&d).rumble, Some((0x8000, 0x4000)));

        let mut d = vec![0u8; 12];
        d[1] = ID_SET_SETTINGS_VALUES; // a settings write — no rumble
        assert_eq!(parse_steam_output(&d).rumble, None);
    }
}
