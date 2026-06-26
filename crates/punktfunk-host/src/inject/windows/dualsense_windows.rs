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
//! Device lifecycle: each pad `SwDeviceCreate`s a `pf_pad_<index>` software devnode (hardware id
//! `pf_dualsense`, enumerator `punktfunk`) on open and `SwDeviceClose`s it on drop, so the virtual
//! DualSense appears/disappears with the session — matching the Linux UHID pad. (The driver itself
//! must already be installed; the installer stages it.)

use super::dualsense_proto::{
    parse_ds_output, serialize_state, DsFeedback, DsState, DS_INPUT_REPORT_LEN, DS_TOUCH_H,
    DS_TOUCH_W,
};
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use anyhow::{anyhow, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::core::{w, GUID, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{
    SwDeviceClose, SwDeviceCreate, HSWDEVICE, SW_DEVICE_CREATE_INFO,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

/// Shared-section layout — the single source of truth is [`pf_driver_proto::gamepad::PadShm`] (offset
/// asserts pin every field; the `pf_dualsense` driver maps the same struct). Derive the size/offsets/magic
/// from it so a layout change is a compile error, not a hand-synced literal (audit §6.1). `pub(super)` so
/// the sibling DualShock 4 backend ([`super::dualshock4_windows`]) reuses the exact offsets.
pub(super) const SHM_SIZE: usize = core::mem::size_of::<pf_driver_proto::gamepad::PadShm>();
pub(super) const SHM_MAGIC: u32 = pf_driver_proto::gamepad::PAD_MAGIC; // "PFDS"
pub(super) const OFF_INPUT: usize = core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, input);
pub(super) const OFF_OUT_SEQ: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, out_seq);
pub(super) const OFF_OUTPUT: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, output);
/// Device-type selector the driver reads to choose which HID identity/descriptor it serves: 0 =
/// DualSense (the default — the section is zeroed), 1 = DualShock 4.
pub(super) const OFF_DEVTYPE: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, device_type);
pub(super) const DEVTYPE_DUALSHOCK4: u8 = pf_driver_proto::gamepad::DEVTYPE_DUALSHOCK4;

/// A single virtual DualSense: the SwDeviceCreate'd `pf_pad_<index>` software devnode (the driver
/// loads on it and the HID DualSense appears to games) plus the shared-memory section the driver maps.
/// Dropping it removes the devnode (`SwDeviceClose`) and unmaps + closes the section.
struct DsWinPad {
    /// Per-session devnode from SwDeviceCreate, when it succeeds (RAII — `SwDeviceClose` on drop).
    /// `None` falls back to an out-of-band `pf_dualsense` devnode (installer/devgen).
    _sw: Option<super::gamepad_raii::SwDevice>,
    /// The named shared section the driver maps (RAII — unmapped + closed on drop).
    shm: super::gamepad_raii::Shm,
    seq: u8,
    ts: u32,
    last_out_seq: u32,
}

/// Context for the `SwDeviceCreate` completion callback: an event to signal + the HRESULT it reports.
#[repr(C)]
struct SwCreateCtx {
    event: HANDLE,
    result: HRESULT,
}

/// `SwDeviceCreate` fires this once PnP has enumerated the device; stash the result and wake the
/// creator, which blocks on the event (so there's no concurrent access to `*ctx`).
unsafe extern "system" fn sw_create_cb(
    _dev: HSWDEVICE,
    result: HRESULT,
    ctx: *const c_void,
    _id: PCWSTR,
) {
    if !ctx.is_null() {
        // SAFETY: ctx is the &mut SwCreateCtx the creator passed; it outlives this callback.
        unsafe {
            let c = ctx as *mut SwCreateCtx;
            (*c).result = result;
            let _ = SetEvent((*c).event);
        }
    }
}

/// The PnP identity for a virtual controller devnode — varies by controller type so the same
/// [`create_swdevice`] builds a DualSense (`VID_054C&PID_0CE6`) or a DualShock 4
/// (`VID_054C&PID_09CC`). The fields map onto the `SW_DEVICE_CREATE_INFO` identity discussed below.
pub(super) struct SwDeviceProfile<'a> {
    /// PnP instance id — distinct namespaces per type (`pf_pad_<idx>` vs `pf_ds4_<idx>`) so the two
    /// never reuse the same devnode shell.
    pub instance: &'a str,
    /// Index for the deterministic per-pad ContainerId.
    pub container_index: u8,
    /// The INF-matched hardware id (`pf_dualsense` / `pf_dualshock4`), listed FIRST so the INF binds.
    pub hwid: &'a str,
    /// The USB VID&PID token (`VID_054C&PID_0CE6`) used to synthesize the USB hardware/compatible ids.
    pub usb_vid_pid: &'a str,
    /// Device description shown in Device Manager.
    pub description: &'a str,
}

