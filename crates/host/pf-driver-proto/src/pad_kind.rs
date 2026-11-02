//! One row per pad kind the Windows pad driver presents: the identity it answers hidclass, SDL
//! and Steam with, and the report protocol it speaks.
//!
//! `pf-gamepad` looks its `device_type` up here for every descriptor, attribute, string and
//! input-shape answer, and [`crate::gamepad`] reads the same rows for VID/PID, report period and
//! clock. Rows are found by `devtype`, never by position or by a range of devtypes.
//!
//! A new kind is a row here, its token in [`crate::gamepad::HWID_DEVTYPES`] and its model line in
//! `pf_gamepad.inx`. The tests fail while any of the three is missing, and while the table
//! changes without a [`crate::gamepad::GAMEPAD_DRIVER_REV`] bump.

use crate::gamepad::{
    DEVTYPE_8BITDO_PRO2, DEVTYPE_8BITDO_PRO3, DEVTYPE_8BITDO_ULTIMATE2, DEVTYPE_DUALSENSE,
    DEVTYPE_DUALSENSE_EDGE, DEVTYPE_DUALSHOCK4, DEVTYPE_HORIPAD_STEAM, DEVTYPE_JOYCON_LEFT,
    DEVTYPE_JOYCON_RIGHT, DEVTYPE_STEAMCONTROLLER, DEVTYPE_STEAMDECK, DEVTYPE_SWITCH_PRO,
    DEVTYPE_TRITON, DEVTYPE_XBOX, DEVTYPE_XBOX_ELITE, DEVTYPE_XBOX_ONE_S, REPORT_PERIOD_US,
};
use crate::{deck, dualsense, dualshock4, eightbitdo, hori, switch, triton, xbox};

/// The protocol a kind speaks beyond its descriptor: its feature contract, its handshake, and
/// how the driver frames and paces its input. Kinds of one family differ only in their row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// DualSense and DualSense Edge: feature blobs `0x05`/`0x09`/`0x20`.
    DualSense,
    /// Feature blobs `0x02`/`0x12`/`0xA3`.
    DualShock4,
    /// Steam's command and response on the one unnumbered feature report.
    SteamDeck,
    /// The Deck's contract, answered as the captured pad where its client recorded one.
    SteamController,
    /// Raw passthrough: id-first input of several lengths, served only when it changes.
    Triton,
    Xbox,
    /// The driver answers the `0x80` command and `0x01` subcommand handshake itself.
    Switch,
    /// Feature `0x06` on the models that declare it ([`eightbitdo::has_caps`]).
    EightBitDo,
    Hori,
}

/// The pad's own clock the driver writes into every served report
/// ([`crate::gamepad::stamp_report_clock`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    None,
    /// Counter (7), packet sequence (12–15), sensor timestamp (28–31), second timer (49–52).
    DualSense,
    /// Six-bit counter in byte 7, a 16/3 µs timestamp (10–11) mirrored into byte 34.
    DualShock4,
    /// Valve's `unPacketNum` (4–7).
    PacketNumber,
    /// The 8-bit timer (1), except in a `0x81` handshake ack.
    SwitchTimer,
    /// A µs IMU clock (27–30).
    Micros32,
    /// A u16 µs clock (10–11).
    Micros16,
}

/// One pad kind.
#[derive(Clone, Copy, Debug)]
pub struct PadKind {
    pub devtype: u8,
    pub family: Family,
    pub vid: u16,
    pub pid: u16,
    /// `bcdDevice` in the HID attributes. SDL folds it into its joystick GUID.
    pub version: u16,
    /// `bcdHID` in the HID descriptor.
    pub bcd_hid: u16,
    /// The report descriptor the Windows driver serves, channel-proof feature included.
    pub rdesc: &'static [u8],
    /// First byte of the input report the host publishes: the report id, or the frame header
    /// of an unnumbered Valve report.
    pub input_id: u8,
    /// Bytes served per input report. hidclass refuses a completion longer than its buffer.
    pub input_len: usize,
    /// The input report served while no host is attached.
    pub neutral: [u8; 64],
    pub manufacturer: &'static str,
    pub product: &'static str,
    /// Microseconds between served input reports.
    pub period_us: u64,
    pub clock: Clock,
}

