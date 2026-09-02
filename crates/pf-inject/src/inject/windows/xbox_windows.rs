//! Virtual Xbox pads on Windows via the UMDF HID minidriver — Xbox Wireless (device-type 4),
//! Xbox One S (5) and Xbox Elite Series 2 (6). HID-visible alternative to
//! [`super::gamepad_windows`]'s XUSB companion: `pf-xusb` registers only
//! `GUID_DEVINTERFACE_XUSB`, so hidapi / DirectInput / WGI never see it.
//!
//! Transport matches the PS/Deck pads (`SwDeviceCreate` + sealed channel). Stamp
//! `device_type` before the magic so the driver resolves identity before hidclass
//! asks for descriptors. Codec: [`super::xbox_proto`]. One descriptor (`XBOX_RDESC`)
//! for all three identities — see `WinXboxIdentity`.
//!
//! Identities are Bluetooth Xbox pads on purpose. Wired ids (`045E:028E`, `045E:02EA`)
//! are vendor-class XUSB/GIP with no HID interface; a HID child claiming one has never
//! existed. Bluetooth ids (`0B13` / `02FD` / `0B22`) are the HID Xbox pads.
//!
//! No rich plane: no touchpad, lightbar, adaptive triggers, or IMU in the HID
//! contract. `apply_rich` / `clear_rich` / `neutralize_gyro` are no-ops; motion is
//! decoded and dropped (`GamepadPref::motion_reaches`).

use super::dualsense_windows::{
    create_swdevice, publish_input, OutputDrain, SwDeviceProfile, OFF_DEVTYPE, OFF_DRIVER_PROTO,
    OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::gamepad_raii::PadChannel;
use super::xbox_proto::{neutral_xbox_report, serialize_xbox_state, XboxState, XBOX_REPORT_LEN};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;
use std::time::Duration;

/// Xbox identity this backend can present. Same transport as `WinDsIdentity`
/// (`super::dualsense_windows`); only PnP identity and `device_type` differ.
/// All three share the driver's `XBOX_RDESC` — identity is VID/PID. Provenance:
/// `packaging/windows/drivers/pf-gamepad/src/lib.rs`.
pub(super) struct WinXboxIdentity {
    /// Stamped into the section; the driver picks VID/PID and product string
    /// from it before hidclass asks.
    pub devtype: u8,
    /// Distinct namespace per identity so two Xbox models never share a
    /// devnode shell.
    pub instance_prefix: &'static str,
    /// INF-matched hardware id. Must be a `pfGamepadXbox` model line in
    /// `pf_gamepad.inx` (`hwid_matches_inf` and
    /// `only_the_xbox_identity_installs_the_xinputhid_section`).
    pub hwid: &'static str,
    /// Synthesized onto the devnode so hidclass derives `HID\VID_045E&PID_xxxx`
    /// — the id SDL/RawInput/WGI and Microsoft's Xbox INFs key off.
    pub usb_vid_pid: &'static str,
    pub description: &'static str,
}

impl WinXboxIdentity {
    /// Xbox Wireless Controller (Series X|S) over Bluetooth, `045E:0B13`.
    /// Default: `0B13` is on Microsoft's `xinputhid.inf` allow-list; a software
    /// devnode still matches none of those entries, so `pfGamepadXbox`'s `AddReg`
    /// writes the two registry values the inbox sections would have written.
    pub(super) const fn wireless() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX,
            instance_prefix: "pf_xbox",
            hwid: "pf_xboxwireless",
            usb_vid_pid: "VID_045E&PID_0B13",
            description: "Punktfunk Virtual Xbox Wireless Controller",
        }
    }

    /// Xbox One S over Bluetooth, `045E:02FD`. `02FD` is only a `BTHENUM` id in
    /// `xinputhid.inf` — no stage-2 `HID\…&IG_00` line, unlike `0B13`. Promotion
    /// rides entirely on our `AddReg`. Unverified on hardware.
    pub(super) const fn one_s() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX_ONE_S,
            instance_prefix: "pf_xbox_ones",
            hwid: "pf_xboxones",
            usb_vid_pid: "VID_045E&PID_02FD",
            description: "Punktfunk Virtual Xbox One S Controller",
        }
    }

    /// Xbox Elite Wireless Controller Series 2, `045E:0B22`.
    /// No paddles: `BTN_PADDLE1..4` still fold for this identity. Once
    /// `xinputhid` promotes the pad it claims the HID collection exclusively,
    /// so extra descriptor buttons may be invisible to every consumer.
    pub(super) const fn elite() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX_ELITE,
            instance_prefix: "pf_xbox_elite",
            hwid: "pf_xboxelite",
            usb_vid_pid: "VID_045E&PID_0B22",
            description: "Punktfunk Virtual Xbox Elite Wireless Controller Series 2",
        }
    }
}

