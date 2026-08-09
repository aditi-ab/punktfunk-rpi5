//! Virtual Xbox pads on Windows via the UMDF HID minidriver — Xbox Wireless (device-type 4),
//! Xbox One S (5) and Xbox Elite Series 2 (6), the HID-visible alternative to
//! [`super::gamepad_windows`]'s XUSB companion.
//!
//! **Why this exists.** `pf-xusb` registers only `GUID_DEVINTERFACE_XUSB` and exposes no HID
//! collection, so Steam's hidapi enumeration, DirectInput, `joy.cpl` and WGI/GameInput cannot see
//! the pad at all — only classic `XInputGetState` via xinput1_4's interface walk ever does. A field
//! report (2026-08-09) spent two weeks on a dead controller for exactly that reason; switching the
//! client to DualSense — a real HID pad through this very driver — fixed it in seconds. This
//! backend gives the Xbox pad the same footing, reusing the driver, sealed channel, INF, signing
//! and install path the PlayStation pads already ship on.
//!
//! Transport is identical to the PS/Deck pads: a `SwDeviceCreate` devnode plus the sealed
//! shared-memory channel, with the identity's `device_type` stamped before the magic so the driver
//! resolves it before hidclass asks for descriptors. The codec is
//! [`super::xbox_proto`]; the report it writes mirrors the driver's `XBOX_RDESC` byte for byte —
//! **one descriptor, all three identities** (see `WinXboxIdentity` below).
//!
//! ⚠️ **Every synthesized USB identity here is a BLUETOOTH Xbox pad on purpose.** The wired ids the
//! rest of the tree uses (`045E:028E` X-Box 360, `045E:02EA` Xbox One S USB) are vendor-class
//! XUSB/GIP devices that expose no HID interface on real hardware — a HID child claiming one is a
//! device that has never existed, and Windows' inbox promotion would have nothing to match. The
//! Bluetooth ids (`0B13` / `02FD` / `0B22`) are the Xbox pads that genuinely ARE HID.
//!
//! ⚠️ **No rich plane.** An Xbox pad has no touchpad, no lightbar, no adaptive triggers and no
//! IMU in its HID contract, so `apply_rich` / `clear_rich` / `neutralize_gyro` are deliberately
//! no-ops — same shape as the Linux xpad backend. Motion sent toward this backend is decoded and
//! dropped, which is what `GamepadPref::motion_reaches` already tells clients.

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

/// One of the Xbox identities this backend can present. Mirrors `WinDsIdentity`
/// (`super::dualsense_windows`) for the PlayStation family: the whole transport (section layout,
/// report codec, output parse, INF install section) is shared, and only the PnP identity plus the
/// `device_type` stamp differ.
///
/// ⭐ **All three share ONE report descriptor in the driver** — `XBOX_RDESC`. In HID terms they are
/// the same pad: same stick pairs, same trigger pair, same hat, same 15 buttons, same rumble output
/// report. A descriptor is the report SHAPE; the identity is what SDL/Steam/Windows key their stock
/// mappings off, and that travels in the VID/PID below. See the `XBOX_RDESC` provenance block in
/// `packaging/windows/drivers/pf-gamepad/src/lib.rs` for why inventing two more hand-written
/// descriptors would be a net loss.
pub(super) struct WinXboxIdentity {
    /// `device_type` stamped into the section — the driver picks its VID/PID and product string
    /// off it, before hidclass asks anything.
    pub devtype: u8,
    /// PnP instance-id prefix — distinct namespaces per identity, so two Xbox models never reuse
    /// the same devnode shell.
    pub instance_prefix: &'static str,
    /// The INF-matched hardware id. Must be one `pf_gamepad.inx` declares, on a model line that
    /// installs `pfGamepadXbox` — a package rename must never touch it
    /// (`dualsense_windows::tests::hwid_matches_inf` and
    /// `only_the_xbox_identity_installs_the_xinputhid_section` enforce both halves).
    pub hwid: &'static str,
    /// The USB VID&PID token synthesized onto the devnode so hidclass derives the real-pad HID
    /// child ids (`HID\VID_045E&PID_xxxx`) — the identity SDL/RawInput/WGI read, and the one
    /// Windows' own Xbox INFs match when they decide whether to promote a HID gamepad.
    pub usb_vid_pid: &'static str,
    /// Device Manager description.
    pub description: &'static str,
}