/// Spawn the per-session virtual controller devnode under enumerator `punktfunk` (instance
/// `profile.instance`). The returned `HSWDEVICE` owns it — `SwDeviceClose` removes it on drop, so the
/// pad appears/disappears with the session and nothing persists.
///
/// **Game-detection identity** (see `docs/windows-dualsense-game-detection.md`). `HIDD_ATTRIBUTES`
/// alone (VID/PID via the IOCTL) satisfies SDL/HIDAPI/RawInput, but a native PS5 path (libScePad-
/// style raw HID) classifies the *connection type* by walking from the HID child to its parent
/// (`CM_Get_Parent`) and string-matching `"USB"`/`"BTHENUM"` in that parent's
/// `DEVPKEY_Device_CompatibleIds`; with no bus identity the pad reads as `UNKNOWN` and the native
/// path rejects it. So we set, via `SW_DEVICE_CREATE_INFO` (NOT `pProperties` — bus/identity info is
/// create-time-only and a `DEVPROPERTY` write of these keys is ignored):
/// - `pszzCompatibleIds` starting with a `USB\` token → the parent walk resolves `bus_type = USB`.
/// - `pszzHardwareIds` = `pf_dualsense` **first** (so the INF still binds our UMDF driver) followed
///   by `USB\VID_054C&PID_0CE6[&REV_0100]`, which makes hidclass derive the real-DualSense child
///   hardware ids `HID\VID_054C&PID_0CE6[&REV_0100]` (the set a genuine USB DS5 exposes).
/// - a deterministic, non-sentinel per-pad `pContainerId` (groups the pad's devnodes; avoids the
///   null-sentinel ContainerId that trips an `xinput1_4` slot-skip bug).
///
/// (Validated live on `.173`: the INF still binds, the child gains the `HID\VID&PID` ids, and the
/// parent walk reports USB. Remaining gap: GameInput parses VID/PID from the child *instance path*
/// `HID\punktfunk\…`, which only a real USB-bus instance path — a bus driver — would change.)
///
/// Two requirements each yield E_INVALIDARG if violated: the enumerator name must not contain `_`
/// (hence `punktfunk`, not `pf_dualsense`), and the completion callback is mandatory (the docs mark
/// `pCallback` as `[in]`, not optional — a NULL callback is rejected). The caller must be
/// Administrator (the host service runs as LocalSystem).
pub(super) fn create_swdevice(p: &SwDeviceProfile) -> Result<HSWDEVICE> {
    // Build a double-NUL-terminated UTF-16 multi-sz from a list of ids.
    let multi_sz = |ids: &[&str]| -> Vec<u16> {
        ids.iter()
            .flat_map(|s| s.encode_utf16().chain(std::iter::once(0)))
            .chain(std::iter::once(0))
            .collect()
    };
    let usb_rev = format!("USB\\{}&REV_0100", p.usb_vid_pid);
    let usb = format!("USB\\{}", p.usb_vid_pid);
    let hwids = multi_sz(&[
        p.hwid, // FIRST → the INF binds our UMDF driver on this id
        usb_rev.as_str(),
        usb.as_str(),
    ]);
    let compat = multi_sz(&[
        usb.as_str(), // a `USB\` token → native bus-type detection resolves USB
        "USB\\Class_03&SubClass_00&Prot_00",
        "USB\\Class_03",
    ]);
    let instid: Vec<u16> = p
        .instance
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let desc: Vec<u16> = p
        .description
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // The pad index, stamped into the device Location — the driver reads it to map `pfds-shm-<index>`
    // (multi-pad). The buffer outlives the SwDeviceCreate call (we wait on the event before return).
    let loc: Vec<u16> = format!("{}", p.container_index)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // Deterministic per-pad ContainerId {50464453-0000-0000-0000-0000000000<idx>} ("PFDS").
    let container = GUID::from_values(
        0x5046_4453,
        0x0000,
        0x0000,
        [0, 0, 0, 0, 0, 0, 0, p.container_index],
    );

    // SAFETY: zeroed then the fields we use are set; cbSize identifies the struct version. The id
    // buffers and `container` outlive the SwDeviceCreate call (we wait on the event before return).
    let mut info: SW_DEVICE_CREATE_INFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SW_DEVICE_CREATE_INFO>() as u32;
    info.pszInstanceId = PCWSTR(instid.as_ptr());
    info.pszzHardwareIds = PCWSTR(hwids.as_ptr());
    info.pszzCompatibleIds = PCWSTR(compat.as_ptr());
    info.pContainerId = &container;
    info.pszDeviceDescription = PCWSTR(desc.as_ptr());
    info.pszDeviceLocation = PCWSTR(loc.as_ptr());
    info.CapabilityFlags = 0x0000_000B; // DriverRequired | SilentInstall | Removable

    // SAFETY: a manual-reset, initially-unsignaled, unnamed event.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
    let mut ctx = SwCreateCtx {
        event,
        result: HRESULT(0),
    };
    // SAFETY: info + the buffers + ctx outlive the call (we wait on the event before returning);
    // windows-rs returns the HSWDEVICE (the C out-param) as the Result value.
    let hsw = match unsafe {
        SwDeviceCreate(
            w!("punktfunk"),
            w!("HTREE\\ROOT\\0"),
            &info,
            None,
            Some(sw_create_cb),
            Some(&mut ctx as *mut SwCreateCtx as *const c_void),
        )
    } {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: event is valid.
            unsafe {
                let _ = CloseHandle(event);
            }
            return Err(anyhow!("SwDeviceCreate failed: {e}"));
        }
    };
    // Block until PnP finishes enumerating (the callback signals), then check its result.
    // SAFETY: event is valid.
    unsafe {
        WaitForSingleObject(event, 10_000);
        let _ = CloseHandle(event);
    }
    if ctx.result.is_err() {
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate enumeration failed: {:?}",
            ctx.result
        ));
    }
    Ok(hsw)
}

