//! Virtual Steam Controller 2 (Triton) over `/dev/uhid` — as-is passthrough for
//! [`GamepadPref::SteamController2`](punktfunk_core::config::GamepadPref). Descriptor,
//! report ids, typed fallback, rumble parser: [`super::triton_proto`].
//!
//! Mainline `hid-steam` does not bind `28DE:1302`, so the node is `hid-generic` hidraw
//! with no evdev. Steam Input over hidraw is the only consumer; `gamepad_mode` does
//! not apply. [`RichInput::HidReport`](punktfunk_core::quic::RichInput) is written
//! unchanged; Steam SET_REPORT / OUTPUT writes are acked and queued for the physical pad.
//!
//! Steam ignores UHID (`Interface: -1`). Preferred transport is
//! [`super::triton_usbip`] (`vhci_hcd`); this module is the fallback when that is missing.

use super::triton_proto::{
    parse_triton_rumble, serialize_triton_state, strip_report_prefix, triton_feature_reply,
    triton_serial, triton_unit_id, TritonState, TRITON_RDESC, TRITON_STATE_LEN, TRITON_VENDOR,
    TRITON_WIRED_PRODUCT,
};
use crate::uhid_abi::{
    put_cstr, BUS_USB, HID_MAX_DESCRIPTOR_SIZE, UHID_CREATE2, UHID_DESTROY, UHID_EVENT_SIZE,
    UHID_GET_REPORT, UHID_GET_REPORT_REPLY, UHID_INPUT2, UHID_OUTPUT, UHID_PATH, UHID_SET_REPORT,
    UHID_SET_REPORT_REPLY,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

/// `/dev/uhid` Triton pad. Drop issues `UHID_DESTROY`.
pub struct TritonPad {
    fd: File,
    /// Synth-mode sequence; the raw path carries the physical pad's own seq.
    seq: u8,
    /// Steam writes since the last service pass, kind-tagged for the 0xCD plane.
    pending_raw: Vec<(u8, Vec<u8>)>,
    /// Last feature SET_REPORT (id-first) — the query half of the Valve GET dance.
    last_set: Vec<u8>,
    serial: String,
    unit_id: u32,
    /// Last GET command logged, so the tester line fires once per distinct cmd.
    last_get_logged: u8,
}

impl TritonPad {
    pub fn open(index: u8) -> Result<TritonPad> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the uhid udev rule installed + are you in 'input'?)")
            })?;
        let mut pad = TritonPad {
            fd,
            seq: 0,
            pending_raw: Vec::new(),
            last_set: Vec::new(),
            serial: triton_serial(index),
            unit_id: triton_unit_id(index),
            last_get_logged: 0,
        };
        pad.send_create2(index).context("UHID_CREATE2 Triton pad")?;
        Ok(pad)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // Steam matches VID/PID, not the product string. Keep the Punktfunk prefix
        // every virtual pad uses.
        put_cstr(
            &mut ev,
            4,
            128,
            &format!("Punktfunk Steam Controller 2 {index}"),
        );
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/triton/{index}"));
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-triton-{index}"));
        ev[260..262].copy_from_slice(&(TRITON_RDESC.len() as u16).to_ne_bytes());
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
        ev[264..268].copy_from_slice(&TRITON_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&TRITON_WIRED_PRODUCT.to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes());
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes());
        ev[280..280 + TRITON_RDESC.len()].copy_from_slice(TRITON_RDESC);
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Client raw bytes verbatim, else a synthesized `0x42` state report from typed fields.
    pub fn write_state(&mut self, st: &TritonState) -> Result<()> {
        if st.raw_len > 0 {
            let len = (st.raw_len as usize).min(st.raw.len());
            return self.write_input(&st.raw[..len]);
        }
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; TRITON_STATE_LEN];
        serialize_triton_state(&mut r, st, self.seq);
        self.write_input(&r)
    }

    fn write_input(&mut self, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[6..6 + data.len()].copy_from_slice(data);
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Non-blocking. Ack SET_REPORT (a stall blocks the writer ~5 s), answer GET_REPORT
    /// from canned state (the Valve query cannot round-trip to the physical pad), and
    /// queue Steam's writes for raw forward. Returns rumble if a `0x80` output was seen.
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
                    let rep = strip_report_prefix(&ev[4..end]);
                    if let Some(r) = parse_triton_rumble(rep) {
                        rumble = Some(r);
                    }
                    self.queue_raw(HID_RAW_OUTPUT, rep);
                }
                UHID_SET_REPORT => {
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    // uhid_set_report: id u32, rnum u8, rtype u8, size u16, data — data at ev[12..].
                    let size = u16::from_ne_bytes([ev[10], ev[11]]) as usize;
                    let end = (12 + size.min(HID_MAX_DESCRIPTOR_SIZE)).min(UHID_EVENT_SIZE);
                    let rep = strip_report_prefix(&ev[12..end]).to_vec();
                    if let Some(r) = parse_triton_rumble(&rep) {
                        rumble = Some(r); // some stacks send haptics on the feature path
                    }
                    // Selects the next GET_REPORT answer (Valve query dance).
                    self.queue_raw(HID_RAW_FEATURE, &rep);
                    self.last_set = rep;
                    let _ = self.reply_set_report(id);
                }
                UHID_GET_REPORT => {
                    // Echo last SET's command with a canned payload. The wrong command type
                    // makes Steam drop the pad; the dance cannot round-trip live.
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let reply = triton_feature_reply(&self.last_set, &self.serial, self.unit_id);
                    if reply[1] != self.last_get_logged {
                        self.last_get_logged = reply[1];
                        tracing::debug!(
                            cmd = %format_args!("{:#04x}", reply[1]),
                            "virtual SC2: answering feature GET"
                        );
                    }
                    let _ = self.reply_get_report(id, &reply);
                }
                _ => {}
            }
        }
        rumble
    }

    /// Cap 32 so a hidraw client gone haywire cannot grow the queue between pumps.
    /// Newest wins — these are level-styled commands.
    fn queue_raw(&mut self, kind: u8, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.pending_raw.len() >= 32 {
            self.pending_raw.remove(0);
        }
        self.pending_raw.push((kind, data.to_vec()));
    }

    fn reply_get_report(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
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
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes());
        self.fd.write_all(&ev).context("UHID_SET_REPORT_REPLY")?;
        Ok(())
    }
}

