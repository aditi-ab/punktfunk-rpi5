//! Virtual Steam Deck controller via UHID — the Steam analogue of the virtual DualSense
//! ([`super::dualsense`]). A UHID device with Valve VID `28DE` / Deck PID `1205` is bound by the
//! kernel `hid-steam` driver, which exposes a full Steam Deck gamepad evdev (incl. the four back
//! grips) **plus** a separate IMU evdev, and — when Steam runs on the host — is re-grabbed by Steam
//! Input with native glyphs + trackpad/gyro/back-button bindings.
//!
//! The transport-independent contract (descriptor, byte-exact serializer, the `XInput`/rich
//! mappers, the rumble parser) lives in [`super::steam_proto`]; this module is the `/dev/uhid`
//! plumbing + the two Steam-specific lifecycle quirks the DualSense path lacks:
//!
//! 1. **`gamepad_mode` entry.** `steam_do_deck_input_event` early-returns under the default
//!    `lizard_mode` until `gamepad_mode` is toggled on — which the kernel only does when the `b9.6`
//!    Steam/menu-right button is held ~450 ms with no hidraw client open. So on the first pad we
//!    best-effort clear `lizard_mode` via sysfs (needs root; bypasses the gate entirely) AND every
//!    pad pulses `b9.6` for [`MODE_ENTER`] at creation. After that an **anti-toggle guard** caps any
//!    continuous `b9.6` (a long in-game Start-hold) below the kernel's 450 ms threshold so play can
//!    never accidentally flip `gamepad_mode` back off.
//! 2. **`UHID_SET_REPORT`.** Steam feedback (`0xEB` rumble) + the kernel's settings/serial writes
//!    arrive as FEATURE set-reports that MUST be answered `err = 0`, or the kernel stalls ~5 s per
//!    command (the DualSense backend only services GET_REPORT + OUTPUT).

use super::steam_proto::{
    btn, parse_steam_output, serial_reply, serialize_deck_state, SteamState, STEAMDECK_PRODUCT,
    STEAMDECK_RDESC, STEAM_REPORT_LEN, STEAM_VENDOR,
};
use crate::gamestream::gamepad::{GamepadEvent, MAX_PADS};
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// /dev/uhid event ABI — same layout as the DualSense backend.
const UHID_PATH: &str = "/dev/uhid";
const UHID_DESTROY: u32 = 1;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const UHID_EVENT_SIZE: usize = 4 + 4372;
const BUS_USB: u16 = 0x03;

/// Hold the `b9.6` mode-switch this long at creation to toggle `gamepad_mode` on (the kernel needs
/// ~450 ms continuous; give margin).
const MODE_ENTER: Duration = Duration::from_millis(650);
/// Cap continuous `b9.6` (Start) below the kernel's 450 ms mode-switch threshold: after this long
/// we insert a one-frame release so an in-game long-Start-hold can't toggle `gamepad_mode` off.
const MENU_HOLD_CAP: Duration = Duration::from_millis(350);

fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
}

/// Best-effort, once per process: clear `hid_steam`'s `lizard_mode` so `steam_do_deck_input_event`
/// stops gating on `gamepad_mode` (gamepad events then always flow). Needs root; on failure the
/// per-pad `b9.6` pulse + guard handle it instead.
fn try_clear_lizard_mode() {
    static TRIED: AtomicBool = AtomicBool::new(false);
    if TRIED.swap(true, Ordering::Relaxed) {
        return;
    }
    match std::fs::write("/sys/module/hid_steam/parameters/lizard_mode", "N") {
        Ok(()) => {
            tracing::info!("cleared hid_steam lizard_mode (Steam Deck gamepad events always flow)")
        }
        Err(e) => tracing::debug!(
            error = %e,
            "could not clear hid_steam lizard_mode (no root?) — using the gamepad_mode pulse + guard"
        ),
    }
}

