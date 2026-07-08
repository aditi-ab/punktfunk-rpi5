//! Virtual Sony DualSense on Windows via the UMDF minidriver (`packaging/windows/drivers/pf-dualsense`).
//!
//! The Windows analogue of the Linux UHID backend ([`super::dualsense`]): same [`DsState`] model and
//! the same byte-level report codec ([`super::dualsense_proto`]), but a different transport. Where
//! the Linux backend writes report `0x01` to `/dev/uhid` and reads report `0x02` via `UHID_OUTPUT`,
//! the Windows backend talks to the UMDF driver over an **unnamed shared DATA section** (256 B `PadShm`:
//! magic `u32@0`, input report `@8`, output seq `u32@72`, output report `@76`) reached over the
//! **sealed channel** ([`PadChannel`], `design/gamepad-channel-sealing.md`): the host duplicates the
//! section handle into the driver's WUDFHost, bootstrapped via the named `Global\pfds-boot-<idx>`
//! mailbox. The driver feeds game `READ_REPORT`s from the input bytes and publishes a game's `0x02`
//! (rumble / lightbar / player-LEDs / adaptive triggers) into the output bytes. `hidclass` gates the
//! device stack, so this user-mode IPC is the only viable channel (a UMDF driver has no control
//! device); see `windows-dualsense-scoping.md`.
//!
//! Device lifecycle: each pad `SwDeviceCreate`s a `pf_pad_<index>` software devnode (hardware id
//! `pf_dualsense`, enumerator `punktfunk`) on open and `SwDeviceClose`s it on drop, so the virtual
//! DualSense appears/disappears with the session — matching the Linux UHID pad. (The driver itself
//! must already be installed; the installer stages it.)

use super::dualsense_proto::{
    parse_ds_output, serialize_state, DsFeedback, DsState, DS_INPUT_REPORT_LEN, DS_TOUCH_H,
    DS_TOUCH_W,
};
use super::gamepad_raii::PadChannel;
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use anyhow::{anyhow, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::core::{w, GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{
    SwDeviceClose, SwDeviceCreate, HSWDEVICE, SW_DEVICE_CREATE_INFO,
};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, WAIT_OBJECT_0};
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
pub(super) const OFF_DRIVER_PROTO: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, driver_proto);
pub(super) const OFF_PAD_INDEX: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, pad_index);
pub(super) const DEVTYPE_DUALSHOCK4: u8 = pf_driver_proto::gamepad::DEVTYPE_DUALSHOCK4;

/// A single virtual DualSense: the SwDeviceCreate'd `pf_pad_<index>` software devnode (the driver
/// loads on it and the HID DualSense appears to games) plus the sealed shared-memory channel.
/// Dropping it removes the devnode (`SwDeviceClose`) and closes both sections.
struct DsWinPad {
    /// Per-session devnode from SwDeviceCreate, when it succeeds (RAII — `SwDeviceClose` on drop).
    /// `None` falls back to an out-of-band `pf_dualsense` devnode (installer/devgen).
    _sw: Option<super::gamepad_raii::SwDevice>,
    /// The sealed channel: unnamed DATA section (`PadShm`) + bootstrap mailbox + handle delivery.
    channel: PadChannel,
    /// Watches the section's `driver_proto` field and logs attach / never-attached diagnosis.
    attach: super::gamepad_raii::DriverAttach,
    seq: u8,
    ts: u32,
    last_out_seq: u32,
}

/// Context for the `SwDeviceCreate` completion callback: an event to signal, the HRESULT it reports,
/// and the PnP instance id PnP assigned (captured for devnode health diagnostics).
#[repr(C)]
struct SwCreateCtx {
    event: HANDLE,
    result: HRESULT,
    instance_id: [u16; 128],
}

/// `SwDeviceCreate` fires this once PnP has enumerated the device; stash the result and wake the
/// creator, which blocks on the event (so there's no concurrent access to `*ctx`).
unsafe extern "system" fn sw_create_cb(
    _dev: HSWDEVICE,
    result: HRESULT,
    ctx: *const c_void,
    id: PCWSTR,
) {
    if !ctx.is_null() {
        // SAFETY: ctx is the &mut SwCreateCtx the creator passed; it outlives this callback (the
        // creator blocks on the event). `id` is a NUL-terminated string for the callback's duration.
        unsafe {
            let c = ctx as *mut SwCreateCtx;
            (*c).result = result;
            if !id.is_null() {
                for i in 0..(*c).instance_id.len() - 1 {
                    let ch = *id.0.add(i);
                    (*c).instance_id[i] = ch;
                    if ch == 0 {
                        break;
                    }
                }
            }
            let _ = SetEvent((*c).event);
        }
    }
}

