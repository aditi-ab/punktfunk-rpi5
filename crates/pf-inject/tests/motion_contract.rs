//! The motion **unit contract**, pinned across every side that has an opinion about it.
//!
//! Gyro aim integrates angular velocity over time, so a scale error is not a cosmetic wrongness —
//! it is every rotation being the wrong size, forever. The wire carries raw `i16` LSBs in the
//! DualSense convention ([`MOTION_GYRO_LSB_PER_DEG_S`] / [`MOTION_ACCEL_LSB_PER_G`]), and each host
//! backend re-states that convention in its own dialect: the Sony pads *declare* it in a fixed
//! calibration feature report the consumer reads its scale out of, the Steam Deck and Switch Pro
//! backends *rescale* into their driver's native resolution.
//!
//! Nothing used to check that those re-statements agreed with the wire. They didn't: the DualShock
//! 4 blob declared 0.5 LSB/°·s and 8192 LSB/g against a wire delivering 20 and 10000, so every DS4
//! session read gyro 40× too fast and accel 1.22× hot — in two byte-identical copies, one of them
//! in a driver that lives in a different cargo workspace. This file is the gate that would have
//! caught it: it applies the *consumer's* arithmetic to each backend's declaration and asserts the
//! result lands back on the wire constants.
//!
//! Adding a motion-capable backend means adding it here.

#![cfg(any(target_os = "linux", target_os = "windows"))]

use pf_inject::dualsense_proto::{
    serialize_state as ds_serialize, DsState, DS_FEATURE_CALIBRATION, DS_INPUT_REPORT_LEN,
    DS_TOUCH_H, DS_TOUCH_W,
};
use pf_inject::dualshock4_proto::{
    serialize_state as ds4_serialize, DS4_FEATURE_CALIBRATION, DS4_INPUT_REPORT_LEN, DS4_TOUCH_H,
    DS4_TOUCH_W,
};
use pf_inject::steam_proto::SteamState;
use pf_inject::steam_remap::motion_wire_to_deck;
use pf_inject::switch_proto::SwitchState;
use punktfunk_core::input::gamepad::{MOTION_ACCEL_LSB_PER_G, MOTION_GYRO_LSB_PER_DEG_S};
use punktfunk_core::quic::RichInput;

/// The Sony IMU-calibration feature report, whose layout is the same for the DualSense (report
/// `0x05`) and the USB DualShock 4 (report `0x02`): report id, three signed bias words, six
/// **interleaved** per-axis `plus`/`minus` words, two gyro `speed` words, then six accel
/// `plus`/`minus` words, all little-endian `i16`.
///
/// ⚠ Interleaved is the **USB** order. A Bluetooth DualShock 4 groups the three plusses before the
/// three minuses, and consumers switch layout on the transport; our virtual pads declare `BUS_USB`,
/// so interleaved is correct here — this was checked and is not a latent bug.
#[derive(Debug, PartialEq, Eq)]
struct SonyImuCalibration {
    gyro_bias: [i16; 3],
    gyro_plus: [i16; 3],
    gyro_minus: [i16; 3],
    /// `speed_plus`, `speed_minus` — the reference rotation rate the plus/minus span was measured
    /// at. Consumers only ever use the sum.
    gyro_speed: [i16; 2],
    accel_plus: [i16; 3],
    accel_minus: [i16; 3],
}

impl SonyImuCalibration {
    fn parse(blob: &[u8], report_id: u8, who: &str) -> SonyImuCalibration {
        assert_eq!(blob.first().copied(), Some(report_id), "{who}: report id");
        assert!(
            blob.len() >= 35,
            "{who}: {} bytes, too short to carry the calibration fields (need 35)",
            blob.len()
        );
        let w = |i: usize| i16::from_le_bytes([blob[i], blob[i + 1]]);
        SonyImuCalibration {
            gyro_bias: [w(1), w(3), w(5)],
            gyro_plus: [w(7), w(11), w(15)],
            gyro_minus: [w(9), w(13), w(17)],
            gyro_speed: [w(19), w(21)],
            accel_plus: [w(23), w(27), w(31)],
            accel_minus: [w(25), w(29), w(33)],
        }
    }

