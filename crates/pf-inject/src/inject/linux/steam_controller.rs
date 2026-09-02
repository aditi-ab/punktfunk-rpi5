//! Virtual Steam Deck / Steam Controller on `/dev/uhid`.
//!
//! Valve VID `28DE` / Deck PID `1205` binds `hid-steam` (gamepad evdev including
//! the four back grips, plus a separate IMU evdev). Steam Input re-grabs it when
//! Steam runs. Descriptor, serializer, mappers, rumble parser: [`super::steam_proto`].
//!
//! Deck `hid-steam` drops events under default `lizard_mode` until `gamepad_mode`
//! is on. The kernel toggles that only when `b9.6` is held ~450 ms with no hidraw
//! client. This module writes sysfs `lizard_mode=N` (needs root) and pulses `b9.6`
//! for [`MODE_ENTER`]. [`MENU_HOLD_CAP`] inserts a one-frame release so a long
//! Start-hold cannot toggle off.
//!
//! Steam rumble (`0xEB`) and kernel settings writes are FEATURE SET_REPORT;
//! answer `err = 0` or the kernel stalls ~5 s per command.

use super::steam_proto::{
    btn, parse_steam_output, sc_from_gamepad, serial_reply, serialize_deck_state,
    serialize_sc_state, SteamModel, SteamState, STEAMDECK_RDESC, STEAM_REPORT_LEN, STEAM_VENDOR,
};
use crate::uhid_abi::{
    put_cstr, request_id, set_report_data, BUS_USB, HID_MAX_DESCRIPTOR_SIZE, UHID_CREATE2,
    UHID_DESTROY, UHID_EVENT_SIZE, UHID_GET_REPORT, UHID_GET_REPORT_REPLY, UHID_INPUT2,
    UHID_OUTPUT, UHID_PATH, UHID_SET_REPORT, UHID_SET_REPORT_REPLY,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::RichInput;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// ~450 ms of continuous `b9.6` toggles `gamepad_mode`; 650 ms is the margin.
const MODE_ENTER: Duration = Duration::from_millis(650);
/// Stay under the kernel's ~450 ms `b9.6` toggle; a one-frame release resets the timer.
const MENU_HOLD_CAP: Duration = Duration::from_millis(350);

/// Once per process, write `hid_steam.lizard_mode=N` so Deck events are not gated. Needs root;
/// on failure the per-pad `b9.6` pulse still enters `gamepad_mode`.
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

/// Virtual Steam Deck or classic Steam Controller on `/dev/uhid`. Drop unbinds `hid-steam`.
pub struct SteamDeckPad {
    fd: File,
    model: SteamModel,
    seq: u32,
    created: Instant,
    /// Continuous `b9.6` hold start for [`MENU_HOLD_CAP`]; `None` while released.
    menu_hold_since: Option<Instant>,
}

impl SteamDeckPad {
    pub fn open(index: u8) -> Result<SteamDeckPad> {
        SteamDeckPad::open_model(index, SteamModel::Deck)
    }

    /// Deck only: classic SC (`ID_CONTROLLER_STATE`) has no `gamepad_mode` gate.
    pub fn open_model(index: u8, model: SteamModel) -> Result<SteamDeckPad> {
        if model == SteamModel::Deck {
            try_clear_lizard_mode();
        }
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
            model,
            seq: 0,
            created: Instant::now(),
            menu_hold_since: None,
        };
        pad.send_create2(index).context("UHID_CREATE2 Steam pad")?;
        Ok(pad)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let (name, phys, uniq) = match self.model {
            SteamModel::Deck => ("Steam Deck", "steam", "steam"),
            SteamModel::Controller => ("Steam Controller", "steamctrl", "steamctrl"),
        };
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // uhid_create2_req at 4: name[128] phys[64] uniq[64] rd_size bus vid pid version country rd_data.
        put_cstr(&mut ev, 4, 128, &format!("Punktfunk {name} {index}"));
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/{phys}/{index}"));
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-{uniq}-{index}"));
        ev[260..262].copy_from_slice(&(STEAMDECK_RDESC.len() as u16).to_ne_bytes());
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
        ev[264..268].copy_from_slice(&STEAM_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&self.model.product().to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes());
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes());
        ev[280..280 + STEAMDECK_RDESC.len()].copy_from_slice(STEAMDECK_RDESC);
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Deck: apply the mode-entry overlay and anti-toggle guard, then serialize.
    pub fn write_state(&mut self, st: &SteamState) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; STEAM_REPORT_LEN];
        match self.model {
            SteamModel::Deck => {
                let mut s = *st;
                s.buttons = self.effective_buttons(st.buttons);
                serialize_deck_state(&mut r, &s, self.seq);
            }
            SteamModel::Controller => serialize_sc_state(&mut r, st, self.seq),
        }

        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        // uhid_input2_req: size u16 at 4, data at 6.
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes());
        ev[6..6 + r.len()].copy_from_slice(&r);
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Create-time `b9.6` pulse still running (Deck only).
    fn in_mode_entry(&self) -> bool {
        self.model == SteamModel::Deck && self.created.elapsed() < MODE_ENTER
    }

    fn effective_buttons(&mut self, mut buttons: u64) -> u64 {
        if self.in_mode_entry() {
            return btn::STEAM_MENU_RIGHT;
        }
        if buttons & btn::MENU != 0 {
            let now = Instant::now();
            match self.menu_hold_since {
                None => self.menu_hold_since = Some(now),
                Some(since) if now.duration_since(since) >= MENU_HOLD_CAP => {
                    buttons &= !btn::MENU; // one-frame release; kernel ~450 ms toggle timer resets
                    self.menu_hold_since = None;
                }
                Some(_) => {}
            }
        } else {
            self.menu_hold_since = None;
        }
        buttons
    }

    /// Non-blocking. Ack GET_REPORT (serial) and SET_REPORT (`err=0` or the kernel stalls ~5 s).
    /// Parse `0xEB` rumble from SET_REPORT or OUTPUT.
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
                    let id = request_id(&ev);
                    // Kernel-declared size; a fixed window truncates or parses leftover bytes in this reused buffer.
                    if let Some(r) = parse_steam_output(set_report_data(&ev)).rumble {
                        rumble = Some(r);
                    }
                    let _ = self.reply_set_report(id);
                }
                _ => {}
            }
        }
        rumble
    }

    fn reply_get_report(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        // uhid_get_report_reply_req: id u32 [4..8], err u16 [8..10], size u16 [10..12], data [12..].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes());
        ev[10..12].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[12..12 + data.len()].copy_from_slice(data);
        self.fd.write_all(&ev).context("UHID_GET_REPORT_REPLY")?;
        Ok(())
    }

    fn reply_set_report(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
        // uhid_set_report_reply_req: id u32 [4..8], err u16 [8..10].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes());
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

/// Per-pad transport from [`open_transport`]. UHID reports `Interface: -1`, so Steam
/// Input will not promote it. Gadget and usbip present USB interface 2, which Steam Input promotes.
pub enum DeckTransport {
    Uhid(SteamDeckPad),
    Gadget(crate::steam_gadget::SteamDeckGadget),
    Usbip(crate::steam_usbip::SteamDeckUsbip),
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
            // Steam Input hidraw-reads promoted transports and bypasses hid-steam's evdev mode gate.
            DeckTransport::Uhid(p) => p.in_mode_entry(),
            DeckTransport::Gadget(_) | DeckTransport::Usbip(_) => false,
        }
    }
}

