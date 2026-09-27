//! Windows virtual Xbox 360 pad via the XUSB companion UMDF driver
//! (`packaging/windows/drivers/pf-xusb`). One pad per client index, visible to classic
//! `XInputGetState` with no kernel bus: `SwDeviceCreate` a `pf_xusb_<index>` devnode
//! (the driver registers `GUID_DEVINTERFACE_XUSB`) and push XInput state into an unnamed
//! DATA section over the sealed channel ([`PadChannel`] — handle duplicated into WUDFHost,
//! bootstrapped via `Global\pfxusb-boot-<index>`; `design/gamepad-channel-sealing.md`).
//! GameStream/Moonlight already speak XInput (low-16 buttons, sticks −32768..32767 +Y up,
//! triggers 0..255), so the copy is ~1:1.
//!
//! Rumble is the reverse path: `XInputSetState` → driver `SET_STATE` into the section →
//! [`GamepadManager::pump_rumble`] onto the 0xCA plane, matching Linux `EV_FF`.

use super::gamepad_raii::{
    create_swdevice, DriverAttach, PadChannel, ProofTransport, SwDevice, SwDeviceProfile,
};
use crate::pad_slots::PadSlots;
use anyhow::Result;
use punktfunk_core::input::{GamepadEvent, MAX_PADS};
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::time::{Duration, Instant};

// Driver maps this same struct; `offset_of!` so a layout change is a compile error.
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

/// INF hardware ids. `pf_xusb` installs the `xinputhid` UpperFilters string WGI admits on;
/// PnP fails a devnode whose filter service is missing, so without it the pad takes the
/// filter-free line and XInput alone sees it.
const XUSB_HWID: &str = "pf_xusb";
const XUSB_UNFILTERED_HWID: &str = "pf_xusb_nofilter";

fn xusb_hwid() -> &'static str {
    if super::xbox_windows::xinputhid_registered() {
        XUSB_HWID
    } else {
        XUSB_UNFILTERED_HWID
    }
}

/// One virtual Xbox 360 pad: `pf_xusb_<index>` plus the sealed `XusbShm` channel.
struct XusbWinPad {
    _sw: SwDevice,
    channel: PadChannel,
    attach: DriverAttach,
    packet: u32,
    last_rumble_seq: u32,
}

