//! Windows virtual Xbox 360 gamepad via the punktfunk **XUSB companion** UMDF driver
//! (`packaging/windows/drivers/pf-xusb`) — the in-tree replacement for ViGEmBus. One virtual Xbox 360
//! controller per client pad index, visible to classic **XInput** (`XInputGetState`) with no kernel
//! bus driver: each pad `SwDeviceCreate`s a `pf_xusb_<index>` devnode (the driver loads on it and
//! registers `GUID_DEVINTERFACE_XUSB`) and the host pushes the XInput state into an **unnamed** shared
//! DATA section the driver reaches over the **sealed channel** ([`PadChannel`] — handle duplicated
//! into its WUDFHost, bootstrapped via `Global\pfxusb-boot-<index>`; see
//! `design/gamepad-channel-sealing.md`). GameStream/Moonlight already speak the XInput conventions
//! (low-16 button bits, sticks −32768..32767 +Y up, triggers 0..255), so the state copy is ~1:1.
//!
//! Rumble flows back the other way: a game writes force-feedback via `XInputSetState`, the driver
//! parses the `SET_STATE` packet into the shared section, and [`GamepadManager::pump_rumble`] relays
//! level changes to the client (the universal 0xCA plane), mirroring the Linux `EV_FF` read path.

use super::gamepad_raii::{sw_create_cb, PadChannel, SwCreateCtx};
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use crate::inject::pad_gate::PadGate;
use anyhow::{anyhow, Result};
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{
    SwDeviceClose, SwDeviceCreate, HSWDEVICE, SW_DEVICE_CREATE_INFO,
};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

// Shared-section layout — the single source of truth is `pf_driver_proto::gamepad::XusbShm` (offset
// asserts pin every field; the `pf_xusb` driver maps the same struct). Derive the size/offsets/magic from
// it so a layout change is a compile error, not a hand-synced literal (audit §6.1).
use pf_driver_proto::gamepad::XusbShm;
const SHM_SIZE: usize = core::mem::size_of::<XusbShm>();
const SHM_MAGIC: u32 = pf_driver_proto::gamepad::XUSB_MAGIC; // "PFXU"
const OFF_PACKET: usize = core::mem::offset_of!(XusbShm, packet);
const OFF_BUTTONS: usize = core::mem::offset_of!(XusbShm, buttons);
const OFF_LT: usize = core::mem::offset_of!(XusbShm, left_trigger);
const OFF_RT: usize = core::mem::offset_of!(XusbShm, right_trigger);
const OFF_LX: usize = core::mem::offset_of!(XusbShm, thumb_lx);
const OFF_LY: usize = core::mem::offset_of!(XusbShm, thumb_ly);
const OFF_RX: usize = core::mem::offset_of!(XusbShm, thumb_rx);
const OFF_RY: usize = core::mem::offset_of!(XusbShm, thumb_ry);
const OFF_RUMBLE_SEQ: usize = core::mem::offset_of!(XusbShm, rumble_seq);
const OFF_RUMBLE: usize = core::mem::offset_of!(XusbShm, rumble_large); // large @28, small @29
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(XusbShm, driver_proto);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(XusbShm, pad_index);

