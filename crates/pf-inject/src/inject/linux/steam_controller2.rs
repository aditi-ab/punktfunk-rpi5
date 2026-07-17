//! Virtual **Steam Controller 2** (Triton) via UHID — the as-is passthrough backend
//! ([`GamepadPref::SteamController2`](punktfunk_core::config::GamepadPref)). The
//! transport-independent contract (descriptor, report ids, the typed fallback serializer, the
//! rumble parser) lives in [`super::triton_proto`]; this module is the `/dev/uhid` plumbing.
//!
//! Deltas vs the Deck backend ([`super::steam_controller`]):
//!
//! 1. **No kernel driver.** Mainline `hid-steam` doesn't bind `28DE:1302`, so the device gets
//!    `hid-generic` + a hidraw node and NO evdev — Steam Input (hidapi over hidraw) is the only
//!    consumer, exactly as it is for the physical pad. No `gamepad_mode` machinery applies.
//! 2. **Raw mirroring.** Input reports arrive verbatim from the client
//!    ([`RichInput::HidReport`](punktfunk_core::quic::RichInput)) and are written unchanged;
//!    everything Steam writes back (SET_REPORT features, OUTPUT haptics) is acked and forwarded
//!    raw for replay on the physical controller.
//! 3. **usbip first, UHID fallback.** Steam ignores UHID devices (`Interface: -1`) for the
//!    Triton exactly as it did for the Deck — CONFIRMED on-glass 2026-07-15 — so the preferred
//!    transport is [`super::triton_usbip`] (`vhci_hcd`), which presents a real USB device
//!    byte-matched to the physical wired pad's captured descriptors. UHID remains the degraded
//!    fallback (hidraw exists, Steam won't list it) for hosts without `vhci_hcd`/root.

use super::triton_proto::{
    parse_triton_rumble, serialize_triton_state, strip_report_prefix, triton_feature_reply,
    triton_serial, triton_unit_id, TritonState, TRITON_RDESC, TRITON_STATE_LEN, TRITON_VENDOR,
    TRITON_WIRED_PRODUCT,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

// /dev/uhid event ABI — same layout as the Deck/DualSense backends.
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

fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
}

/// A virtual Steam Controller 2 backed by `/dev/uhid`. Dropping it destroys the device.
pub struct TritonPad {
    fd: File,
    /// Synth-mode sequence counter (the raw path carries the physical pad's own seq).
    seq: u8,
    /// Raw reports Steam wrote since the last service pass, kind-tagged for the 0xCD plane.
    pending_raw: Vec<(u8, Vec<u8>)>,
    /// The last feature SET_REPORT (id-first) — the query half of the Valve GET dance.
    last_set: Vec<u8>,
    serial: String,
    unit_id: u32,
    /// Last GET query command logged, so the tester-facing log line fires once per distinct cmd.
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
        // The physical pad's USB product string is "Steam Controller"; keep the punktfunk prefix
        // convention every virtual pad uses (Steam matches on VID/PID, not the name).
        put_cstr(
            &mut ev,
            4,
            128,
            &format!("Punktfunk Steam Controller 2 {index}"),
        ); // name[128]
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/triton/{index}")); // phys[64]
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-triton-{index}")); // uniq[64]
        ev[260..262].copy_from_slice(&(TRITON_RDESC.len() as u16).to_ne_bytes()); // rd_size
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus
        ev[264..268].copy_from_slice(&TRITON_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&TRITON_WIRED_PRODUCT.to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes()); // version
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
        ev[280..280 + TRITON_RDESC.len()].copy_from_slice(TRITON_RDESC);
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Mirror one report out: the client's raw bytes verbatim in as-is mode, else a synthesized
    /// minimal `0x42` state report from the typed fallback fields.
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
        ev[4..6].copy_from_slice(&(data.len() as u16).to_ne_bytes()); // input2.size
        ev[6..6 + data.len()].copy_from_slice(data); // input2.data
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Service the device, non-blocking: ack SET_REPORTs (a stalled ack blocks the writer ~5 s),
    /// answer GET_REPORTs (best-effort canned reply — the query/answer feature dance can't
    /// round-trip to the physical pad synchronously), and queue every report Steam wrote for raw
    /// forwarding. Returns the rumble level if a `0x80` output report was seen this pass.
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
                    // Remember the command — it selects the NEXT GET_REPORT's answer (the Valve
                    // query dance) — and forward it raw to the physical pad.
                    self.queue_raw(HID_RAW_FEATURE, &rep);
                    self.last_set = rep;
                    let _ = self.reply_set_report(id);
                }
                UHID_GET_REPORT => {
                    // The answer half of the Valve query dance: echo the LAST SET's command with
                    // a plausible payload (attributes / serial). Answering with the wrong command
                    // type makes Steam drop the pad — confirmed on-glass 2026-07-15; the dance
                    // can't round-trip to the physical pad synchronously.
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
                _ => {} // Start/Stop/Open/Close — ignore
            }
        }
        rumble
    }

    /// Queue a raw report for the 0xCD plane, capped so a hidraw client gone haywire can't grow
    /// the queue unboundedly between pumps (newest wins — these are level-styled commands).
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