/// Xbox identities in wire order (`device_type` 4, 5, 6). INF tests sweep this
/// table: each `hwid` must sit on a `pfGamepadXbox` model line, and no
/// non-Xbox model line may. `static` not `const`: [`XboxWinProto`] holds
/// `&'static WinXboxIdentity`, and `&CONST[i]` is not `'static` without
/// rvalue static promotion.
pub(super) static XBOX_IDENTITIES: [WinXboxIdentity; 3] = [
    WinXboxIdentity::wireless(),
    WinXboxIdentity::one_s(),
    WinXboxIdentity::elite(),
];

/// Virtual Xbox pad: `SwDeviceCreate`'d `pf_xbox_<index>` plus the sealed
/// channel. Drop removes the devnode and closes both sections.
pub struct XboxWinPad {
    /// RAII: `SwDeviceClose` on drop.
    _sw: Option<super::gamepad_raii::SwDevice>,
    channel: PadChannel,
    attach: super::gamepad_raii::DriverAttach,
    /// v2.3 input-seqlock generation — see `publish_input`.
    input_gen: u32,
    /// Ring drain (v2.1+) or legacy latest-slot seq (old driver).
    drain: OutputDrain,
}

impl XboxWinPad {
    /// Stamp `device_type` first, then pad index, then the neutral report, then
    /// the magic last, and spawn the Bluetooth Xbox identity's devnode.
    fn open(index: u8, id: &WinXboxIdentity) -> Result<XboxWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: `base` is SHM_SIZE writable bytes; OFF_* offsets are in range.
        // `device_type` must land before the magic — the driver reads it on
        // attach, and a late stamp enumerates as DualSense.
        unsafe {
            *base.add(OFF_DEVTYPE) = id.devtype;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // `2` = host drains the v2.2 long ring (see DualSense open).
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(
                base.add(OFF_INPUT) as *mut [u8; XBOX_REPORT_LEN],
                neutral_xbox_report(),
            );
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("{}_{index}", id.instance_prefix);
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            // Per-family tag. The three identities share it: only one can hold
            // a given pad index, so their containers never collide.
            container_tag: 0x5046_5842, // "PFXB"
            container_index: index,
            hwid: id.hwid,
            usb_vid_pid: id.usb_vid_pid,
            // Bluetooth pad, not USB composite — no interface number. Deck
            // Steam promotion needs `&MI_02`; Xbox does not.
            usb_mi: None,
            description: id.description,
        })?; // Swallowing latched the slot to a pad with no devnode.
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = Some(super::gamepad_raii::SwDevice::new(hsw));
        // Driver must read `device_type` before hidclass asks for descriptors,
        // or the pad enumerates as DualSense. 1500 ms bounds that wait.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(XboxWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                id.hwid,
                "pf_gamepad.inf", // one package, every identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    /// Publish `st` under the v2.3 seqlock so a driver read cannot land mid-copy.
    fn write_state(&mut self, st: &XboxState) {
        let r = serialize_xbox_state(st);
        // SAFETY: `data_base()` is a live SHM_SIZE-byte section; `r` is the
        // codec's fixed-size report.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    fn service(&mut self) -> (Option<(u16, u16, u16, u16)>, bool) {
        self.channel.pump();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        };
        self.attach.observe(proto);
        let mut rumble = None;
        let base = self.channel.data_base();
        let resync = self.drain.drain(base, |bytes| {
            if let Some(r) = parse_xbox_output(bytes) {
                rumble = Some(r); // last rumble-carrying report wins
            }
        });
        (rumble, resync)
    }
}

