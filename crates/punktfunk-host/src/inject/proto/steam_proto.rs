//! Transport-independent Steam Controller / Steam Deck HID contract — the Steam analogue of
//! [`super::dualsense_proto`]. Descriptor, command/feature IDs, the serial GET_REPORT reply, and
//! the input-report serializer that the kernel `hid-steam` driver parses.
//!
//! **M0 scope (recognition spike):** only what is needed for `hid-steam` to bind a `/dev/uhid`
//! `28DE:1205` device and create its evdevs —
//!   * [`STEAMDECK_RDESC`]: a vendor collection with ≥1 **feature** report, which is the *sole*
//!     thing `steam_is_valve_interface()` checks (`!list_empty(&FEATURE.report_list)`);
//!   * [`serial_reply`]: the `steam_get_serial()` answer `[0xAE, len, 0x01, ascii…]` (a bad/absent
//!     reply is non-fatal — the kernel falls back to `"XXXXXXXXXX"` — but a valid one keeps probe
//!     instant);
//!   * [`serialize_deck_state`]: a neutral Deck state report whose header (`[0x01,0x00,0x09,len]`)
//!     `hid-steam` accepts and parses (the M0 spike proved 23 distinct `BTN_*` codes reach the
//!     evdev). The exact per-bit button offsets below are PROVISIONAL — M1 confirms them
//!     line-by-line against the lab kernel's `steam_do_deck_input_event` (the v6.12-sourced
//!     `byte 8 bit 7 = BTN_A` did NOT match on the 7.0 box).
//!
//! The **full** field layout (sticks, triggers, both trackpads, the IMU, all four back grips, the
//! `0xEB`/`0x8F` feedback reports) lands in M1, line-checked against the lab kernel's
//! `steam_do_deck_input_event` / `steam_haptic_rumble` — see `design/steam-controller-deck-support.md`.
#![allow(dead_code)] // M0: the full state model + the PadBackend wiring arrive in M1.

/// Valve. `hid-steam` matches purely by VID/PID over `BUS_USB`
/// (`HID_USB_DEVICE(0x28DE, 0x1205, STEAM_QUIRK_DECK)`), so a UHID device with these IDs binds.
pub const STEAM_VENDOR: u32 = 0x28DE;
/// Steam Deck built-in controller (same PID on LCD + OLED).
pub const STEAMDECK_PRODUCT: u32 = 0x1205;
/// Classic Steam Controller, wired (report id 1; a later identity behind the same manager).
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
/// Input report message types: SC = `ID_CONTROLLER_STATE`, Deck = `ID_CONTROLLER_DECK_STATE`.
pub const ID_CONTROLLER_STATE: u8 = 0x01;
pub const ID_CONTROLLER_DECK_STATE: u8 = 0x09;

/// Minimal vendor-defined HID report descriptor: one application collection with a 64-byte input
/// report and a 64-byte feature report, both UNNUMBERED (report id 0). `hid-steam` is a raw-event
/// driver (`steam_raw_event` consumes reports before HID field parsing), so the field layout is
/// cosmetic — but `steam_probe` requires `hid_parse` to succeed AND a non-empty FEATURE report
/// list (`steam_is_valve_interface`), so the feature item is mandatory.
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

// PROVISIONAL Deck button bits (from the v6.12 steam_do_deck_input_event listing) — NOT yet
// on-box validated: the M0 spike's mash-probe confirmed the report PARSES (the full BTN_* set
// fires), but byte 8 bit 7 alone did not produce BTN_A on the 7.0 box, so M1 must line-check the
// real per-bit map against the lab kernel before these are trusted.
/// `data[8]` bit 7 → (claimed) `BTN_A`.
pub const DECK_B8_A: u8 = 0x80;
/// `data[9]` bit 5 → (claimed) `BTN_MODE` (the Steam button).
pub const DECK_B9_STEAM: u8 = 0x20;

/// M0 controller state: just the five button bytes the Deck report packs (8, 9, 10, 13, 14). The
/// sticks, triggers, trackpads and IMU stay neutral (signed-centred at 0) for the recognition
/// spike; M1 fills them from the wire frame + rich-input planes.
#[derive(Clone, Copy, Default)]
pub struct SteamState {
    pub b8: u8,
    pub b9: u8,
    pub b10: u8,
    pub b13: u8,
    pub b14: u8,
}

