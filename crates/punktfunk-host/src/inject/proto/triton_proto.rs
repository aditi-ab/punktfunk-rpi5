//! Transport-independent contract for the virtual **Steam Controller 2** (2026, Valve "Ibex" /
//! SDL "Triton", wired `28DE:1302`) — the as-is passthrough sibling of [`super::steam_proto`].
//!
//! Unlike the Deck/classic-SC backends, this device is NOT re-synthesized from typed wire state:
//! the client captures the physical controller (USB Puck / wired / BLE) and forwards its raw
//! input reports verbatim ([`RichInput::HidReport`](punktfunk_core::quic::RichInput)); the host
//! mirrors them out unchanged, and everything the host's hidraw consumer writes back (Steam's
//! lizard-off / IMU-enable feature reports, `0x80` rumble output reports) is forwarded raw to the
//! client for replay on the real controller ([`HidOutput::HidRaw`](punktfunk_core::quic::HidOutput)).
//! Mainline `hid-steam` does not bind this PID, so no kernel evdev exists — **Steam Input is the
//! consumer**, driving the hidraw node exactly as it drives the physical pad.
//!
//! Protocol ground truth: SDL's `SDL_hidapi_steam_triton.c` + `steam/controller_structs.h`
//! (Valve-maintained). Input report ids `0x42`/`0x45` (`TritonMTUNoQuat_t`, 46 bytes with id),
//! `0x47` (adds a trackpad timestamp), `0x43` battery, `0x46`/`0x79` wireless status. Feature
//! reports are 64 bytes on report id `1`; haptics are OUTPUT reports `0x80..=0x85`.
//!
//! A typed **fallback synthesizer** is kept for the degraded case (a client that declared the
//! kind but sends no raw feed): buttons/sticks/triggers from the ordinary gamepad plane are
//! serialized into a minimal `0x42` state report. The first raw report permanently switches the
//! pad to as-is mode.

use punktfunk_core::input::gamepad as gs;

/// Valve vendor id (same as [`super::steam_proto::STEAM_VENDOR`], repeated to keep this module
/// self-contained).
pub const TRITON_VENDOR: u32 = 0x28DE;
/// The wired Steam Controller 2 identity the virtual pad presents. The BLE (`0x1303`) and Puck
/// dongle (`0x1304`/`0x1305`) identities are client-side transports only — Steam treats the wired
/// PID as the canonical controller.
pub const TRITON_WIRED_PRODUCT: u32 = 0x1302;

/// Triton input-report ids (`ETritonReportIDTypes`, SDL `controller_structs.h`).
pub const ID_TRITON_CONTROLLER_STATE: u8 = 0x42;
pub const ID_TRITON_BATTERY_STATUS: u8 = 0x43;
pub const ID_TRITON_CONTROLLER_STATE_BLE: u8 = 0x45;
pub const ID_TRITON_CONTROLLER_STATE_TIMESTAMP: u8 = 0x47;

/// Haptic OUTPUT report ids (`ID_OUT_REPORT_*`). Only rumble is parsed host-side (for the
/// universal 0xCA plane); every output report is forwarded raw regardless.
pub const ID_OUT_REPORT_HAPTIC_RUMBLE: u8 = 0x80;

/// Fixed report size: 64-byte feature reports, input reports at most 64 (state is 46/54).
pub const TRITON_REPORT_LEN: usize = 64;

/// The `TritonMTUNoQuat_t` state payload (46 bytes with the leading report id).
pub const TRITON_STATE_LEN: usize = 46;