impl PadKind {
    /// The 9-byte `HID_DESCRIPTOR`: one report descriptor of [`Self::rdesc`]'s length.
    pub const fn hid_descriptor(&self) -> [u8; 9] {
        let bcd = self.bcd_hid.to_le_bytes();
        let len = (self.rdesc.len() as u16).to_le_bytes();
        [0x09, 0x21, bcd[0], bcd[1], 0x00, 0x01, 0x22, len[0], len[1]]
    }
}

const SONY: &str = "Sony Interactive Entertainment";
const VALVE: &str = "Valve Software";
const MICROSOFT: &str = "Microsoft";
const NINTENDO: &str = "Nintendo Co., Ltd.";

/// The Bluetooth Xbox pads: the wired ids are vendor-class XUSB/GIP devices with no HID
/// interface. One `bcdDevice` for all three, since the devnode's synthesized ids say `REV_0100`.
const XBOX_VERSION: u16 = 0x0407;

/// Every pad kind, one row per `DEVTYPE_*`.
pub static PAD_KINDS: [PadKind; 16] = [
    PadKind {
        devtype: DEVTYPE_DUALSENSE,
        family: Family::DualSense,
        vid: 0x054C,
        pid: 0x0CE6,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &dualsense::RDESC,
        input_id: 0x01,
        input_len: 64,
        neutral: dualsense::NEUTRAL_REPORT,
        manufacturer: SONY,
        product: "DualSense Wireless Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::DualSense,
    },
    PadKind {
        devtype: DEVTYPE_DUALSHOCK4,
        family: Family::DualShock4,
        vid: 0x054C,
        pid: 0x09CC,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &dualshock4::RDESC,
        input_id: 0x01,
        input_len: 64,
        neutral: dualshock4::NEUTRAL_REPORT,
        manufacturer: SONY,
        product: "Wireless Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::DualShock4,
    },
    PadKind {
        devtype: DEVTYPE_DUALSENSE_EDGE,
        family: Family::DualSense,
        vid: 0x054C,
        pid: 0x0DF2,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &dualsense::EDGE_RDESC,
        input_id: 0x01,
        input_len: 64,
        neutral: dualsense::NEUTRAL_REPORT,
        manufacturer: SONY,
        product: "DualSense Edge Wireless Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::DualSense,
    },
    PadKind {
        devtype: DEVTYPE_STEAMDECK,
        family: Family::SteamDeck,
        vid: 0x28DE,
        pid: 0x1205,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &deck::RDESC,
        input_id: 0x01,
        input_len: 64,
        neutral: deck::NEUTRAL_REPORT,
        manufacturer: VALVE,
        product: "Steam Deck Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::PacketNumber,
    },
    PadKind {
        devtype: DEVTYPE_XBOX,
        family: Family::Xbox,
        vid: 0x045E,
        pid: 0x0B13,
        version: XBOX_VERSION,
        bcd_hid: 0x0100,
        rdesc: &xbox::SERIES_RDESC,
        input_id: 0x01,
        input_len: xbox::SERIES_INPUT_LEN,
        neutral: xbox::NEUTRAL_REPORT,
        manufacturer: MICROSOFT,
        product: "Xbox Wireless Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::None,
    },
    PadKind {
        devtype: DEVTYPE_XBOX_ONE_S,
        family: Family::Xbox,
        vid: 0x045E,
        pid: 0x02FD,
        version: XBOX_VERSION,
        bcd_hid: 0x0100,
        rdesc: &xbox::NO_SHARE_RDESC,
        input_id: 0x01,
        input_len: xbox::NO_SHARE_INPUT_LEN,
        neutral: xbox::NEUTRAL_REPORT,
        manufacturer: MICROSOFT,
        // A real One S says this over Bluetooth too: only the PID tells it from the Series pad.
        product: "Xbox Wireless Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::None,
    },
    PadKind {
        devtype: DEVTYPE_XBOX_ELITE,
        family: Family::Xbox,
        vid: 0x045E,
        pid: 0x0B22,
        version: XBOX_VERSION,
        bcd_hid: 0x0100,
        rdesc: &xbox::NO_SHARE_RDESC,
        input_id: 0x01,
        input_len: xbox::NO_SHARE_INPUT_LEN,
        neutral: xbox::NEUTRAL_REPORT,
        manufacturer: MICROSOFT,
        product: "Xbox Elite Wireless Controller Series 2",
        period_us: REPORT_PERIOD_US,
        clock: Clock::None,
    },
    PadKind {
        devtype: DEVTYPE_TRITON,
        family: Family::Triton,
        vid: 0x28DE,
        pid: 0x1302,
        // The captured pad's `bcdDevice` and `bcdHID`.
        version: 0x0307,
        bcd_hid: 0x0111,
        rdesc: &triton::RDESC,
        input_id: 0x42,
        input_len: TRITON_STATE_LEN,
        neutral: triton::NEUTRAL_REPORT,
        manufacturer: VALVE,
        product: "Steam Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::None,
    },
    PadKind {
        devtype: DEVTYPE_SWITCH_PRO,
        family: Family::Switch,
        vid: 0x057E,
        pid: 0x2009,
        version: 0x0200,
        bcd_hid: 0x0111,
        rdesc: &switch::RDESC_WITH_PROOF,
        input_id: 0x30,
        input_len: switch::REPORT_LEN,
        neutral: switch::neutral_report(DEVTYPE_SWITCH_PRO),
        manufacturer: NINTENDO,
        product: "Pro Controller",
        // `0x30` every 8 ms over Bluetooth, inside `hid-nintendo`'s 8–17 ms window.
        period_us: 8_000,
        clock: Clock::SwitchTimer,
    },
    PadKind {
        devtype: DEVTYPE_8BITDO_ULTIMATE2,
        family: Family::EightBitDo,
        vid: eightbitdo::VENDOR,
        pid: 0x6012,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &eightbitdo::RDESC_WITH_PROOF,
        input_id: eightbitdo::REPORT_ID,
        input_len: eightbitdo::REPORT_LEN,
        neutral: eightbitdo::NEUTRAL_REPORT,
        manufacturer: "8BitDo",
        product: "8BitDo Ultimate 2 Wireless",
        // SDL's fixed step for this pad over Bluetooth; it carries no clock to correct it.
        period_us: 8_333,
        clock: Clock::None,
    },
    PadKind {
        devtype: DEVTYPE_8BITDO_PRO2,
        family: Family::EightBitDo,
        vid: eightbitdo::VENDOR,
        pid: 0x6003,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &eightbitdo::RDESC_CAPS_WITH_PROOF,
        input_id: eightbitdo::REPORT_ID,
        input_len: eightbitdo::REPORT_LEN,
        neutral: eightbitdo::NEUTRAL_REPORT,
        manufacturer: "8BitDo",
        product: "8BitDo Pro 2",
        period_us: REPORT_PERIOD_US,
        clock: Clock::Micros32,
    },
    PadKind {
        devtype: DEVTYPE_8BITDO_PRO3,
        family: Family::EightBitDo,
        vid: eightbitdo::VENDOR,
        pid: 0x6009,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &eightbitdo::RDESC_CAPS_WITH_PROOF,
        input_id: eightbitdo::REPORT_ID,
        input_len: eightbitdo::REPORT_LEN,
        neutral: eightbitdo::NEUTRAL_REPORT,
        manufacturer: "8BitDo",
        product: "8BitDo Pro 3",
        period_us: REPORT_PERIOD_US,
        clock: Clock::Micros32,
    },
    PadKind {
        devtype: DEVTYPE_HORIPAD_STEAM,
        family: Family::Hori,
        vid: hori::VENDOR,
        pid: hori::PRODUCT,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &hori::RDESC_WITH_PROOF,
        input_id: hori::REPORT_ID,
        input_len: hori::REPORT_LEN,
        neutral: hori::NEUTRAL_REPORT,
        manufacturer: "HORI CO.,LTD.",
        product: hori::NAME,
        period_us: REPORT_PERIOD_US,
        clock: Clock::Micros16,
    },
    PadKind {
        devtype: DEVTYPE_JOYCON_LEFT,
        family: Family::Switch,
        vid: 0x057E,
        pid: 0x2006,
        version: 0x0200,
        bcd_hid: 0x0111,
        rdesc: &switch::RDESC_WITH_PROOF,
        input_id: 0x30,
        input_len: switch::REPORT_LEN,
        neutral: switch::neutral_report(DEVTYPE_JOYCON_LEFT),
        manufacturer: NINTENDO,
        product: "Joy-Con (L)",
        // A Joy-Con pushes `0x30` every 15 ms.
        period_us: 15_000,
        clock: Clock::SwitchTimer,
    },
    PadKind {
        devtype: DEVTYPE_JOYCON_RIGHT,
        family: Family::Switch,
        vid: 0x057E,
        pid: 0x2007,
        version: 0x0200,
        bcd_hid: 0x0111,
        rdesc: &switch::RDESC_WITH_PROOF,
        input_id: 0x30,
        input_len: switch::REPORT_LEN,
        neutral: switch::neutral_report(DEVTYPE_JOYCON_RIGHT),
        manufacturer: NINTENDO,
        product: "Joy-Con (R)",
        period_us: 15_000,
        clock: Clock::SwitchTimer,
    },
    PadKind {
        devtype: DEVTYPE_STEAMCONTROLLER,
        family: Family::SteamController,
        vid: 0x28DE,
        pid: 0x1102,
        version: 0x0100,
        bcd_hid: 0x0100,
        rdesc: &deck::SC_RDESC,
        input_id: 0x01,
        input_len: 64,
        neutral: deck::SC_NEUTRAL_REPORT,
        manufacturer: VALVE,
        product: "Wired Controller",
        period_us: REPORT_PERIOD_US,
        clock: Clock::PacketNumber,
    },
];