impl Drop for TritonPad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// usbip (`vhci_hcd`) first — a real USB device Steam lists — with UHID as fallback.
/// No gadget rung: no captured gadget layout for Triton, and usbip is universal.
pub enum TritonTransport {
    Usbip(crate::triton_usbip::TritonUsbip),
    Uhid(TritonPad),
}

/// One `service()` pass: rumble `(left, right)` plus raw `(kind, payload)` writes.
type TritonServiced = (Option<(u16, u16)>, Vec<(u8, Vec<u8>)>);

impl TritonTransport {
    fn write_state(&mut self, st: &TritonState) {
        match self {
            TritonTransport::Usbip(u) => u.write_state(st),
            TritonTransport::Uhid(p) => {
                let _ = p.write_state(st);
            }
        }
    }

    fn service(&mut self) -> TritonServiced {
        match self {
            TritonTransport::Usbip(u) => {
                let fb = u.service();
                (fb.rumble, fb.raw)
            }
            TritonTransport::Uhid(p) => {
                let rumble = p.service();
                (rumble, std::mem::take(&mut p.pending_raw))
            }
        }
    }
}

/// Best Steam-visible SC2 transport: usbip (`vhci_hcd`) then UHID. Steam ignores the
/// UHID leg (`Interface: -1`), so fallback is hidraw-only — log the `vhci_hcd` remedy.
fn open_transport(idx: u8, puck: bool) -> Result<TritonTransport> {
    if crate::steam_usbip::usbip_preferred() {
        let opened = if puck {
            crate::triton_usbip::TritonUsbip::open_puck(idx)
        } else {
            crate::triton_usbip::TritonUsbip::open(idx)
        };
        match opened {
            Ok(u) => return Ok(TritonTransport::Usbip(u)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "usbip SC2 unavailable — falling back to UHID")
            }
        }
    }
    let p = TritonPad::open(idx)?;
    tracing::warn!(
        index = idx,
        "virtual Steam Controller 2 created as UHID — Steam WON'T list it (no USB interface; \
         confirmed on-glass). Load vhci_hcd (usbip) so the pad arrives as a real USB device: \
         `sudo modprobe vhci_hcd`, and ensure it loads at boot."
    );
    Ok(TritonTransport::Uhid(p))
}