/// InputPlumber hidraw-grabs managed pads and re-emits them under another identity.
/// A grab remaps the Deck (trackpads as stick/mouse, gyro gone). One-shot `/proc` comm scan.
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

/// Steam-Input-promotable Deck: `raw_gadget` → `usbip`/`vhci_hcd` → UHID.
/// UHID works everywhere; `Interface: -1` so Steam Input will not promote it.
fn open_transport(idx: u8) -> Result<DeckTransport> {
    warn_if_inputplumber();
    use crate::{steam_gadget, steam_usbip};
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
    if steam_usbip::usbip_preferred() {
        match steam_usbip::SteamDeckUsbip::open(idx) {
            Ok(u) => return Ok(DeckTransport::Usbip(u)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "usbip Deck unavailable — falling back to UHID")
            }
        }
    }
    let p = SteamDeckPad::open(idx)?;
    tracing::warn!(
        index = idx,
        "virtual Steam Deck created as UHID hid-steam — Steam Input WON'T promote it (no USB \
         interface), so it won't appear in Game Mode. Load vhci_hcd (usbip) so the pad arrives as a \
         real USB device: `sudo modprobe vhci_hcd`, and ensure it loads at boot."
    );
    Ok(DeckTransport::Uhid(p))
}

/// Deck [`PadProto`]: [`open_transport`], [`SteamState`] mappers, handshake `service`.
/// Slot table / unplug / heartbeat / rumble dedup live in [`UhidManager`]. Mode-entry
/// pulse rides [`force_heartbeat`](PadProto::force_heartbeat).
#[derive(Default)]
pub struct SteamProto;