/// The wired Triton's largest input, `0x42`; hidclass sizes its read buffer from it.
const TRITON_STATE_LEN: usize = match triton::input_len(0x42) {
    Some(len) => len,
    None => panic!("the Triton descriptor declares input 0x42"),
};

/// The row for `devtype`, or `None` for a value this build does not know.
pub const fn pad_kind(devtype: u8) -> Option<&'static PadKind> {
    let mut i = 0;
    while i < PAD_KINDS.len() {
        if PAD_KINDS[i].devtype == devtype {
            return Some(&PAD_KINDS[i]);
        }
        i += 1;
    }
    None
}

/// The row the driver serves `devtype` as: a value this build does not know gets the
/// DualSense answers.
pub fn served_kind(devtype: u8) -> &'static PadKind {
    match pad_kind(devtype) {
        Some(kind) => kind,
        None => &PAD_KINDS[0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad::HWID_DEVTYPES;
    use crate::rdesc::{report_lens, FEATURE, INPUT};

    /// The `pub const DEVTYPE_*: u8 = N;` lines of `gamepad.rs`, as `(name, value)`.
    fn declared_devtypes() -> Vec<(String, u8)> {
        let src = include_str!("gamepad.rs");
        let out: Vec<(String, u8)> = src
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub const DEVTYPE_"))
            .filter_map(|rest| {
                let (name, value) = rest.split_once(": u8 = ")?;
                let value = value.strip_suffix(';')?.parse().ok()?;
                Some((alloc::format!("DEVTYPE_{name}"), value))
            })
            .collect();
        assert!(
            out.len() >= 16,
            "parsed {} DEVTYPE_* constants; the declaration shape changed",
            out.len()
        );
        out
    }

    /// Every `DEVTYPE_*` constant has exactly one row, and every row names a declared constant.
    #[test]
    fn every_devtype_has_exactly_one_row() {
        let declared = declared_devtypes();
        for (name, value) in &declared {
            let rows = PAD_KINDS.iter().filter(|k| k.devtype == *value).count();
            assert_eq!(rows, 1, "{name} = {value} has {rows} rows in PAD_KINDS");
            assert_eq!(pad_kind(*value).map(|k| k.devtype), Some(*value));
        }
        for kind in &PAD_KINDS {
            assert!(
                declared.iter().any(|(_, v)| *v == kind.devtype),
                "row {} names no DEVTYPE_* constant",
                kind.devtype
            );
        }
        assert!(pad_kind(u8::MAX).is_none());
        assert_eq!(served_kind(u8::MAX).devtype, DEVTYPE_DUALSENSE);
    }

    /// Lines of `src` with a range bounded by a `DEVTYPE_*` constant, or a match arm on an
    /// integer range. Either changes meaning when a kind is added between its ends.
    fn adjacent_ranges(src: &str) -> Vec<&str> {
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        src.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| {
                let parts: Vec<&str> = l.split("..").collect();
                let devtype_bound = parts.windows(2).any(|w| {
                    let before = w[0].rsplit(|c| !word(c)).next().unwrap_or("");
                    let after = w[1].trim_start_matches('=').trim_start();
                    before.starts_with("DEVTYPE_") || after.starts_with("DEVTYPE_")
                });
                let arm = l.rsplit_once(" @ ").map_or(*l, |(_, a)| a);
                let int_arm = l.contains("=>")
                    && arm.split_once("..").is_some_and(|(a, _)| {
                        !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit())
                    });
                devtype_bound || int_arm
            })
            .collect()
    }

    /// No match or loop over pad kinds depends on two devtypes being adjacent: the driver and
    /// [`crate::gamepad`] dispatch through the table.
    #[test]
    fn no_devtype_range_depends_on_adjacency() {
        let driver = include_str!("../../../../packaging/windows/drivers/pf-gamepad/src/lib.rs");
        for (file, src) in [
            ("gamepad.rs", include_str!("gamepad.rs")),
            ("pf-gamepad/src/lib.rs", driver),
        ] {
            let hits = adjacent_ranges(src);
            assert!(hits.is_empty(), "{file}: {hits:#?}");
        }
        // The scan still sees the shapes it exists for.
        let devtype_range = alloc::format!("DEVTYPE_A{}DEVTYPE_B => x", "..=");
        assert_eq!(adjacent_ranges(&devtype_range).len(), 1);
        assert_eq!(adjacent_ranges("4..=6 => XBOX_VER,").len(), 1);
        assert_eq!(adjacent_ranges("dt @ 4..=6 => xbox::rdesc(dt),").len(), 1);
        assert!(adjacent_ranges("r[12..16].copy_from_slice(&s);").is_empty());
        assert!(adjacent_ranges("Some(len) => &report[..len],").is_empty());
    }

    /// The HID descriptor names the report descriptor's own length, so hidclass reads it whole.
    #[test]
    fn hid_descriptor_declares_the_report_descriptor_length() {
        for kind in &PAD_KINDS {
            let d = kind.hid_descriptor();
            assert_eq!(
                usize::from(u16::from_le_bytes([d[7], d[8]])),
                kind.rdesc.len()
            );
            assert_eq!(u16::from_le_bytes([d[2], d[3]]), kind.bcd_hid);
        }
        let ds = served_kind(DEVTYPE_DUALSENSE).hid_descriptor();
        assert_eq!(ds, [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x11, 0x01]);
    }

    /// Each row's input shape is what its descriptor declares, and every kind the host proves
    /// over feature `0x85` declares that report. Without it the pad serves neutral forever.
    #[test]
    fn rows_agree_with_their_descriptors() {
        for kind in &PAD_KINDS {
            let lens = report_lens(kind.rdesc);
            let unnumbered = matches!(kind.family, Family::SteamDeck | Family::SteamController);
            let declared = if unnumbered {
                lens.get(&(INPUT, 0)).map(|n| n - 1)
            } else {
                lens.get(&(INPUT, kind.input_id)).copied()
            };
            assert_eq!(declared, Some(kind.input_len), "devtype {}", kind.devtype);
            assert_eq!(kind.neutral[0], kind.input_id, "devtype {}", kind.devtype);
            let by_command = matches!(
                kind.family,
                Family::SteamDeck | Family::SteamController | Family::Triton
            );
            assert!(
                by_command || lens.contains_key(&(FEATURE, 0x85)),
                "devtype {} declares no proof feature",
                kind.devtype
            );
        }
        for &dt in &[DEVTYPE_XBOX, DEVTYPE_XBOX_ONE_S, DEVTYPE_XBOX_ELITE] {
            let kind = served_kind(dt);
            assert_eq!(kind.rdesc, xbox::rdesc(dt));
            assert_eq!(kind.input_len, xbox::input_len(dt));
        }
        for &dt in &eightbitdo::DEVTYPES {
            assert_eq!(served_kind(dt).rdesc, eightbitdo::rdesc(dt, true));
        }
        for kind in &PAD_KINDS {
            let dt = kind.devtype;
            assert_eq!(kind.family == Family::Switch, switch::is_switch(dt), "{dt}");
            if kind.family == Family::Switch {
                assert_eq!(kind.neutral, switch::neutral_report(dt));
            }
        }
    }

    /// `(install section, hardware id)` for every model line of `pf_gamepad.inx`, the id
    /// lowercase with any `root\` prefix dropped.
    fn inf_model_ids() -> Vec<(String, String)> {
        let inf = include_str!("../../../../packaging/windows/drivers/pf-gamepad/pf_gamepad.inx");
        let ids: Vec<(String, String)> = inf
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with(';'))
            .filter_map(|l| l.split_once('=')?.1.split_once(','))
            .filter(|(section, _)| section.trim().starts_with("pfGamepad"))
            .flat_map(|(section, ids)| {
                ids.split(',').map(move |id| {
                    let id = id.trim().to_ascii_lowercase();
                    let id = id.strip_prefix("root\\").unwrap_or(&id).to_owned();
                    (section.trim().to_owned(), id)
                })
            })
            .collect();
        assert!(
            ids.len() >= PAD_KINDS.len(),
            "parsed {} model ids out of pf_gamepad.inx; the [Models] shape changed",
            ids.len()
        );
        ids
    }

    /// Every kind binds: its devtype has a hardware-id token and every token a model line, or
    /// PnP binds inbox `input.inf` and the pad never starts. Every model line names a kind, or
    /// the driver refuses the devnode it binds.
    #[test]
    fn every_kind_has_an_inf_model_line() {
        let inf = inf_model_ids();
        for kind in &PAD_KINDS {
            assert!(
                HWID_DEVTYPES.iter().any(|&(_, dt)| dt == kind.devtype),
                "devtype {} has no token in HWID_DEVTYPES",
                kind.devtype
            );
        }
        for (token, devtype) in HWID_DEVTYPES {
            assert!(
                pad_kind(devtype).is_some(),
                "{token} names devtype {devtype}, no row"
            );
            assert!(
                inf.iter().any(|(_, id)| id == token),
                "{token} has no model line in pf_gamepad.inx"
            );
        }
        for (section, id) in &inf {
            assert!(
                HWID_DEVTYPES.iter().any(|(token, _)| token == id),
                "pf_gamepad.inx binds {id} ({section}), which names no pad kind"
            );
        }
    }

    /// FNV-1a over every field of every row.
    fn table_hash() -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
            // A separator, so moving a byte between fields changes the hash.
            h = (h ^ 0xFF).wrapping_mul(0x0100_0000_01b3);
        };
        for k in &PAD_KINDS {
            eat(&[k.devtype, k.family as u8, k.clock as u8, k.input_id]);
            eat(&k.vid.to_le_bytes());
            eat(&k.pid.to_le_bytes());
            eat(&k.version.to_le_bytes());
            eat(&k.bcd_hid.to_le_bytes());
            eat(k.rdesc);
            eat(&(k.input_len as u64).to_le_bytes());
            eat(&k.neutral);
            eat(k.manufacturer.as_bytes());
            eat(k.product.as_bytes());
            eat(&k.period_us.to_le_bytes());
        }
        h
    }

    /// The driver revision the table was last pinned at, and the table's hash then. A table
    /// change is a driver change the host must be able to see: bump `GAMEPAD_DRIVER_REV`, then
    /// pin the new pair here.
    const PINNED: (u32, u64) = (6, 0x782a_9d97_2df1_04a2);

    #[test]
    fn a_table_change_bumps_the_driver_revision() {
        let now = (crate::gamepad::GAMEPAD_DRIVER_REV, table_hash());
        assert_eq!(
            now, PINNED,
            "the pad table or GAMEPAD_DRIVER_REV moved: bump the revision for a table change, \
             then pin this pair"
        );
    }
}