    /// LSB per °/s that the **kernel** derives for axis `i`: `hid-playstation` sets
    /// `sens_numer = (speed_plus + speed_minus) * GYRO_RES_PER_DEG_S` and
    /// `sens_denom = |plus - bias| + |minus - bias|`, then reports
    /// `raw * sens_numer / sens_denom` in units of 1/`GYRO_RES_PER_DEG_S` °/s — so the
    /// `GYRO_RES_PER_DEG_S` cancels and the resolution the pad *advertises* is `denom / speed_2x`,
    /// independent of the driver's internal fixed-point scale.
    ///
    /// Returned as an integer because a fractional answer is itself a defect: no consumer can
    /// round-trip a resolution it cannot express, and the assert below is where the pre-2026-08
    /// DS4 blob (32/64 = 0.5) fails.
    fn kernel_gyro_lsb_per_deg_s(&self, i: usize, who: &str) -> i64 {
        let speed_2x = self.gyro_speed[0] as i64 + self.gyro_speed[1] as i64;
        assert!(
            speed_2x != 0,
            "{who}: gyro speed_plus + speed_minus is zero"
        );
        let denom = (self.gyro_plus[i] as i64 - self.gyro_bias[i] as i64).abs()
            + (self.gyro_minus[i] as i64 - self.gyro_bias[i] as i64).abs();
        assert_eq!(
            denom % speed_2x,
            0,
            "{who} axis {i}: declares a fractional {denom}/{speed_2x} LSB per °/s"
        );
        denom / speed_2x
    }

    /// The same number as SDL derives it (`SDL_hidapi_ps4` / `SDL_hidapi_ps5`: `plus - minus` over
    /// the speed sum, ignoring the bias). It agrees with the kernel's form only for a symmetric,
    /// zero-bias blob — and both consumers read the same virtual pad, so a blob they disagree
    /// about is a bug no matter which one is "right".
    fn sdl_gyro_lsb_per_deg_s(&self, i: usize) -> f64 {
        (self.gyro_plus[i] as f64 - self.gyro_minus[i] as f64)
            / (self.gyro_speed[0] as f64 + self.gyro_speed[1] as f64)
    }

    /// LSB per g for axis `i`: consumers take `range_2g = plus - minus` as the span of **2 g**, so
    /// one g is half of it.
    fn accel_lsb_per_g(&self, i: usize, who: &str) -> i64 {
        let range_2g = self.accel_plus[i] as i64 - self.accel_minus[i] as i64;
        assert_eq!(
            range_2g % 2,
            0,
            "{who} axis {i}: odd accel range {range_2g} has no exact 1 g"
        );
        range_2g / 2
    }

    /// The raw value a consumer treats as zero g (`plus - range_2g / 2`). Our pads pass the wire
    /// through unscaled, and the wire's zero is 0, so this must be 0 — a non-zero bias would show
    /// up as a constant phantom acceleration.
    fn accel_zero_point(&self, i: usize) -> i64 {
        let range_2g = self.accel_plus[i] as i64 - self.accel_minus[i] as i64;
        self.accel_plus[i] as i64 - range_2g / 2
    }
}

/// Every Sony-dialect backend declares exactly the wire's units, to both of its consumers.
#[test]
fn sony_calibration_blobs_declare_the_wire_units() {
    let wire_gyro = MOTION_GYRO_LSB_PER_DEG_S as i64;
    let wire_accel = MOTION_ACCEL_LSB_PER_G as i64;

    for (who, blob, report_id) in [
        ("DualSense 0x05", DS_FEATURE_CALIBRATION, 0x05u8),
        ("DualShock 4 0x02", DS4_FEATURE_CALIBRATION, 0x02u8),
    ] {
        let cal = SonyImuCalibration::parse(blob, report_id, who);
        for axis in 0..3 {
            assert_eq!(
                cal.kernel_gyro_lsb_per_deg_s(axis, who),
                wire_gyro,
                "{who} axis {axis}: gyro resolution the kernel derives"
            );
            assert_eq!(
                cal.sdl_gyro_lsb_per_deg_s(axis),
                wire_gyro as f64,
                "{who} axis {axis}: gyro resolution SDL derives"
            );
            assert_eq!(
                cal.accel_lsb_per_g(axis, who),
                wire_accel,
                "{who} axis {axis}: accel resolution"
            );
            assert_eq!(
                cal.accel_zero_point(axis),
                0,
                "{who} axis {axis}: accel zero point must be the wire's 0"
            );
        }
    }
}

