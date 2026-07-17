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
    create_swdevice, SwDeviceProfile, DEVTYPE_DUALSHOCK4, OFF_DEVTYPE, OFF_DRIVER_PROTO, OFF_INPUT,
    OFF_OUTPUT, OFF_OUT_SEQ, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::dualshock4_proto::{
    parse_ds4_output, serialize_state, Ds4Feedback, DS4_INPUT_REPORT_LEN, DS4_TOUCH_H, DS4_TOUCH_W,
};
use super::gamepad_raii::PadChannel;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::Duration;

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
    last_out_seq: u32,
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
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS4_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS4_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_ds4_{index}");
        let (hsw, instance_id) = match create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: "pf_dualshock4",
            usb_vid_pid: "VID_054C&PID_09CC",
            usb_mi: None,
            description: "punktfunk Virtual DualShock 4",
        }) {
            Ok((h, id)) => (Some(h), id),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; DualShock 4 devnode unavailable");
                (None, None)
            }
        };
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
                "pf_dualsense.inf", // one driver package serves both HID identities
                "C:\\Users\\Public\\pfds-driver.log",
                boot_name,
                instance_id,
            ),
            counter: 0,
            ts: 0,
            last_out_seq: 0,
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

    /// Poll the section's output slot; parse a new `0x05` report (rumble / lightbar) into a
    /// [`Ds4Feedback`]. Returns empty feedback if the driver hasn't published anything new. Also
    /// ticks the sealed-channel delivery and feeds the driver-attach health watcher (the driver's
    /// ~125 Hz timer stamps `driver_proto`).
    fn service(&mut self) -> Ds4Feedback {
        self.channel.pump();
        let mut fb = Ds4Feedback::default();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        };
        self.attach.observe(proto);
        // SAFETY: base points at SHM_SIZE bytes.
        let seq = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_OUT_SEQ) as *const u32)
        };
        if seq != self.last_out_seq {
            self.last_out_seq = seq;
            fb.fresh = true;
            let mut out = [0u8; 64];
            // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.channel.data_base().add(OFF_OUTPUT),
                    out.as_mut_ptr(),
                    64,
                )
            };
            parse_ds4_output(&out, &mut fb);
        }
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
            game_drove: Some(fb.fresh),
        }
    }
}

/// All virtual DualShock 4 pads of a session — the Windows analogue of
/// [`DualShock4Manager`](super::dualshock4::DualShock4Manager), with the same method surface (via
/// the shared [`UhidManager`]) as the Windows DualSense manager so the session input thread drives
/// either backend identically.
pub type DualShock4WindowsManager = UhidManager<Ds4WinProto>;
