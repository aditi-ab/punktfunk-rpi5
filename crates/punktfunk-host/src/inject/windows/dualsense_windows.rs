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
use super::gamepad_raii::{sw_create_cb, PadChannel, SwCreateCtx};
use crate::inject::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{anyhow, Result};
use punktfunk_core::quic::RichInput;
use std::ffi::c_void;
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::time::Duration;
use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{
    SwDeviceClose, SwDeviceCreate, HSWDEVICE, SW_DEVICE_CREATE_INFO,
};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

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
pub(super) const DEVTYPE_DUALSENSE_EDGE: u8 = pf_driver_proto::gamepad::DEVTYPE_DUALSENSE_EDGE;

/// A single virtual DualSense: the SwDeviceCreate'd `pf_pad_<index>` software devnode (the driver
/// loads on it and the HID DualSense appears to games) plus the sealed shared-memory channel.
/// Dropping it removes the devnode (`SwDeviceClose`) and closes both sections.
/// `pub`: the type appears as `type Pad` in the `PadProto` impl (a public trait), like the
/// Linux pads.
pub struct DsWinPad {
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

/// The identity a [`DsWinPad`] enumerates with — the plain DualSense or the Edge share the whole
/// transport (section layout, input report shape, output parse); only the `device_type` stamp and
/// the PnP identity differ. The DS4 differs in report codec too, so it keeps its own pad type.
pub(super) struct WinDsIdentity {
    /// `device_type` stamped into the section (the driver picks its HID identity off it).
    pub devtype: u8,
    /// PnP instance-id prefix (`pf_pad` / `pf_edge`) — distinct namespaces per type.
    pub instance_prefix: &'static str,
    /// The INF-matched hardware id.
    pub hwid: &'static str,
    /// The USB VID&PID token for the synthesized bus identity.
    pub usb_vid_pid: &'static str,
    /// Device Manager description.
    pub description: &'static str,
}

impl WinDsIdentity {
    pub(super) const fn dualsense() -> WinDsIdentity {
        WinDsIdentity {
            devtype: 0,
            instance_prefix: "pf_pad",
            hwid: "pf_dualsense",
            usb_vid_pid: "VID_054C&PID_0CE6",
            description: "punktfunk Virtual DualSense",
        }
    }

