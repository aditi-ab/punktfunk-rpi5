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
    edge_paddle_bits, parse_ds_output, serialize_state, DsFeedback, DsState, DS_EDGE_PRODUCT,
    DS_FEATURE_CALIBRATION, DS_FEATURE_FIRMWARE, DS_FEATURE_PAIRING, DS_INPUT_REPORT_LEN,
    DS_PRODUCT, DS_TOUCH_H, DS_TOUCH_W, DS_VENDOR, DUALSENSE_EDGE_RDESC, DUALSENSE_RDESC,
};
use crate::inject::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::RichInput;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

// /dev/uhid event ABI (linux/uhid.h). `struct uhid_event` is __packed__: a u32 `type` then a
// union whose largest member is uhid_create2_req (128+64+64 + 2+2 + 4*4 + rd_data[4096] = 4372).
const UHID_PATH: &str = "/dev/uhid";
const UHID_DESTROY: u32 = 1;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
const UHID_EVENT_SIZE: usize = 4 + 4372; // type + union (create2)
const BUS_USB: u16 = 0x03;

/// Copy a NUL-padded C string field into the event buffer.
fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]); // rest already zero (NUL-terminated)
}

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
    ts: u32,
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
        let mut ds = DualSensePad { fd, seq: 0, ts: 0 };
        ds.send_create2(index, id)
            .context("UHID_CREATE2 DualSense")?;
        Ok(ds)
    }

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
        self.ts = self.ts.wrapping_add(1); // monotonic sensor timestamp is all the kernel needs
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.seq, self.ts);

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
                    let data: &[u8] = match ev[8] {
                        0x05 => DS_FEATURE_CALIBRATION,
                        0x09 => DS_FEATURE_PAIRING,
                        0x20 => DS_FEATURE_FIRMWARE,
                        _ => &[],
                    };
                    let _ = self.reply_get_report(id, data);
                }
                _ => {} // Start/Stop/Open/Close/SetReport — ignore
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
    remap: crate::inject::steam_remap::RemapConfig,
}

impl Default for DsLinuxProto {
    fn default() -> DsLinuxProto {
        DsLinuxProto {
            remap: crate::inject::steam_remap::RemapConfig::from_env(),
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
    fn merge_frame(&self, prev: &DsState, f: &crate::gamestream::gamepad::GamepadFrame) -> DsState {
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
        }
    }
}

/// All virtual DualSense Edge pads of a session — `PUNKTFUNK_GAMEPAD=edge`, or the per-pad kind a
/// client declares for a paddle-bearing physical controller.
pub type DualSenseEdgeManager = UhidManager<DsEdgeLinuxProto>;
