//! Virtual Sony DualSense on Windows via the UMDF minidriver (`packaging/windows/dualsense-driver`).
//!
//! The Windows analogue of the Linux UHID backend ([`super::dualsense`]): same [`DsState`] model and
//! the same byte-level report codec ([`super::dualsense_proto`]), but a different transport. Where
//! the Linux backend writes report `0x01` to `/dev/uhid` and reads report `0x02` via `UHID_OUTPUT`,
//! the Windows backend talks to the UMDF driver over a **named shared-memory section**
//! `Global\pfds-shm-<idx>` (256 B: magic `u32@0`, input report `@8`, output seq `u32@72`, output
//! report `@76`). The host creates the section (privileged → a permissive SDDL so the WUDFHost can
//! open it); the driver maps it from its timer, feeds game `READ_REPORT`s from the input bytes, and
//! publishes a game's `0x02` (rumble / lightbar / player-LEDs / adaptive triggers) into the output
//! bytes. `hidclass` gates the device stack, so this user-mode IPC is the only viable channel (a
//! UMDF driver has no control device); see `windows-dualsense-scoping.md`.
//!
//! Device lifecycle: the `root\pf_dualsense` devnode is currently created out-of-band (the dev-box
//! `devgen` for tests; the installer for fleet use). Per-session creation via `SwDeviceCreate` (so the
//! pad appears/disappears with the session, matching the Linux UHID lifecycle) is the next step —
//! see [`DsWinPad::open`].

use super::dualsense_proto::{
    parse_ds_output, serialize_state, DsFeedback, DsState, DS_INPUT_REPORT_LEN, DS_TOUCH_H,
    DS_TOUCH_W,
};
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use anyhow::{anyhow, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// Shared-section layout — must match `packaging/windows/dualsense-driver/src/lib.rs`.
const SHM_SIZE: usize = 256;
const SHM_MAGIC: u32 = 0x5046_4453; // "PFDS"
const OFF_INPUT: usize = 8;
const OFF_OUT_SEQ: usize = 72;
const OFF_OUTPUT: usize = 76;

/// A single virtual DualSense: the shared-memory section the driver maps (and, in future, the
/// `HSWDEVICE` from `SwDeviceCreate`). Dropping it unmaps + closes the section.
struct DsWinPad {
    map: HANDLE,
    view: *mut u8,
    seq: u8,
    ts: u32,
    last_out_seq: u32,
}

impl DsWinPad {
    /// Create + map the section `Global\pfds-shm-<index>` and stamp the magic so the driver accepts
    /// it. (TODO: also `SwDeviceCreate("root\\pf_dualsense")` here to spawn the devnode per session;
    /// for now the devnode is created out-of-band by the installer / dev-box `devgen`.)
    fn open(index: u8) -> Result<DsWinPad> {
        let name = HSTRING::from(format!("Global\\pfds-shm-{index}"));

        // A permissive DACL so the WUDFHost (whatever account it runs as) can open the section.
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL literal is valid; psd receives an allocated descriptor (freed by the OS
        // when the process exits — acceptable for a host-lifetime object).
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:(A;;GA;;;WD)"),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };

        // SAFETY: anonymous (pagefile-backed) section of SHM_SIZE bytes with the SDDL above.
        let map = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                SHM_SIZE as u32,
                PCWSTR(name.as_ptr()),
            )?
        };
        // SAFETY: map is a valid section handle; map the whole thing.
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, SHM_SIZE) };
        if view.Value.is_null() {
            // SAFETY: map is valid.
            unsafe {
                let _ = CloseHandle(map);
            }
            return Err(anyhow!("MapViewOfFile failed for {name}"));
        }
        let base = view.Value as *mut u8;
        // Zero the section then stamp the magic LAST (the driver only accepts it once magic is set).
        // SAFETY: base points at SHM_SIZE writable bytes.
        unsafe {
            std::ptr::write_bytes(base, 0, SHM_SIZE);
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        Ok(DsWinPad {
            map,
            view: base,
            seq: 0,
            ts: 0,
            last_out_seq: 0,
        })
    }

    /// Serialize `st` into report `0x01` and publish it to the section's input slot.
    fn write_state(&mut self, st: &DsState) {
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(1);
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.seq, self.ts);
        // SAFETY: view points at SHM_SIZE bytes; input slot is OFF_INPUT..OFF_INPUT+64.
        unsafe { std::ptr::copy_nonoverlapping(r.as_ptr(), self.view.add(OFF_INPUT), r.len()) };
    }

    /// Poll the section's output slot; parse a new `0x02` report (rumble / LEDs / triggers) into a
    /// [`DsFeedback`] for pad `pad`. Returns empty feedback if the driver hasn't published anything new.
    fn service(&mut self, pad: u8) -> DsFeedback {
        let mut fb = DsFeedback::default();
        // SAFETY: view points at SHM_SIZE bytes.
        let seq = unsafe { std::ptr::read_unaligned(self.view.add(OFF_OUT_SEQ) as *const u32) };
        if seq != self.last_out_seq {
            self.last_out_seq = seq;
            let mut out = [0u8; 64];
            // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe {
                std::ptr::copy_nonoverlapping(self.view.add(OFF_OUTPUT), out.as_mut_ptr(), 64)
            };
            parse_ds_output(pad, &out, &mut fb);
        }
        fb
    }
}