/// A virtual Steam Deck backed by `/dev/uhid`. Dropping it destroys the device (the kernel tears
/// down the bound `hid-steam` interface + both evdevs).
pub struct SteamDeckPad {
    fd: File,
    seq: u32,
    created: Instant,
    /// When `b9.6` started being continuously held in our OUTPUT (anti-toggle guard); `None` = not.
    menu_hold_since: Option<Instant>,
}

impl SteamDeckPad {
    pub fn open(index: u8) -> Result<SteamDeckPad> {
        try_clear_lizard_mode();
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the uhid udev rule installed + are you in 'input'?)")
            })?;
        let mut pad = SteamDeckPad {
            fd,
            seq: 0,
            created: Instant::now(),
            menu_hold_since: None,
        };
        pad.send_create2(index).context("UHID_CREATE2 Steam Deck")?;
        Ok(pad)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        put_cstr(&mut ev, 4, 128, &format!("Punktfunk Steam Deck {index}")); // name[128]
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/steam/{index}")); // phys[64]
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-steam-{index}")); // uniq[64]
        ev[260..262].copy_from_slice(&(STEAMDECK_RDESC.len() as u16).to_ne_bytes()); // rd_size
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus
        ev[264..268].copy_from_slice(&STEAM_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&STEAMDECK_PRODUCT.to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
        ev[280..280 + STEAMDECK_RDESC.len()].copy_from_slice(STEAMDECK_RDESC);
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Serialize `st` (with the gamepad-mode entry overlay + anti-toggle guard applied) and write it.
    pub fn write_state(&mut self, st: &SteamState) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let mut s = *st;
        s.buttons = self.effective_buttons(st.buttons);
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, &s, self.seq);

        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes()); // input2.size
        ev[6..6 + r.len()].copy_from_slice(&r); // input2.data
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// True while still pulsing the mode-switch at creation (the caller force-writes during this).
    fn in_mode_entry(&self) -> bool {
        self.created.elapsed() < MODE_ENTER
    }

    /// During mode entry, force `b9.6` held (override). Afterwards, pass the real buttons through but
    /// drop `b9.6` for one frame whenever it's been continuously held past [`MENU_HOLD_CAP`].
    fn effective_buttons(&mut self, mut buttons: u64) -> u64 {
        if self.in_mode_entry() {
            return btn::STEAM_MENU_RIGHT;
        }
        if buttons & btn::MENU != 0 {
            let now = Instant::now();
            match self.menu_hold_since {
                None => self.menu_hold_since = Some(now),
                Some(since) if now.duration_since(since) >= MENU_HOLD_CAP => {
                    buttons &= !btn::MENU; // one-frame release resets the kernel's mode-switch timer
                    self.menu_hold_since = None;
                }
                Some(_) => {}
            }
        } else {
            self.menu_hold_since = None;
        }
        buttons
    }

    /// Service the device, non-blocking: answer the kernel's GET_REPORT (serial) + SET_REPORT
    /// (settings / rumble — ack `err=0`) and parse any rumble feedback (`0xEB`, on either the
    /// SET_REPORT or OUTPUT path) into `(low, high)` for the universal rumble plane.
    pub fn service(&mut self) -> Option<(u16, u16)> {
        let mut rumble = None;
        let mut ev = [0u8; UHID_EVENT_SIZE];
        while let Ok(n) = self.fd.read(&mut ev) {
            if n < UHID_EVENT_SIZE {
                break;
            }
            match u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]) {
                UHID_OUTPUT => {
                    let size = u16::from_ne_bytes([ev[4100], ev[4101]]) as usize;
                    let end = 4 + size.min(HID_MAX_DESCRIPTOR_SIZE);
                    if let Some(r) = parse_steam_output(&ev[4..end]).rumble {
                        rumble = Some(r);
                    }
                }
                UHID_GET_REPORT => {
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let _ = self.reply_get_report(id, &serial_reply("PUNKTFUNK01"));
                }
                UHID_SET_REPORT => {
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    // SET_REPORT data: [report-id 0, cmd, …] at ev[12..]. Surface rumble, then ack.
                    let end = (12 + 16).min(UHID_EVENT_SIZE);
                    if let Some(r) = parse_steam_output(&ev[12..end]).rumble {
                        rumble = Some(r);
                    }
                    let _ = self.reply_set_report(id);
                }
                _ => {} // Start/Stop/Open/Close — ignore
            }
        }
        rumble
    }

    fn reply_get_report(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err 0
        ev[10..12].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[12..12 + data.len()].copy_from_slice(data);
        self.fd.write_all(&ev).context("UHID_GET_REPORT_REPLY")?;
        Ok(())
    }

    fn reply_set_report(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err 0 (ack)
        self.fd.write_all(&ev).context("UHID_SET_REPORT_REPLY")?;
        Ok(())
    }
}

