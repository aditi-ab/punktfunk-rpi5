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
    create_swdevice, SwDeviceProfile, OFF_DEVTYPE, OFF_DRIVER_PROTO, OFF_INPUT, OFF_OUTPUT,
    OFF_OUT_SEQ, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::gamepad_raii::PadChannel;
use super::steam_proto::{
    neutral_deck_report, parse_steam_output, serialize_deck_state, SteamState, STEAM_REPORT_LEN,
};
use crate::inject::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;
use std::time::Duration;

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
    last_out_seq: u32,
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
            std::ptr::write_unaligned(
                base.add(OFF_INPUT) as *mut [u8; STEAM_REPORT_LEN],
                neutral_deck_report(),
            );
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_deck_{index}");
        let (hsw, instance_id) = match create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_index: index,
            hwid: "pf_steamdeck",
            usb_vid_pid: "VID_28DE&PID_1205",
            // The wired Deck controller interface — WITHOUT this the HID child carries no MI_
            // token, hidapi reports interface 0, and Steam never claims the pad (the N4
            // spike's run-1 failure).
            usb_mi: Some(2),
            description: "punktfunk Virtual Steam Deck",
        }) {
            Ok((h, i)) => (Some(h), i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; Steam Deck devnode unavailable");
                (None, None)
            }
        };
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // Bounded eager delivery — the driver must read `device_type = 3` before hidclass asks
        // it for descriptors, or the pad would enumerate with the default DualSense identity.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(DeckWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                "pf_steamdeck",
                "pf_dualsense.inf", // one driver package serves every identity
                "C:\\Users\\Public\\pfds-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
            last_out_seq: 0,
        })
    }

    /// Serialize `st` into the Deck state frame and publish it to the section's input slot.
    fn write_state(&mut self, st: &SteamState) {
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, st, self.seq);
        // SAFETY: base points at SHM_SIZE bytes; input slot is OFF_INPUT..OFF_INPUT+64.
        unsafe {
            std::ptr::copy_nonoverlapping(
                r.as_ptr(),
                self.channel.data_base().add(OFF_INPUT),
                r.len(),
            )
        };
    }

    /// Poll the section's output slot; parse a newly-published Steam command (`0xEB` rumble /
    /// `0x8F` haptic pulse — republished by the driver off SET_FEATURE) into feedback. Also
    /// ticks the sealed-channel delivery and the driver-attach health watcher.
    fn service(&mut self) -> Option<(u16, u16)> {
        self.channel.pump();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        };
        self.attach.observe(proto);
        // SAFETY: base points at SHM_SIZE bytes.
        let seq = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_OUT_SEQ) as *const u32)
        };
        if seq == self.last_out_seq {
            return None;
        }
        self.last_out_seq = seq;
        let mut out = [0u8; 64];
        // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.channel.data_base().add(OFF_OUTPUT),
                out.as_mut_ptr(),
                64,
            )
        };
        parse_steam_output(&out).rumble
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

    fn write_state(&self, pad: &mut DeckWinPad, st: &SteamState) {
        pad.write_state(st);
    }

    /// Poll the section for Steam's feedback: motor rumble on the universal 0xCA plane. The
    /// Deck has no rich host→client feedback plane (no lightbar / adaptive triggers), so
    /// `hidout` stays empty — parity with the Linux backend.
    fn service(&self, pad: &mut DeckWinPad, _idx: u8) -> PadFeedback {
        // The Deck poll returns `Some` exactly when a fresh output report landed (a seq bump), so
        // its presence is the game-activity signal, even when the rumble level is unchanged.
        let rumble = pad.service();
        PadFeedback {
            rumble,
            hidout: Vec::new(),
            game_drove: Some(rumble.is_some()),
        }
    }
}

/// All virtual Steam Deck pads of a Windows session — the analogue of the Linux
/// `SteamControllerManager`, with the same method surface (via the shared [`UhidManager`]) as
/// the other Windows pad managers.
pub type SteamDeckWindowsManager = UhidManager<DeckWinProto>;