impl WinXboxIdentity {
    /// Xbox Wireless Controller (Series X|S) over Bluetooth, `045E:0B13` — the default.
    ///
    /// ⭐ Its PID is on Microsoft's `xinputhid.inf` allow-list twice (measured on `.173`,
    /// 2026-08-09). That is not what promotes OUR pad — a software devnode matches no allow-list
    /// entry, so `pfGamepadXbox`'s `AddReg` writes the two registry values those sections would
    /// have written — but it is why this identity, not one of the two below, stays the default.
    pub(super) const fn wireless() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX,
            instance_prefix: "pf_xbox",
            hwid: "pf_xboxwireless",
            usb_vid_pid: "VID_045E&PID_0B13",
            description: "Punktfunk Virtual Xbox Wireless Controller",
        }
    }

    /// Xbox One S controller over Bluetooth, `045E:02FD`.
    ///
    /// ⚠️ `02FD` appears in `xinputhid.inf` only as a `BTHENUM` (classic-BT bus) id — it has **no**
    /// stage-2 `HID\…&IG_00` model line, unlike `0B13`. Promotion here rides entirely on our own
    /// `AddReg`, so it should behave identically; but if a servicing update ever makes promotion
    /// depend on Microsoft's list again, this is the identity that loses it first. UNVERIFIED on
    /// glass.
    pub(super) const fn one_s() -> WinXboxIdentity {
        WinXboxIdentity {
            devtype: pf_driver_proto::gamepad::DEVTYPE_XBOX_ONE_S,
            instance_prefix: "pf_xbox_ones",
            hwid: "pf_xboxones",
            usb_vid_pid: "VID_045E&PID_02FD",
            description: "Punktfunk Virtual Xbox One S Controller",
        }
    }

    /// Xbox Elite Wireless Controller Series 2, `045E:0B22` — the pad
    /// `tools/hid-descriptor-dump` captured on `.173`, so the one identity here whose real hardware
    /// has been measured directly.
    ///
    /// ⚠️ **No paddles yet.** `BTN_PADDLE1..4` still fold/drop for this identity exactly as for the
    /// other two; the Elite is merely the first Xbox pad that *could* carry them natively. Adding
    /// them is blocked on a measurement, not on effort — once `xinputhid` promotes the pad it
    /// claims the HID collection exclusively, so extra buttons declared in the descriptor may be
    /// invisible to every consumer anyway (handoff §3.6). Measure before building.
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

/// Every Xbox identity this backend can build, in wire order (`device_type` 4, 5, 6).
///
/// A table rather than three loose constructors because the INF tests sweep it: each entry's
/// `hwid` must appear in `pf_gamepad.inx` **on a `pfGamepadXbox` model line**, and every non-Xbox
/// model line must NOT be on that section. A new identity added here without its INF line fails
/// those tests instead of failing on a user's box.
///
/// `static`, not `const`, on purpose: [`XboxWinProto`] holds a `&'static WinXboxIdentity`, and a
/// `const` is inlined at each use site — `&CONST[i]` would depend on rvalue static promotion to
/// come out `'static` at all.
pub(super) static XBOX_IDENTITIES: [WinXboxIdentity; 3] = [
    WinXboxIdentity::wireless(),
    WinXboxIdentity::one_s(),
    WinXboxIdentity::elite(),
];

/// A single virtual Xbox pad: the `SwDeviceCreate`'d `pf_xbox_<index>` devnode plus the sealed
/// shared-memory channel. Dropping it removes the devnode and closes both sections.
pub struct XboxWinPad {
    /// Per-session devnode from SwDeviceCreate, when it succeeds (RAII — `SwDeviceClose` on drop).
    _sw: Option<super::gamepad_raii::SwDevice>,
    /// The sealed channel: unnamed DATA section (`PadShm`) + bootstrap mailbox + handle delivery.
    channel: PadChannel,
    /// Watches the section's `driver_proto` field and logs attach / never-attached diagnosis.
    attach: super::gamepad_raii::DriverAttach,
    /// This pad's v2.3 input-seqlock generation — see `publish_input`.
    input_gen: u32,
    /// Output-plane cursor: ring drain (v2.1+ driver) or legacy latest-slot seq (old driver).
    drain: OutputDrain,
}