    pub(super) const fn dualsense_edge() -> WinDsIdentity {
        WinDsIdentity {
            devtype: DEVTYPE_DUALSENSE_EDGE,
            instance_prefix: "pf_edge",
            hwid: "pf_dualsenseedge",
            usb_vid_pid: "VID_054C&PID_0DF2",
            description: "punktfunk Virtual DualSense Edge",
        }
    }
}

impl DsWinPad {
    /// Create the sealed channel (unnamed DATA section + `Global\pfds-boot-<index>` mailbox), stamp
    /// the device type FIRST (so it's visible the moment magic is) + the pad index + a neutral
    /// report + the magic LAST, then spawn the devnode (the driver loads on it and receives the
    /// DATA handle over the bootstrap). The devnode lives for the pad's lifetime — dropping the pad
    /// removes it (`SwDeviceClose`).
    pub(super) fn open(index: u8, id: &WinDsIdentity) -> Result<DsWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = id.devtype;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        // Spawn the per-session devnode via SwDeviceCreate; `SwDeviceClose` removes it on drop. On the
        // rare failure we keep the section + data plane and fall back to an out-of-band devnode
        // (installer / dev-box devgen) — its persistent driver polls the same mailbox name.
        let inst = format!("{}_{index}", id.instance_prefix);
        let (hsw, instance_id) = match create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_index: index,
            hwid: id.hwid,
            usb_vid_pid: id.usb_vid_pid,
            description: id.description,
        }) {
            Ok((h, i)) => (Some(h), i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), hwid = id.hwid, "SwDeviceCreate failed; falling back to an out-of-band devnode");
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
                id.hwid,
                "pf_dualsense.inf", // one driver package serves every PS identity
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
    pub(super) fn write_state(&mut self, st: &DsState) {
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(1);
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.seq, self.ts);
        // SAFETY: base points at SHM_SIZE bytes; input slot is OFF_INPUT..OFF_INPUT+64. Unlike the
        // XUSB `packet` / DualSense `out_seq` fields, the input path has NO driver-polled change-detect
        // field to publish last: the `pf_dualsense` driver streams the whole `input` region to game
        // READ_REPORTs on its ~125 Hz timer, and the report's own sequence counter (r[7], mid-report)
        // is consumed by the game's HID stack, not the driver — so it cannot serve as a separable
        // publish flag without a seqlock generation the driver `Acquire`-reads (a `PadShm` layout +
        // driver change, deferred). The `Release` fence after the copy orders the report-body stores
        // ahead of this pad's next `Release` publish (the bootstrap/seq stores in `channel.pump()`),
        // giving the copy Release visibility on a weakly-ordered core (ARM64); on x86-TSO it is a
        // no-op. Residual: absent a driver-side `Acquire` on a per-frame input generation, a torn
        // single frame is still theoretically possible but self-heals on the next ~250 Hz write.
        unsafe {
            std::ptr::copy_nonoverlapping(
                r.as_ptr(),
                self.channel.data_base().add(OFF_INPUT),
                r.len(),
            );
            fence(Ordering::Release);
        };
    }

    /// Poll the section's output slot; parse a new `0x02` report (rumble / LEDs / triggers) into a
    /// [`DsFeedback`] for pad `pad`. Returns empty feedback if the driver hasn't published anything
    /// new. Also ticks the sealed-channel delivery and feeds the driver-attach health watcher (the
    /// driver's ~125 Hz timer stamps `driver_proto` while it has the section mapped).
    pub(super) fn service(&mut self, pad: u8) -> DsFeedback {
        self.channel.pump();
        let mut fb = DsFeedback::default();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        };
        self.attach.observe(proto);
        // SAFETY: base points at SHM_SIZE bytes; `OFF_OUT_SEQ` (== 72) is 4-aligned off the
        // page-aligned base, so the `AtomicU32` view is valid. The driver bumps `out_seq` AFTER
        // writing the `output` report, so an `Acquire` load here orders the `output` copy below after
        // it — a fresh seq guarantees a coherent snapshot of the output bytes on a weakly-ordered core
        // (ARM64). On x86-TSO it is a plain load.
        let seq = unsafe {
            (*(self.channel.data_base().add(OFF_OUT_SEQ) as *const AtomicU32))
                .load(Ordering::Acquire)
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

/// The Windows-DualSense half of the shared stateful manager (see [`PadProto`]): the UMDF
/// sealed-channel open, the same [`DsState`] mappers as `linux/dualsense.rs`, and the section
/// feedback poll. Lifecycle (slot table, unplug sweep, heartbeat, dedup) lives in [`UhidManager`].
pub struct DsWinProto {
    /// Fallback policy for the Steam back grips a client may send (the DualSense has no back-button
    /// HID slot). `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop. Parity with `linux/dualsense.rs`.
    remap: crate::inject::steam_remap::RemapConfig,
}

impl Default for DsWinProto {
    fn default() -> DsWinProto {
        DsWinProto {
            remap: crate::inject::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for DsWinProto {
    type Pad = DsWinPad;
    type State = DsState;
    const LABEL: &'static str = "DualSense/Windows";
    const DEVICE: &'static str = "DualSense";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DsWinPad> {
        let p = DsWinPad::open(idx, &WinDsIdentity::dualsense())?;
        tracing::info!(
            index = idx,
            "virtual DualSense created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Merge buttons/sticks/triggers from the frame, preserving touch + motion + pad clicks (rich-
    /// plane fields that must survive a button-only frame) — exactly as `linux/dualsense.rs` does.
    fn merge_frame(&self, prev: &DsState, f: &crate::gamestream::gamepad::GamepadFrame) -> DsState {
        // Steam back grips have no DualSense slot — fold them onto standard buttons per the
        // configured policy (default drop) so they aren't silently lost.
        let buttons = crate::inject::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
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
        st.apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DsWinPad, st: &DsState) {
        pad.write_state(st);
    }

    /// Poll the section for a game's feedback: motor rumble on the universal 0xCA plane, the rich
    /// lightbar/player-LED/trigger events on the 0xCD plane.
    fn service(&self, pad: &mut DsWinPad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            rumble: fb.rumble,
            hidout: fb.hidout,
        }
    }
}

/// **N4 spike** (gamepad-new-types §6, timeboxed): create a software-devnode HID **Steam Deck**
/// (`device_type = 3`, `VID_28DE&PID_1205`) and hold it for `secs`, streaming the neutral Deck
/// frame, so the go/no-go question — does Steam Input on Windows promote a software-devnode HID
/// Deck, or does it require a real USB bus identity (the documented GameInput instance-path
/// gap)? — can be answered by watching Steam's `logs/controller.txt` / controller settings
/// while this holds. Never used by a session; wired to the `deck-windows-spike` subcommand.
pub fn deck_spike_hold(index: u8, secs: u64) -> Result<()> {
    let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
    let mut channel = PadChannel::create(boot_name, SHM_SIZE)?;
    let base = channel.data_base();
    // Neutral Deck input frame: [0x01, 0x00, ID_CONTROLLER_DECK_STATE=0x09, 0x3C], all released.
    let mut neutral = [0u8; 64];
    (neutral[0], neutral[2], neutral[3]) = (0x01, 0x09, 0x3C);
    // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range. Device-type
    // FIRST, magic LAST — the same publish order the session pads use.
    unsafe {
        *base.add(OFF_DEVTYPE) = pf_driver_proto::gamepad::DEVTYPE_STEAMDECK_SPIKE;
        std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
        std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; 64], neutral);
        std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
    }
    let inst = format!("pf_deckspike_{index}");
    let (hsw, _) = create_swdevice(&SwDeviceProfile {
        instance: &inst,
        container_index: index,
        hwid: "pf_steamdeck",
        usb_vid_pid: "VID_28DE&PID_1205",
        description: "punktfunk Virtual Steam Deck (spike)",
    })?;
    let _sw = super::gamepad_raii::SwDevice::new(hsw);
    channel.deliver_eager(std::time::Duration::from_millis(1500));
    println!(
        "virtual Steam Deck devnode up (28DE:1205, device_type 3) — holding {secs}s.\n\
         Observe: Get-PnpDevice -PresentOnly | findstr 1205; Steam logs\\controller.txt for a\n\
         detect/promote line; Steam Settings > Controller for a 'Steam Deck' entry.\n\
         GO = Steam lists/promotes it; NO-GO = it never appears (the Linux `Interface: -1` gap\n\
         applies verbatim — document and keep the SteamDeck->DualSense Windows fold)."
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last_out_seq = 0u32;
    while std::time::Instant::now() < deadline {
        channel.pump();
        // Log any feature/output traffic Steam sends — each one is spike evidence.
        // SAFETY: base points at SHM_SIZE bytes; OFF_OUT_SEQ is in range.
        let seq = unsafe {
            std::ptr::read_unaligned(channel.data_base().add(OFF_OUT_SEQ) as *const u32)
        };
        if seq != last_out_seq {
            last_out_seq = seq;
            let mut out = [0u8; 16];
            // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    channel.data_base().add(OFF_OUTPUT),
                    out.as_mut_ptr(),
                    16,
                )
            };
            println!("  output report from a client (Steam?): {out:02x?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("deck-windows-spike: done (devnode removed on exit)");
    Ok(())
}

/// All virtual DualSense pads of a session — the Windows analogue of
/// [`DualSenseManager`](super::dualsense::DualSenseManager). Same method surface (via the shared
/// [`UhidManager`]) so the session input thread drives either backend identically. The heartbeat
/// keeps the section fresh (the driver's timer streams whatever's in it) — parity with the UHID
/// backend's silence heartbeat.
pub type DualSenseWindowsManager = UhidManager<DsWinProto>;