impl Drop for SteamDeckPad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// All virtual Steam Deck pads of a session — the Steam analogue of
/// [`DualSenseManager`](super::dualsense::DualSenseManager), selected with `PUNKTFUNK_GAMEPAD=steamdeck`.
/// Button/stick frames arrive via [`handle`](Self::handle); the right trackpad + motion via
/// [`apply_rich`](Self::apply_rich); [`pump`](Self::pump) services the kernel handshake + routes
/// rumble back; [`heartbeat`](Self::heartbeat) keeps the pad alive (and drives the mode-entry pulse).
/// The transport a manager pad drives. UHID is universal but Steam Input won't promote it (a UHID
/// device has no USB interface number, `Interface: -1`); the USB **gadget** (`raw_gadget`, SteamOS)
/// and **usbip** (`vhci_hcd`, universal) both present the controller on USB interface 2, which Steam
/// Input *does* promote. Selected per-pad by [`open_transport`].
enum DeckTransport {
    Uhid(SteamDeckPad),
    Gadget(crate::inject::steam_gadget::SteamDeckGadget),
    Usbip(crate::inject::steam_usbip::SteamDeckUsbip),
}

impl DeckTransport {
    fn write_state(&mut self, st: &SteamState) {
        match self {
            DeckTransport::Uhid(p) => {
                let _ = p.write_state(st);
            }
            DeckTransport::Gadget(g) => g.write_state(st),
            DeckTransport::Usbip(u) => u.write_state(st),
        }
    }
    fn service(&mut self) -> Option<(u16, u16)> {
        match self {
            DeckTransport::Uhid(p) => p.service(),
            DeckTransport::Gadget(g) => g.service().rumble,
            DeckTransport::Usbip(u) => u.service().rumble,
        }
    }
    fn in_mode_entry(&self) -> bool {
        match self {
            // Only the UHID pad needs the gamepad-mode entry pulse: the promoted transports are
            // read raw via hidraw by Steam Input, which bypasses the kernel's evdev mode gate.
            DeckTransport::Uhid(p) => p.in_mode_entry(),
            DeckTransport::Gadget(_) | DeckTransport::Usbip(_) => false,
        }
    }
}

/// One-shot diagnostic: InputPlumber (shipped and enabled by default on Bazzite) hidraw-grabs
/// controllers it decides to manage and re-emits them under a different identity — historically
/// the Deck config re-emitted an Xbox Elite pad with the trackpads routed to a mouse target. If
/// it grabs our virtual Deck, everything downstream of hid-steam looks wrong (trackpads surface
/// as a stick/mouse, gyro vanishes) while punktfunk's own logs stay clean — so name the suspect
/// up front. Best-effort process-name scan; no dependency on its D-Bus API.
fn warn_if_inputplumber() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if !ONCE.swap(false, Ordering::Relaxed) {
        return;
    }
    let running = std::fs::read_dir("/proc")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "inputplumber")
        });
    if running {
        tracing::warn!(
            "InputPlumber is running on this host — if it manages the virtual Steam Deck pad, \
             games see InputPlumber's re-emitted device instead (trackpads may arrive as a \
             stick/mouse, gyro may vanish). Check `inputplumber devices` and exclude the \
             virtual pad from management if inputs look remapped."
        );
    }
}

