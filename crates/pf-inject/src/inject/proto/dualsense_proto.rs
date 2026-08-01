//! Transport-independent DualSense HID contract — shared by the Linux UHID backend
//! ([`super::dualsense`]) and the Windows UMDF-driver backend ([`super::dualsense_windows`]).
//!
//! This is the pure logic: the report descriptor, feature blobs, the [`DsState`] controller model
//! and its `GameStream`/XInput mapper, the input-report serializer (report `0x01`) and the
//! output-report parser (report `0x02`, a game's rumble / lightbar / player-LED / adaptive-trigger
//! feedback). Neither half depends on a transport — the Linux backend writes `0x01` to `/dev/uhid`
//! and reads `0x02` via `UHID_OUTPUT`; the Windows backend pushes `0x01` to the UMDF driver and
//! pulls `0x02` back over its control channel — but both build/parse the exact same bytes.
//!
//! The descriptor + field layout are the canonical inputtino ones (games-on-whales/inputtino
//! `src/uhid/include/uhid/ps5.hpp`), so `hid-playstation` (Linux) and `hidclass` (Windows) bind the
//! same as a real USB DualSense.

use punktfunk_core::quic::{HidOutput, RichInput};

// Feature reports the host stack GET_REPORTs during init — without these replies the kernel
// (`hid-playstation`) never finishes calibration and creates no input devices. Verbatim from
// inputtino (each array's first byte is the report id). The pairing report carries a fixed
// virtual MAC.
#[rustfmt::skip]
// FIXME(cal-len): the descriptor declares report 0x05 as a 40-byte feature (id + 40 = 41 total),
// but this blob is 42 bytes (one trailing pad byte too many). Linux `hid-playstation` tolerates it
// (the backend is live-validated), and `hidclass` truncates to the declared length, so it is not
// currently blocking; trim the trailing 0x00 to 41 once a physical DualSense is available to
// re-verify motion calibration on both backends.
pub const DS_FEATURE_CALIBRATION: &[u8] = &[ // report 0x05 (motion calibration)
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0xF4, 0x01, 0xF4, 0x01, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
pub const DS_FEATURE_PAIRING: &[u8] = &[ // report 0x09 (pairing info: MAC at bytes 1..7)
    0x09, 0x74, 0xE7, 0xD6, 0x3A, 0x53, 0x35, 0x08, 0x25, 0x00, 0x1E, 0x00, 0xEE, 0x74, 0xD0, 0xBC,
    0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
pub const DS_FEATURE_FIRMWARE: &[u8] = &[ // report 0x20 (firmware info / build date); bytes 44..46
    // = update version, kept ABOVE Sony's real releases (0x0630 as of 2026-08) — an older value
    // makes PlayStation Accessories and libScePad titles demand a firmware update the virtual pad
    // cannot take ("can't complete the update"), and ≥ 0x0224 is what puts writers on the
    // COMPATIBLE_VIBRATION2 convention parse_ds_output accepts alongside flag0.
    0x20, 0x4A, 0x75, 0x6E, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3A, 0x34,
    0x37, 0x3A, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xC1, 0xC8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x99, 0x09, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x0B, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The pairing reply (report `0x09`) for wire pad `pad`: [`DS_FEATURE_PAIRING`] with the MAC's low
/// octet offset by the pad index. The MAC must be **unique per pad**: `hid-playstation` adopts it
/// as the HID `uniq` (replacing whatever uniq the device was created with), and SDL/Steam dedup
/// controllers by that serial — with identical MACs a second virtual pad reads as the *first* pad
/// re-appearing over another transport and is merged/ignored.
pub fn ds_pairing_reply(pad: u8) -> [u8; 20] {
    let mut r = [0u8; 20];
    r.copy_from_slice(DS_FEATURE_PAIRING);
    r[1] = r[1].wrapping_add(pad); // MAC lives at bytes 1..7, LSB first
    r
}

/// Sony DualSense USB HID report descriptor (273 bytes), verbatim from inputtino — the exact
/// descriptor `hid-playstation` (Linux) / `hidclass` (Windows) parses to bind a DualSense.
#[rustfmt::skip]
pub const DUALSENSE_RDESC: &[u8] = &[
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

/// Sony DualSense **Edge** USB HID report descriptor (389 bytes) — a verbatim real-device
/// capture (hid-recorder, hhd-dev/hwinfo `devices/ds5_edge`, cross-checked byte-for-byte against
/// the raw usbmon pcap in the same repo and the descriptor Handheld Daemon ships for ITS virtual
/// UHID Edge). vs the plain DS5 descriptor: output report `0x02` grows 47→63 bytes, feature
/// `0xF2` 15→52, and 19 vendor feature reports (`0x60..=0x7B`, the Edge profile slots) are
/// appended — input report `0x01` is bit-identical (the Edge's Fn/back buttons ride previously
/// reserved bits of `buttons[2]`, see [`btn2`]).
#[rustfmt::skip]
pub const DUALSENSE_EDGE_RDESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x3F, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xB1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2F, 0xB1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xB1, 0x02, 0x85, 0x0A, 0x09, 0x25, 0x95, 0x1A, 0xB1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xB1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x2A, 0x95, 0x09, 0xB1, 0x02,
    0x85, 0x83, 0x09, 0x2B, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x84, 0x09, 0x2C, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x85, 0x09, 0x2D, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xA0, 0x09, 0x2E, 0x95, 0x01, 0xB1, 0x02,
    0x85, 0xE0, 0x09, 0x2F, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x30, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0xF1, 0x09, 0x31, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2, 0x09, 0x32, 0x95, 0x34, 0xB1, 0x02,
    0x85, 0xF4, 0x09, 0x35, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF5, 0x09, 0x36, 0x95, 0x03, 0xB1, 0x02,
    0x85, 0x60, 0x09, 0x41, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x61, 0x09, 0x42, 0xB1, 0x02, 0x85, 0x62,
    0x09, 0x43, 0xB1, 0x02, 0x85, 0x63, 0x09, 0x44, 0xB1, 0x02, 0x85, 0x64, 0x09, 0x45, 0xB1, 0x02,
    0x85, 0x65, 0x09, 0x46, 0xB1, 0x02, 0x85, 0x68, 0x09, 0x47, 0xB1, 0x02, 0x85, 0x70, 0x09, 0x48,
    0xB1, 0x02, 0x85, 0x71, 0x09, 0x49, 0xB1, 0x02, 0x85, 0x72, 0x09, 0x4A, 0xB1, 0x02, 0x85, 0x73,
    0x09, 0x4B, 0xB1, 0x02, 0x85, 0x74, 0x09, 0x4C, 0xB1, 0x02, 0x85, 0x75, 0x09, 0x4D, 0xB1, 0x02,
    0x85, 0x76, 0x09, 0x4E, 0xB1, 0x02, 0x85, 0x77, 0x09, 0x4F, 0xB1, 0x02, 0x85, 0x78, 0x09, 0x50,
    0xB1, 0x02, 0x85, 0x79, 0x09, 0x51, 0xB1, 0x02, 0x85, 0x7A, 0x09, 0x52, 0xB1, 0x02, 0x85, 0x7B,
    0x09, 0x53, 0xB1, 0x02, 0xC0,
];

pub const DS_VENDOR: u32 = 0x054C; // Sony Interactive Entertainment
pub const DS_PRODUCT: u32 = 0x0CE6; // DualSense Wireless Controller
pub const DS_EDGE_PRODUCT: u32 = 0x0DF2; // DualSense Edge Wireless Controller
/// USB input report `0x01` is 64 bytes total (report id + 63-byte body).
pub const DS_INPUT_REPORT_LEN: usize = 64;
/// The DualSense touchpad's reported resolution (the kernel exposes it as ABS_MT 0..1920/1080).
pub const DS_TOUCH_W: u16 = 1920;
pub const DS_TOUCH_H: u16 = 1080;

/// Bit positions inside the DualSense face/dpad button byte (`buttons[0]`, low nibble = hat).
pub mod btn0 {
    pub const SQUARE: u8 = 0x10;
    pub const CROSS: u8 = 0x20;
    pub const CIRCLE: u8 = 0x40;
    pub const TRIANGLE: u8 = 0x80;
}
/// `buttons[1]`: shoulders, triggers-as-buttons, create/options, stick clicks.
pub mod btn1 {
    pub const L1: u8 = 0x01;
    pub const R1: u8 = 0x02;
    pub const L2: u8 = 0x04;
    pub const R2: u8 = 0x08;
    pub const CREATE: u8 = 0x10; // "Share"
    pub const OPTIONS: u8 = 0x20;
    pub const L3: u8 = 0x40;
    pub const R3: u8 = 0x80;
}
/// `buttons[2]`: PS, touchpad click, mute — plus, on the DualSense **Edge**, the two Fn and two
/// back buttons in bits 4–7 (kernel `DS_EDGE_BUTTONS_*` / SDL `SDL_GAMEPAD_BUTTON_PS5_*`; the
/// plain DS5 leaves those bits reserved). The kernel maps them to `BTN_TRIGGER_HAPPY1..4`
/// (Fn-L, Fn-R, back-L, back-R) since 7.2; SDL/Steam read them off hidraw on any kernel.
pub mod btn2 {
    pub const PS: u8 = 0x01;
    pub const TOUCHPAD: u8 = 0x02;
    /// Mic-mute / capture button — set from the wire `BTN_MISC1` in `DsState::from_gamepad`.
    pub const MUTE: u8 = 0x04;
    /// Edge left Fn button (below the left stick).
    pub const EDGE_FN_LEFT: u8 = 0x10;
    /// Edge right Fn button.
    pub const EDGE_FN_RIGHT: u8 = 0x20;
    /// Edge left back button (rear paddle).
    pub const EDGE_BACK_LEFT: u8 = 0x40;
    /// Edge right back button (rear paddle).
    pub const EDGE_BACK_RIGHT: u8 = 0x80;
}

/// Map the wire back-grip bits onto the DualSense Edge's `buttons[2]` bits — the reason the Edge
/// backend exists: all four client paddles (Deck grips L4/L5/R4/R5, Elite P1–P4) land on native
/// slots instead of the fold/drop policy. Wire PADDLE1/2 = R4/L4 (the primary pair, Steam
/// convention) → the Edge's right/left BACK buttons; PADDLE3/4 = R5/L5 → the right/left Fn
/// buttons (real-HW Fn is profile-switch chrome, but on a virtual pad the bits reach consumers
/// as ordinary buttons — kernel `BTN_TRIGGER_HAPPY1/2`, SDL `LEFT/RIGHT_FUNCTION`).
pub fn edge_paddle_bits(buttons: u32) -> u8 {
    use punktfunk_core::input::gamepad as gs;
    let mut b = 0;
    if buttons & gs::BTN_PADDLE1 != 0 {
        b |= btn2::EDGE_BACK_RIGHT; // R4
    }
    if buttons & gs::BTN_PADDLE2 != 0 {
        b |= btn2::EDGE_BACK_LEFT; // L4
    }
    if buttons & gs::BTN_PADDLE3 != 0 {
        b |= btn2::EDGE_FN_RIGHT; // R5
    }
    if buttons & gs::BTN_PADDLE4 != 0 {
        b |= btn2::EDGE_FN_LEFT; // L5
    }
    b
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
    /// Per-contact-slot click state from the rich plane (`TouchpadEx.click` — a Steam pad's
    /// physical pad-click). The serializers OR any held slot into the touchpad-click button
    /// bit: the DualSense has ONE clickable pad, so either Deck pad clicking counts. Lives
    /// outside `buttons` because `from_gamepad` rebuilds those from every button frame —
    /// managers must persist this across rebuilds like `touch`/`gyro`/`accel`.
    pub touch_click: [bool; 2],
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
        // The mic-mute / capture button (Deck '…' QAM on the Steam path). Clients send it as
        // BTN_MISC1; without this the DualSense mute button was inert on every PlayStation-family
        // virtual pad. Rebuilt from the wire bit each frame like PS/TOUCHPAD, so no persistence gap.
        if on(gs::BTN_MISC1) {
            s.buttons[2] |= btn2::MUTE;
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

    /// Apply one rich client→host event (touchpad contact / motion sample) into this state —
    /// the ONE mapping shared by every DualSense-family backend (Linux UHID, Windows UMDF,
    /// DS4 both ways; `touch_w`/`touch_h` are the pad's advertised extents, 1920×1080 vs
    /// 1920×942).
    ///
    /// Wire touch coordinates are screen convention (+x right, +y down) — same as the
    /// DualSense pad's own (top-left origin), so no flip here.
    ///
    /// A Steam Deck / Steam Controller client sends TWO pads as `TouchpadEx` surfaces; the
    /// DualSense has one pad with two contact slots, so the surfaces SPLIT it — left pad →
    /// contact 0 on the left half, right pad → contact 1 on the right half. That mirrors the
    /// physical thumb layout and lands exactly on the split-pad zones games and Steam Input
    /// already use for the DS4/DualSense touchpad. Pad clicks ride `touch_click` (the
    /// serializer ORs them into the touchpad-click button — one clickable pad, either
    /// surface counts); dropping them was the "Deck pad click does nothing on a DualSense
    /// host" gap.
    pub fn apply_rich(&mut self, rich: RichInput, touch_w: u16, touch_h: u16) {
        // Normalized position → pad extents. The kernel/driver advertises 0..=W-1 / 0..=H-1.
        let scale = |n: u32, extent: u16| ((n * (extent - 1) as u32) / u16::MAX as u32) as u16;
        match rich {
            RichInput::Touchpad {
                finger,
                active,
                x,
                y,
                ..
            } => {
                // The DualSense touchpad carries two contacts; clamp to a valid slot and keep
                // the reported contact id consistent with it (the wire `finger` is untrusted).
                let slot = (finger as usize).min(1);
                self.touch[slot] = Touch {
                    active,
                    id: slot as u8,
                    x: scale(x as u32, touch_w),
                    y: scale(y as u32, touch_h),
                };
            }
            RichInput::Motion { gyro, accel, .. } => {
                // The wire is already DualSense-convention units (20 LSB/°·s, 10000 LSB/g).
                self.gyro = gyro;
                self.accel = accel;
            }
            RichInput::TouchpadEx {
                surface,
                finger,
                touch,
                click,
                x,
                y,
                ..
            } => {
                let n = |v: i16| ((v as i32) + 32768) as u32; // signed centre-0 → 0..=65535
                let half = touch_w / 2;
                let (slot, tx) = match surface {
                    // The single / DualSense pad: full extent, slot by finger.
                    0 => ((finger as usize).min(1), scale(n(x), touch_w)),
                    // Steam LEFT pad → contact 0 on the left half.
                    1 => (0, scale(n(x), half)),
                    // Steam RIGHT pad (or anything newer) → contact 1 on the right half.
                    _ => (1, half + scale(n(x), half)),
                };
                self.touch[slot] = Touch {
                    active: touch,
                    id: slot as u8,
                    x: tx,
                    y: scale(n(y), touch_h),
                };
                self.touch_click[slot] = click;
            }
            // Raw as-is passthrough reports belong to the Triton backend, never a DS state.
            RichInput::HidReport { .. } => {}
        }
    }

    /// `buttons[2]` as serialized: the live button frame plus the touchpad-click bit when a
    /// rich-plane pad click is held (see [`DsState::touch_click`]).
    pub fn buttons2_with_click(&self) -> u8 {
        let mut b = self.buttons[2];
        if self.touch_click.iter().any(|c| *c) {
            b |= btn2::TOUCHPAD;
        }
        b
    }
}

/// Serialize a full input report `0x01` (pure — unit-testable without a transport). Field
/// offsets per the kernel's `struct dualsense_input_report`, this report's one consumer:
/// x..rz 0-5, seq 6, buttons[4] 7-10, reserved[4] 11-14, gyro[3] 15-20, accel[3] 21-26,
/// sensor_timestamp 27-30, reserved2 31, points[2] 32-39 (static_assert(sizeof == 63)).
/// The report id occupies r[0], so struct offset N = r[N + 1].
pub fn serialize_state(r: &mut [u8; DS_INPUT_REPORT_LEN], st: &DsState, seq: u8, ts: u32) {
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
    r[10] = st.buttons2_with_click(); // off 9 (PS/touchpad-click/mute; rich pad clicks OR in)
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

/// What one service pass extracted from the device's HID output reports.
/// Rich feedback (lightbar / player LEDs / adaptive triggers) rides the HID-output plane (0xCD);
/// motor rumble rides the universal rumble plane (0xCA) so non-DualSense clients still feel it.
#[derive(Default)]
pub struct DsFeedback {
    pub hidout: Vec<HidOutput>,
    /// `(low, high)` motor levels (0..=0xFFFF), if a report carried them.
    pub rumble: Option<(u16, u16)>,
    /// The driver's output-report ring overflowed this poll — pending reports were DISCARDED and
    /// feedback state is unknown; the [`UhidManager`](crate::uhid_manager) must resync (silence +
    /// re-armed dedups). Set by the backend's section drain, never by the parser.
    pub resync: bool,
}

/// Parse a DualSense USB output report (`0x02`) into a [`DsFeedback`]. The byte layout below is
/// the USB DualSense common report; only the well-understood fields (motor rumble, lightbar RGB,
/// player LEDs) are surfaced — adaptive-trigger blocks are forwarded raw for the client.
///
/// Every field is gated on the report's valid-flags (`valid_flag0` at data[1], `valid_flag1`
/// at data[2]) — writers only set the bits for fields they mean to change (the rest is zeroed),
/// so an ungated parse would turn every plain rumble write into a lightbar-off + triggers-off
/// broadcast.
pub fn parse_ds_output(pad: u8, data: &[u8], fb: &mut DsFeedback) {
    // data[0] is the report id (0x02). Be defensive about short reports.
    if data.first() != Some(&0x02) || data.len() < 48 {
        return;
    }
    let flag0 = data[1]; // BIT0 compat vibration, BIT1 haptics select, BIT2 R2, BIT3 L2
    let flag1 = data[2]; // BIT2 lightbar, BIT4 player indicators
                         // Motor rumble: high-frequency (small/right) motor at data[3], low-frequency (big/left) at
                         // data[4]. Scale 0..255 → 0..0xFFFF, same (low, high) convention as the uinput pad's mixer,
                         // and route to the universal rumble plane (0xCA).
                         // Writers on firmware ≥ 2.24 signal rumble via COMPATIBLE_VIBRATION2 in valid_flag2
                         // (data[39] BIT2) instead of flag0 BIT0. Our feature report advertises a version
                         // above 2.24 (DS_FEATURE_FIRMWARE bytes 44..46, chosen to keep Sony's updater
                         // quiet), so the kernel and SDL write the v2 flag — while older writers, and any
                         // that never read the version, stay on flag0. Both conventions must land here: a
                         // rumble dropped on either — including stops — is silently ignored, and a missed
                         // stop buzzes for the rest of the session (the 500 ms refresh re-sends stale state
                         // forever).
    if flag0 & 0x03 != 0 || data[39] & 0x04 != 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Steam dual-pad → DualSense touchpad SPLIT: left pad (surface 1) lands contact 0
    /// on the left half, right pad (surface 2) contact 1 on the right half; y follows the
    /// shared screen convention (top → 0) with no flip; pad clicks set the touchpad-click
    /// button bit in the serialized report.
    #[test]
    fn steam_surfaces_split_the_touchpad() {
        let mut s = DsState::neutral();
        // Left pad, centre → middle of the LEFT half.
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 1,
                finger: 0,
                touch: true,
                click: false,
                x: 0,
                y: 0,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert!(s.touch[0].active);
        assert_eq!(s.touch[0].id, 0);
        assert_eq!(s.touch[0].x, (DS_TOUCH_W / 2 - 1) / 2); // centre of 0..=959
        assert_eq!(s.touch[0].y, (DS_TOUCH_H - 1) / 2);
        // Right pad, top-right corner → right edge of the RIGHT half, y = 0 (screen top).
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 2,
                finger: 0,
                touch: true,
                click: true,
                x: i16::MAX,
                y: i16::MIN,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert!(s.touch[1].active);
        assert_eq!(s.touch[1].id, 1);
        assert_eq!(s.touch[1].x, DS_TOUCH_W - 1);
        assert_eq!(s.touch[1].y, 0);
        // The right pad's click reaches the (single) touchpad-click button bit.
        assert!(s.touch_click[1]);
        assert_eq!(s.buttons2_with_click() & btn2::TOUCHPAD, btn2::TOUCHPAD);
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &s, 0, 0);
        assert_eq!(r[10] & btn2::TOUCHPAD, btn2::TOUCHPAD);
        // Releasing the click clears the bit again.
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 2,
                finger: 0,
                touch: true,
                click: false,
                x: 0,
                y: 0,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(s.buttons2_with_click() & btn2::TOUCHPAD, 0);
    }

    /// The single-surface forms keep their full-pad mapping: unsigned `Touchpad` and
    /// `TouchpadEx` surface 0 both span the whole touchpad, slot picked by finger.
    #[test]
    fn single_surface_spans_full_pad() {
        let mut s = DsState::neutral();
        s.apply_rich(
            RichInput::Touchpad {
                pad: 0,
                finger: 0,
                active: true,
                x: 65535,
                y: 65535,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(
            (s.touch[0].x, s.touch[0].y),
            (DS_TOUCH_W - 1, DS_TOUCH_H - 1)
        );
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 0,
                finger: 1,
                touch: true,
                click: false,
                x: i16::MAX,
                y: i16::MAX,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(
            (s.touch[1].x, s.touch[1].y),
            (DS_TOUCH_W - 1, DS_TOUCH_H - 1)
        );
        // Motion is unit-passthrough (wire is already DualSense convention).
        s.apply_rich(
            RichInput::Motion {
                pad: 0,
                gyro: [100, -200, 300],
                accel: [-1000, 2000, -3000],
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(s.gyro, [100, -200, 300]);
        assert_eq!(s.accel, [-1000, 2000, -3000]);
    }

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

    /// Writers set only the valid-flag bits for the fields they mean to change (the rest of the
    /// report is zeroed) — a plain rumble write must NOT blank the lightbar / player LEDs /
    /// triggers, and an LED-only write must not stop the motors.
    #[test]
    fn parse_output_respects_valid_flags() {
        // Rumble write: only the vibration flags set, everything else zero.
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

        // An explicit stop — vibration flag SET, motors zero — parses as `Some((0, 0))`, never as
        // absence: this is what distinguishes "the game stopped the motors" from "this report says
        // nothing about rumble", and what `rumble_drove` (the abandoned-rumble watchdog's activity
        // signal) keys on.
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0x03; // compatible vibration + haptics select
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        assert_eq!(fb.rumble, Some((0, 0)));
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

    /// The wire touchpad-click / guide / mute bits (Moonlight's extended positions) land in
    /// `buttons[2]`.
    #[test]
    fn from_gamepad_maps_touchpad_click() {
        use punktfunk_core::input::gamepad as gs;
        let s = DsState::from_gamepad(gs::BTN_TOUCHPAD | gs::BTN_GUIDE, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], btn2::PS | btn2::TOUCHPAD);
        // BTN_MISC1 → the mic-mute / capture button (G6: was previously dropped entirely).
        let s = DsState::from_gamepad(gs::BTN_MISC1, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], btn2::MUTE);
        let s = DsState::from_gamepad(gs::BTN_A, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], 0);
    }

    /// The Edge paddle map, pinned against hid-playstation's `DS_EDGE_BUTTONS_*` masks (bits
    /// 4–7 of `buttons[2]`) and SDL's `SDL_GAMEPAD_BUTTON_PS5_*` (same byte off hidraw):
    /// PADDLE1/2 (R4/L4) → right/left BACK, PADDLE3/4 (R5/L5) → right/left Fn — and the mapped
    /// bits land in the serialized report's byte 10 next to the ordinary buttons[2] bits.
    #[test]
    fn edge_paddles_map_to_native_bits() {
        use punktfunk_core::input::gamepad as gs;
        assert_eq!(edge_paddle_bits(0), 0);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE1), btn2::EDGE_BACK_RIGHT);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE2), btn2::EDGE_BACK_LEFT);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE3), btn2::EDGE_FN_RIGHT);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE4), btn2::EDGE_FN_LEFT);
        // Exact kernel/SDL bit values (a one-bit slip ships dead paddles).
        assert_eq!(btn2::EDGE_FN_LEFT, 0x10);
        assert_eq!(btn2::EDGE_FN_RIGHT, 0x20);
        assert_eq!(btn2::EDGE_BACK_LEFT, 0x40);
        assert_eq!(btn2::EDGE_BACK_RIGHT, 0x80);
        // All four + a non-paddle bit: paddles map, the rest is ignored here.
        let all = gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4 | gs::BTN_A;
        assert_eq!(edge_paddle_bits(all), 0xF0);
        // Serialized: the Edge merge ORs into buttons[2]; byte 10 carries both the paddles and
        // the ordinary bits (e.g. a simultaneous PS press).
        let mut s = DsState::from_gamepad(gs::BTN_GUIDE, 0, 0, 0, 0, 0, 0);
        s.buttons[2] |= edge_paddle_bits(gs::BTN_PADDLE2 | gs::BTN_PADDLE3);
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &s, 0, 0);
        assert_eq!(r[10], btn2::PS | btn2::EDGE_BACK_LEFT | btn2::EDGE_FN_RIGHT);
    }

    /// The Edge descriptor is the real-device capture: exact length, the three deltas vs the
    /// plain DS5 descriptor (output 0x02 count 63, feature 0xF2 count 52, the appended profile
    /// feature reports), and an unchanged input-report prefix (report 0x01 is bit-identical —
    /// the serializer needs no Edge variant).
    #[test]
    fn edge_descriptor_shape() {
        assert_eq!(DUALSENSE_RDESC.len(), 273);
        assert_eq!(DUALSENSE_EDGE_RDESC.len(), 389);
        // Identical through the input-report + output-report-id prefix; the first delta is the
        // output report 0x02's Report Count at offset 109 (47 → 63 bytes of payload).
        assert_eq!(DUALSENSE_EDGE_RDESC[..109], DUALSENSE_RDESC[..109]);
        assert_eq!(DUALSENSE_RDESC[109], 0x2F);
        assert_eq!(DUALSENSE_EDGE_RDESC[109], 0x3F);
        assert_eq!(*DUALSENSE_EDGE_RDESC.last().unwrap(), 0xC0);
    }

    /// A short / wrong-id report yields nothing.
    #[test]
    fn parse_output_rejects_garbage() {
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &[0x01, 0, 0], &mut fb); // wrong report id, too short
        assert!(fb.rumble.is_none());
        assert!(fb.hidout.is_empty());
    }

    /// The pairing reply keeps the report id and differs across pads ONLY in the MAC low octet —
    /// distinct serials so SDL/Steam never dedup two virtual pads into one controller.
    #[test]
    fn pairing_reply_mac_is_per_pad() {
        assert_eq!(ds_pairing_reply(0).as_slice(), DS_FEATURE_PAIRING);
        let (a, b) = (ds_pairing_reply(1), ds_pairing_reply(2));
        assert_eq!(a[0], 0x09); // report id untouched
        assert_eq!(a[1], DS_FEATURE_PAIRING[1].wrapping_add(1));
        assert_eq!(b[1], DS_FEATURE_PAIRING[1].wrapping_add(2));
        assert_eq!(a[2..], b[2..]); // everything but the low octet identical
    }
}