impl DsWinPad {
    /// Create + map the section `Global\pfds-shm-<index>`, stamp the magic, then spawn the
    /// `root\pf_dualsense` devnode (the driver loads on it and maps the section). The devnode lives
    /// for the pad's lifetime — dropping the pad removes it (`SwDeviceClose`).
    fn open(index: u8) -> Result<DsWinPad> {
        let shm = super::gamepad_raii::Shm::create(
            &HSTRING::from(pf_driver_proto::gamepad::pad_shm_name(index)),
            SHM_SIZE,
        )?;
        let base = shm.base();
        // Stamp the neutral input report, then the magic LAST (the driver only accepts the section
        // once magic is set). The device-type stays 0 (DualSense — the section is already zeroed).
        // SAFETY: base points at SHM_SIZE writable bytes.
        unsafe {
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        // Spawn the per-session devnode via SwDeviceCreate; `SwDeviceClose` removes it on drop. On the
        // rare failure we keep the section + data plane and fall back to an out-of-band `pf_dualsense`
        // devnode (installer / dev-box devgen).
        let inst = format!("pf_pad_{index}");
        let hsw = match create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_index: index,
            hwid: "pf_dualsense",
            usb_vid_pid: "VID_054C&PID_0CE6",
            description: "punktfunk Virtual DualSense",
        }) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; falling back to an out-of-band pf_dualsense devnode");
                None
            }
        };
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        Ok(DsWinPad {
            _sw,
            shm,
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
        // SAFETY: base points at SHM_SIZE bytes; input slot is OFF_INPUT..OFF_INPUT+64.
        unsafe {
            std::ptr::copy_nonoverlapping(r.as_ptr(), self.shm.base().add(OFF_INPUT), r.len())
        };
    }

    /// Poll the section's output slot; parse a new `0x02` report (rumble / LEDs / triggers) into a
    /// [`DsFeedback`] for pad `pad`. Returns empty feedback if the driver hasn't published anything new.
    fn service(&mut self, pad: u8) -> DsFeedback {
        let mut fb = DsFeedback::default();
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
            parse_ds_output(pad, &out, &mut fb);
        }
        fb
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