/// Spawn the `pf_xusb_<index>` companion devnode (hardware id `pf_xusb`, enumerator `punktfunk`). The
/// INF (System class) binds our UMDF driver, which registers the XUSB interface. Unlike the HID pads,
/// no USB compatible-ids are needed — XInput finds the device by the interface GUID, not VID/PID — but
/// we still pass a deterministic non-null `pContainerId` (the null-sentinel trips an `xinput1_4`
/// slot-skip bug). `SwDeviceClose` removes it on drop.
fn create_swdevice(index: u8) -> Result<(HSWDEVICE, Option<String>)> {
    let hwids: Vec<u16> = "pf_xusb".encode_utf16().chain([0u16, 0u16]).collect();
    let instid: Vec<u16> = format!("pf_xusb_{index}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let desc: Vec<u16> = "punktfunk Virtual Xbox 360 (XUSB)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // The pad index, stamped into the device Location — the driver reads it to poll `pfxusb-boot-<index>`
    // (multi-pad). The buffer must outlive the SwDeviceCreate call (it does; we wait on the event).
    let loc: Vec<u16> = format!("{index}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let container = GUID::from_values(0x5046_5855, 0x0000, 0x0000, [0, 0, 0, 0, 0, 0, 0, index]);

    // SAFETY: zeroed then the fields we use are set; the buffers + container outlive the call.
    let mut info: SW_DEVICE_CREATE_INFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SW_DEVICE_CREATE_INFO>() as u32;
    info.pszInstanceId = PCWSTR(instid.as_ptr());
    info.pszzHardwareIds = PCWSTR(hwids.as_ptr());
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
    // SAFETY: info + buffers + ctx outlive the call (we wait on the event before returning).
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
            return Err(anyhow!("SwDeviceCreate(pf_xusb) failed: {e}"));
        }
    };
    // SAFETY: event valid; block until PnP finishes enumerating, then check the callback result.
    let wait = unsafe { WaitForSingleObject(event, 10_000) };
    // SAFETY: event is valid.
    unsafe {
        let _ = CloseHandle(event);
    }
    if wait != WAIT_OBJECT_0 {
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate(pf_xusb) enumeration callback never fired (10s) — PnP may be wedged"
        ));
    }
    if ctx.result.is_err() {
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate(pf_xusb) enumeration failed: {:?}",
            ctx.result
        ));
    }
    Ok((hsw, ctx.instance_id()))
}

/// A single virtual Xbox 360 pad: the `pf_xusb_<index>` devnode plus the sealed shared-memory channel.
struct XusbWinPad {
    /// Owns the `pf_xusb_<index>` devnode (dropped → `SwDeviceClose`). `None` if `SwDeviceCreate` failed.
    _sw: Option<super::gamepad_raii::SwDevice>,
    /// The sealed channel: the unnamed DATA section (the `XusbShm`) + the bootstrap mailbox + the
    /// handle-delivery state machine (drop closes both sections).
    channel: PadChannel,
    /// Watches the section's `driver_proto` field and logs attach / never-attached diagnosis.
    attach: super::gamepad_raii::DriverAttach,
    packet: u32,
    last_rumble_seq: u32,
}

