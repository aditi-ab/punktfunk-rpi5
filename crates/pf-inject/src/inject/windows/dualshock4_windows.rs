//! Virtual Sony DualShock 4 on Windows via the UMDF minidriver — the PS4 sibling of
//! [`super::dualsense_windows`]. Same transport (a per-session `SwDeviceCreate` devnode + the sealed
//! shared-memory channel bootstrapped via `Global\pfds-boot-<idx>`), same controller model
//! ([`DsState`]); only the PnP identity (`VID_054C&PID_09CC`, hardware id `pf_dualshock4`) and the
//! report codec ([`super::dualshock4_proto`]) differ. The host stamps `device_type = 1` (DualShock 4)
//! into the DATA section so the one UMDF driver serves the DS4 descriptor / attributes / features
//! instead of the DualSense ones. Feedback is motor rumble (universal 0xCA plane) + the lightbar
//! (0xCD `Led`); a DS4 has no adaptive triggers / player LEDs.

use super::dualsense_proto::DsState;
use super::dualsense_windows::{
    create_swdevice, OutputDrain, SwDeviceProfile, DEVTYPE_DUALSHOCK4, OFF_DEVTYPE,
    OFF_DRIVER_PROTO, OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::dualshock4_proto::{
    parse_ds4_output, serialize_state, Ds4Feedback, DS4_INPUT_REPORT_LEN, DS4_TOUCH_H, DS4_TOUCH_W,
};
use super::gamepad_raii::PadChannel;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::Duration;

/// The hardware id this pad's devnode carries. Must be one `pf_gamepad.inx` declares — a package
/// rename must never touch it (`dualsense_windows::tests::hwid_matches_inf` enforces that).
pub(super) const DS4_HWID: &str = "pf_dualshock4";

/// A single virtual DualShock 4: the `SwDeviceCreate`'d `pf_ds4_<index>` devnode plus the sealed
/// shared-memory channel. Dropping it removes the devnode and closes both sections.
/// `pub`: the type appears as `type Pad` in the `PadProto` impl (a public trait), like the
/// Linux pads.
pub struct Ds4WinPad {
    /// Per-session devnode from SwDeviceCreate, when it succeeds (RAII — `SwDeviceClose` on drop).
    _sw: Option<super::gamepad_raii::SwDevice>,
    /// The sealed channel: unnamed DATA section (`PadShm`) + bootstrap mailbox + handle delivery.
    channel: PadChannel,
    /// Watches the section's `driver_proto` field and logs attach / never-attached diagnosis.
    attach: super::gamepad_raii::DriverAttach,
    counter: u8,
    ts: u16,
    /// Output-plane cursors: ring drain (v2.1 driver) or legacy latest-slot seq (old driver).
    drain: OutputDrain,
}

impl Ds4WinPad {
    /// Create the sealed channel, stamp `device_type = DualShock 4` + the pad index + a neutral
    /// report + the magic LAST, then spawn the `pf_ds4_<index>` devnode (the driver loads on it and
    /// receives the DATA handle over the bootstrap).
    fn open(index: u8) -> Result<Ds4WinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // device-type FIRST (so it's visible the moment magic is), pad index, neutral report,
        // magic LAST.
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = DEVTYPE_DUALSHOCK4;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // Ring capability (v2.1), stamped before the magic so the driver sees it on attach.
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 1);
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS4_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS4_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_ds4_{index}");
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: DS4_HWID,
            usb_vid_pid: "VID_054C&PID_09CC",
            usb_mi: None,
            description: "punktfunk Virtual DualShock 4",
        })?; // Propagate, do NOT swallow — see below.
        let (hsw, instance_id) = (Some(hsw), instance_id);
        // Swallowing a create failure here (the previous behaviour) latched the pad slot to
        // `Some(pad)` with no live devnode: `PadSlots::ensure` short-circuits on `is_some()` and
        // `gate.on_success()` cleared the backoff, so the create-gate that exists precisely to
        // self-heal a transient PnP failure never retried. The game saw no controller for the whole
        // session unless the client unplugged the pad. Matches the XUSB sibling, which propagates.
        // The DATA section goes to whoever THIS devnode says is serving it — not to whatever pid
        // the LocalService-writable mailbox names (security-review 2026-07-28).
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // Bounded eager delivery — for the DS4 this is what closes the identity race: the driver
        // must read `device_type = 1` from the delivered DATA section before hidclass asks it for
        // descriptors, or the pad would enumerate with the (default) DualSense identity.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(Ds4WinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                "pf_dualshock4",
                "pf_gamepad.inf", // one driver package serves both HID identities
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            counter: 0,
            ts: 0,
            drain: OutputDrain::new(),
        })
    }

    /// Serialize `st` into report `0x01` and publish it to the section's input slot.
    fn write_state(&mut self, st: &DsState) {
        self.counter = self.counter.wrapping_add(1);
        self.ts = self.ts.wrapping_add(188); // ~1ms in the DS4's 5.33µs sensor-clock units
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.counter, self.ts);
        // SAFETY: base points at SHM_SIZE bytes; input slot is OFF_INPUT..OFF_INPUT+64.
        unsafe {
            std::ptr::copy_nonoverlapping(
                r.as_ptr(),
                self.channel.data_base().add(OFF_INPUT),
                r.len(),
            )
        };
    }

    /// Drain the section's output plane; parse every new `0x05` report (rumble / lightbar) into a
    /// [`Ds4Feedback`], oldest → newest — so a stop-then-LED burst yields the stop AND the LED
    /// state, never just the latest report. Returns empty feedback if the driver hasn't published
    /// anything new. Also ticks the sealed-channel delivery and feeds the driver-attach health
    /// watcher (the driver's ~125 Hz timer stamps `driver_proto`).
    fn service(&mut self) -> Ds4Feedback {
        self.channel.pump();
        let mut fb = Ds4Feedback::default();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        };
        self.attach.observe(proto);
        let base = self.channel.data_base();
        fb.resync = self
            .drain
            .drain(base, |bytes| parse_ds4_output(bytes, &mut fb));
        fb
    }
}