impl XboxWinPad {
    /// Create the sealed channel, stamp `device_type` FIRST + the pad index + the neutral report +
    /// the magic LAST, then spawn the devnode under `id`'s Bluetooth Xbox identity.
    fn open(index: u8, id: &WinXboxIdentity) -> Result<XboxWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range. The
        // device_type MUST land before the magic — the driver reads it the moment it attaches, and
        // a late stamp enumerates the pad with the default DualSense identity (the Deck's bug).
        unsafe {
            *base.add(OFF_DEVTYPE) = id.devtype;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // Ring capability `2` = "this host drains the v2.2 long ring" (see the DualSense open).
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
            // Per-FAMILY tag, like "PFDS" for the whole PlayStation family: the three Xbox
            // identities share it because only one of them can ever hold a given pad index (the
            // router keeps a live device in its owning manager), so their containers never collide.
            container_tag: 0x5046_5842, // "PFXB"
            container_index: index,
            hwid: id.hwid,
            usb_vid_pid: id.usb_vid_pid,
            // A Bluetooth pad is not a USB composite device, so there is no interface number to
            // synthesize — unlike the Deck, whose Steam promotion gate needs `&MI_02`.
            usb_mi: None,
            description: id.description,
        })?; // Propagate — swallowing latched the slot to a pad with no devnode (see the DS4 twin).
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = Some(super::gamepad_raii::SwDevice::new(hsw));
        // Bounded eager delivery — the driver must read the `device_type` stamp before hidclass
        // asks it for descriptors, or the pad enumerates as a DualSense.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(XboxWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                id.hwid,
                "pf_gamepad.inf", // one driver package serves every identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    /// Serialize `st` and publish it to the section's input slot under the v2.3 seqlock, so a
    /// driver read can never land mid-copy.
    fn write_state(&mut self, st: &XboxState) {
        let r = serialize_xbox_state(st);
        // SAFETY: `data_base()` points at a live SHM_SIZE-byte section and `r` is the codec's
        // fixed-size report.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    /// Poll the section's output slot for a game's rumble, tick the sealed-channel delivery and
    /// feed the driver-attach health watcher.
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
                rumble = Some(r); // oldest → newest: the last rumble-carrying report wins
            }
        });
        (rumble, resync)
    }
}

/// Parse an Xbox output report into `(low, high, left_trigger, right_trigger)` motor levels on the
/// wire's 0..65535 scale.
///
/// The Bluetooth Xbox rumble report is id `0x03`: `[id, enable, left_trigger, right_trigger,
/// left, right, duration, delay, loop]`, with magnitudes on a **0..100** scale (not 0..255 — a
/// detail that silently costs 60 % of the rumble range if you assume otherwise). The `enable`
/// mask picks which motors the values apply to; bit 2 is the left (low-frequency) motor and bit 3
/// the right (high-frequency) one, matching how the wire's `low`/`high` pair is used elsewhere.
///
/// Bytes 2/3 are the two impulse-trigger motors, which ride the 0xCA plane's v3 tail
/// (design/trigger-rumble-plane.md). This pad is the only backend in the tree that can ever source
/// them: XUSB's `SET_STATE` carries `rumble_large`/`rumble_small` and evdev's `FF_RUMBLE` carries
/// strong/weak, so neither packet has a field to lose. They are scaled by the same 0..100 closure
/// as the handles rather than a copy of it — assuming 0..255 here would read a full-scale `100` as
/// ~39 %, which on a real pad reads as "trigger rumble works but is weirdly weak", the hardest
/// class of bug to attribute.
///
/// ⚠️ Never seen a real report — this shape is from the documented protocol, not a capture.
///
/// ⚠️ **The two TRIGGER `enable` bits are conjecture, not measurement.** Bits 2/3 = left/right
/// handle are known; bit 0 = left trigger and bit 1 = right trigger are inferred from the report's
/// field order (triggers first, handles second) and from nothing else. A live capture (design WP0)
/// settles it. Getting it wrong yields "the triggers buzz when the game asked for the handles",
/// so nothing downstream may treat this assignment as established — and the tests below are
/// deliberately written with mask vectors that hold whichever bits turn out to be right.
fn parse_xbox_output(bytes: &[u8]) -> Option<(u16, u16, u16, u16)> {
    // The driver republishes output reports report-id-prefixed, like the PS backends.
    if bytes.len() < 6 || bytes[0] != 0x03 {
        return None;
    }
    let enable = bytes[1];
    let scale = |v: u8| -> u16 { (v.min(100) as u32 * 65535 / 100) as u16 };
    let gated = |bit: u8, v: u8| if enable & bit != 0 { scale(v) } else { 0 };
    Some((
        gated(0x04, bytes[4]),
        gated(0x08, bytes[5]),
        // UNVERIFIED bit assignment — see the second ⚠️ above before trusting either of these.
        gated(0x01, bytes[2]),
        gated(0x02, bytes[3]),
    ))
}

