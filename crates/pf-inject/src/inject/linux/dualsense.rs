//! Virtual Sony DualSense via UHID — the rich-controller path (roadmap §5).
//!
//! Unlike the uinput X-Box-360 pad ([`super::gamepad`]), which only carries buttons + axes + a
//! rumble back-channel, a UHID device presents a *real* DualSense HID interface to the kernel:
//! `hid-playstation` binds it (matched by VID `054C`/PID `0CE6`) and exposes the full controller
//! — gamepad, motion sensors, touchpad, lightbar + player LEDs, and adaptive triggers — to games.
//! The host writes HID **input** reports (report `0x01`, our controller state) and reads HID
//! **output** reports (report `0x02`, a game's rumble/LED/trigger feedback) back, which it
//! forwards to the client as [`punktfunk_core::quic::HidOutput`].
//!
//! The transport-independent contract (report descriptor, feature blobs, [`DsState`], the `0x01`
//! serializer and `0x02` parser) lives in [`super::dualsense_proto`], shared with the Windows
//! UMDF-driver backend; this module is just the `/dev/uhid` plumbing around it.

use super::dualsense_proto::{
    ds_pairing_reply, edge_paddle_bits, parse_ds_output, serialize_state, DsFeedback, DsState,
    DS_EDGE_PRODUCT, DS_FEATURE_CALIBRATION, DS_FEATURE_FIRMWARE, DS_INPUT_REPORT_LEN, DS_PRODUCT,
    DS_TOUCH_H, DS_TOUCH_W, DS_VENDOR, DUALSENSE_EDGE_RDESC, DUALSENSE_RDESC,
};
use crate::sensor_clock::SensorClock;
use crate::uhid_abi::{
    put_cstr, BUS_USB, HID_MAX_DESCRIPTOR_SIZE, UHID_CREATE2, UHID_DESTROY, UHID_EVENT_SIZE,
    UHID_GET_REPORT, UHID_GET_REPORT_REPLY, UHID_INPUT2, UHID_OUTPUT, UHID_PATH, UHID_SET_REPORT,
    UHID_SET_REPORT_REPLY,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::RichInput;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

/// The UHID identity a [`DualSensePad`] is created with — the plain DualSense or the Edge (same
/// driver, same report codec; the Edge differs by PID + descriptor and carries the four extra
/// `buttons[2]` bits). Mirrors the uinput pad's `PadIdentity` shape.
pub struct DsUhidIdentity {
    product: u32,
    rdesc: &'static [u8],
    /// Device name prefix ("Punktfunk <name> <index>").
    name: &'static str,
    /// Path token for the phys string ("punktfunk/<phys>/<index>").
    phys: &'static str,
    /// Short slug for the uniq string ("punktfunk-<slug>-<index>").
    slug: &'static str,
}

impl DsUhidIdentity {
    pub const fn dualsense() -> DsUhidIdentity {
        DsUhidIdentity {
            product: DS_PRODUCT,
            rdesc: DUALSENSE_RDESC,
            name: "DualSense",
            phys: "dualsense",
            slug: "ds",
        }
    }

    pub const fn dualsense_edge() -> DsUhidIdentity {
        DsUhidIdentity {
            product: DS_EDGE_PRODUCT,
            rdesc: DUALSENSE_EDGE_RDESC,
            name: "DualSense Edge",
            phys: "dualsense-edge",
            slug: "dsedge",
        }
    }
}

/// A virtual DualSense / DualSense Edge backed by `/dev/uhid` (hand-rolled codec — no bindgen,
/// mirroring the uinput pad's style). Dropping it destroys the device (the kernel tears down the
/// bound `hid-playstation` interface).
pub struct DualSensePad {
    fd: File,
    seq: u8,
    clock: SensorClock,
}

impl DualSensePad {
    /// Create the UHID pad for wire index `index` under `id`'s identity (`index` is used only to
    /// make the device name/uniq unique).
    pub fn open(index: u8, id: &DsUhidIdentity) -> Result<DualSensePad> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the 60-punktfunk.rules uhid rule installed + are you in 'input'?)")
            })?;
        let mut ds = DualSensePad {
            fd,
            seq: 0,
            clock: SensorClock::dualsense(),
        };
        ds.send_create2(index, id)
            .context("UHID_CREATE2 DualSense")?;
        Ok(ds)
    }

    /// Send UHID_CREATE2 under `id`'s identity. The uniq written here is cosmetic:
    /// `hid-playstation` replaces it with the MAC from the pairing feature report (see
    /// [`ds_pairing_reply`]) as soon as it binds.
    fn send_create2(&mut self, index: u8, id: &DsUhidIdentity) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // union (uhid_create2_req) starts at byte 4.
        put_cstr(&mut ev, 4, 128, &format!("Punktfunk {} {index}", id.name)); // name[128]
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/{}/{index}", id.phys)); // phys[64]
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-{}-{index}", id.slug)); // uniq[64]
        ev[260..262].copy_from_slice(&(id.rdesc.len() as u16).to_ne_bytes()); // rd_size
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus
        ev[264..268].copy_from_slice(&DS_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&id.product.to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
        ev[280..280 + id.rdesc.len()].copy_from_slice(id.rdesc); // rd_data
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Serialize `st` into report `0x01` and write it to the kernel (UHID_INPUT2).
    pub fn write_state(&mut self, st: &DsState) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let ts = self.clock.ds_ticks(Instant::now());
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.seq, ts);

        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes()); // input2.size
        ev[6..6 + r.len()].copy_from_slice(&r); // input2.data
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Service the device, non-blocking: answer the kernel's feature-report GET_REPORTs (calibration
    /// / pairing / firmware — required during `hid-playstation` init, or no input devices appear)
    /// and parse any HID OUTPUT reports (rumble / lightbar / player LEDs / adaptive triggers) into
    /// a [`DsFeedback`] for pad `pad`. Call frequently — especially right after [`open`] so the
    /// init handshake completes. The fd is `O_NONBLOCK`, so once drained `read` returns `WouldBlock`.
    pub fn service(&mut self, pad: u8) -> DsFeedback {
        let mut fb = DsFeedback::default();
        let mut ev = [0u8; UHID_EVENT_SIZE];
        while let Ok(n) = self.fd.read(&mut ev) {
            if n < UHID_EVENT_SIZE {
                break;
            }
            match u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]) {
                UHID_OUTPUT => {
                    // uhid_output_req: data[4096] at [4..4100], size u16 at [4100..4102].
                    let size = u16::from_ne_bytes([ev[4100], ev[4101]]) as usize;
                    let end = 4 + size.min(HID_MAX_DESCRIPTOR_SIZE);
                    parse_ds_output(pad, &ev[4..end], &mut fb);
                }
                UHID_GET_REPORT => {
                    // uhid_get_report_req: id u32 [4..8], rnum u8 [8].
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    // Per-pad MAC: hid-playstation adopts it as the HID uniq, and SDL/Steam
                    // dedup controllers by that serial (see `ds_pairing_reply`).
                    let pairing = ds_pairing_reply(pad);
                    let data: &[u8] = match ev[8] {
                        0x05 => DS_FEATURE_CALIBRATION,
                        0x09 => &pairing,
                        0x20 => DS_FEATURE_FIRMWARE,
                        _ => &[],
                    };
                    let _ = self.reply_get_report(id, data);
                }
                UHID_SET_REPORT => {
                    // Ack (err=0) so a SET_REPORT writer doesn't block on the kernel's 5 s
                    // timeout. Nothing to parse: every known DualSense writer sends its feedback
                    // as OUTPUT reports (handled above), never SET_REPORT.
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let _ = self.reply_set_report(id);
                }
                _ => {} // Start/Stop/Open/Close — ignore
            }
        }
        fb
    }

    fn reply_get_report(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        // uhid_get_report_reply_req: id u32 [4..8], err u16 [8..10], size u16 [10..12], data [12..].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        let err: u16 = if data.is_empty() { 5 } else { 0 }; // EIO if we don't know the report
        ev[8..10].copy_from_slice(&err.to_ne_bytes());
        ev[10..12].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[12..12 + data.len()].copy_from_slice(data);
        self.fd
            .write_all(&ev)
            .context("write UHID_GET_REPORT_REPLY")?;
        Ok(())
    }

    fn reply_set_report(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
        // uhid_set_report_reply_req: id u32 [4..8], err u16 [8..10].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes()); // err 0 (ack)
        self.fd
            .write_all(&ev)
            .context("write UHID_SET_REPORT_REPLY")?;
        Ok(())
    }
}