/// The Windows-DualShock-4 half of the shared stateful manager (see [`PadProto`]): the UMDF
/// sealed-channel open (device-type 1), the same [`DsState`] mappers as `linux/dualshock4.rs`, and
/// the section feedback poll. Lifecycle (slot table, unplug sweep, heartbeat, dedup) lives in
/// [`UhidManager`]; the lightbar dedup that used to be a bespoke `last_led` vec now rides the
/// shared `HidoutDedup` (identical semantics — `Led` is compared against the last-forwarded value
/// and re-armed on create/unplug).
pub struct Ds4WinProto {
    /// Fallback policy for the Steam back grips a client may send (the DS4 has no back-button HID
    /// slot). `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop. Parity with `linux/dualshock4.rs`.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for Ds4WinProto {
    fn default() -> Ds4WinProto {
        Ds4WinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for Ds4WinProto {
    type Pad = Ds4WinPad;
    type State = DsState;
    const LABEL: &'static str = "DualShock 4/Windows";
    const DEVICE: &'static str = "DualShock 4";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<Ds4WinPad> {
        let p = Ds4WinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual DualShock 4 created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Merge buttons/sticks/triggers from the frame, preserving touch + motion + pad clicks (rich-
    /// plane fields that must survive a button-only frame) — exactly as `linux/dualshock4.rs` does.
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        // Steam back grips have no DS4 slot — fold them onto standard buttons per the configured
        // policy (default drop) so they aren't silently lost.
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        let mut s = DsState::from_gamepad(
            buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.touch = prev.touch;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.touch_click = prev.touch_click;
        s
    }

    /// The shared DualSense-family mapping (dualsense_proto::DsState::apply_rich): Steam dual pads
    /// split the one touchpad left/right, pad clicks ride touch_click.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
    }

    fn write_state(&self, pad: &mut Ds4WinPad, st: &DsState) {
        pad.write_state(st);
    }

    /// Poll the section for a game's feedback: motor rumble on the universal 0xCA plane, the
    /// lightbar as a 0xCD `Led` event (a DS4 has no player LEDs / adaptive triggers).
    fn service(&self, pad: &mut Ds4WinPad, idx: u8) -> PadFeedback {
        let fb = pad.service();
        PadFeedback {
            rumble: fb.rumble,
            hidout: fb
                .led
                .map(|(r, g, b)| HidOutput::Led { pad: idx, r, g, b })
                .into_iter()
                .collect(),
            // Rumble-plane liveness, not any-report liveness — see the DualSense backend
            // (`parse_ds4_output` gates rumble on flag0 bit0 the same way).
            rumble_drove: Some(fb.rumble.is_some()),
            resync: fb.resync,
        }
    }
}

/// All virtual DualShock 4 pads of a session — the Windows analogue of
/// [`DualShock4Manager`](super::dualshock4::DualShock4Manager), with the same method surface (via
/// the shared [`UhidManager`]) as the Windows DualSense manager so the session input thread drives
/// either backend identically.
pub type DualShock4WindowsManager = UhidManager<Ds4WinProto>;