/// Open the best Steam-Input-promotable Deck transport available, in preference order:
/// **`raw_gadget` (SteamOS validated fast-path) → `usbip`/`vhci_hcd` (universal, Secure-Boot-clean)
/// → UHID (universal, but `Interface: -1` so Steam Input won't promote it).** Each rung degrades to
/// the next on failure, so a host lacking the gadget modules still gets a *promotable* Deck via
/// usbip, and one lacking both still gets a working (if non-promoted) UHID pad.
fn open_transport(idx: u8) -> Result<DeckTransport> {
    warn_if_inputplumber();
    use crate::inject::{steam_gadget, steam_usbip};
    // 1. raw_gadget — the validated SteamOS fast-path (default on there).
    if steam_gadget::gadget_preferred() {
        steam_gadget::ensure_modules();
        match steam_gadget::SteamDeckGadget::open(idx) {
            Ok(g) => {
                tracing::info!(
                    index = idx,
                    "virtual Steam Deck created (USB gadget — Steam Input recognizes it)"
                );
                return Ok(DeckTransport::Gadget(g));
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "USB-gadget Deck unavailable — trying usbip")
            }
        }
    }
    // 2. usbip/vhci_hcd — the universal, in-tree, Secure-Boot-clean transport (default on elsewhere).
    if steam_usbip::usbip_preferred() {
        match steam_usbip::SteamDeckUsbip::open(idx) {
            Ok(u) => return Ok(DeckTransport::Usbip(u)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "usbip Deck unavailable — falling back to UHID")
            }
        }
    }
    // 3. UHID — universal fallback (works everywhere; Steam Input won't promote it). This is a
    // DEGRADED outcome, not a normal one: a UHID device has no USB interface number (Interface: -1),
    // so Steam Input ignores it and the controller never appears in Game Mode / can't navigate.
    // Reaching here almost always means `vhci_hcd` isn't loaded (the host runs unprivileged and
    // can't modprobe it) — load it at boot (packaging ships modules-load.d/punktfunk.conf +
    // 60-punktfunk.rules; on a systemd-sysext host `punktfunk-sysext` mirrors both into /etc).
    let p = SteamDeckPad::open(idx)?;
    tracing::warn!(
        index = idx,
        "virtual Steam Deck created as UHID hid-steam — Steam Input WON'T promote it (no USB \
         interface), so it won't appear in Game Mode. Load vhci_hcd (usbip) so the pad arrives as a \
         real USB device: `sudo modprobe vhci_hcd`, and ensure it loads at boot."
    );
    Ok(DeckTransport::Uhid(p))
}

pub struct SteamControllerManager {
    pads: Vec<Option<DeckTransport>>,
    state: Vec<SteamState>,
    last_rumble: Vec<(u16, u16)>,
    last_write: Vec<Instant>,
    broken: bool,
}

impl Default for SteamControllerManager {
    fn default() -> SteamControllerManager {
        SteamControllerManager::new()
    }
}

impl SteamControllerManager {
    pub fn new() -> SteamControllerManager {
        SteamControllerManager {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            state: vec![SteamState::neutral(); MAX_PADS],
            last_rumble: vec![(0, 0); MAX_PADS],
            last_write: vec![Instant::now(); MAX_PADS],
            broken: false,
        }
    }