/// Xbox output report → `(low, high, left_trigger, right_trigger)` on 0..65535.
///
/// Bluetooth rumble report id `0x03`:
/// `[id, enable, left_trigger, right_trigger, left, right, duration, delay, loop]`.
/// Magnitudes are **0..100**, not 0..255 — treating them as 0..255 silently
/// costs 60 % of the range (`100` → ~39 %).
///
/// `enable` bit 2 = left (low) motor, bit 3 = right (high). Trigger motors
/// ride the 0xCA plane's v3 tail (`design/trigger-rumble-plane.md`); this is
/// the only backend that can source them.
///
/// Shape is from the documented protocol, not a capture. Trigger `enable`
/// bits are conjecture: bits 2/3 (handles) are known; bit 0 = left trigger
/// and bit 1 = right trigger are inferred from field order only. Tests use
/// masks that hold whichever trigger bits turn out to be right.
fn parse_xbox_output(bytes: &[u8]) -> Option<(u16, u16, u16, u16)> {
    // Driver republishes output reports report-id-prefixed, like the PS backends.
    if bytes.len() < 6 || bytes[0] != 0x03 {
        return None;
    }
    let enable = bytes[1];
    let scale = |v: u8| -> u16 { (v.min(100) as u32 * 65535 / 100) as u16 };
    let gated = |bit: u8, v: u8| if enable & bit != 0 { scale(v) } else { 0 };
    Some((
        gated(0x04, bytes[4]),
        gated(0x08, bytes[5]),
        // Unverified trigger bits — see `parse_xbox_output`.
        gated(0x01, bytes[2]),
        gated(0x02, bytes[3]),
    ))
}

/// Windows-Xbox `PadProto`. Lifecycle lives in [`UhidManager`]. Identity is a
/// field, not three types: same codec, same output parse, same rumble plane.
/// `Default` is Xbox Wireless, so `XboxWindowsManager::new()` is unchanged.
pub struct XboxWinProto {
    identity: &'static WinXboxIdentity,
}

impl Default for XboxWinProto {
    fn default() -> XboxWinProto {
        XboxWinProto {
            identity: &XBOX_IDENTITIES[0],
        }
    }
}

impl XboxWinProto {
    /// `045E:02FD` — `UhidManager::with_backend(XboxWinProto::one_s())`.
    pub fn one_s() -> XboxWinProto {
        XboxWinProto {
            identity: &XBOX_IDENTITIES[1],
        }
    }

    /// `045E:0B22`.
    pub fn elite() -> XboxWinProto {
        XboxWinProto {
            identity: &XBOX_IDENTITIES[2],
        }
    }
}