impl XusbWinPad {
    /// Unnamed DATA + `Global\pfxusb-boot-<index>` mailbox. Stamp pad index, then magic LAST
    /// (the driver accepts the section only once magic is set).
    fn open(index: u8) -> Result<XusbWinPad> {
        let boot_name = pf_driver_proto::gamepad::xusb_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // Index first; magic LAST. The driver rejects the section until magic is set.
        // SAFETY: base points at SHM_SIZE writable bytes; OFF_PAD_INDEX is in range.
        unsafe {
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        // `?` so PadSlots retries; a swallowed failure latched a phantom pad for the session.
        let hwid = xusb_hwid();
        let (sw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &format!("pf_xusb_{index}"),
            container_tag: 0x5046_5855, // "PFXU"
            container_index: index,
            hwid,
            // XInput finds the device by `GUID_DEVINTERFACE_XUSB`, not VID/PID.
            usb_vid_pid: None,
            usb_mi: None,
            description: "Punktfunk Virtual Xbox 360 (XUSB)",
            enumerator: "punktfunk",
        })?;
        channel.bind_devnode(index as u32, instance_id.clone(), ProofTransport::XusbIoctl);
        // 1500 ms: EvtDeviceAdd publishes the pid immediately; miss and `service` keeps pumping.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(XusbWinPad {
            _sw: sw,
            channel,
            attach: DriverAttach::new(
                hwid,
                "pf_xusb.inf",
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pfxusb-driver.log",
                boot_name,
                instance_id,
            ),
            packet: 0,
            last_rumble_seq: 0,
        })
    }

    /// Write XInput state; `packet` last so XInput sees a coherent snapshot.
    #[allow(clippy::too_many_arguments)]
    fn write_state(&mut self, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16) {
        self.packet = self.packet.wrapping_add(1);
        let base = self.channel.data_base();
        // SAFETY: `base` is the mapped `SHM_SIZE` section; every `OFF_*` is in range.
        // Single owner (`&mut self`). `packet` LAST: `Release` fence then `Release`
        // store so an `Acquire` load never sees a torn body on ARM64 (x86-TSO: plain
        // stores). `OFF_PACKET` (== 4) is 4-aligned off the page-aligned base.
        unsafe {
            std::ptr::write_unaligned(base.add(OFF_BUTTONS) as *mut u16, buttons);
            *base.add(OFF_LT) = lt;
            *base.add(OFF_RT) = rt;
            std::ptr::write_unaligned(base.add(OFF_LX) as *mut i16, lx);
            std::ptr::write_unaligned(base.add(OFF_LY) as *mut i16, ly);
            std::ptr::write_unaligned(base.add(OFF_RX) as *mut i16, rx);
            std::ptr::write_unaligned(base.add(OFF_RY) as *mut i16, ry);
            fence(Ordering::Release);
            (*(base.add(OFF_PACKET) as *const AtomicU32)).store(self.packet, Ordering::Release);
        }
    }

    /// New rumble `(large, small)` if `rumble_seq` moved. Also pumps handle delivery and attach.
    fn service(&mut self) -> Option<(u8, u8)> {
        self.channel.pump();
        let base = self.channel.data_base();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe { std::ptr::read_unaligned(base.add(OFF_DRIVER_PROTO) as *const u32) };
        self.attach.observe(proto);
        // SAFETY: base points at SHM_SIZE bytes; `OFF_RUMBLE_SEQ` (== 24) is 4-aligned off the
        // page-aligned base, so the `AtomicU32` view is valid. The driver bumps `rumble_seq` AFTER
        // writing the rumble bytes, so an `Acquire` load here orders the `rumble_large`/`rumble_small`
        // reads below after it — a fresh seq guarantees a coherent snapshot of the rumble bytes on a
        // weakly-ordered core (ARM64). On x86-TSO it is a plain load.
        let seq =
            unsafe { (*(base.add(OFF_RUMBLE_SEQ) as *const AtomicU32)).load(Ordering::Acquire) };
        if seq == self.last_rumble_seq {
            return None;
        }
        self.last_rumble_seq = seq;
        // SAFETY: rumble bytes at OFF_RUMBLE / OFF_RUMBLE+1.
        let (large, small) = unsafe { (*base.add(OFF_RUMBLE), *base.add(OFF_RUMBLE + 1)) };
        Some((large, small))
    }
}

// Shared with UHID (`uhid_manager::rumble_idle_timeout`, default 2.5 s). XInput
// vibration is level-triggered and persists until the game writes zero, so a
// latched rumble would drone forever. Window sits above SDL's ~2 s resend so
// an SDL host refreshes the clock before force-off.
/// Session Xbox 360 pads — Windows analogue of Linux uinput-xpad (`new`/`handle`/`pump_rumble`).
pub struct GamepadManager {
    slots: PadSlots<XusbWinPad>,
    last_rumble: Vec<(u8, u8)>,
    /// Last `SET_STATE` per pad. Non-zero rumble older than `rumble_idle_timeout` is forced off.
    last_active: Vec<Instant>,
}

impl Default for GamepadManager {
    fn default() -> GamepadManager {
        GamepadManager::new()
    }
}

impl GamepadManager {
    pub fn new() -> GamepadManager {
        GamepadManager {
            slots: PadSlots::new(
                "Xbox 360/Windows",
                "Xbox 360",
                " (install/repair: punktfunk-host.exe driver install --gamepad)",
            ),
            last_rumble: vec![(0, 0); MAX_PADS],
            last_active: (0..MAX_PADS).map(|_| Instant::now()).collect(),
        }
    }

    /// Show this session's pads to one seat alone
    /// ([`PadSlots::expose_in`](crate::pad_slots::PadSlots::expose_in)).
    pub fn expose_in(&mut self, dir: Option<std::path::PathBuf>) {
        self.slots.expose_in(dir);
    }

    /// Pads actually built. Harness-only; see [`crate::uhid_manager::UhidManager::live_pads`].
    pub fn live_pads(&self) -> usize {
        self.slots.live()
    }

