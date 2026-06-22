//! Windows virtual Xbox 360 gamepad via the punktfunk **XUSB companion** UMDF driver
//! (`packaging/windows/xusb-driver`) — the in-tree replacement for ViGEmBus. One virtual Xbox 360
//! controller per client pad index, visible to classic **XInput** (`XInputGetState`) with no kernel
//! bus driver: each pad `SwDeviceCreate`s a `pf_xusb_<index>` devnode (the driver loads on it and
//! registers `GUID_DEVINTERFACE_XUSB`) and the host pushes the XInput state into the shared section
//! `Global\pfxusb-shm-<index>`. GameStream/Moonlight already speak the XInput conventions (low-16
//! button bits, sticks −32768..32767 +Y up, triggers 0..255), so the state copy is ~1:1.
//!
//! Rumble flows back the other way: a game writes force-feedback via `XInputSetState`, the driver
//! parses the `SET_STATE` packet into the shared section, and [`GamepadManager::pump_rumble`] relays
//! level changes to the client (the universal 0xCA plane), mirroring the Linux `EV_FF` read path.
//!
//! NB: the driver currently maps `Global\pfxusb-shm-0` (hardcoded), so a single pad (index 0) is
//! fully correct; mixed multi-pad needs the driver to read its own index first (same limitation as
//! the DualSense backend).

use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use anyhow::{anyhow, Result};
use std::ffi::c_void;
use windows::core::{w, GUID, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{
    SwDeviceClose, SwDeviceCreate, HSWDEVICE, SW_DEVICE_CREATE_INFO,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

// Shared-section layout — must match `packaging/windows/xusb-driver/src/lib.rs`.
const SHM_SIZE: usize = 64;
const SHM_MAGIC: u32 = 0x5558_4650; // "PFXU"
const OFF_PACKET: usize = 4;
const OFF_BUTTONS: usize = 8;
const OFF_LT: usize = 10;
const OFF_RT: usize = 11;
const OFF_LX: usize = 12;
const OFF_LY: usize = 14;
const OFF_RX: usize = 16;
const OFF_RY: usize = 18;
const OFF_RUMBLE_SEQ: usize = 24;
const OFF_RUMBLE: usize = 28; // large @28, small @29

/// Context for the `SwDeviceCreate` completion callback: an event to signal + the HRESULT it reports.
#[repr(C)]
struct SwCreateCtx {
    event: HANDLE,
    result: HRESULT,
}

/// `SwDeviceCreate` fires this once PnP has enumerated the device; stash the result + wake the creator.
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

/// Spawn the `pf_xusb_<index>` companion devnode (hardware id `pf_xusb`, enumerator `punktfunk`). The
/// INF (System class) binds our UMDF driver, which registers the XUSB interface. Unlike the HID pads,
/// no USB compatible-ids are needed — XInput finds the device by the interface GUID, not VID/PID — but
/// we still pass a deterministic non-null `pContainerId` (the null-sentinel trips an `xinput1_4`
/// slot-skip bug). `SwDeviceClose` removes it on drop.
fn create_swdevice(index: u8) -> Result<HSWDEVICE> {
    let hwids: Vec<u16> = "pf_xusb".encode_utf16().chain([0u16, 0u16]).collect();
    let instid: Vec<u16> = format!("pf_xusb_{index}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let desc: Vec<u16> = "punktfunk Virtual Xbox 360 (XUSB)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // The pad index, stamped into the device Location — the driver reads it to map `pfxusb-shm-<index>`
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
    let mut ctx = SwCreateCtx {
        event,
        result: HRESULT(0),
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
    unsafe {
        WaitForSingleObject(event, 10_000);
        let _ = CloseHandle(event);
    }
    if ctx.result.is_err() {
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate(pf_xusb) enumeration failed: {:?}",
            ctx.result
        ));
    }
    Ok(hsw)
}

/// A single virtual Xbox 360 pad: the `pf_xusb_<index>` devnode plus the mapped shared section.
struct XusbWinPad {
    hsw: Option<HSWDEVICE>,
    map: HANDLE,
    view: *mut u8,
    packet: u32,
    last_rumble_seq: u32,
}

impl XusbWinPad {
    /// Create + map `Global\pfxusb-shm-<index>`, stamp the magic, then spawn the devnode.
    fn open(index: u8) -> Result<XusbWinPad> {
        let name = HSTRING::from(format!("Global\\pfxusb-shm-{index}"));

        // Permissive DACL so the WUDFHost (whatever account) can open the section.
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL literal valid; psd receives an OS-freed descriptor (host-lifetime — fine).
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
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let hsw = match create_swdevice(index) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; XUSB devnode unavailable");
                None
            }
        };
        Ok(XusbWinPad {
            hsw,
            map,
            view: base,
            packet: 0,
            last_rumble_seq: 0,
        })
    }

    /// Publish the XInput state to the section and bump the packet number (XInput uses it to detect
    /// change). `buttons` is the XINPUT_GAMEPAD_* bitmap; sticks are i16, triggers u8.
    #[allow(clippy::too_many_arguments)]
    fn write_state(&mut self, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16) {
        self.packet = self.packet.wrapping_add(1);
        // SAFETY: view points at SHM_SIZE bytes; all offsets are in range.
        unsafe {
            std::ptr::write_unaligned(self.view.add(OFF_BUTTONS) as *mut u16, buttons);
            *self.view.add(OFF_LT) = lt;
            *self.view.add(OFF_RT) = rt;
            std::ptr::write_unaligned(self.view.add(OFF_LX) as *mut i16, lx);
            std::ptr::write_unaligned(self.view.add(OFF_LY) as *mut i16, ly);
            std::ptr::write_unaligned(self.view.add(OFF_RX) as *mut i16, rx);
            std::ptr::write_unaligned(self.view.add(OFF_RY) as *mut i16, ry);
            std::ptr::write_unaligned(self.view.add(OFF_PACKET) as *mut u32, self.packet);
        }
    }

    /// Poll the section for a game's rumble (the driver bumps `rumble_seq` on each SET_STATE). Returns
    /// `(large, small)` motor levels (0..=255) when a new one arrived.
    fn service(&mut self) -> Option<(u8, u8)> {
        // SAFETY: view points at SHM_SIZE bytes.
        let seq = unsafe { std::ptr::read_unaligned(self.view.add(OFF_RUMBLE_SEQ) as *const u32) };
        if seq == self.last_rumble_seq {
            return None;
        }
        self.last_rumble_seq = seq;
        // SAFETY: rumble bytes at OFF_RUMBLE / OFF_RUMBLE+1.
        let (large, small) =
            unsafe { (*self.view.add(OFF_RUMBLE), *self.view.add(OFF_RUMBLE + 1)) };
        Some((large, small))
    }
}