impl PadProto for SteamProto {
    type Pad = DeckTransport;
    type State = SteamState;
    const LABEL: &'static str = "Steam Deck";
    const DEVICE: &'static str = "Steam Deck";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DeckTransport> {
        open_transport(idx)
    }

    fn neutral(&self) -> SteamState {
        SteamState::neutral()
    }

    /// Keep prev trackpad + motion; those arrive on the rich plane.
    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SteamState {
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
        // Click is its own field, not `buttons`. Serialize ORs `RPAD_CLICK`; preserving it cannot strand wire BTN_TOUCHPAD.
        s.lpad_click = prev.lpad_click;
        s.rpad_click = prev.rpad_click;
        s
    }

    fn apply_rich(&self, st: &mut SteamState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn neutralize_gyro(&self, st: &mut SteamState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut SteamState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut DeckTransport, st: &SteamState) {
        pad.write_state(st);
    }

    /// Handshake + rumble. No host→client rich feedback, so `hidout` stays empty.
    fn service(&self, pad: &mut DeckTransport, _idx: u8) -> PadFeedback {
        let rumble = pad.service();
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: Vec::new(),
            // `Some` iff `0xEB` this poll. Arms abandoned-rumble force-off; hidraw never surfaces a stop.
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }

    /// Keep writing while the create-time `b9.6` pulse is still running.
    fn force_heartbeat(&self, pad: &DeckTransport) -> bool {
        pad.in_mode_entry()
    }
}

/// Session Steam Deck pads (`PUNKTFUNK_GAMEPAD=steamdeck`).
pub type SteamControllerManager = UhidManager<SteamProto>;

/// Classic Steam Controller [`PadProto`]. `hid-steam` under wired-SC identity `28DE:1102`
/// (`ID_CONTROLLER_STATE`). UHID only: usbip/gadget present the Deck's 3-interface USB
/// layout, which Steam Input will not promote as an SC.
///
/// One stick + two pads + two grips ([`sc_from_gamepad`]/[`serialize_sc_state`]):
/// wire right stick drives the right pad; left-pad contact shadows the left stick;
/// PADDLE1/2 land on the two grips (3/4 fold via remap). No FF, no sensors evdev.
pub struct ScProto {
    /// Fold for wire paddles beyond the SC's two grips (PADDLE3/4).
    remap: crate::steam_remap::RemapConfig,
}