impl PadProto for XboxWinProto {
    type Pad = XboxWinPad;
    type State = XboxState;
    const LABEL: &'static str = "Xbox Wireless/Windows";
    const DEVICE: &'static str = "Xbox Wireless Controller";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<XboxWinPad> {
        let p = XboxWinPad::open(idx, self.identity)?;
        tracing::info!(
            index = idx,
            identity = self.identity.usb_vid_pid,
            description = self.identity.description,
            "virtual Xbox pad created (Windows UMDF HID)"
        );
        Ok(p)
    }

    fn neutral(&self) -> XboxState {
        XboxState::default()
    }

    /// Frame fully replaces state — no rich-plane fields to preserve.
    fn merge_frame(&self, _prev: &XboxState, f: &punktfunk_core::input::GamepadFrame) -> XboxState {
        XboxState::from_gamepad(
            f.buttons,
            f.left_trigger,
            f.right_trigger,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
        )
    }

    /// No rich plane on an Xbox pad.
    fn apply_rich(&self, _st: &mut XboxState, _rich: RichInput) {}

    /// No motion plane, so never stale gyro.
    fn neutralize_gyro(&self, _st: &mut XboxState) -> bool {
        false
    }

    fn clear_rich(&self, _st: &mut XboxState) {}

    fn write_state(&self, pad: &mut XboxWinPad, st: &XboxState) {
        pad.write_state(st);
    }

    /// Motor rumble on 0xCA. `hidout` stays empty — no lightbar or adaptive
    /// triggers. Parity with Linux xpad.
    fn service(&self, pad: &mut XboxWinPad, _idx: u8) -> PadFeedback {
        let (rumble, resync) = pad.service();
        PadFeedback {
            rumble,
            hidout: Vec::new(),
            rumble_drove: Some(rumble.is_some()),
            resync,
        }
    }
}

/// Session table of virtual Xbox pads, same surface as the other Windows pad
/// managers via [`UhidManager`].
pub type XboxWindowsManager = UhidManager<XboxWinProto>;

#[cfg(test)]
mod tests {
    use super::*;

    // Enable masks here must not pin conjectured trigger bits (see
    // `parse_xbox_output`). `0xFF` / `0x00` enable all / none; `0x0C` / `0xF3`
    // split the measured handle bits from every other bit.

    #[test]
    fn rumble_scales_off_the_zero_to_hundred_protocol_range() {
        let full = [0x03, 0x0F, 0, 0, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&full), Some((65535, 65535, 0, 0)));
        let half = [0x03, 0x04, 0, 0, 50, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&half), Some((32767, 0, 0, 0)));
    }

    /// Trigger magnitudes share the handles' 0..100 range. A full-scale `100`
    /// is `65535`, not `25700` (the 0..255 misread).
    #[test]
    fn trigger_magnitudes_are_not_a_zero_to_255_range() {
        let full = [0x03, 0xFF, 100, 100, 0, 0, 0, 0, 1];
        assert_eq!(parse_xbox_output(&full), Some((0, 0, 65535, 65535)));
        let half = [0x03, 0xFF, 50, 25, 0, 0, 0, 0, 1];
        assert_eq!(parse_xbox_output(&half), Some((0, 0, 32767, 16383)));
    }

    /// Values above 0..100 clamp, not wrap — all four actuators share the
    /// scale closure.
    #[test]
    fn out_of_range_magnitudes_clamp() {
        let over = [0x03, 0xFF, 255, 255, 255, 255, 0, 0, 1];
        assert_eq!(parse_xbox_output(&over), Some((65535, 65535, 65535, 65535)));
    }

    #[test]
    fn the_enable_mask_gates_each_motor() {
        let none = [0x03, 0x00, 100, 100, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&none), Some((0, 0, 0, 0)));
        let right_only = [0x03, 0x08, 0, 0, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&right_only), Some((0, 65535, 0, 0)));
    }

    /// `0x0C` is the two measured handle bits; `0xF3` is every other bit.
    /// Isolates handles from triggers without naming trigger enable bits.
    #[test]
    fn a_trigger_only_report_leaves_the_handles_silent() {
        let triggers_only = [0x03, 0xF3, 100, 40, 100, 100, 0, 0, 1];
        assert_eq!(
            parse_xbox_output(&triggers_only),
            Some((0, 0, 65535, 26214))
        );
        let handles_only = [0x03, 0x0C, 100, 100, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&handles_only), Some((65535, 65535, 0, 0)));
    }

    #[test]
    fn non_rumble_reports_are_ignored() {
        assert_eq!(parse_xbox_output(&[0x01, 0x0F, 0, 0, 100, 100]), None);
        assert_eq!(parse_xbox_output(&[0x03, 0x0F, 0]), None);
        assert_eq!(parse_xbox_output(&[]), None);
    }
}
