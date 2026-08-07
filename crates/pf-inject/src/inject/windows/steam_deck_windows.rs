//! Virtual Steam Deck controller on Windows via the UMDF minidriver — the Windows analogue of
//! the Linux UHID Deck ([`super::steam_controller`]'s `SteamProto`), sharing its whole codec
//! ([`super::steam_proto`]: the byte-exact `ID_CONTROLLER_DECK_STATE` serializer, the
//! `XInput`/rich mappers, the `0xEB` rumble parser).
//!
//! Transport = the sealed shared-memory channel + a `SwDeviceCreate` devnode (device-type 3),
//! like the PS pads — with the promotion lever the N4 spike proved: the synthesized USB
//! hardware ids carry **`&MI_02`** (the Deck's wired controller interface), which hidclass
//! mirrors into the HID child and hidapi/Steam parse as `bInterfaceNumber`. Steam Input then
//! claims the pad exactly like a physical wired Deck (`!! Steam controller device opened`,
//! XInput slot reserved — observed live on `.173`), so games get native Deck glyphs +
//! trackpads + gyro + back grips through Steam's own remapping.
//!
//! Feedback: Steam drives Deck rumble (`0xEB`) and trackpad haptic pulses (`0x8F`) via
//! SET_FEATURE on the unnumbered report; the driver republishes those into the section's
//! output slot (report-id-0 prefixed), where [`parse_steam_output`] reads the exact wire shape
//! the Linux path sees. No gamepad-mode entry pulse here — that gate lives in the Linux
//! kernel's evdev parser; Steam-on-Windows reads the raw reports directly.

use super::dualsense_windows::{
    create_swdevice, publish_input, OutputDrain, SwDeviceProfile, OFF_DEVTYPE, OFF_DRIVER_PROTO,
    OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::gamepad_raii::PadChannel;
use super::steam_proto::{
    neutral_deck_report, parse_steam_output, serialize_deck_state, SteamState, STEAM_REPORT_LEN,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;
use std::time::Duration;

/// The hardware id this pad's devnode carries. Must be one `pf_gamepad.inx` declares — a package
/// rename must never touch it (`dualsense_windows::tests::hwid_matches_inf` enforces that).
pub(super) const DECK_HWID: &str = "pf_steamdeck";

/// A single virtual Steam Deck: the `SwDeviceCreate`'d `pf_deck_<index>` devnode plus the sealed
/// shared-memory channel. Dropping it removes the devnode and closes both sections.
/// `pub`: the type appears as `type Pad` in the `PadProto` impl (a public trait).
pub struct DeckWinPad {
    /// Per-session devnode from SwDeviceCreate, when it succeeds (RAII — `SwDeviceClose` on drop).
    _sw: Option<super::gamepad_raii::SwDevice>,
    /// The sealed channel: unnamed DATA section (`PadShm`) + bootstrap mailbox + handle delivery.
    channel: PadChannel,
    /// Watches the section's `driver_proto` field and logs attach / never-attached diagnosis.
    attach: super::gamepad_raii::DriverAttach,
    seq: u32,
    /// This pad's v2.3 input-seqlock generation — see `publish_input`.
    input_gen: u32,
    /// Output-plane cursors: ring drain (v2.1 driver) or legacy latest-slot seq (old driver).
    drain: OutputDrain,
}

impl DeckWinPad {
    /// Create the sealed channel, stamp `device_type = Steam Deck` FIRST + the pad index + the
    /// neutral Deck frame + the magic LAST, then spawn the `pf_deck_<index>` devnode with the
    /// `MI_02` USB identity Steam's promotion gate requires.
    fn open(index: u8) -> Result<DeckWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = pf_driver_proto::gamepad::DEVTYPE_STEAMDECK;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // Ring capability `2` = "this host drains the v2.2 long ring", stamped before the
            // magic so the driver sees it on attach (see the DualSense open path + PadShm docs).
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(
                base.add(OFF_INPUT) as *mut [u8; STEAM_REPORT_LEN],
                neutral_deck_report(),
            );
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_deck_{index}");
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: DECK_HWID,
            usb_vid_pid: "VID_28DE&PID_1205",
            // The wired Deck controller interface — WITHOUT this the HID child carries no MI_
            // token, hidapi reports interface 0, and Steam never claims the pad (the N4
            // spike's run-1 failure).
            usb_mi: Some(2),
            description: "Punktfunk Virtual Steam Deck",
        })?; // Propagate — swallowing latched the slot to a pad with no devnode (see the DS4 twin).
        let (hsw, instance_id) = (Some(hsw), instance_id);
        // The DATA section goes to whoever THIS devnode says is serving it — not to whatever pid
        // the LocalService-writable mailbox names (security-review 2026-07-28).
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // Bounded eager delivery — the driver must read `device_type = 3` before hidclass asks
        // it for descriptors, or the pad would enumerate with the default DualSense identity.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(DeckWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                "pf_steamdeck",
                "pf_gamepad.inf", // one driver package serves every identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    /// Serialize `st` into the Deck state frame and publish it to the section's input slot.
    fn write_state(&mut self, st: &SteamState) {
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, st, self.seq);
        // This path had neither the trailing `Release` its DualSense sibling carried nor any
        // publish marker, so a driver read could land mid-copy AND the body stores had no ordering
        // against the pad's next publish. `publish_input` gives it both (v2.3 seqlock).
        // SAFETY: `data_base()` points at a live PAD_SHM_SIZE-byte section and `r` is the 64-byte
        // Deck state frame.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    /// Poll the section's output slot; parse a newly-published Steam command (`0xEB` rumble /
    /// `0x8F` haptic pulse — republished by the driver off SET_FEATURE) into feedback. Also
    /// ticks the sealed-channel delivery and the driver-attach health watcher.
    fn service(&mut self) -> (Option<(u16, u16)>, bool) {
        self.channel.pump();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        };
        self.attach.observe(proto);
        let mut rumble = None;
        let base = self.channel.data_base();
        let resync = self.drain.drain(base, |bytes| {
            // Oldest → newest: the last report that carried rumble wins (0x8F trackpad-haptic
            // reports carry none and pass through without disturbing it).
            if let Some(r) = parse_steam_output(bytes).rumble {
                rumble = Some(r);
            }
        });
        (rumble, resync)
    }
}

