//! Virtual **Steam Controller 2** over USB/IP (`vhci_hcd`) — the Steam-promotable transport for
//! the as-is passthrough backend ([`super::steam_controller2`]). The UHID leg was confirmed
//! on-glass to be invisible to Steam (`Interface: -1`, the same gap the Deck had pre-usbip), so
//! this presents a *real* USB device instead, byte-matched to an `lsusb -v` capture of the
//! physical WIRED pad (2026-07-15, firmware bcdDevice 3.07):
//!
//! - device: bcdUSB 2.00, class `EF/02/01` (IAD convention, as shipped), Full Speed,
//!   strings `Valve Software` / `Steam Controller`, 1 configuration (bus powered, 500 mA);
//! - one interface (#0): HID `03/00/00`, bcdHID 1.11, EP `0x81` interrupt-IN + `0x01`
//!   interrupt-OUT, 64-byte packets, `bInterval 1` (1 kHz).
//!
//! (The Puck dongle presents a 7-interface layout — CDC pair + controllers on 2..5 — but the
//! wired identity is simpler and, per SDL's own matcher, the wired PID is accepted on ANY
//! interface number. Wired is what we emulate.)
//!
//! Semantics mirror the UHID leg: interrupt-IN streams the client's raw reports verbatim (or the
//! typed-fallback `0x42` synth), interrupt-OUT captures Steam's haptic output reports (`0x80`
//! rumble parsed for the 0xCA plane, everything forwarded raw), EP0 SET_REPORT features are
//! remembered + forwarded raw, and EP0 GET_REPORT answers a canned Triton-shaped serial (the
//! query dance can't round-trip to the physical pad synchronously). The vhci plumbing + attach
//! choreography come from [`super::steam_usbip`] (one source of truth with the Deck).

use super::steam_usbip::{attach_device, boxed, UsbipAttachment};
use super::triton_proto::{
    parse_triton_rumble, serialize_triton_state, TritonState, TRITON_RDESC, TRITON_STATE_LEN,
};
use anyhow::Result;
use std::any::Any;
use std::sync::{Arc, Mutex};
use usbip_sim::{
    Direction, SetupPacket, UsbDevice, UsbEndpoint, UsbInterface, UsbInterfaceHandler, UsbSpeed,
    Version,
};

const TRITON_VENDOR: u16 = 0x28DE;
const TRITON_WIRED_PRODUCT: u16 = 0x1302;

/// Everything Steam wrote to the device since the last service pass.
#[derive(Debug, Default)]
pub(crate) struct TritonUsbFeedback {
    /// `(low, high)` from the last `0x80` rumble output report.
    pub rumble: Option<(u16, u16)>,
    /// Raw reports to forward, `(kind, bytes)` — kind = `HID_RAW_OUTPUT`/`HID_RAW_FEATURE`.
    pub raw: Vec<(u8, Vec<u8>)>,
}

/// The wired pad's serial, FVPF-prefixed: [`super::steam_controller`]'s physical-Steam-controller
/// conflict gate recognizes `FVPF…` (`HID_UNIQ`) as one of punktfunk's own virtual pads, so a
/// concurrent session never mistakes this device for real hardware (the vhci sysfs path is the
/// second belt). Shaped like the real `FXA…` serials (13 chars).
fn triton_serial(index: u8) -> String {
    format!("FVPF1302{index:02}D03")
}

/// The 9-byte HID class descriptor: bcdHID **1.11**, country 0, one report descriptor — the
/// captured wired values ([`super::steam_usbip`]'s shared helper bakes the Deck's 1.10/33).
fn triton_hid_desc() -> Vec<u8> {
    let l = TRITON_RDESC.len() as u16;
    vec![
        0x09,
        0x21,
        0x11,
        0x01,
        0,
        1,
        0x22,
        (l & 0xff) as u8,
        (l >> 8) as u8,
    ]
}

/// Interface 0: streams the current report on interrupt-IN, captures Steam's writes.
#[derive(Debug)]
struct TritonHandler {
    /// The current input report (zero-padded to the 64-byte packet size), shared with
    /// [`TritonUsbip::write_state`].
    report: Arc<Mutex<[u8; 64]>>,
    feedback: Arc<Mutex<TritonUsbFeedback>>,
    serial: String,
}

impl TritonHandler {
    /// Queue one raw report for forwarding, newest-wins bounded like the UHID leg.
    fn queue_raw(&self, kind: u8, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        if let Ok(mut fb) = self.feedback.lock() {
            if fb.raw.len() >= 32 {
                fb.raw.remove(0);
            }
            fb.raw.push((kind, data));
        }
    }
}