impl SteamState {
    pub fn neutral() -> SteamState {
        SteamState::default()
    }

    /// Press/release `BTN_A` (the spike's toggle target).
    pub fn set_a(&mut self, down: bool) {
        if down {
            self.b8 |= DECK_B8_A;
        } else {
            self.b8 &= !DECK_B8_A;
        }
    }
}

/// Serialize a neutral-plus-buttons Deck state into the 64-byte unnumbered report. Header is
/// `[0x01, 0x00, 0x09, len]` + a little-endian frame counter; `steam_raw_event` drops anything
/// where `size != 64 || data[0] != 1 || data[1] != 0`, then switches on `data[2]`.
pub fn serialize_deck_state(r: &mut [u8; STEAM_REPORT_LEN], st: &SteamState, seq: u32) {
    r.fill(0);
    r[0] = 0x01;
    r[1] = 0x00;
    r[2] = ID_CONTROLLER_DECK_STATE;
    r[3] = 0x3C; // payload length; the kernel ignores it
    r[4..8].copy_from_slice(&seq.to_le_bytes());
    r[8] = st.b8;
    r[9] = st.b9;
    r[10] = st.b10;
    r[13] = st.b13;
    r[14] = st.b14;
}

/// Build the `steam_get_serial` GET_REPORT reply: `[0xAE, len, ATTRIB_STR_UNIT_SERIAL, ascii…]`,
/// padded to 64 bytes. The kernel validates `reply[0] == 0xAE && 1 <= reply[1] <= 21 &&
/// reply[2] == 1`; the serial ASCII follows at byte 3.
pub fn serial_reply(serial: &str) -> [u8; STEAM_REPORT_LEN] {
    let mut buf = [0u8; STEAM_REPORT_LEN];
    let bytes = serial.as_bytes();
    let len = bytes.len().clamp(1, 21);
    buf[0] = ID_GET_STRING_ATTRIBUTE;
    buf[1] = len as u8;
    buf[2] = ATTRIB_STR_UNIT_SERIAL;
    buf[3..3 + len].copy_from_slice(&bytes[..len]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `steam_is_valve_interface()` binds the device iff the descriptor declares ≥1 feature report,
    /// so the descriptor MUST contain a Feature main item (0xB1) — plus an Input item (0x81) for the
    /// state report. A regression here silently makes `hid-steam` treat the device as a
    /// keyboard/mouse boot interface and never create the gamepad.
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

    /// The report header is exactly what `steam_raw_event` requires (`data[0]==1, data[1]==0,
    /// data[2]==0x09`), the frame counter is little-endian, and `set_a` toggles byte 8 bit 7.
    #[test]
    fn serialize_header_seq_and_button() {
        let mut st = SteamState::neutral();
        st.set_a(true);
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, &st, 0xAABB_CCDD);
        assert_eq!(&r[0..4], &[0x01, 0x00, 0x09, 0x3C]);
        assert_eq!(&r[4..8], &[0xDD, 0xCC, 0xBB, 0xAA]); // seq LE
        assert_eq!(r[8] & DECK_B8_A, DECK_B8_A);
        st.set_a(false);
        serialize_deck_state(&mut r, &st, 0);
        assert_eq!(r[8] & DECK_B8_A, 0);
    }

    /// The serial reply passes `steam_get_serial`'s validation (`reply[0]==0xAE`, `1<=reply[1]<=21`,
    /// `reply[2]==1`) and carries the ASCII at byte 3.
    #[test]
    fn serial_reply_passes_kernel_validation() {
        let r = serial_reply("PUNKTFUNK01");
        assert_eq!(r[0], ID_GET_STRING_ATTRIBUTE);
        assert!((1..=21).contains(&r[1]));
        assert_eq!(r[2], ATTRIB_STR_UNIT_SERIAL);
        assert_eq!(&r[3..3 + r[1] as usize], b"PUNKTFUNK01");
    }
}