impl SwCreateCtx {
    fn instance_id(&self) -> Option<String> {
        let len = self.instance_id.iter().position(|&c| c == 0)?;
        (len > 0).then(|| String::from_utf16_lossy(&self.instance_id[..len]))
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
/// **Game-detection identity** (see `design/windows-dualsense-game-detection.md`). `HIDD_ATTRIBUTES`
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
pub(super) fn create_swdevice(p: &SwDeviceProfile) -> Result<(HSWDEVICE, Option<String>)> {
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
    // The pad index, stamped into the device Location — the driver reads it to poll `pfds-boot-<index>`
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
    // `result` starts as E_FAIL, NOT S_OK: if the wait below times out, a zero-initialised HRESULT
    // would read as success and mask the failure (found by the 2026-07 driver-health audit).
    let mut ctx = SwCreateCtx {
        event,
        result: E_FAIL,
        instance_id: [0; 128],
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
    let wait = unsafe { WaitForSingleObject(event, 10_000) };
    // SAFETY: event is valid.
    unsafe {
        let _ = CloseHandle(event);
    }
    if wait != WAIT_OBJECT_0 {
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate enumeration callback never fired (10s) — PnP may be wedged"
        ));
    }
    if ctx.result.is_err() {
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate enumeration failed: {:?}",
            ctx.result
        ));
    }
    Ok((hsw, ctx.instance_id()))
}

impl DsWinPad {
    /// Create the sealed channel (unnamed DATA section + `Global\pfds-boot-<index>` mailbox), stamp
    /// the pad index + neutral report + the magic LAST, then spawn the `pf_pad_<index>` devnode (the
    /// driver loads on it and receives the DATA handle over the bootstrap). The devnode lives for the
    /// pad's lifetime — dropping the pad removes it (`SwDeviceClose`).
    fn open(index: u8) -> Result<DsWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // Stamp the pad index (the driver validates it on attach) + the neutral input report, then
        // the magic LAST (the driver only accepts the section once magic is set). The device-type
        // stays 0 (DualSense — the section arrives zeroed).
        // SAFETY: base points at SHM_SIZE writable bytes; OFF_PAD_INDEX/OFF_INPUT are in range.
        unsafe {
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        // Spawn the per-session devnode via SwDeviceCreate; `SwDeviceClose` removes it on drop. On the
        // rare failure we keep the section + data plane and fall back to an out-of-band `pf_dualsense`
        // devnode (installer / dev-box devgen) — its persistent driver polls the same mailbox name.
        let inst = format!("pf_pad_{index}");
        let (hsw, instance_id) = match create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_index: index,
            hwid: "pf_dualsense",
            usb_vid_pid: "VID_054C&PID_0CE6",
            description: "punktfunk Virtual DualSense",
        }) {
            Ok((h, id)) => (Some(h), id),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; falling back to an out-of-band pf_dualsense devnode");
                (None, None)
            }
        };
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // Bounded eager delivery so the driver holds the DATA section before hidclass asks it for
        // descriptors (the driver reads `device_type` from the section to pick its HID identity).
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(DsWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                "pf_dualsense",
                "pf_dualsense.inf",
                "C:\\Users\\Public\\pfds-driver.log",
                boot_name,
                instance_id,
            ),
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
            std::ptr::copy_nonoverlapping(
                r.as_ptr(),
                self.channel.data_base().add(OFF_INPUT),
                r.len(),
            )
        };
    }

    /// Poll the section's output slot; parse a new `0x02` report (rumble / LEDs / triggers) into a
    /// [`DsFeedback`] for pad `pad`. Returns empty feedback if the driver hasn't published anything
    /// new. Also ticks the sealed-channel delivery and feeds the driver-attach health watcher (the
    /// driver's ~125 Hz timer stamps `driver_proto` while it has the section mapped).
    fn service(&mut self, pad: u8) -> DsFeedback {
        self.channel.pump();
        let mut fb = DsFeedback::default();
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
        self.state[idx].apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
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
                tracing::error!(error = %format!("{e:#}"), "virtual DualSense creation failed — controller input disabled until the next client connect (install/repair: punktfunk-host.exe driver install --gamepad)");
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