impl UsbInterfaceHandler for TritonHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        triton_hid_desc()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        use punktfunk_core::quic::{HID_RAW_FEATURE, HID_RAW_OUTPUT};
        if ep.is_ep0() {
            Ok(match (setup.request_type, setup.request) {
                // GET report descriptor (standard, interface recipient).
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => TRITON_RDESC.to_vec(),
                // HID GET_REPORT (feature): the query/answer dance can't reach the physical pad
                // synchronously — answer a plausible Triton-shaped serial blob (id-1 framing,
                // the same canned-reply approach the virtual Deck validated on-glass). Logged
                // for tuning against Steam's real expectations.
                (0xA1, 0x01) => {
                    tracing::debug!(
                        value = format!("{:#06x}", setup.value),
                        "virtual SC2 usbip: GET_REPORT — canned serial reply"
                    );
                    triton_serial_reply(&self.serial).to_vec()
                }
                // HID SET_REPORT (feature): forward raw for replay on the physical pad. EP0 OUT
                // data may or may not carry the report-id byte depending on the writer's stack
                // (the id also rides wValue's low byte) — normalize to id-first for the client.
                (0x21, 0x09) => {
                    let id = (setup.value & 0xFF) as u8;
                    let framed = if req.first() == Some(&id) && id != 0 {
                        req.to_vec()
                    } else {
                        let mut v = Vec::with_capacity(req.len() + 1);
                        v.push(id);
                        v.extend_from_slice(req);
                        v
                    };
                    self.queue_raw(HID_RAW_FEATURE, framed);
                    vec![]
                }
                (0x21, 0x0A) | (0x21, 0x0B) => vec![], // SET_IDLE / SET_PROTOCOL
                _ => vec![],
            })
        } else if let Direction::In = ep.direction() {
            // Interrupt-IN poll (paced by bInterval = 1 ms): the current report, zero-padded —
            // exactly the 64-byte packets the real wired pad produces.
            let r = self.report.lock().map(|g| *g).unwrap_or([0u8; 64]);
            Ok(r.to_vec())
        } else {
            // Interrupt-OUT: Steam's haptic output reports (`SDL_hid_write` — id-first framing
            // on the wire already). Parse rumble for the universal plane, forward everything raw.
            if !req.is_empty() {
                if let Some(r) = parse_triton_rumble(req) {
                    if let Ok(mut fb) = self.feedback.lock() {
                        fb.rumble = Some(r);
                    }
                }
                self.queue_raw(HID_RAW_OUTPUT, req.to_vec());
            }
            Ok(vec![])
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// The Valve feature GET reply (`[report-id 1][ID_GET_STRING_ATTRIBUTE][len][unit-serial]
/// [ascii…]`), zero-padded to the 64-byte feature size — kept in sync with the UHID leg's reply.
fn triton_serial_reply(serial: &str) -> [u8; 64] {
    const ID_GET_STRING_ATTRIBUTE: u8 = 0xAE;
    const ATTRIB_STR_UNIT_SERIAL: u8 = 0x01;
    let mut buf = [0u8; 64];
    let bytes = serial.as_bytes();
    let len = bytes.len().clamp(1, 21);
    buf[0] = 0x01;
    buf[1] = ID_GET_STRING_ATTRIBUTE;
    buf[2] = len as u8;
    buf[3] = ATTRIB_STR_UNIT_SERIAL;
    buf[4..4 + len].copy_from_slice(&bytes[..len]);
    buf
}

/// Assemble the simulated wired Steam Controller 2 (see the module docs for the capture it
/// matches). The handler shares `report` + `feedback` with the owning [`TritonUsbip`].
fn build_triton_device(
    index: u8,
    report: &Arc<Mutex<[u8; 64]>>,
    feedback: &Arc<Mutex<TritonUsbFeedback>>,
) -> UsbDevice {
    let ep = |addr: u8| UsbEndpoint {
        address: addr,
        attributes: 0x03,    // interrupt
        max_packet_size: 64, // wMaxPacketSize 0x0040
        interval: 1,         // bInterval 1 — the real pad's 1 kHz
    };
    let mut dev = UsbDevice::new(0);
    dev.vendor_id = TRITON_VENDOR;
    dev.product_id = TRITON_WIRED_PRODUCT;
    dev.usb_version = Version::from(0x0200u16); // bcdUSB 2.00
    dev.device_bcd = Version::from(0x0307u16); // bcdDevice 3.07 (the captured firmware)
    dev.device_class = 0xEF; // Miscellaneous / IAD — as the real pad ships
    dev.device_subclass = 0x02;
    dev.device_protocol = 0x01;
    dev.speed = UsbSpeed::Full as u32; // negotiated Full Speed (12 Mbps) on the capture
    dev.set_manufacturer_name("Valve Software");
    dev.set_product_name("Steam Controller");
    dev.set_serial_number(&triton_serial(index));
    dev.unset_configuration_name(); // real iConfiguration = 0
    dev.with_interface(
        0x03, // HID
        0x00,
        0x00,
        None, // real iInterface = 0
        vec![ep(0x81), ep(0x01)],
        boxed(TritonHandler {
            report: report.clone(),
            feedback: feedback.clone(),
            serial: triton_serial(index),
        }),
    )
}

/// A virtual Steam Controller 2 presented over USB/IP. Dropping it detaches the `vhci_hcd` port
/// (the device disappears, Steam releases it) and stops the emulation server.
pub struct TritonUsbip {
    report: Arc<Mutex<[u8; 64]>>,
    feedback: Arc<Mutex<TritonUsbFeedback>>,
    _attach: UsbipAttachment,
    seq: u8,
}

impl TritonUsbip {
    /// Bind a virtual wired SC2 and attach it locally via `vhci_hcd` (root + `vhci_hcd` loaded;
    /// see [`super::steam_usbip::attach_device`]). `index` varies only the serial.
    pub fn open(index: u8) -> Result<TritonUsbip> {
        let report = Arc::new(Mutex::new(neutral_report()));
        let feedback = Arc::new(Mutex::new(TritonUsbFeedback::default()));
        let attach = attach_device(
            || build_triton_device(index, &report, &feedback),
            &format!("virtual Steam Controller 2 {index}"),
        )?;
        Ok(TritonUsbip {
            report,
            feedback,
            _attach: attach,
            seq: 0,
        })
    }

    /// Mirror one report onto the interrupt-IN stream: the client's raw bytes verbatim in as-is
    /// mode (zero-padded to the 64-byte packet), else a synthesized minimal `0x42` state report.
    pub fn write_state(&mut self, st: &TritonState) {
        let mut r = [0u8; 64];
        if st.raw_len > 0 {
            let len = (st.raw_len as usize).min(st.raw.len()).min(r.len());
            r[..len].copy_from_slice(&st.raw[..len]);
        } else {
            self.seq = self.seq.wrapping_add(1);
            let mut s = [0u8; TRITON_STATE_LEN];
            serialize_triton_state(&mut s, st, self.seq);
            r[..TRITON_STATE_LEN].copy_from_slice(&s);
        }
        if let Ok(mut g) = self.report.lock() {
            *g = r;
        }
    }

    /// Drain everything Steam wrote to the device since the last pass.
    pub fn service(&mut self) -> TritonUsbFeedback {
        self.feedback
            .lock()
            .map(|mut f| std::mem::take(&mut *f))
            .unwrap_or_default()
    }
}

/// An idle `0x42` state report — what the interrupt-IN endpoint streams before the first write.
fn neutral_report() -> [u8; 64] {
    let mut r = [0u8; 64];
    let mut s = [0u8; TRITON_STATE_LEN];
    serialize_triton_state(&mut s, &TritonState::neutral(), 0);
    r[..TRITON_STATE_LEN].copy_from_slice(&s);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The simulated device matches the captured wired identity byte-for-byte where Steam looks:
    /// VID/PID, device class triplet, bcdDevice, ONE HID interface with the IN+OUT endpoint pair.
    #[test]
    fn device_matches_wired_capture() {
        let report = Arc::new(Mutex::new([0u8; 64]));
        let feedback = Arc::new(Mutex::new(TritonUsbFeedback::default()));
        let dev = build_triton_device(3, &report, &feedback);
        assert_eq!((dev.vendor_id, dev.product_id), (0x28DE, 0x1302));
        assert_eq!(
            (dev.device_class, dev.device_subclass, dev.device_protocol),
            (0xEF, 0x02, 0x01)
        );
        assert_eq!(dev.speed, UsbSpeed::Full as u32);
        assert_eq!(dev.interfaces.len(), 1);
        let i = &dev.interfaces[0];
        assert_eq!(
            (
                i.interface_class,
                i.interface_subclass,
                i.interface_protocol
            ),
            (0x03, 0x00, 0x00)
        );
        let eps: Vec<(u8, u8, u16, u8)> = i
            .endpoints
            .iter()
            .map(|e| (e.address, e.attributes, e.max_packet_size, e.interval))
            .collect();
        assert_eq!(eps, vec![(0x81, 3, 64, 1), (0x01, 3, 64, 1)]);
        // bcdHID 1.11 + the served report descriptor's length in the HID class descriptor.
        let hid = triton_hid_desc();
        assert_eq!(&hid[2..4], &[0x11, 0x01]);
        assert_eq!(
            u16::from_le_bytes([hid[7], hid[8]]) as usize,
            TRITON_RDESC.len()
        );
        assert!(triton_serial(3).starts_with("FVPF")); // the conflict-gate exclusion prefix
    }

    /// Steam's interrupt-OUT rumble lands in the feedback (parsed + queued raw); EP0 feature
    /// writes are normalized to id-first framing whichever way the stack framed them.
    #[test]
    fn out_and_feature_writes_are_captured() {
        use punktfunk_core::quic::{HID_RAW_FEATURE, HID_RAW_OUTPUT};
        let report = Arc::new(Mutex::new([0u8; 64]));
        let feedback = Arc::new(Mutex::new(TritonUsbFeedback::default()));
        let mut h = TritonHandler {
            report,
            feedback: feedback.clone(),
            serial: triton_serial(0),
        };
        let iface_dummy = UsbInterface {
            interface_class: 3,
            interface_subclass: 0,
            interface_protocol: 0,
            endpoints: vec![],
            string_interface: 0,
            class_specific_descriptor: vec![],
            handler: boxed(IdleDummy),
        };
        let ep_out = UsbEndpoint {
            address: 0x01,
            attributes: 0x03,
            max_packet_size: 64,
            interval: 1,
        };
        let ep0 = UsbEndpoint {
            address: 0x00,
            attributes: 0x00, // control
            max_packet_size: 64,
            interval: 0,
        };
        // Rumble output report: [0x80, type, intensity u16, left u16+gain, right u16+gain].
        let mut rumble = [0u8; 10];
        rumble[0] = 0x80;
        rumble[4..6].copy_from_slice(&0x2000u16.to_le_bytes());
        rumble[7..9].copy_from_slice(&0x4000u16.to_le_bytes());
        h.handle_urb(&iface_dummy, ep_out, 10, SetupPacket::default(), &rumble)
            .unwrap();
        // Feature SET_REPORT with the id NOT in the payload (it rides wValue) → normalized.
        let setup = SetupPacket {
            request_type: 0x21,
            request: 0x09,
            value: 0x0301,
            index: 0,
            length: 5,
        };
        h.handle_urb(&iface_dummy, ep0, 5, setup, &[0x87, 3, 9, 0, 0])
            .unwrap();
        let fb = feedback.lock().unwrap();
        assert_eq!(fb.rumble, Some((0x2000, 0x4000)));
        assert_eq!(fb.raw.len(), 2);
        assert_eq!(fb.raw[0].0, HID_RAW_OUTPUT);
        assert_eq!(fb.raw[0].1[0], 0x80);
        assert_eq!(fb.raw[1].0, HID_RAW_FEATURE);
        assert_eq!(&fb.raw[1].1[..3], &[0x01, 0x87, 3]); // id-first for the client replay
    }

    #[derive(Debug)]
    struct IdleDummy;
    impl UsbInterfaceHandler for IdleDummy {
        fn get_class_specific_descriptor(&self) -> Vec<u8> {
            vec![]
        }
        fn handle_urb(
            &mut self,
            _i: &UsbInterface,
            _e: UsbEndpoint,
            _l: u32,
            _s: SetupPacket,
            _r: &[u8],
        ) -> std::io::Result<Vec<u8>> {
            Ok(vec![])
        }
        fn as_any(&mut self) -> &mut dyn Any {
            self
        }
    }

    /// On-box smoke (root + `vhci_hcd`): attach the virtual wired SC2, confirm the USB device
    /// enumerates with the Valve identity on a REAL interface number (the whole point vs UHID),
    /// and that it tears down on drop. `#[ignore]`d in CI.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root + vhci_hcd"]
    fn usbip_triton_enumerates_and_tears_down() {
        super::super::steam_usbip::ensure_modules();
        let mut pad = TritonUsbip::open(0).expect("open TritonUsbip (root + vhci_hcd?)");
        let mut st = TritonState::neutral();
        let raw: &[u8] = &[0x42, 1, 0x01, 0, 0, 0]; // A held (truncated report is fine)
        st.raw[..raw.len()].copy_from_slice(raw);
        st.raw_len = raw.len() as u8;
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(1500) {
            pad.write_state(&st);
            let _ = pad.service();
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        // A real USB HID device now exists: /sys/bus/hid device named ...:28DE:1302 whose path
        // resolves through vhci_hcd (NOT /devices/virtual), carrying interface number 0.
        let found = std::fs::read_dir("/sys/bus/hid/devices")
            .expect("/sys/bus/hid/devices")
            .flatten()
            .find(|e| e.file_name().to_string_lossy().contains(":28DE:1302"));
        let entry = found.expect("virtual 28DE:1302 did not enumerate via vhci_hcd");
        let target = std::fs::read_link(entry.path()).expect("hid device link");
        assert!(
            target.to_string_lossy().contains("vhci_hcd"),
            "28DE:1302 present but not via vhci_hcd: {}",
            target.display()
        );
        drop(pad);
        std::thread::sleep(std::time::Duration::from_millis(400));
        let still = std::fs::read_dir("/sys/bus/hid/devices")
            .expect("/sys/bus/hid/devices")
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(":28DE:1302"));
        assert!(!still, "device not torn down on drop");
    }
}