    pub fn handle(&mut self, ev: &GamepadEvent) {
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival (Steam Deck)");
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                for (i, slot) in self.pads.iter_mut().enumerate() {
                    if slot.is_some() && f.active_mask & (1 << i) == 0 {
                        tracing::info!(index = i, "controller unplugged (Steam Deck)");
                        *slot = None;
                        self.state[i] = SteamState::neutral();
                        self.last_rumble[i] = (0, 0);
                    }
                }
                if f.active_mask & (1 << idx) == 0 {
                    return;
                }
                self.ensure(idx);
                // Merge buttons/sticks/triggers, preserving the rich-plane fields (trackpad + motion
                // arrive separately and must survive a button-only frame).
                let prev = self.state[idx];
                let mut s = SteamState::from_gamepad(
                    f.buttons,
                    f.ls_x,
                    f.ls_y,
                    f.rs_x,
                    f.rs_y,
                    f.left_trigger,
                    f.right_trigger,
                );
                s.rpad_x = prev.rpad_x;
                s.rpad_y = prev.rpad_y;
                s.lpad_x = prev.lpad_x;
                s.lpad_y = prev.lpad_y;
                s.gyro = prev.gyro;
                s.accel = prev.accel;
                s.buttons |= prev.buttons & (btn::RPAD_TOUCH | btn::LPAD_TOUCH);
                self.state[idx] = s;
                self.write(idx);
            }
        }
    }

    /// Apply a rich client→host event (right trackpad / motion) to an existing pad.
    pub fn apply_rich(&mut self, rich: RichInput) {
        let idx = match rich {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. } => pad as usize,
        };
        if idx >= MAX_PADS || self.pads[idx].is_none() {
            return;
        }
        self.state[idx].apply_rich(rich);
        self.write(idx);
    }

    fn write(&mut self, idx: usize) {
        let st = self.state[idx];
        if let Some(pad) = self.pads[idx].as_mut() {
            pad.write_state(&st);
        }
        self.last_write[idx] = Instant::now();
    }

    /// Re-emit each live pad's current report when silent past `max_gap`, and force a steady stream
    /// while a pad is still pulsing its gamepad-mode entry (so the `b9.6` toggle completes even with
    /// no game input).
    pub fn heartbeat(&mut self, max_gap: Duration) {
        let now = Instant::now();
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_ref() else {
                continue;
            };
            if pad.in_mode_entry() || now.duration_since(self.last_write[i]) >= max_gap {
                self.write(i);
            }
        }
    }

    fn ensure(&mut self, idx: usize) {
        if idx >= MAX_PADS || self.pads[idx].is_some() || self.broken {
            return;
        }
        match open_transport(idx as u8) {
            Ok(t) => {
                self.pads[idx] = Some(t);
                self.state[idx] = SteamState::neutral();
                self.last_rumble[idx] = (0, 0);
                self.last_write[idx] = Instant::now();
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual Steam Deck creation failed — controller input disabled");
                self.broken = true;
            }
        }
    }

    /// Service every pad: answer the kernel handshake and forward rumble on the universal plane.
    /// `rumble` fires `(index, low, high)` only on a level change. The Steam Deck has no rich
    /// host→client feedback plane (no lightbar / adaptive triggers), so `hidout` goes unused.
    pub fn pump(&mut self, mut rumble: impl FnMut(u16, u16, u16), _hidout: impl FnMut(HidOutput)) {
        for i in 0..self.pads.len() {
            let Some(pad) = self.pads[i].as_mut() else {
                continue;
            };
            if let Some(r) = pad.service() {
                if self.last_rumble[i] != r {
                    self.last_rumble[i] = r;
                    rumble(i as u16, r.0, r.1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find the evdev node for a kernel input device by exact name (e.g. `"Steam Deck"`).
    fn find_node(name: &str) -> Option<String> {
        let devs = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
        for block in devs.split("\n\n") {
            if !block
                .lines()
                .any(|l| l.trim() == format!("N: Name=\"{name}\""))
            {
                continue;
            }
            for l in block.lines() {
                if let Some(h) = l.strip_prefix("H: Handlers=") {
                    if let Some(ev) = h.split_whitespace().find(|t| t.starts_with("event")) {
                        return Some(format!("/dev/input/{ev}"));
                    }
                }
            }
        }
        None
    }

    /// Read the evdev's current key bitmap (`EVIOCGKEY`) and test whether `code` is down.
    fn key_is_down(node: &str, code: u16) -> bool {
        use std::os::unix::io::AsRawFd;
        let Ok(f) = std::fs::File::open(node) else {
            return false;
        };
        let mut bits = [0u8; 96];
        const EVIOCGKEY: libc::c_ulong = (2 << 30) | (96 << 16) | (0x45 << 8) | 0x18;
        // SAFETY: EVIOCGKEY copies the current key-state bitmap of the evdev behind the valid fd
        // `f` into `bits`; 96 bytes covers KEY_MAX/8, so the kernel never writes past the buffer.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGKEY, bits.as_mut_ptr()) };
        rc >= 0 && (bits[(code / 8) as usize] >> (code % 8)) & 1 == 1
    }

    /// Read the current value of an absolute axis (`EVIOCGABS`) — the first `i32` of `input_absinfo`.
    fn abs_value(node: &str, abs: u16) -> Option<i32> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(node).ok()?;
        let mut info = [0u8; 24]; // struct input_absinfo { value, min, max, fuzz, flat, resolution }
        let req: libc::c_ulong =
            (2 << 30) | (24 << 16) | (0x45 << 8) | (0x40 + abs as libc::c_ulong);
        // SAFETY: EVIOCGABS fills the 24-byte input_absinfo for the valid evdev fd `f`; we read only
        // the leading i32 `value`. The buffer is exactly sizeof(input_absinfo), so no overflow.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, info.as_mut_ptr()) };
        (rc >= 0).then(|| i32::from_ne_bytes([info[0], info[1], info[2], info[3]]))
    }

    /// On-box smoke test for the real backend: a `SteamDeckPad` must bind `hid-steam` (creating both
    /// the gamepad + IMU evdevs), enter `gamepad_mode` via the creation pulse, and land a held button
    /// on the evdev (`BTN_A`, code 0x130) — proving the entry overlay + byte-exact serialize path —
    /// then tear the device down on drop. Touches `/dev/uhid`, so it is `#[ignore]`d in CI; run on a
    /// box with `hid-steam` + `input`-group access: `cargo test -p punktfunk-host -- --ignored`.
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs hid-steam + the input group"]
    fn backend_binds_and_input_flows() {
        use punktfunk_core::input::gamepad as gs;
        const BTN_A: u16 = 0x130;
        const ABS_HAT0X: u16 = 0x10; // left trackpad X
        let mut pad = SteamDeckPad::open(0).expect("open SteamDeckPad (/dev/uhid + input group?)");
        // Drive the full M3 wire path: build state through `from_gamepad` (BTN_A + the L4 back grip)
        // and `apply_rich` (a left-pad TouchpadEx contact), then hold it past MODE_ENTER (the b9.6
        // pulse), servicing the handshake.
        let mut st = SteamState::from_gamepad(gs::BTN_A | gs::BTN_PADDLE2, 0, 0, 0, 0, 0, 0);
        st.apply_rich(RichInput::TouchpadEx {
            pad: 0,
            surface: 1,
            finger: 0,
            touch: true,
            click: false,
            x: -8000,
            y: 9000,
            pressure: 0,
        });
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1200) {
            let _ = pad.service();
            pad.write_state(&st).expect("write_state");
            std::thread::sleep(Duration::from_millis(4));
        }
        let devs = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(devs.contains("Steam Deck"), "gamepad evdev not created");
        assert!(
            devs.contains("Steam Deck Motion Sensors"),
            "IMU evdev not created"
        );
        let node = find_node("Steam Deck").expect("gamepad evdev node");
        assert!(
            key_is_down(&node, BTN_A),
            "BTN_A not down — gamepad_mode entry or serialize failed"
        );
        // The left trackpad contact (TouchpadEx surface 1, gated on LPAD_TOUCH) reaches ABS_HAT0X.
        assert_eq!(
            abs_value(&node, ABS_HAT0X),
            Some(-8000),
            "left trackpad (TouchpadEx surface 1) did not reach ABS_HAT0X"
        );
        drop(pad);
        std::thread::sleep(Duration::from_millis(200));
        let devs = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            !devs.contains("Steam Deck Motion Sensors"),
            "device not torn down on drop"
        );
    }
}