impl Drop for TritonPad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// The transport a manager pad drives: usbip (`vhci_hcd`, a real USB device Steam lists) with
/// UHID as the degraded fallback — the same ladder shape as the Deck's [`super::steam_controller`],
/// minus the gadget rung (no captured gadget layout for the Triton, and usbip is universal).
pub enum TritonTransport {
    Usbip(crate::triton_usbip::TritonUsbip),
    Uhid(TritonPad),
}

/// One transport `service()` pass: Steam's latest rumble `(left, right)` plus the raw
/// `(kind, payload)` reports it wrote since the last pass.
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

    /// `(rumble, raw reports)` Steam wrote since the last pass.
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

/// Open the best Steam-visible SC2 transport: **usbip (`vhci_hcd`) → UHID.** Steam is confirmed
/// (on-glass 2026-07-15) to ignore the UHID leg, so reaching the fallback means the pad exists as
/// hidraw only — flagged loudly, with the vhci_hcd remedy in the log.
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

/// The Triton-specific half of the shared stateful manager (see [`PadProto`]): raw mirroring
/// with the typed fallback, and the raw-forwarding service pass.
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

    /// Typed fallback merge. Once raw reports flow (`raw_len > 0`) the frame only refreshes the
    /// typed fields for diagnostics — `write_state` keeps mirroring the raw report.
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
        // As-is mode is sticky: a typed frame between two raw reports must not flap the pad back
        // to synth mode (the client sends BOTH planes — typed keeps the degrade paths alive).
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
        // Touchpad/Motion/TouchpadEx: nothing to fold — the raw feed carries pads + IMU natively,
        // and the synth fallback has no surface for them.
    }

    fn write_state(&self, pad: &mut TritonTransport, st: &TritonState) {
        pad.write_state(st);
    }

    /// Ack + queue Steam's writes, then hand them to the pump as raw 0xCD events; rumble ALSO
    /// rides the universal 0xCA plane (deduped) so the client's phone-mirror path keeps working.
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
            rumble,
            hidout,
            game_drove: None,
        }
    }
}

/// All virtual Steam Controller 2 pads of a session — `PUNKTFUNK_GAMEPAD=steamcontroller2`
/// (aliases `sc2`/`ibex`), or the per-pad kind an Android client declares for a captured
/// physical pad.
pub type Triton2Manager = UhidManager<TritonProto>;

#[cfg(test)]
mod tests {
    use super::*;

    /// On-box smoke: the virtual SC2 must create a hidraw node under `hid-generic` (no evdev —
    /// nothing binds the PID) carrying the Valve identity, mirror a raw state report verbatim,
    /// and tear down on drop. `#[ignore]`d in CI (touches `/dev/uhid`); run on a Linux box:
    /// `cargo test -p punktfunk-host -- --ignored triton`.
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs the input group"]
    fn triton_backend_creates_hidraw_and_mirrors_raw() {
        let mut pad = TritonPad::open(0).expect("open TritonPad (/dev/uhid + input group?)");
        // Mirror one raw report (as the client would forward it).
        let mut st = TritonState::neutral();
        let raw: &[u8] = &[0x42, 1, 0x01, 0, 0, 0, 0xFF, 0x7F]; // A held, LT full — truncated is fine
        st.raw[..raw.len()].copy_from_slice(raw);
        st.raw_len = raw.len() as u8;
        for _ in 0..50 {
            let _ = pad.service();
            pad.write_state(&st).expect("write_state");
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        // The device exists with the Valve identity (hidraw only; /proc/bus/input has no entry).
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