impl Drop for DsWinPad {
    fn drop(&mut self) {
        // SAFETY: view came from MapViewOfFile; map from CreateFileMappingW.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut c_void,
            });
            let _ = CloseHandle(self.map);
        }
    }
}

/// All virtual DualSense pads of a session — the Windows analogue of
/// [`DualSenseManager`](super::dualsense::DualSenseManager). Same method surface so the session input
/// thread drives either backend identically.
pub struct DualSenseWindowsManager {
    pads: Vec<Option<DsWinPad>>,
    state: Vec<DsState>,
    last_rumble: Vec<(u16, u16)>,
    last_write: Vec<Instant>,
    broken: bool,
}

impl Default for DualSenseWindowsManager {
    fn default() -> DualSenseWindowsManager {
        DualSenseWindowsManager::new()
    }
}

impl DualSenseWindowsManager {
    pub fn new() -> DualSenseWindowsManager {
        DualSenseWindowsManager {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            state: vec![DsState::neutral(); MAX_PADS],
            last_rumble: vec![(0, 0); MAX_PADS],
            last_write: vec![Instant::now(); MAX_PADS],
            broken: false,
        }
    }

    /// Handle one decoded controller event (create/destroy by mask, then merge button/stick state).
    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival (DualSense/Windows)");
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                for (i, slot) in self.pads.iter_mut().enumerate() {
                    if slot.is_some() && f.active_mask & (1 << i) == 0 {
                        tracing::info!(index = i, "controller unplugged (DualSense/Windows)");
                        *slot = None;
                        self.state[i] = DsState::neutral();
                        self.last_rumble[i] = (0, 0);
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
            RichInput::Touchpad { pad, .. } | RichInput::Motion { pad, .. } => pad as usize,
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
                t.x = ((x as u32 * (DS_TOUCH_W - 1) as u32) / u16::MAX as u32) as u16;
                t.y = ((y as u32 * (DS_TOUCH_H - 1) as u32) / u16::MAX as u32) as u16;
            }
            RichInput::Motion { gyro, accel, .. } => {
                self.state[idx].gyro = gyro;
                self.state[idx].accel = accel;
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

    /// Re-emit each live pad's current report if it's been silent for `max_gap` (the driver's timer
    /// streams whatever's in the section, so this just keeps the section fresh / future-proofs parity
    /// with the UHID backend's heartbeat).
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
        match DsWinPad::open(idx as u8) {
            Ok(p) => {
                tracing::info!(
                    index = idx,
                    "virtual DualSense created (Windows UMDF shm channel)"
                );
                self.pads[idx] = Some(p);
                self.state[idx] = DsState::neutral();
                self.last_rumble[idx] = (0, 0);
                self.last_write[idx] = Instant::now();
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual DualSense creation failed — controller input disabled");
                self.broken = true;
            }
        }
    }

    /// Service every pad: poll the section for a game's feedback. `rumble` fires `(index, low, high)`
    /// only on change (universal 0xCA plane); `hidout` fires for each rich DualSense feedback event
    /// (lightbar / player LEDs / adaptive triggers — 0xCD plane).
    pub fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16),
        mut hidout: impl FnMut(HidOutput),
    ) {
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_mut() else {
                continue;
            };
            let fb = pad.service(i as u8);
            if let Some(r) = fb.rumble {
                if self.last_rumble[i] != r {
                    self.last_rumble[i] = r;
                    rumble(i as u16, r.0, r.1);
                }
            }
            for h in fb.hidout {
                hidout(h);
            }
        }
    }
}