/// Minimal vendor-defined HID report descriptor, mirroring [`super::steam_proto::STEAMDECK_RDESC`]
/// with an added OUTPUT item: the Triton receives haptics as output reports (`SDL_hid_write`),
/// not feature-only like the Deck, so hidapi consumers expect a writable interrupt-OUT-style
/// report to exist. All items unnumbered 64-byte — the raw bytes we mirror carry the Valve
/// report-type byte first, exactly like the physical device's stream.
#[rustfmt::skip]
pub const TRITON_RDESC: &[u8] = &[
    0x06, 0x00, 0xFF, // Usage Page (Vendor-Defined 0xFF00)
    0x09, 0x01,       // Usage (0x01)
    0xA1, 0x01,       // Collection (Application)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x00, //   Logical Maximum (255)
    0x75, 0x08,       //   Report Size (8 bits)
    0x95, 0x40,       //   Report Count (64)
    0x09, 0x01,       //   Usage (0x01)
    0x81, 0x02,       //   Input (Data,Var,Abs)    — the state/battery/wireless report stream
    0x09, 0x01,       //   Usage (0x01)
    0x95, 0x40,       //   Report Count (64)
    0x91, 0x02,       //   Output (Data,Var,Abs)   — haptic commands (0x80 rumble, 0x81 pulse, …)
    0x09, 0x01,       //   Usage (0x01)
    0x95, 0x40,       //   Report Count (64)
    0xB1, 0x02,       //   Feature (Data,Var,Abs)  — settings/attributes (report id 1 on the wire)
    0xC0,             // End Collection
];

/// Triton button bits in the state report's `buttons` u32 — transcribed verbatim from SDL's
/// `TritonButtons`. Only the bits the typed fallback synthesizes are named; the raw path carries
/// whatever the physical pad set.
pub mod tbtn {
    pub const A: u32 = 0x0000_0001;
    pub const B: u32 = 0x0000_0002;
    pub const X: u32 = 0x0000_0004;
    pub const Y: u32 = 0x0000_0008;
    pub const QAM: u32 = 0x0000_0010;
    pub const R3: u32 = 0x0000_0020;
    pub const VIEW: u32 = 0x0000_0040;
    pub const R4: u32 = 0x0000_0080;
    pub const R5: u32 = 0x0000_0100;
    pub const RB: u32 = 0x0000_0200;
    pub const DPAD_DOWN: u32 = 0x0000_0400;
    pub const DPAD_RIGHT: u32 = 0x0000_0800;
    pub const DPAD_LEFT: u32 = 0x0000_1000;
    pub const DPAD_UP: u32 = 0x0000_2000;
    pub const MENU: u32 = 0x0000_4000;
    pub const L3: u32 = 0x0000_8000;
    pub const STEAM: u32 = 0x0001_0000;
    pub const L4: u32 = 0x0002_0000;
    pub const L5: u32 = 0x0004_0000;
    pub const LB: u32 = 0x0008_0000;
    pub const RPAD_TOUCH: u32 = 0x0020_0000;
    pub const RPAD_CLICK: u32 = 0x0040_0000;
    pub const RT_CLICK: u32 = 0x0080_0000;
    pub const LPAD_TOUCH: u32 = 0x0200_0000;
    pub const LPAD_CLICK: u32 = 0x0400_0000;
    pub const LT_CLICK: u32 = 0x0800_0000;
}

/// One virtual Triton pad's report state. In as-is mode (`raw_len > 0`) the raw report IS the
/// state; the typed fields only feed the fallback synthesizer until the first raw report lands.
#[derive(Clone, Copy)]
pub struct TritonState {
    /// The last raw report the client forwarded (report-id byte first); `raw_len == 0` until the
    /// first one arrives, after which the typed fields below stop mattering.
    pub raw: [u8; TRITON_REPORT_LEN],
    pub raw_len: u8,
    /// Typed fallback fields (Triton bit layout / raw axis units), from the ordinary wire plane.
    pub buttons: u32,
    pub lt: u16,
    pub rt: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
}

impl TritonState {
    pub fn neutral() -> TritonState {
        TritonState {
            raw: [0u8; TRITON_REPORT_LEN],
            raw_len: 0,
            buttons: 0,
            lt: 0,
            rt: 0,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
        }
    }