/// The Windows-Deck half of the shared stateful manager (see [`PadProto`]): the sealed-channel
/// open under the promoted Deck identity, the same [`SteamState`] mappers as the Linux backend,
/// and the section feedback poll. Lifecycle (slot table, unplug sweep, heartbeat, rumble dedup)
/// lives in [`UhidManager`].
#[derive(Default)]
pub struct DeckWinProto;

impl PadProto for DeckWinProto {
    type Pad = DeckWinPad;
    type State = SteamState;
    const LABEL: &'static str = "Steam Deck/Windows";
    const DEVICE: &'static str = "Steam Deck";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DeckWinPad> {
        let p = DeckWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Steam Deck created (Windows UMDF shm channel, MI_02 promoted identity)"
        );
        Ok(p)
    }

    fn neutral(&self) -> SteamState {
        SteamState::neutral()
    }

    /// Merge buttons/sticks/triggers, preserving the rich-plane fields (trackpads + motion +
    /// pad clicks arrive separately and must survive a button-only frame) — identical to the
    /// Linux `SteamProto::merge_frame`.
    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SteamState {
        use super::steam_proto::btn;
        let mut s = SteamState::from_gamepad(
            f.buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.rpad_x = prev.rpad_x;
        s.rpad_y = prev.rpad_y;
        s.lpad_x = prev.lpad_x;
        s.lpad_y = prev.lpad_y;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.buttons |= prev.buttons & (btn::RPAD_TOUCH | btn::LPAD_TOUCH);
        s.lpad_click = prev.lpad_click;
        s.rpad_click = prev.rpad_click;
        s
    }

    fn apply_rich(&self, st: &mut SteamState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn neutralize_gyro(&self, st: &mut SteamState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut SteamState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut DeckWinPad, st: &SteamState) {
        pad.write_state(st);
    }

    /// Poll the section for Steam's feedback: motor rumble on the universal 0xCA plane. The
    /// Deck has no rich host→client feedback plane (no lightbar / adaptive triggers), so
    /// `hidout` stays empty — parity with the Linux backend.
    fn service(&self, pad: &mut DeckWinPad, _idx: u8) -> PadFeedback {
        // The Deck drain surfaces `Some` exactly when a rumble-carrying report landed, so its
        // presence is the rumble-plane activity signal, even at an unchanged level.
        let (rumble, resync) = pad.service();
        PadFeedback {
            rumble,
            hidout: Vec::new(),
            rumble_drove: Some(rumble.is_some()),
            resync,
        }
    }
}

/// All virtual Steam Deck pads of a Windows session — the analogue of the Linux
/// `SteamControllerManager`, with the same method surface (via the shared [`UhidManager`]) as
/// the other Windows pad managers.
pub type SteamDeckWindowsManager = UhidManager<DeckWinProto>;