impl Drop for XusbWinPad {
    fn drop(&mut self) {
        // SAFETY: hsw (if any) owns the devnode; view/map from MapViewOfFile/CreateFileMappingW.
        unsafe {
            if let Some(h) = self.hsw {
                SwDeviceClose(h);
            }
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut c_void,
            });
            let _ = CloseHandle(self.map);
        }
    }
}

/// All virtual Xbox 360 pads of a session — the Windows analogue of the Linux uinput-xpad manager,
/// now backed by the XUSB companion driver. Same method surface (`new`/`handle`/`pump_rumble`) the
/// session input thread already drives.
pub struct GamepadManager {
    pads: Vec<Option<XusbWinPad>>,
    last_rumble: Vec<(u8, u8)>,
    broken: bool,
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
            broken: false,
        }
    }

    fn ensure(&mut self, idx: usize) {
        if idx >= MAX_PADS || self.pads[idx].is_some() || self.broken {
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
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual Xbox 360 creation failed — controller input disabled (is the pf_xusb driver installed?)");
                self.broken = true;
            }
        }
    }

    pub fn handle(&mut self, ev: &GamepadEvent) {
        let GamepadEvent::State(f) = ev else {
            return; // Arrival metadata — the pad is created lazily on the first State
        };
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

    /// Relay any changed rumble level to the client. XUSB motors are 0..255; the wire carries
    /// 0..65535, so scale by 257. `large` (low-frequency) → the datagram's `low`, `small`
    /// (high-frequency) → `high` — matching the other backends.
    pub fn pump_rumble(&mut self, mut send: impl FnMut(u16, u16, u16)) {
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_mut() else {
                continue;
            };
            if let Some((large, small)) = pad.service() {
                if self.last_rumble[i] != (large, small) {
                    self.last_rumble[i] = (large, small);
                    send(i as u16, large as u16 * 257, small as u16 * 257);
                }
            }
        }
    }
}