/// The two rescaling backends land a wire sample on their driver's native resolution.
#[test]
fn rescaling_backends_convert_the_wire_into_their_native_units() {
    // One reference sample: 100 °/s and exactly 1 g, expressed on the wire.
    let wire_gyro = (100 * MOTION_GYRO_LSB_PER_DEG_S) as i16;
    let wire_accel = MOTION_ACCEL_LSB_PER_G as i16;

    // Steam Deck: `hid-steam` fixes STEAM_DECK_GYRO_RES_PER_DPS = 16 and ACCEL_RES_PER_G = 16384.
    let (gyro, accel) = motion_wire_to_deck([wire_gyro; 3], [wire_accel; 3]);
    assert_eq!(gyro, [100 * 16; 3], "Deck gyro: 100 °/s at 16 LSB/°·s");
    assert_eq!(accel, [16384; 3], "Deck accel: 1 g at 16384 LSB/g");

    // Switch Pro: `hid-nintendo` fixes JC_IMU_GYRO_RES_PER_DPS = 14.247 and ACCEL_RES_PER_G = 4096,
    // and consumes our report 1:1 because the factory-calibration blob we serve is the driver's own
    // identity default. 100 °/s × 14.247 = 1424.7, truncated.
    let mut st = SwitchState::neutral();
    st.apply_motion([wire_gyro; 3], [wire_accel; 3]);
    assert_eq!(st.gyro, [1424; 3], "Switch gyro: 100 °/s at 14.247 LSB/°·s");
    assert_eq!(st.accel, [4096; 3], "Switch accel: 1 g at 4096 LSB/g");
}

/// The DualSense / DualShock 4 backends hand the wire sample to the report codec **unscaled** —
/// which is only correct because their calibration blobs declare the wire's own units above. If
/// someone ever adds a rescale here, the blobs have to move with it (or vice versa).
#[test]
fn sony_backends_pass_the_wire_sample_through_unscaled() {
    let gyro = [(100 * MOTION_GYRO_LSB_PER_DEG_S) as i16, -640, 7];
    let accel = [0, 0, MOTION_ACCEL_LSB_PER_G as i16];
    let motion = RichInput::Motion {
        pad: 0,
        gyro,
        accel,
    };

    // Both Sony backends share `DsState::apply_rich`, differing only in touchpad extent.
    for (who, w, h) in [
        ("DualSense", DS_TOUCH_W, DS_TOUCH_H),
        ("DualShock 4", DS4_TOUCH_W, DS4_TOUCH_H),
    ] {
        let mut st = DsState::neutral();
        st.apply_rich(motion, w, h);
        assert_eq!(st.gyro, gyro, "{who} rescaled the wire gyro");
        assert_eq!(st.accel, accel, "{who} rescaled the wire accel");
    }
}

/// The client→report path end to end, in the units that matter: a wire Motion sample must reach
/// the HID report's motion fields as those exact little-endian values. The proto tests already pin
/// the OFFSETS; nothing pinned that the VALUE arrives unscaled — which is the half the calibration
/// blobs above are a promise about.
#[test]
fn a_wire_motion_sample_reaches_the_report_bytes_unchanged() {
    let g = (100 * MOTION_GYRO_LSB_PER_DEG_S) as i16; // 100 °/s  = 2000 = 0x07D0
    let a = MOTION_ACCEL_LSB_PER_G as i16; // 1 g      = 10000 = 0x2710
    let motion = RichInput::Motion {
        pad: 0,
        gyro: [g, -g, 0],
        accel: [0, 0, a],
    };
    let gyro_le = [0xD0, 0x07, 0x30, 0xF8, 0x00, 0x00]; // 2000, −2000, 0
    let accel_le = [0x00, 0x00, 0x00, 0x00, 0x10, 0x27]; // 0, 0, 10000

    // DualSense report 0x01: gyro at bytes 16..22, accel at 22..28.
    let mut st = DsState::neutral();
    st.apply_rich(motion, DS_TOUCH_W, DS_TOUCH_H);
    let mut r = [0u8; DS_INPUT_REPORT_LEN];
    ds_serialize(&mut r, &st, 0, 0);
    assert_eq!(&r[16..22], &gyro_le, "DualSense report gyro");
    assert_eq!(&r[22..28], &accel_le, "DualSense report accel");

    // DualShock 4 report 0x01: gyro at 13..19, accel at 19..25.
    let mut st = DsState::neutral();
    st.apply_rich(motion, DS4_TOUCH_W, DS4_TOUCH_H);
    let mut r = [0u8; DS4_INPUT_REPORT_LEN];
    ds4_serialize(&mut r, &st, 0, 0);
    assert_eq!(&r[13..19], &gyro_le, "DualShock 4 report gyro");
    assert_eq!(&r[19..25], &accel_le, "DualShock 4 report accel");
}