impl Default for ScProto {
    fn default() -> ScProto {
        ScProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for ScProto {
    type Pad = SteamDeckPad;
    type State = SteamState;
    const LABEL: &'static str = "Steam Controller";
    const DEVICE: &'static str = "Steam Controller";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<SteamDeckPad> {
        let p = SteamDeckPad::open_model(idx, SteamModel::Controller)?;
        tracing::info!(
            index = idx,
            "virtual Steam Controller created (UHID hid-steam)"
        );
        Ok(p)
    }

    fn neutral(&self) -> SteamState {
        SteamState::neutral()
    }

    /// PADDLE1/2 map natively in [`sc_from_gamepad`]. Mask them out of the fold so 3/4 cannot
    /// double-fire the same grips.
    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SteamState {
        use punktfunk_core::input::gamepad as gs;
        let native = f.buttons & (gs::BTN_PADDLE1 | gs::BTN_PADDLE2);
        let folded = crate::steam_remap::fold_paddles(
            f.buttons & !(gs::BTN_PADDLE1 | gs::BTN_PADDLE2),
            self.remap.paddles,
        );
        let mut s = sc_from_gamepad(
            folded | native,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.lpad_x = prev.lpad_x;
        s.lpad_y = prev.lpad_y;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.buttons |= prev.buttons & btn::LPAD_TOUCH;
        s.lpad_click = prev.lpad_click;
        // Right pad is the wire right stick; a rich contact (TouchpadEx surface 2) overrides only while the stick is centered.
        if f.rs_x == 0 && f.rs_y == 0 {
            s.rpad_x = prev.rpad_x;
            s.rpad_y = prev.rpad_y;
            s.buttons |= prev.buttons & btn::RPAD_TOUCH;
            s.rpad_click = prev.rpad_click;
        }
        s
    }

    fn apply_rich(&self, st: &mut SteamState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn neutralize_gyro(&self, st: &mut SteamState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut SteamState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut SteamDeckPad, st: &SteamState) {
        let _ = pad.write_state(st);
    }

    /// Serial GET_REPORT + settings SET_REPORT. No FF device; rumble only from hidraw `0xEB`.
    fn service(&self, pad: &mut SteamDeckPad, _idx: u8) -> PadFeedback {
        let rumble = pad.service();
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: Vec::new(),
            // `Some` iff hidraw `0xEB` this poll. Arms abandoned-rumble force-off; hidraw never surfaces a stop.
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }
}

/// Session classic Steam Controllers (`PUNKTFUNK_GAMEPAD=steamcontroller`).
pub type SteamCtrlManager = UhidManager<ScProto>;

#[cfg(test)]
mod tests {
    use super::*;

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

    fn key_is_down(node: &str, code: u16) -> bool {
        use std::os::unix::io::AsRawFd;
        let Ok(f) = std::fs::File::open(node) else {
            return false;
        };
        let mut bits = [0u8; 96];
        const EVIOCGKEY: libc::c_ulong = (2 << 30) | (96 << 16) | (0x45 << 8) | 0x18;
        // SAFETY: `f` is a valid evdev fd. EVIOCGKEY copies the key-state bitmap into `bits`.
        // 96 bytes is KEY_MAX/8, so the kernel never writes past the buffer.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGKEY, bits.as_mut_ptr()) };
        rc >= 0 && (bits[(code / 8) as usize] >> (code % 8)) & 1 == 1
    }

    fn abs_value(node: &str, abs: u16) -> Option<i32> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(node).ok()?;
        let mut info = [0u8; 24]; // struct input_absinfo { value, min, max, fuzz, flat, resolution }
        let req: libc::c_ulong =
            (2 << 30) | (24 << 16) | (0x45 << 8) | (0x40 + abs as libc::c_ulong);
        // SAFETY: `f` is a valid evdev fd. EVIOCGABS writes 24-byte `input_absinfo` into `info`.
        // We read only the leading i32 `value`; the buffer is exactly that size.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, info.as_mut_ptr()) };
        (rc >= 0).then(|| i32::from_ne_bytes([info[0], info[1], info[2], info[3]]))
    }

    /// Bind `hid-steam` (gamepad + IMU), enter `gamepad_mode` via the create pulse, land
    /// BTN_A and left-pad ABS_HAT0X, tear down on drop. Needs `hid-steam` + `input` group.
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs hid-steam + the input group"]
    fn backend_binds_and_input_flows() {
        use punktfunk_core::input::gamepad as gs;
        const BTN_A: u16 = 0x130;
        const ABS_HAT0X: u16 = 0x10; // left trackpad X
        let mut pad = SteamDeckPad::open(0).expect("open SteamDeckPad (/dev/uhid + input group?)");
        // Past MODE_ENTER so the b9.6 pulse finishes and the handshake is serviced.
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

    /// Classic-SC `28DE:1102`: bind with no mode-entry pulse; BTN_A and right-stick land
    /// on ABS_RX (right pad).
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs hid-steam + the input group"]
    fn sc_backend_binds_and_input_flows() {
        use punktfunk_core::input::gamepad as gs;
        const BTN_A: u16 = 0x130;
        const ABS_RX: u16 = 0x03;
        let mut pad = SteamDeckPad::open_model(0, SteamModel::Controller)
            .expect("open SC pad (/dev/uhid + input group?)");
        let st = sc_from_gamepad(gs::BTN_A | gs::BTN_PADDLE1, 0, 0, 9000, 0, 0, 0);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(900) {
            let _ = pad.service();
            pad.write_state(&st).expect("write_state");
            std::thread::sleep(Duration::from_millis(4));
        }
        let devs = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            devs.contains("Steam Controller"),
            "SC gamepad evdev not created"
        );
        let node = find_node("Steam Controller").expect("SC evdev node");
        assert!(
            key_is_down(&node, BTN_A),
            "BTN_A not down — SC serialize failed (no mode gate should apply)"
        );
        assert_eq!(
            abs_value(&node, ABS_RX),
            Some(9000),
            "wire right stick did not land on the right pad (ABS_RX)"
        );
    }
}