    /// Typed fallback: fold one wire button/stick frame into Triton fields. Mapping follows the
    /// Deck backend's conventions (PADDLE1/2/3/4 = R4/L4/R5/L5, MISC1 = QAM, the DualSense
    /// touchpad-click wire bit = right-pad click); sticks are already the device convention
    /// (+y up), triggers scale 0..255 → 0..32767.
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> TritonState {
        let on = |bit: u32| buttons & bit != 0;
        let trig = |v: u8| ((v as u32 * 32767) / 255) as u16;
        let mut b = 0u32;
        let set = |b: &mut u32, on: bool, m: u32| {
            if on {
                *b |= m;
            }
        };
        set(&mut b, on(gs::BTN_A), tbtn::A);
        set(&mut b, on(gs::BTN_B), tbtn::B);
        set(&mut b, on(gs::BTN_X), tbtn::X);
        set(&mut b, on(gs::BTN_Y), tbtn::Y);
        set(&mut b, on(gs::BTN_LB), tbtn::LB);
        set(&mut b, on(gs::BTN_RB), tbtn::RB);
        set(&mut b, on(gs::BTN_BACK), tbtn::VIEW);
        set(&mut b, on(gs::BTN_START), tbtn::MENU);
        set(&mut b, on(gs::BTN_GUIDE), tbtn::STEAM);
        set(&mut b, on(gs::BTN_LS_CLICK), tbtn::L3);
        set(&mut b, on(gs::BTN_RS_CLICK), tbtn::R3);
        set(&mut b, on(gs::BTN_DPAD_UP), tbtn::DPAD_UP);
        set(&mut b, on(gs::BTN_DPAD_DOWN), tbtn::DPAD_DOWN);
        set(&mut b, on(gs::BTN_DPAD_LEFT), tbtn::DPAD_LEFT);
        set(&mut b, on(gs::BTN_DPAD_RIGHT), tbtn::DPAD_RIGHT);
        set(&mut b, on(gs::BTN_TOUCHPAD), tbtn::RPAD_CLICK);
        set(&mut b, on(gs::BTN_PADDLE1), tbtn::R4);
        set(&mut b, on(gs::BTN_PADDLE2), tbtn::L4);
        set(&mut b, on(gs::BTN_PADDLE3), tbtn::R5);
        set(&mut b, on(gs::BTN_PADDLE4), tbtn::L5);
        set(&mut b, on(gs::BTN_MISC1), tbtn::QAM);
        // "Fully pressed" digital shadow of the analog triggers (the physical pad's own
        // threshold is a hard pull, not first-contact).
        set(&mut b, lt >= 240, tbtn::LT_CLICK);
        set(&mut b, rt >= 240, tbtn::RT_CLICK);
        TritonState {
            raw: [0u8; TRITON_REPORT_LEN],
            raw_len: 0,
            buttons: b,
            lt: trig(lt),
            rt: trig(rt),
            lx,
            ly,
            rx,
            ry,
        }
    }
}

/// Serialize the typed fallback state into a minimal `0x42` `TritonMTUNoQuat_t` report:
/// `[0x42][seq u8][buttons u32][trigL i16][trigR i16][sticks i16×4][lpad x/y + pressure]
/// [rpad x/y + pressure][imu ts u32 + accel i16×3 + gyro i16×3]` — pads and IMU stay zero
/// (no raw feed = no trackpad/motion source; Steam only sees IMU data after enabling
/// `SETTING_IMU_MODE` on a real feed anyway).
pub fn serialize_triton_state(buf: &mut [u8; TRITON_STATE_LEN], st: &TritonState, seq: u8) {
    buf.fill(0);
    buf[0] = ID_TRITON_CONTROLLER_STATE;
    buf[1] = seq;
    buf[2..6].copy_from_slice(&st.buttons.to_le_bytes());
    buf[6..8].copy_from_slice(&(st.lt as i16).to_le_bytes());
    buf[8..10].copy_from_slice(&(st.rt as i16).to_le_bytes());
    buf[10..12].copy_from_slice(&st.lx.to_le_bytes());
    buf[12..14].copy_from_slice(&st.ly.to_le_bytes());
    buf[14..16].copy_from_slice(&st.rx.to_le_bytes());
    buf[16..18].copy_from_slice(&st.ry.to_le_bytes());
    // [18..30] left/right pad + pressures stay zero; [30..46] IMU stays zero.
}

/// One service pass's extracted feedback: the raw reports to forward (kind-tagged for
/// [`HidOutput::HidRaw`](punktfunk_core::quic::HidOutput)) plus the rumble level parsed out of a
/// `0x80` report for the universal 0xCA plane (drives the phone-mirror path on clients whose
/// physical pad already gets the raw report).
#[derive(Default)]
pub struct TritonFeedback {
    /// `(low, high)` — `left.speed`/`right.speed` of the last rumble output report seen.
    pub rumble: Option<(u16, u16)>,
    /// Raw reports to forward: `(kind, bytes)` with kind = `HID_RAW_OUTPUT`/`HID_RAW_FEATURE`.
    pub raw: Vec<(u8, Vec<u8>)>,
}

