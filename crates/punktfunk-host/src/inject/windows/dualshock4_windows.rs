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
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use crate::inject::pad_gate::PadGate;
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::{Duration, Instant};

/// A single virtual DualShock 4: the `SwDeviceCreate`'d `pf_ds4_<index>` devnode plus the sealed
/// shared-memory channel. Dropping it removes the devnode and closes both sections.
struct Ds4WinPad {
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
            container_index: index,
            hwid: "pf_dualshock4",
            usb_vid_pid: "VID_054C&PID_09CC",
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

/// All virtual DualShock 4 pads of a session — the Windows analogue of
/// [`DualShock4Manager`](super::dualshock4::DualShock4Manager), with the same method surface as the
/// Windows DualSense manager so the session input thread drives either backend identically.
pub struct DualShock4WindowsManager {
    pads: Vec<Option<Ds4WinPad>>,
    state: Vec<DsState>,
    last_rumble: Vec<(u16, u16)>,
    last_led: Vec<Option<(u8, u8, u8)>>,
    last_write: Vec<Instant>,
    /// Create-retry gate: a transient UMDF-channel failure backs off and retries instead of
    /// permanently disabling every pad for the session.
    gate: PadGate,
    /// Fallback policy for the Steam back grips a client may send (the DS4 has no back-button HID
    /// slot). `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop. Parity with `linux/dualshock4.rs`.
    remap: crate::inject::steam_remap::RemapConfig,
}

impl Default for DualShock4WindowsManager {
    fn default() -> DualShock4WindowsManager {
        DualShock4WindowsManager::new()
    }
}

impl DualShock4WindowsManager {
    pub fn new() -> DualShock4WindowsManager {
        DualShock4WindowsManager {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            state: vec![DsState::neutral(); MAX_PADS],
            last_rumble: vec![(0, 0); MAX_PADS],
            last_led: vec![None; MAX_PADS],
            last_write: vec![Instant::now(); MAX_PADS],
            gate: PadGate::new(),
            remap: crate::inject::steam_remap::RemapConfig::from_env(),
        }
    }

    /// Handle one decoded controller event (create/destroy by mask, then merge button/stick state).
    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival (DualShock 4/Windows)");
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                for (i, slot) in self.pads.iter_mut().enumerate() {
                    if slot.is_some() && f.active_mask & (1 << i) == 0 {
                        tracing::info!(index = i, "controller unplugged (DualShock 4/Windows)");
                        *slot = None;
                        self.state[i] = DsState::neutral();
                        self.last_rumble[i] = (0, 0);
                        self.last_led[i] = None;
                    }
                }
                if f.active_mask & (1 << idx) == 0 {
                    return;
                }
                self.ensure(idx);
                let prev = self.state[idx];
                // Steam back grips have no DS4 slot — fold them onto standard buttons per the
                // configured policy (default drop) so they aren't silently lost, exactly as
                // `linux/dualshock4.rs` does.
                let buttons =
                    crate::inject::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
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
                self.state[idx] = s;
                self.write(idx);
            }
        }
    }

    /// Apply one rich client→host event (touchpad contact / motion sample) to an existing pad.
    pub fn apply_rich(&mut self, rich: RichInput) {
        let idx = match rich {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. } => pad as usize,
        };
        if idx >= MAX_PADS || self.pads[idx].is_none() {
            return;
        }
        // The shared DualSense-family mapping (dualsense_proto::DsState::apply_rich): Steam
        // dual pads split the one touchpad left/right, pad clicks ride touch_click.
        self.state[idx].apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
        self.write(idx);
    }

    fn write(&mut self, idx: usize) {
        let st = self.state[idx];
        if let Some(pad) = self.pads[idx].as_mut() {
            pad.write_state(&st);
        }
        self.last_write[idx] = Instant::now();
    }

    /// Re-emit each live pad's current report if it's been silent for `max_gap` (parity with the
    /// other backends' heartbeat — keeps the section fresh).
    pub fn heartbeat(&mut self, max_gap: Duration) {
        let now = Instant::now();
        for i in 0..self.pads.len() {
            if self.pads[i].is_some() && now.duration_since(self.last_write[i]) >= max_gap {
                self.write(i);
            }
        }
    }

    fn ensure(&mut self, idx: usize) {
        if idx >= MAX_PADS || self.pads[idx].is_some() || !self.gate.allow(Instant::now()) {
            return;
        }
        match Ds4WinPad::open(idx as u8) {
            Ok(p) => {
                tracing::info!(
                    index = idx,
                    "virtual DualShock 4 created (Windows UMDF shm channel)"
                );
                self.pads[idx] = Some(p);
                self.state[idx] = DsState::neutral();
                self.last_rumble[idx] = (0, 0);
                self.last_led[idx] = None;
                self.last_write[idx] = Instant::now();
                self.gate.on_success();
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual DualShock 4 creation failed — retrying with backoff (install/repair: punktfunk-host.exe driver install --gamepad)");
                self.gate.on_failure(Instant::now());
            }
        }
    }

    /// Service every pad: poll the section for a game's feedback. `rumble` fires `(index, low, high)`
    /// only on change (universal 0xCA plane); `hidout` fires the lightbar (0xCD `Led`), deduped.
    pub fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16),
        mut hidout: impl FnMut(HidOutput),
    ) {
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_mut() else {
                continue;
            };
            let fb = pad.service();
            if let Some(r) = fb.rumble {
                if self.last_rumble[i] != r {
                    self.last_rumble[i] = r;
                    rumble(i as u16, r.0, r.1);
                }
            }
            if let Some(rgb) = fb.led {
                if self.last_led[i] != Some(rgb) {
                    self.last_led[i] = Some(rgb);
                    hidout(HidOutput::Led {
                        pad: i as u8,
                        r: rgb.0,
                        g: rgb.1,
                        b: rgb.2,
                    });
                }
            }
        }
    }
}