    fn ensure(&mut self, idx: usize) {
        if self.slots.ensure(idx, XusbWinPad::open) {
            tracing::info!(
                index = idx,
                "virtual Xbox 360 created (Windows XUSB companion)"
            );
            self.last_rumble[idx] = (0, 0);
            self.last_active[idx] = Instant::now();
        }
    }

    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival (Xbox 360/Windows)");
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Mask bit cleared: arm grace here; the drop lands on a later `pump_rumble`.
                // XUSB has no rich plane to clear on re-claim.
                let swept = self.slots.sweep(f.active_mask).dropped;
                self.reset_swept(swept);
                if f.active_mask & (1 << idx) == 0 {
                    return;
                }
                self.ensure(idx);
                if let Some(pad) = self.slots.get_mut(idx) {
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

    /// Clear rumble clocks for indices a sweep or reap just dropped.
    fn reset_swept(&mut self, swept: u16) {
        for i in 0..MAX_PADS {
            if swept & (1 << i) != 0 {
                self.last_rumble[i] = (0, 0);
                self.last_active[i] = Instant::now();
            }
        }
    }

    /// Relay changed rumble. Motors are 0..255, wire is 0..65535, so ×257.
    /// `large` → `low`, `small` → `high`. Trigger args stay 0: `SET_STATE` is
    /// `XINPUT_VIBRATION` (two motors); impulse rumble is HID/WGI only.
    pub fn pump_rumble(&mut self, mut send: impl FnMut(u16, u16, u16, u16, u16)) {
        // Reap unplugs whose removal frame only armed grace; else the devnode outlives the pad.
        let swept = self.slots.reap();
        self.reset_swept(swept);
        for (i, pad) in self.slots.iter_mut() {
            if let Some((large, small)) = pad.service() {
                // Seq moved: refresh even if the level is unchanged, so a held rumble stays live.
                self.last_active[i] = Instant::now();
                if self.last_rumble[i] != (large, small) {
                    self.last_rumble[i] = (large, small);
                    send(i as u16, large as u16 * 257, small as u16 * 257, 0, 0);
                }
            } else if self.last_rumble[i] != (0, 0)
                && crate::uhid_manager::rumble_idle_timeout()
                    .is_some_and(|t| self.last_active[i].elapsed() >= t)
            {
                // Latched rumble, no SET_STATE for the idle window — force off.
                tracing::info!(
                    index = i,
                    prev_low = self.last_rumble[i].0 as u16 * 257,
                    prev_high = self.last_rumble[i].1 as u16 * 257,
                    "rumble: stale residual (game stopped driving the pad) — forcing off"
                );
                self.last_rumble[i] = (0, 0);
                send(i as u16, 0, 0, 0, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Both XUSB ids have a model line, and only `pf_xusb`'s install writes the `xinputhid`
    /// UpperFilters string: on a machine without that service it fails the devnode.
    #[test]
    fn xusb_hwids_match_inf() {
        let inx = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/windows/drivers/pf-xusb/pf_xusb.inx"
        );
        let inf = std::fs::read_to_string(inx).expect("read pf_xusb.inx");
        let lines: Vec<&str> = inf
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with(';'))
            .collect();
        let section_for = |hwid: &str| {
            lines.iter().find_map(|l| {
                let (section, ids) = l.split_once('=')?.1.split_once(',')?;
                ids.split(',')
                    .any(|i| i.trim().eq_ignore_ascii_case(hwid))
                    .then(|| section.trim().to_string())
            })
        };
        let hw_block = |section: &str| -> Vec<&str> {
            let head = format!("[{section}.NT.HW]").to_ascii_lowercase();
            lines
                .iter()
                .skip_while(|l| l.to_ascii_lowercase() != head)
                .skip(1)
                .take_while(|l| !l.starts_with('['))
                .copied()
                .collect()
        };
        let filtered = section_for(super::XUSB_HWID).expect("pf_xusb model line");
        let plain = section_for(super::XUSB_UNFILTERED_HWID).expect("pf_xusb_nofilter model line");
        assert!(
            hw_block(&filtered).iter().any(|l| l.starts_with("AddReg=")),
            "{filtered} lost the xinputhid AddReg WGI admits the pad on"
        );
        let plain_hw = hw_block(&plain);
        assert!(
            !plain_hw.is_empty() && !plain_hw.iter().any(|l| l.starts_with("AddReg=")),
            "{plain} must install without the xinputhid UpperFilters string"
        );
    }
}