/// Triton [`PadProto`]: raw mirroring with typed fallback, and raw-forwarding `service`.
#[derive(Default)]
pub struct TritonProto {
    puck: bool,
}

impl TritonProto {
    pub fn puck() -> Self {
        Self { puck: true }
    }
}

impl PadProto for TritonProto {
    type Pad = TritonTransport;
    type State = TritonState;
    const LABEL: &'static str = "Steam Controller 2";
    const DEVICE: &'static str = "Steam Controller 2";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<TritonTransport> {
        open_transport(idx, self.puck)
    }

    fn neutral(&self) -> TritonState {
        TritonState::neutral()
    }

    /// Typed fallback. Once `raw_len > 0`, only refresh typed fields for diagnostics;
    /// `write_state` keeps mirroring the raw report.
    fn merge_frame(
        &self,
        prev: &TritonState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> TritonState {
        let mut s = TritonState::from_gamepad(
            f.buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        // As-is is sticky: a typed frame between two raw reports must not flap back to synth
        // (the client sends both planes so degrade paths stay alive).
        s.raw = prev.raw;
        s.raw_len = prev.raw_len;
        s
    }

    fn apply_rich(&self, st: &mut TritonState, rich: RichInput) {
        if let RichInput::HidReport { len, data, .. } = rich {
            let len = (len as usize).min(data.len()).min(st.raw.len());
            if len == 0 {
                return;
            }
            st.raw[..len].copy_from_slice(&data[..len]);
            st.raw_len = len as u8;
        }
        // Touchpad/Motion/TouchpadEx: the raw feed already carries pads + IMU; synth has no surface.
    }

    // `neutralize_gyro` / `clear_rich` stay the no-op defaults: this backend never sees
    // `RichInput::Motion`, and motion lives inside an opaque passthrough report.
    // A stopped raw feed is the client's own last report; re-emit it.

    fn write_state(&self, pad: &mut TritonTransport, st: &TritonState) {
        pad.write_state(st);
    }

    /// Ack + queue Steam's writes onto 0xCD; rumble also rides 0xCA (deduped) so the
    /// client's phone-mirror path keeps working.
    fn service(&self, pad: &mut TritonTransport, idx: u8) -> PadFeedback {
        let (rumble, raw) = pad.service();
        let hidout = raw
            .into_iter()
            .map(|(kind, data)| HidOutput::HidRaw {
                pad: idx,
                kind,
                data,
            })
            .collect();
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout,
            // Steam is a hidraw writer here too, so abandoned-rumble force-off applies
            // (the 0xCD passthrough plane is unaffected).
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }
}

/// Session's virtual SC2 pads — `PUNKTFUNK_GAMEPAD=steamcontroller2` (aliases `sc2`/`ibex`),
/// or the per-pad kind an Android client declares for a captured physical pad.
pub type Triton2Manager = UhidManager<TritonProto>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a hidraw node under `hid-generic` (no evdev — nothing binds the PID) with
    /// the Valve identity, mirrors a raw state report, tears down on drop. Ignored in CI
    /// (touches `/dev/uhid`); on a Linux box: `cargo test -p punktfunk-host -- --ignored triton`.
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs the input group"]
    fn triton_backend_creates_hidraw_and_mirrors_raw() {
        let mut pad = TritonPad::open(0).expect("open TritonPad (/dev/uhid + input group?)");
        let mut st = TritonState::neutral();
        let raw: &[u8] = &[0x42, 1, 0x01, 0, 0, 0, 0xFF, 0x7F]; // truncated fixture is enough
        st.raw[..raw.len()].copy_from_slice(raw);
        st.raw_len = raw.len() as u8;
        for _ in 0..50 {
            let _ = pad.service();
            pad.write_state(&st).expect("write_state");
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        let found = std::fs::read_dir("/sys/bus/hid/devices")
            .map(|d| {
                d.flatten()
                    .any(|e| e.file_name().to_string_lossy().contains(":28DE:1302"))
            })
            .unwrap_or(false);
        assert!(found, "virtual 28DE:1302 HID device not created");
        drop(pad);
    }
}