impl XusbWinPad {
    /// Create the sealed channel (unnamed DATA section + `Global\pfxusb-boot-<index>` mailbox), stamp
    /// the pad index then the magic LAST, spawn the devnode, and eagerly deliver the DATA handle once
    /// the driver publishes its pid.
    fn open(index: u8) -> Result<XusbWinPad> {
        let boot_name = pf_driver_proto::gamepad::xusb_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // The section arrives zeroed; stamp the pad index (the driver validates it against its own
        // devnode index on attach) then the magic LAST (the driver only accepts it once magic is set).
        // SAFETY: base points at SHM_SIZE writable bytes; OFF_PAD_INDEX is in range.
        unsafe {
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let (hsw, instance_id) = match create_swdevice(index) {
            Ok((h, id)) => (Some(h), id),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; XUSB devnode unavailable");
                (None, None)
            }
        };
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // Bounded eager delivery: the driver's EvtDeviceAdd publishes its pid right away; handing it
        // the DATA handle before we return means the pad is live for the game's first XInput poll.
        // On a missing/old driver this waits out the window once and the service pump takes over.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(XusbWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                "pf_xusb",
                "pf_xusb.inf",
                "C:\\Users\\Public\\pfxusb-driver.log",
                boot_name,
                instance_id,
            ),
            packet: 0,
            last_rumble_seq: 0,
        })
    }

    /// Publish the XInput state to the section and bump the packet number (XInput uses it to detect
    /// change). `buttons` is the XINPUT_GAMEPAD_* bitmap; sticks are i16, triggers u8.
    #[allow(clippy::too_many_arguments)]
    fn write_state(&mut self, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16) {
        self.packet = self.packet.wrapping_add(1);
        let base = self.channel.data_base();
        // SAFETY: `base` is the start of the mapped section (`SHM_SIZE` bytes, owned by `Shm`); every
        // `OFF_*` is a fixed in-range offset into it and `write_unaligned` handles the unaligned field
        // writes. Single owner (`&mut self`), so no concurrent writer races these stores.
        unsafe {
            std::ptr::write_unaligned(base.add(OFF_BUTTONS) as *mut u16, buttons);
            *base.add(OFF_LT) = lt;
            *base.add(OFF_RT) = rt;
            std::ptr::write_unaligned(base.add(OFF_LX) as *mut i16, lx);
            std::ptr::write_unaligned(base.add(OFF_LY) as *mut i16, ly);
            std::ptr::write_unaligned(base.add(OFF_RX) as *mut i16, rx);
            std::ptr::write_unaligned(base.add(OFF_RY) as *mut i16, ry);
            std::ptr::write_unaligned(base.add(OFF_PACKET) as *mut u32, self.packet);
        }
    }

    /// Poll the section for a game's rumble (the driver bumps `rumble_seq` on each SET_STATE). Returns
    /// `(large, small)` motor levels (0..=255) when a new one arrived. Also ticks the sealed-channel
    /// delivery (a late-binding driver gets its handle here) and feeds the driver-attach health
    /// watcher (the driver stamps `driver_proto` once it maps the delivered section + per IOCTL).
    fn service(&mut self) -> Option<(u8, u8)> {
        self.channel.pump();
        let base = self.channel.data_base();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe { std::ptr::read_unaligned(base.add(OFF_DRIVER_PROTO) as *const u32) };
        self.attach.observe(proto);
        // SAFETY: base points at SHM_SIZE bytes.
        let seq = unsafe { std::ptr::read_unaligned(base.add(OFF_RUMBLE_SEQ) as *const u32) };
        if seq == self.last_rumble_seq {
            return None;
        }
        self.last_rumble_seq = seq;
        // SAFETY: rumble bytes at OFF_RUMBLE / OFF_RUMBLE+1.
        let (large, small) = unsafe { (*base.add(OFF_RUMBLE), *base.add(OFF_RUMBLE + 1)) };
        Some((large, small))
    }
}

/// All virtual Xbox 360 pads of a session — the Windows analogue of the Linux uinput-xpad manager,
/// now backed by the XUSB companion driver. Same method surface (`new`/`handle`/`pump_rumble`) the
/// session input thread already drives.
/// How long a non-zero rumble may stay latched with the game NOT driving the pad (no `SET_STATE`)
/// before it is forced off. XInput vibration is level-triggered — it persists until the game sets
/// it to zero — so a game that latches a rumble and then stops calling `XInputSetState` (a residual
/// left at a menu / loading screen, or a plain forgotten stop) would otherwise drone to the client
/// forever (measured: a stuck `(0,512)` resent every 500 ms for 5.5 minutes). A real controller
/// stops when the app stops driving it; this mirrors that. It is keyed on game ACTIVITY (any
/// `SET_STATE`, even an unchanged one), so a rumble the game keeps asserting is never cut — only an
/// abandoned residual is. Kept above SDL's ~2 s internal rumble resend so an SDL-driven host game
/// (which re-issues the same level every ~2 s) refreshes the activity clock before this fires.
const RUMBLE_IDLE_TIMEOUT: Duration = Duration::from_millis(2500);

pub struct GamepadManager {
    pads: Vec<Option<XusbWinPad>>,
    last_rumble: Vec<(u8, u8)>,
    /// When the game last drove each pad (bumped `rumble_seq` via `SET_STATE`). A non-zero
    /// `last_rumble` older than [`RUMBLE_IDLE_TIMEOUT`] against this is a stale residual — see the
    /// const's docs.
    last_active: Vec<Instant>,
    /// Create-retry gate: a transient XUSB-companion failure backs off and retries instead of
    /// permanently disabling every pad for the session.
    gate: PadGate,
}

