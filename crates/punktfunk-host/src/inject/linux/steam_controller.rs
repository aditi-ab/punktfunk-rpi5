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
    btn, parse_steam_output, sc_from_gamepad, serial_reply, serialize_deck_state,
    serialize_sc_state, SteamModel, SteamState, STEAMDECK_RDESC, STEAM_REPORT_LEN, STEAM_VENDOR,
};
use crate::inject::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::RichInput;
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

/// A virtual Steam Deck **or classic Steam Controller** backed by `/dev/uhid` (same driver, two
/// identities/report layouts — see [`SteamModel`]). Dropping it destroys the device (the kernel
/// tears down the bound `hid-steam` interface + its evdevs).
pub struct SteamDeckPad {
    fd: File,
    model: SteamModel,
    seq: u32,
    created: Instant,
    /// When `b9.6` started being continuously held in our OUTPUT (anti-toggle guard); `None` = not.
    menu_hold_since: Option<Instant>,
}

impl SteamDeckPad {
    pub fn open(index: u8) -> Result<SteamDeckPad> {
        SteamDeckPad::open_model(index, SteamModel::Deck)
    }

    /// Open under a specific Steam identity. The classic Controller's `ID_CONTROLLER_STATE` path
    /// has NO `gamepad_mode` gate in the kernel (only the Deck's parser early-returns under
    /// lizard mode), so the SC skips the whole mode-entry machinery.
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
        put_cstr(&mut ev, 4, 128, &format!("Punktfunk {name} {index}")); // name[128]
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/{phys}/{index}")); // phys[64]
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-{uniq}-{index}")); // uniq[64]
        ev[260..262].copy_from_slice(&(STEAMDECK_RDESC.len() as u16).to_ne_bytes()); // rd_size
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus
        ev[264..268].copy_from_slice(&STEAM_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&self.model.product().to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
        ev[280..280 + STEAMDECK_RDESC.len()].copy_from_slice(STEAMDECK_RDESC);
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Serialize `st` under this pad's model (Deck reports get the gamepad-mode entry overlay +
    /// anti-toggle guard applied) and write it.
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
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes()); // input2.size
        ev[6..6 + r.len()].copy_from_slice(&r); // input2.data
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// True while still pulsing the mode-switch at creation (the caller force-writes during this).
    /// Deck-only — the SC's kernel parser has no mode gate.
    fn in_mode_entry(&self) -> bool {
        self.model == SteamModel::Deck && self.created.elapsed() < MODE_ENTER
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

/// The transport a manager pad drives. UHID is universal but Steam Input won't promote it (a UHID
/// device has no USB interface number, `Interface: -1`); the USB **gadget** (`raw_gadget`, SteamOS)
/// and **usbip** (`vhci_hcd`, universal) both present the controller on USB interface 2, which Steam
/// Input *does* promote. Selected per-pad by [`open_transport`]. (`pub`: the type appears as
/// `type Pad` in the `PadProto` impl, a public trait.)
pub enum DeckTransport {
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

/// The Steam-Deck-specific half of the shared stateful manager (see [`PadProto`]): the transport
/// open (usbip → gadget → UHID fallback via [`open_transport`], which logs its own per-transport
/// outcome), the [`SteamState`] mappers, and the kernel-handshake service pass. Lifecycle (slot
/// table, unplug sweep, heartbeat, rumble dedup) lives in [`UhidManager`]; the gamepad-mode-entry
/// pulse rides the [`force_heartbeat`](PadProto::force_heartbeat) hook.
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

    /// Merge buttons/sticks/triggers, preserving the rich-plane fields (trackpad + motion arrive
    /// separately and must survive a button-only frame).
    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &crate::gamestream::gamepad::GamepadFrame,
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
        // Trackpad CLICK arrives on the rich plane too and must survive a button-only frame,
        // exactly like touch/coords/motion above. It lives in its own fields (not `buttons`,
        // which `from_gamepad` just rebuilt) so preserving it can't strand the BTN_TOUCHPAD
        // wire-button's RPAD_CLICK — the two are OR'd only at serialize.
        s.lpad_click = prev.lpad_click;
        s.rpad_click = prev.rpad_click;
        s
    }

    fn apply_rich(&self, st: &mut SteamState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut DeckTransport, st: &SteamState) {
        pad.write_state(st);
    }

    /// Answer the kernel handshake and forward rumble on the universal plane. The Steam Deck has
    /// no rich host→client feedback plane (no lightbar / adaptive triggers), so `hidout` stays
    /// empty.
    fn service(&self, pad: &mut DeckTransport, _idx: u8) -> PadFeedback {
        PadFeedback {
            rumble: pad.service(),
            hidout: Vec::new(),
            game_drove: None,
        }
    }

    /// Force a steady stream while a pad is still pulsing its gamepad-mode entry (so the `b9.6`
    /// toggle completes even with no game input).
    fn force_heartbeat(&self, pad: &DeckTransport) -> bool {
        pad.in_mode_entry()
    }
}

/// All virtual Steam Deck pads of a session — the Steam analogue of
/// [`DualSenseManager`](super::dualsense::DualSenseManager), selected with
/// `PUNKTFUNK_GAMEPAD=steamdeck`. Button/stick frames arrive via `handle`; the trackpads + motion
/// via `apply_rich`; `pump` services the kernel handshake + routes rumble back; `heartbeat` keeps
/// the pad alive (and drives the mode-entry pulse) — all from the shared [`UhidManager`].
pub type SteamControllerManager = UhidManager<SteamProto>;

/// The **classic Steam Controller** half of the shared stateful manager: the same `hid-steam`
/// driver under the wired-SC identity (`28DE:1102`, `ID_CONTROLLER_STATE`), UHID-only in v1 —
/// the usbip/gadget transports present the Deck's captured 3-interface USB device, and the SC's
/// wired interface layout hasn't been captured, so there is no Steam-Input promotion (the same
/// degraded-but-working state the Deck had pre-usbip; acceptable for discontinued hardware).
///
/// Deltas vs the Deck (see [`sc_from_gamepad`]/[`serialize_sc_state`]): one stick + two pads +
/// two grips — the wire right stick drives the right pad, a left-pad contact shadows the left
/// stick, wire PADDLE1/2 land on the two grips (3/4 fold via the remap policy), and the kernel
/// registers neither FF rumble nor a sensors evdev for this model (feedback stays empty).
pub struct ScProto {
    /// Fallback policy for the wire paddles beyond the SC's two grips (PADDLE3/4).
    remap: crate::inject::steam_remap::RemapConfig,
}

impl Default for ScProto {
    fn default() -> ScProto {
        ScProto {
            remap: crate::inject::steam_remap::RemapConfig::from_env(),
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

    /// Merge buttons/sticks/triggers, preserving the rich-plane fields. PADDLE1/2 map natively to
    /// the SC's two grips inside [`sc_from_gamepad`]; only 3/4 go through the fold policy — mask
    /// the native pair out of the fold input so the policy can't double-fire them.
    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &crate::gamestream::gamepad::GamepadFrame,
    ) -> SteamState {
        use punktfunk_core::input::gamepad as gs;
        let native = f.buttons & (gs::BTN_PADDLE1 | gs::BTN_PADDLE2);
        let folded = crate::inject::steam_remap::fold_paddles(
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
        // The right pad carries the wire right stick each frame; a rich right-pad contact
        // (TouchpadEx surface 2) overrides it only while the stick is centered — the stick is
        // the primary camera surface on this mapping.
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

    fn write_state(&self, pad: &mut SteamDeckPad, st: &SteamState) {
        let _ = pad.write_state(st);
    }

    /// Answer the kernel handshake (serial GET_REPORT + settings SET_REPORTs). The kernel
    /// registers no FF device for the classic SC, so rumble feedback can only arrive from a
    /// hidraw client (`0xEB`) — surfaced if it ever does.
    fn service(&self, pad: &mut SteamDeckPad, _idx: u8) -> PadFeedback {
        PadFeedback {
            rumble: pad.service(),
            hidout: Vec::new(),
            game_drove: None,
        }
    }
}

/// All virtual classic Steam Controllers of a session — `PUNKTFUNK_GAMEPAD=steamcontroller`, or
/// the per-pad kind a client declares for a physical SC.
pub type SteamCtrlManager = UhidManager<ScProto>;

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

    /// On-box smoke for the classic-SC identity: binds `hid-steam` as `28DE:1102`, input flows
    /// with NO mode-entry pulse (the SC parser has no gamepad_mode gate), a held A + right-stick
    /// deflection land on the evdev (BTN_A + ABS_RX — the right PAD surface), and a grip lands
    /// on BTN_GRIPR (0x2c5? — kernel BTN_GRIPR = 0x2c5 on new kernels / check via bitmap).
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
