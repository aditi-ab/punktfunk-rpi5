//! Virtual Sony DualShock 4 on Windows via the UMDF minidriver — the PS4 sibling of
//! [`super::dualsense_windows`]. Same transport (a per-session `SwDeviceCreate` devnode + the
//! `Global\pfds-shm-<idx>` shared section the driver maps), same controller model ([`DsState`]); only
//! the PnP identity (`VID_054C&PID_09CC`, hardware id `pf_dualshock4`) and the report codec
//! ([`super::dualshock4_proto`]) differ. The host stamps `device_type = 1` (DualShock 4) into the
//! section so the one UMDF driver serves the DS4 descriptor / attributes / features instead of the
//! DualSense ones. Feedback is motor rumble (universal 0xCA plane) + the lightbar (0xCD `Led`); a DS4
//! has no adaptive triggers / player LEDs.

use super::dualsense_proto::DsState;
use super::dualsense_windows::{
    create_swdevice, SwDeviceProfile, DEVTYPE_DUALSHOCK4, OFF_DEVTYPE, OFF_INPUT, OFF_OUTPUT,
    OFF_OUT_SEQ, SHM_MAGIC, SHM_SIZE,
};
use super::dualshock4_proto::{
    parse_ds4_output, serialize_state, Ds4Feedback, DS4_INPUT_REPORT_LEN, DS4_TOUCH_H, DS4_TOUCH_W,
};
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::time::{Duration, Instant};
use windows::core::HSTRING;

/// A single virtual DualShock 4: the `SwDeviceCreate`'d `pf_ds4_<index>` devnode plus the mapped
/// shared section. Dropping it removes the devnode and unmaps + closes the section.
struct Ds4WinPad {
    /// Per-session devnode from SwDeviceCreate, when it succeeds (RAII — `SwDeviceClose` on drop).
    _sw: Option<super::gamepad_raii::SwDevice>,
    /// The named shared section the driver maps (RAII — unmapped + closed on drop).
    shm: super::gamepad_raii::Shm,
    counter: u8,
    ts: u16,
    last_out_seq: u32,
}

impl Ds4WinPad {
    /// Create + map the section, stamp `device_type = DualShock 4` + a neutral report + the magic,
    /// then spawn the `pf_ds4_<index>` devnode (the driver loads on it and maps the section).
    fn open(index: u8) -> Result<Ds4WinPad> {
        let shm = super::gamepad_raii::Shm::create(
            &HSTRING::from(pf_driver_proto::gamepad::pad_shm_name(index)),
            SHM_SIZE,
        )?;
        let base = shm.base();
        // device-type FIRST (so it's visible the moment magic is), neutral report, magic LAST.
        // SAFETY: base points at SHM_SIZE writable bytes; OFF_DEVTYPE/OFF_INPUT are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = DEVTYPE_DUALSHOCK4;
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS4_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS4_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_ds4_{index}");
        let hsw = match create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_index: index,
            hwid: "pf_dualshock4",
            usb_vid_pid: "VID_054C&PID_09CC",
            description: "punktfunk Virtual DualShock 4",
        }) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; DualShock 4 devnode unavailable");
                None
            }
        };
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        Ok(Ds4WinPad {
            _sw,
            shm,
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
            std::ptr::copy_nonoverlapping(r.as_ptr(), self.shm.base().add(OFF_INPUT), r.len())
        };
    }

    /// Poll the section's output slot; parse a new `0x05` report (rumble / lightbar) into a
    /// [`Ds4Feedback`]. Returns empty feedback if the driver hasn't published anything new.
    fn service(&mut self) -> Ds4Feedback {
        let mut fb = Ds4Feedback::default();
        // SAFETY: base points at SHM_SIZE bytes.
        let seq =
            unsafe { std::ptr::read_unaligned(self.shm.base().add(OFF_OUT_SEQ) as *const u32) };
        if seq != self.last_out_seq {
            self.last_out_seq = seq;
            let mut out = [0u8; 64];
            // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe {
                std::ptr::copy_nonoverlapping(self.shm.base().add(OFF_OUTPUT), out.as_mut_ptr(), 64)
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
    broken: bool,
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
            broken: false,
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
                let mut s = DsState::from_gamepad(
                    f.buttons,
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
        match rich {
            RichInput::Touchpad {
                finger,
                active,
                x,
                y,
                ..
            } => {
                let slot = (finger as usize).min(1);
                let t = &mut self.state[idx].touch[slot];
                t.active = active;
                t.id = slot as u8;
                t.x = ((x as u32 * (DS4_TOUCH_W - 1) as u32) / u16::MAX as u32) as u16;
                t.y = ((y as u32 * (DS4_TOUCH_H - 1) as u32) / u16::MAX as u32) as u16;
            }
            RichInput::Motion { gyro, accel, .. } => {
                self.state[idx].gyro = gyro;
                self.state[idx].accel = accel;
            }
            RichInput::TouchpadEx {
                surface,
                finger,
                touch,
                x,
                y,
                ..
            } => {
                // A Steam right/single pad maps onto the one DS4 touchpad (signed centre-0 →
                // 0..=65535); surface 1 (the Steam left pad) has no DS4 equivalent.
                if surface != 1 {
                    let slot = (finger as usize).min(1);
                    let n = |v: i16| ((v as i32) + 32768) as u32;
                    let t = &mut self.state[idx].touch[slot];
                    t.active = touch;
                    t.id = slot as u8;
                    t.x = (n(x) * (DS4_TOUCH_W - 1) as u32 / u16::MAX as u32) as u16;
                    t.y = (n(y) * (DS4_TOUCH_H - 1) as u32 / u16::MAX as u32) as u16;
                }
            }
        }
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
        if idx >= MAX_PADS || self.pads[idx].is_some() || self.broken {
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
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual DualShock 4 creation failed — controller input disabled");
                self.broken = true;
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