impl Drop for DualSensePad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// The DualSense-specific half of the shared stateful manager (see [`PadProto`]): UHID transport
/// open, the [`DsState`] mappers, and the kernel-handshake service pass. Everything lifecycle-
/// shaped (slot table, unplug sweep, heartbeat, feedback dedup) lives in [`UhidManager`].
pub struct DsLinuxProto {
    /// Fallback policy for the Steam back grips a client may send (the DualSense has no back-button
    /// HID slot). `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for DsLinuxProto {
    fn default() -> DsLinuxProto {
        DsLinuxProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for DsLinuxProto {
    type Pad = DualSensePad;
    type State = DsState;
    const LABEL: &'static str = "DualSense";
    const DEVICE: &'static str = "DualSense";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DualSensePad> {
        let p = DualSensePad::open(idx, &DsUhidIdentity::dualsense())?;
        tracing::info!(
            index = idx,
            "virtual DualSense created (UHID hid-playstation)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Merge buttons/sticks/triggers from the frame, preserving touch + motion + pad clicks (those
    /// come on the rich-input plane and must survive a button-only frame).
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        // Steam back grips have no DualSense slot — fold them onto standard buttons per the
        // configured policy (default drop) so they aren't silently lost.
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
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

    fn neutralize_gyro(&self, st: &mut DsState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut DsState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut DualSensePad, st: &DsState) {
        let _ = pad.write_state(st);
    }

    /// Answer the kernel's init handshake (it blocks `hid-playstation` init until its GET_REPORTs
    /// are answered — call frequently) and parse a game's feedback: motor rumble on the universal
    /// 0xCA plane, the rich lightbar/player-LED/trigger events on the 0xCD plane.
    fn service(&self, pad: &mut DualSensePad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            rumble: fb.rumble,
            hidout: fb.hidout,
            // Rumble-plane liveness (arms the shared abandoned-rumble force-off). evdev-FF games
            // going through hid-playstation get their stops surfaced reliably, but Steam Input
            // drives this pad over hidraw DIRECTLY — the same abandonment semantics as a Windows
            // game, so the same watchdog applies. SDL-class writers re-assert a held level every
            // ~2 s (inside the idle window), and a writer that goes silent on a latched level is
            // cut exactly as real firmware decay would cut it on a physical pad.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// All virtual DualSense pads of a session — the rich-controller analog of
/// [`GamepadManager`](super::gamepad::GamepadManager), selected with `PUNKTFUNK_GAMEPAD=dualsense`.
///
/// Unlike the uinput pad, a DualSense carries touchpad + motion, which arrive on a *separate*
/// rich-input plane (`apply_rich`) from the button/stick frames (`handle`); the shared
/// [`UhidManager`] keeps each pad's full [`DsState`], re-emits the merged report whenever either
/// source changes, and heartbeats it through input silence (a real DualSense streams report `0x01`
/// continuously — `hid-playstation`/Proton/SDL treat a multi-second gap as an unplug).
pub type DualSenseManager = UhidManager<DsLinuxProto>;

/// The DualSense **Edge** half of the shared stateful manager: the plain-DualSense transport and
/// report codec under the Edge USB identity (`054C:0DF2` + the Edge descriptor), with the four
/// wire back-grip bits mapped onto the Edge's native `buttons[2]` slots instead of the
/// fold/drop policy — the whole point of this backend (a client's Deck grips / Elite paddles
/// stop vanishing). No remap config: every paddle has a native home.
///
/// Kernel note: `hid-playstation` binds the Edge PID since 6.1 (forced vibration-v2 output), but
/// only kernels ≥ 7.2 surface the Fn/back bits as evdev keys (`BTN_TRIGGER_HAPPY1..4`); SDL /
/// Steam Input read the report off hidraw and see them on any kernel.
#[derive(Default)]
pub struct DsEdgeLinuxProto;

impl PadProto for DsEdgeLinuxProto {
    type Pad = DualSensePad;
    type State = DsState;
    const LABEL: &'static str = "DualSense Edge";
    const DEVICE: &'static str = "DualSense Edge";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DualSensePad> {
        let p = DualSensePad::open(idx, &DsUhidIdentity::dualsense_edge())?;
        tracing::info!(
            index = idx,
            "virtual DualSense Edge created (UHID hid-playstation)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Merge buttons/sticks/triggers from the frame, preserving the rich-plane fields — like the
    /// plain DualSense, EXCEPT the wire paddles are not folded away: they land on the Edge's own
    /// `buttons[2]` bits (rebuilt from every button frame, so no extra persistence).
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let mut s = DsState::from_gamepad(
            f.buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.buttons[2] |= edge_paddle_bits(f.buttons);
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

    fn neutralize_gyro(&self, st: &mut DsState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut DsState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut DualSensePad, st: &DsState) {
        let _ = pad.write_state(st);
    }

    /// Same kernel handshake + feedback parse as the plain DualSense — the Edge's GET_REPORT set
    /// (calibration 0x05 / pairing 0x09 / firmware 0x20) and output report 0x02 are identical
    /// (the Edge's rumble arrives via the vibration-v2 valid_flag2 bit, which
    /// [`parse_ds_output`] already handles).
    fn service(&self, pad: &mut DualSensePad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            rumble: fb.rumble,
            hidout: fb.hidout,
            // Rumble-plane liveness (arms the shared abandoned-rumble force-off). evdev-FF games
            // going through hid-playstation get their stops surfaced reliably, but Steam Input
            // drives this pad over hidraw DIRECTLY — the same abandonment semantics as a Windows
            // game, so the same watchdog applies. SDL-class writers re-assert a held level every
            // ~2 s (inside the idle window), and a writer that goes silent on a latched level is
            // cut exactly as real firmware decay would cut it on a physical pad.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// All virtual DualSense Edge pads of a session — `PUNKTFUNK_GAMEPAD=edge`, or the per-pad kind a
/// client declares for a paddle-bearing physical controller.
pub type DualSenseEdgeManager = UhidManager<DsEdgeLinuxProto>;

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::HidOutput;
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant};

    /// evdev nodes whose input-device name contains `name`: (full name, /dev/input/eventN).
    fn find_nodes(name: &str) -> Vec<(String, String)> {
        let s = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        let mut out = Vec::new();
        let mut cur = String::new();
        for line in s.lines() {
            if let Some(n) = line.strip_prefix("N: Name=") {
                cur = n.trim_matches('"').to_string();
            } else if let Some(h) = line.strip_prefix("H: Handlers=") {
                if cur.contains(name) {
                    if let Some(ev) = h.split_whitespace().find(|t| t.starts_with("event")) {
                        out.push((cur.clone(), format!("/dev/input/{ev}")));
                    }
                }
            }
        }
        out
    }

    /// Whether the evdev at `node` advertises EV_FF (0x15) — the rumble-capable gamepad node
    /// (the touchpad / motion / headset siblings don't).
    fn has_ff(node: &str) -> bool {
        let Ok(f) = std::fs::OpenOptions::new().read(true).open(node) else {
            return false;
        };
        let mut bits = [0u8; 8];
        // EVIOCGBIT(0, 8): the device's event-type bitmap.
        let req: libc::c_ulong = (2 << 30) | (8 << 16) | (0x45 << 8) | 0x20;
        // SAFETY: EVIOCGBIT(0) copies at most 8 bytes (EV_MAX/8 < 8) into the live `bits` buffer
        // behind the valid evdev fd `f`; the kernel never writes past the ioctl's size argument.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, bits.as_mut_ptr()) };
        rc >= 0 && (bits[0x15 / 8] >> (0x15 % 8)) & 1 == 1
    }

    /// Upload an FF_RUMBLE effect on `node` and play it, exactly like SDL's evdev haptic backend.
    /// Returns the OPEN fd with the id — closing the fd erases the process's effects (stopping
    /// the rumble), so the caller must hold it while asserting.
    fn evdev_rumble(node: &str, strong: u16, weak: u16) -> std::io::Result<(std::fs::File, i16)> {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)?;
        // struct ff_effect (48 B): type u16, id s16, direction u16, trigger, replay{len,delay},
        // pad to 16, union (ff_rumble_effect { strong, weak }).
        let mut eff = [0u8; 48];
        eff[0..2].copy_from_slice(&0x50u16.to_ne_bytes()); // FF_RUMBLE
        eff[2..4].copy_from_slice(&(-1i16).to_ne_bytes()); // id: kernel assigns
        eff[10..12].copy_from_slice(&5000u16.to_ne_bytes()); // replay.length ms
        eff[16..18].copy_from_slice(&strong.to_ne_bytes());
        eff[18..20].copy_from_slice(&weak.to_ne_bytes());
        // EVIOCSFF = _IOW('E', 0x80, struct ff_effect)
        let req: libc::c_ulong = (1 << 30) | (48 << 16) | (0x45 << 8) | 0x80;
        // SAFETY: EVIOCSFF reads/writes the 48-byte ff_effect behind the valid fd `f`; `eff` is
        // exactly sizeof(struct ff_effect) and outlives the synchronous call.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, eff.as_mut_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let id = i16::from_ne_bytes([eff[2], eff[3]]);
        // struct input_event (24 B on 64-bit): timeval 16, type u16, code u16, value s32.
        let mut ev = [0u8; 24];
        ev[16..18].copy_from_slice(&0x15u16.to_ne_bytes()); // EV_FF
        ev[18..20].copy_from_slice(&(id as u16).to_ne_bytes());
        ev[20..24].copy_from_slice(&1i32.to_ne_bytes()); // play
        f.write_all(&ev)?;
        Ok((f, id))
    }

    /// `(HID_NAME, HID_UNIQ, /dev/hidrawN)` for every hidraw class device.
    fn hidraw_devices() -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir("/sys/class/hidraw") else {
            return out;
        };
        for e in dir.flatten() {
            let ue = std::fs::read_to_string(e.path().join("device/uevent")).unwrap_or_default();
            let field = |k: &str| {
                ue.lines()
                    .find_map(|l| l.strip_prefix(k))
                    .unwrap_or_default()
                    .to_string()
            };
            out.push((
                field("HID_NAME="),
                field("HID_UNIQ="),
                format!("/dev/{}", e.file_name().to_string_lossy()),
            ));
        }
        out
    }

    /// Service `pad` for `ms`, accumulating every captured feedback pass (all rumble levels in
    /// order + all rich events) while keeping the input heartbeat going.
    fn collect(pad: &mut DualSensePad, st: &DsState, ms: u64) -> (Vec<(u16, u16)>, Vec<HidOutput>) {
        let start = Instant::now();
        let (mut levels, mut hidout) = (Vec::new(), Vec::<HidOutput>::new());
        while start.elapsed() < Duration::from_millis(ms) {
            let fb = pad.service(0);
            levels.extend(fb.rumble);
            hidout.extend(fb.hidout);
            let _ = pad.write_state(st);
            std::thread::sleep(Duration::from_millis(4));
        }
        (levels, hidout)
    }

    /// On-box proof of the full Linux feedback surface, playing the GAME's role against a real
    /// kernel: chain A drives rumble through evdev force feedback (`hid-playstation`'s ff-memless
    /// → UHID_OUTPUT — what SDL/Steam fall back to without hidraw); chain B writes a raw DS5
    /// output report to the pad's hidraw node (SDL/Steam's real path, and the ONLY way adaptive
    /// triggers can arrive) and expects rumble + lightbar + player LEDs + both trigger blocks
    /// back verbatim. Also pins the per-pad pairing MAC: two pads must present distinct uniqs or
    /// SDL/Steam dedup them into one controller.
    #[test]
    #[ignore = "creates real /dev/uhid devices; needs hid-playstation, the input group, and the 60-punktfunk.rules hidraw rules"]
    fn feedback_flows_via_evdev_ff_and_hidraw() {
        let mut pad0 = DualSensePad::open(0, &DsUhidIdentity::dualsense()).expect("open pad 0");
        let mut pad1 = DualSensePad::open(1, &DsUhidIdentity::dualsense()).expect("open pad 1");
        let st = DsState::neutral();
        // Let hid-playstation complete its GET_REPORT handshakes and register input devices.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            let _ = pad0.service(0);
            let _ = pad1.service(1);
            let _ = pad0.write_state(&st);
            let _ = pad1.write_state(&st);
            std::thread::sleep(Duration::from_millis(4));
        }
        let nodes = find_nodes("Punktfunk DualSense 0");
        assert!(
            !nodes.is_empty(),
            "hid-playstation did not bind the uhid device"
        );
        let ff_node = nodes
            .iter()
            .map(|(_, n)| n.as_str())
            .find(|n| has_ff(n))
            .expect("no FF-capable evdev among the pad's input devices");

        // Per-pad MAC: hid-playstation adopts the pairing-report MAC as HID_UNIQ; the two pads
        // must differ (the SDL/Steam serial-dedup regression, see `ds_pairing_reply`).
        let hidraws = hidraw_devices();
        let uniq = |name: &str| {
            hidraws
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, u, _)| u.clone())
                .unwrap_or_else(|| panic!("no hidraw for {name} in {hidraws:?}"))
        };
        assert_ne!(
            uniq("Punktfunk DualSense 0"),
            uniq("Punktfunk DualSense 1"),
            "pads share one pairing MAC — SDL/Steam will dedup them into one controller"
        );

        // ---- Chain A: evdev force feedback ----
        let (ff_fd, _) = evdev_rumble(ff_node, 0xC000, 0x4000).expect("EVIOCSFF/play");
        let (levels, _) = collect(&mut pad0, &st, 1000);
        assert!(
            levels.iter().any(|&(l, h)| l > 0 || h > 0),
            "evdev FF rumble never surfaced as UHID_OUTPUT: {levels:?}"
        );
        drop(ff_fd); // closing erases the effect: the stop must surface too
        let (levels, _) = collect(&mut pad0, &st, 800);
        assert!(
            levels.contains(&(0, 0)),
            "erase-on-close never produced a rumble stop: {levels:?}"
        );

        // ---- Chain B: raw DS5 output report over hidraw ----
        let hr = hidraws
            .iter()
            .find(|(n, _, _)| n == "Punktfunk DualSense 0")
            .map(|(_, _, d)| d.clone())
            .unwrap();
        let mut rep = [0u8; 48];
        rep[0] = 0x02; // USB output report id
        rep[1] = 0x03 | 0x04 | 0x08; // flag0: compat vibration + haptics select + R2 + L2
        rep[2] = 0x04 | 0x10; // flag1: lightbar + player LEDs
        rep[3] = 0x60; // motor right (high)
        rep[4] = 0xA0; // motor left (low)
        rep[11] = 0x21; // R2 trigger block: weapon mode + params
        rep[12] = 0x04;
        rep[13] = 0x07;
        rep[22] = 0x26; // L2 trigger block: vibration mode + params
        rep[23] = 0x02;
        rep[44] = 0x04; // player LED middle
        rep[45] = 0x10;
        rep[46] = 0x20;
        rep[47] = 0x30;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&hr)
            .and_then(|mut f| std::io::Write::write_all(&mut f, &rep))
            .unwrap_or_else(|e| {
                panic!(
                    "cannot write {hr} as this user ({e}) — Steam/SDL would be equally blocked; \
                     are the 60-punktfunk.rules hidraw rules installed?"
                )
            });
        let (levels, hidout) = collect(&mut pad0, &st, 1000);
        assert!(
            levels.contains(&(0xA000, 0x6000)),
            "hidraw rumble did not surface: {levels:?}"
        );
        let triggers: Vec<_> = hidout
            .iter()
            .filter_map(|h| match h {
                HidOutput::Trigger { which, effect, .. } => Some((*which, effect.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            triggers.len(),
            2,
            "expected both trigger blocks: {hidout:?}"
        );
        assert!(
            triggers.contains(&(1, rep[11..22].to_vec())),
            "R2 block not verbatim"
        );
        assert!(
            triggers.contains(&(0, rep[22..33].to_vec())),
            "L2 block not verbatim"
        );
        assert!(
            hidout.iter().any(|h| matches!(
                h,
                HidOutput::Led {
                    r: 0x10,
                    g: 0x20,
                    b: 0x30,
                    ..
                }
            )),
            "lightbar not surfaced: {hidout:?}"
        );
        assert!(
            hidout
                .iter()
                .any(|h| matches!(h, HidOutput::PlayerLeds { bits: 0x04, .. })),
            "player LEDs not surfaced: {hidout:?}"
        );
    }
}