/// The Windows-Xbox half of the shared stateful manager (see [`PadProto`]). Lifecycle (slot table,
/// unplug sweep, heartbeat, rumble dedup) lives in [`UhidManager`], exactly as for the PS pads.
///
/// The identity is a field rather than three separate proto types because nothing else about the
/// backend varies: same codec, same output parse, same rumble plane. `Default` is the Xbox Wireless
/// Controller, so `XboxWindowsManager::new()` keeps its previous meaning exactly.
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
    /// The Xbox One S identity (`045E:02FD`) — `UhidManager::with_backend(XboxWinProto::one_s())`.
    pub fn one_s() -> XboxWinProto {
        XboxWinProto {
            identity: &XBOX_IDENTITIES[1],
        }
    }

    /// The Xbox Elite Series 2 identity (`045E:0B22`).
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

    /// Every control this pad has arrives in the frame, so a frame fully replaces the state —
    /// there are no rich-plane fields to preserve (contrast the Deck's trackpads/motion).
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

    /// No rich plane on an Xbox pad — see the module note.
    fn apply_rich(&self, _st: &mut XboxState, _rich: RichInput) {}

    /// No motion plane, so there is never stale gyro to neutralize.
    fn neutralize_gyro(&self, _st: &mut XboxState) -> bool {
        false
    }

    fn clear_rich(&self, _st: &mut XboxState) {}

    fn write_state(&self, pad: &mut XboxWinPad, st: &XboxState) {
        pad.write_state(st);
    }

    /// Motor rumble on the universal 0xCA plane. No rich host→client feedback (no lightbar or
    /// adaptive triggers), so `hidout` stays empty — parity with the Linux xpad backend.
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

/// All virtual Xbox pads of a Windows session, with the same method surface (via the shared
/// [`UhidManager`]) as the other Windows pad managers.
pub type XboxWindowsManager = UhidManager<XboxWinProto>;

#[cfg(test)]
mod tests {
    use super::*;

    // Every `enable` vector in this module is chosen so its assertion holds whichever bits the
    // TRIGGER actuators turn out to use — the assignment is conjecture (see the ⚠️ on
    // `parse_xbox_output`) and a test asserting it would pin a guess as if it were the contract.
    // The safe masks: `0xFF` enables everything that exists, `0x00` enables nothing, and
    // `0x0C` / `0xF3` split the two MEASURED handle bits from every other bit. No vector below
    // names a trigger enable bit.

    /// Both handle motors at full scale, with the triggers idle.
    #[test]
    fn rumble_scales_off_the_zero_to_hundred_protocol_range() {
        let full = [0x03, 0x0F, 0, 0, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&full), Some((65535, 65535, 0, 0)));
        // Half on the left motor only.
        let half = [0x03, 0x04, 0, 0, 50, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&half), Some((32767, 0, 0, 0)));
    }

    /// The trigger magnitudes are on the SAME 0..100 protocol range as the handles, so a
    /// full-scale `100` is `65535` — not `25700`, which is what reading them as 0..255 would give
    /// and which reads on a real pad as "trigger rumble works but is weirdly weak". Named for the
    /// regression so it cannot be "fixed" the wrong way later.
    #[test]
    fn trigger_magnitudes_are_not_a_zero_to_255_range() {
        let full = [0x03, 0xFF, 100, 100, 0, 0, 0, 0, 1];
        assert_eq!(parse_xbox_output(&full), Some((0, 0, 65535, 65535)));
        let half = [0x03, 0xFF, 50, 25, 0, 0, 0, 0, 1];
        assert_eq!(parse_xbox_output(&half), Some((0, 0, 32767, 16383)));
    }

    /// A value above the protocol's 0..100 range must clamp, not wrap past full scale — on all
    /// four actuators, since the triggers reuse the handles' scale closure.
    #[test]
    fn out_of_range_magnitudes_clamp() {
        let over = [0x03, 0xFF, 255, 255, 255, 255, 0, 0, 1];
        assert_eq!(parse_xbox_output(&over), Some((65535, 65535, 65535, 65535)));
    }

    /// The enable mask gates each motor independently — a report that enables nothing is a stop.
    #[test]
    fn the_enable_mask_gates_each_motor() {
        let none = [0x03, 0x00, 100, 100, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&none), Some((0, 0, 0, 0)));
        let right_only = [0x03, 0x08, 0, 0, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&right_only), Some((0, 65535, 0, 0)));
    }

    /// The case the whole trigger-rumble plane exists for, and the one nothing else in the tree
    /// can produce: a racing title driving the impulse triggers hard while the handles stay
    /// silent. `0x0C` is the two measured handle bits; `0xF3` is every OTHER bit, so this pair
    /// isolates the handles from the triggers without claiming which bits the triggers are.
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

    /// Anything that is not the rumble report — or is truncated — is ignored rather than parsed
    /// out of whatever bytes happen to be there.
    #[test]
    fn non_rumble_reports_are_ignored() {
        assert_eq!(parse_xbox_output(&[0x01, 0x0F, 0, 0, 100, 100]), None);
        assert_eq!(parse_xbox_output(&[0x03, 0x0F, 0]), None);
        assert_eq!(parse_xbox_output(&[]), None);
    }
}