/// The idle-motion watchdog's semantics, which only make sense in these units: angular velocity
/// goes to zero when the feed stops, acceleration does not — a still controller still measures
/// gravity, and blanking it would read as free-fall.
#[test]
fn neutralizing_motion_keeps_gravity() {
    let mut st = DsState::neutral();
    st.gyro = [(100 * MOTION_GYRO_LSB_PER_DEG_S) as i16; 3];
    st.accel = [0, 0, MOTION_ACCEL_LSB_PER_G as i16];

    assert!(st.neutralize_gyro(), "reported no change while rotating");
    assert_eq!(st.gyro, [0; 3]);
    assert_eq!(st.accel, [0, 0, MOTION_ACCEL_LSB_PER_G as i16]);
    assert!(!st.neutralize_gyro(), "a still pad must report no change");

    let mut deck = SteamState::neutral();
    deck.gyro = [(100 * MOTION_GYRO_LSB_PER_DEG_S) as i16; 3];
    deck.accel = [0, 0, 16384];
    assert!(deck.neutralize_gyro());
    assert_eq!(deck.gyro, [0; 3]);
    assert_eq!(deck.accel, [0, 0, 16384], "Deck gravity must survive too");
}

// ---- the Windows UMDF driver's copies ----

/// `packaging/windows/drivers/pf-gamepad` is a separate WDK cargo workspace: it cannot depend on
/// pf-inject, so it carries its own copies of the calibration blobs. That is exactly the shape the
/// DS4 bug shipped in — one wrong table living in two files, where fixing one reads as fixing it.
/// Rather than trust a "keep in sync" comment, derive the units from the driver's own source.
const DRIVER_SRC: &str = include_str!("../../../packaging/windows/drivers/pf-gamepad/src/lib.rs");

/// Pull the bytes out of a `static NAME: [u8; N] = [ … ];` (or `const NAME: &[u8] = &[ … ];`)
/// literal in Rust source. Deliberately dumb: the arrays it reads are `#[rustfmt::skip]` tables of
/// `0x..` bytes, and a scan that breaks fails this test loudly rather than passing vacuously.
fn extract_byte_array(src: &str, name: &str) -> Vec<u8> {
    let decl = src
        .find(&format!("{name}:"))
        .unwrap_or_else(|| panic!("{name} not found in the driver source"));
    let eq = src[decl..]
        .find('=')
        .unwrap_or_else(|| panic!("{name}: no `=` after the declaration"))
        + decl;
    let open = src[eq..]
        .find('[')
        .unwrap_or_else(|| panic!("{name}: no `[` after the `=`"))
        + eq;
    let close = src[open..]
        .find("];")
        .unwrap_or_else(|| panic!("{name}: array literal is not closed by `];`"))
        + open;
    let bytes: Vec<u8> = src[open + 1..close]
        .lines()
        .map(|l| l.split("//").next().unwrap_or("")) // drop trailing comments
        .flat_map(|l| l.split(','))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"));
            match hex {
                Some(h) => u8::from_str_radix(h, 16),
                None => t.parse(),
            }
            .unwrap_or_else(|_| panic!("{name}: {t:?} is not a byte literal"))
        })
        .collect();
    assert!(!bytes.is_empty(), "{name}: extracted no bytes");
    bytes
}

/// The driver's blobs declare the same calibration as pf-inject's, field for field, and therefore
/// the same units. Trailing padding may differ (the two transports declare different feature
/// lengths), so this compares the parsed fields rather than raw bytes.
#[test]
fn windows_driver_blobs_match_the_canonical_ones() {
    let wire_gyro = MOTION_GYRO_LSB_PER_DEG_S as i64;
    let wire_accel = MOTION_ACCEL_LSB_PER_G as i64;

    for (who, canonical, report_id) in [
        ("DualSense 0x05", DS_FEATURE_CALIBRATION, 0x05u8),
        ("DualShock 4 0x02", DS4_FEATURE_CALIBRATION, 0x02u8),
    ] {
        let name = if report_id == 0x05 {
            "DS_FEATURE_CALIBRATION"
        } else {
            "DS4_FEATURE_CALIBRATION"
        };
        let driver_blob = extract_byte_array(DRIVER_SRC, name);
        let driver = SonyImuCalibration::parse(&driver_blob, report_id, &format!("driver {who}"));
        assert_eq!(
            driver,
            SonyImuCalibration::parse(canonical, report_id, who),
            "the UMDF driver's {name} has drifted from pf-inject's"
        );
        for axis in 0..3 {
            let d = format!("driver {who}");
            assert_eq!(driver.kernel_gyro_lsb_per_deg_s(axis, &d), wire_gyro);
            assert_eq!(driver.accel_lsb_per_g(axis, &d), wire_accel);
        }
    }
}