impl Default for GamepadManager {
    fn default() -> GamepadManager {
        GamepadManager::new()
    }
}

impl GamepadManager {
    pub fn new() -> GamepadManager {
        GamepadManager {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            last_rumble: vec![(0, 0); MAX_PADS],
            last_active: (0..MAX_PADS).map(|_| Instant::now()).collect(),
            gate: PadGate::new(),
        }
    }

    fn ensure(&mut self, idx: usize) {
        if idx >= MAX_PADS || self.pads[idx].is_some() || !self.gate.allow(Instant::now()) {
            return;
        }
        match XusbWinPad::open(idx as u8) {
            Ok(p) => {
                tracing::info!(
                    index = idx,
                    "virtual Xbox 360 created (Windows XUSB companion)"
                );
                self.pads[idx] = Some(p);
                self.last_rumble[idx] = (0, 0);
                self.last_active[idx] = Instant::now();
                self.gate.on_success();
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual Xbox 360 creation failed — retrying with backoff (install/repair: punktfunk-host.exe driver install --gamepad)");
                self.gate.on_failure(Instant::now());
            }
        }
    }

    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival (Xbox 360/Windows)");
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index.max(0) as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Unplugs: drop any allocated pad whose mask bit cleared.
                for (i, slot) in self.pads.iter_mut().enumerate() {
                    if slot.is_some() && f.active_mask & (1 << i) == 0 {
                        tracing::info!(index = i, "controller unplugged (Xbox 360/Windows)");
                        *slot = None;
                        self.last_rumble[i] = (0, 0);
                        self.last_active[i] = Instant::now();
                    }
                }
                if f.active_mask & (1 << idx) == 0 {
                    return;
                }
                self.ensure(idx);
                if let Some(pad) = self.pads[idx].as_mut() {
                    pad.write_state(
                        (f.buttons & 0xffff) as u16,
                        f.left_trigger,
                        f.right_trigger,
                        f.ls_x,
                        f.ls_y,
                        f.rs_x,
                        f.rs_y,
                    );
                }
            }
        }
    }

    /// Relay any changed rumble level to the client. XUSB motors are 0..255; the wire carries
    /// 0..65535, so scale by 257. `large` (low-frequency) → the datagram's `low`, `small`
    /// (high-frequency) → `high` — matching the other backends.
    pub fn pump_rumble(&mut self, mut send: impl FnMut(u16, u16, u16)) {
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_mut() else {
                continue;
            };
            if let Some((large, small)) = pad.service() {
                // The game drove the pad this poll (SET_STATE bumped the seq) — refresh the
                // activity clock even when the level is unchanged, so a rumble it keeps asserting
                // never trips the stale-residual timeout below.
                self.last_active[i] = Instant::now();
                if self.last_rumble[i] != (large, small) {
                    self.last_rumble[i] = (large, small);
                    send(i as u16, large as u16 * 257, small as u16 * 257);
                }
            } else if self.last_rumble[i] != (0, 0)
                && self.last_active[i].elapsed() >= RUMBLE_IDLE_TIMEOUT
            {
                // A non-zero rumble is latched but the game has not driven the pad for
                // RUMBLE_IDLE_TIMEOUT — a residual it forgot to stop. Force it off (and forward
                // the zero) so the resend loop stops droning it to the client. See the const docs.
                tracing::info!(
                    index = i,
                    prev_low = self.last_rumble[i].0 as u16 * 257,
                    prev_high = self.last_rumble[i].1 as u16 * 257,
                    "rumble: stale residual (game stopped driving the pad) — forcing off"
                );
                self.last_rumble[i] = (0, 0);
                send(i as u16, 0, 0);
            }
        }
    }
}