/// Parse a Triton haptic-rumble OUTPUT report (`MsgHapticRumble`, 10 bytes with id):
/// `[0x80][type u8][intensity u16][left.speed u16][left.gain i8][right.speed u16][right.gain i8]`.
/// Returns `(left_speed, right_speed)` as `(low, high)`.
pub fn parse_triton_rumble(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 10 || data[0] != ID_OUT_REPORT_HAPTIC_RUMBLE {
        return None;
    }
    let le = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
    Some((le(4), le(7)))
}

/// Strip the hidraw unnumbered-report `0x00` prefix if present: Triton report/command ids are all
/// non-zero (`0x42+` input, `0x80+` output, `1` feature), so a leading zero can only be the
/// synthetic report-id byte hidraw prepends on this unnumbered virtual descriptor.
pub fn strip_report_prefix(data: &[u8]) -> &[u8] {
    match data {
        [0, rest @ ..] if !rest.is_empty() => rest,
        d => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typed fallback lands the canonical wire mapping on the SDL-documented bit positions
    /// and byte offsets.
    #[test]
    fn fallback_state_serializes_sdl_layout() {
        let st = TritonState::from_gamepad(
            gs::BTN_A | gs::BTN_START | gs::BTN_PADDLE1 | gs::BTN_MISC1,
            1000,
            -2000,
            3000,
            -32768,
            255,
            0,
        );
        assert_eq!(
            st.buttons,
            tbtn::A | tbtn::MENU | tbtn::R4 | tbtn::QAM | tbtn::LT_CLICK
        );
        assert_eq!(st.lt, 32767); // exact full-scale, not the *128 approximation
        let mut r = [0u8; TRITON_STATE_LEN];
        serialize_triton_state(&mut r, &st, 7);
        assert_eq!(r[0], ID_TRITON_CONTROLLER_STATE);
        assert_eq!(r[1], 7);
        assert_eq!(u32::from_le_bytes([r[2], r[3], r[4], r[5]]), st.buttons);
        assert_eq!(i16::from_le_bytes([r[6], r[7]]), 32767); // sTriggerLeft
        assert_eq!(i16::from_le_bytes([r[10], r[11]]), 1000); // sLeftStickX
        assert_eq!(i16::from_le_bytes([r[16], r[17]]), -32768); // sRightStickY
        assert!(r[18..].iter().all(|&b| b == 0)); // pads + IMU zero
    }

    /// A rumble output report parses to `(left_speed, right_speed)`; other ids don't.
    #[test]
    fn rumble_output_report_parses() {
        // [0x80, type, intensity(2), left.speed(2), left.gain, right.speed(2), right.gain]
        let mut d = [0u8; 10];
        d[0] = ID_OUT_REPORT_HAPTIC_RUMBLE;
        d[4..6].copy_from_slice(&0x1234u16.to_le_bytes());
        d[7..9].copy_from_slice(&0x5678u16.to_le_bytes());
        assert_eq!(parse_triton_rumble(&d), Some((0x1234, 0x5678)));
        d[0] = 0x81; // haptic pulse — not rumble
        assert_eq!(parse_triton_rumble(&d), None);
        assert_eq!(parse_triton_rumble(&d[..8]), None); // short
    }

    /// The hidraw `0x00` unnumbered prefix strips; genuine command bytes survive.
    #[test]
    fn report_prefix_strips_only_leading_zero() {
        assert_eq!(strip_report_prefix(&[0x00, 0x80, 1, 2]), &[0x80, 1, 2]);
        assert_eq!(strip_report_prefix(&[0x80, 1, 2]), &[0x80, 1, 2]);
        assert_eq!(strip_report_prefix(&[0x01, 0x87]), &[0x01, 0x87]); // feature id 1 kept
        assert_eq!(strip_report_prefix(&[0x00]), &[0x00]); // lone zero: nothing to strip to
    }
}
