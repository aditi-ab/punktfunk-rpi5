// usbip Deck PoC: present a real 3-interface USB Steam Deck (28DE:1205, controller on interface 2)
// over the usbip protocol via the `usbip` crate, so vhci_hcd can attach it LOCALLY — the Secure-Boot-
// clean, universal alternative to dummy_hcd+raw_gadget. Validates that Steam recognizes a usbip-
// presented interface-2 Deck (on Bazzite, where dummy_hcd isn't available).
//
// Run as root with vhci_hcd loaded, then locally:  usbip attach -r 127.0.0.1 -b 0-0-0
use std::any::Any;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use usbip::{Direction, SetupPacket, UsbDevice, UsbEndpoint, UsbInterface, UsbInterfaceHandler, UsbIpServer};

// ---- captured-from-hardware report descriptors (a real Steam Deck) ----
const RDESC_MOUSE: &[u8] = &[
    0x05,0x01,0x09,0x02,0xa1,0x01,0x09,0x01,0xa1,0x00,0x05,0x09,0x19,0x01,0x29,0x02,
    0x15,0x00,0x25,0x01,0x75,0x01,0x95,0x02,0x81,0x02,0x75,0x06,0x95,0x01,0x81,0x01,
    0x05,0x01,0x09,0x30,0x09,0x31,0x15,0x81,0x25,0x7f,0x75,0x08,0x95,0x02,0x81,0x06,
    0x95,0x01,0x09,0x38,0x81,0x06,0x05,0x0c,0x0a,0x38,0x02,0x95,0x01,0x81,0x06,0xc0,0xc0];
const RDESC_KBD: &[u8] = &[
    0x05,0x01,0x09,0x06,0xa1,0x01,0x05,0x07,0x19,0xe0,0x29,0xe7,0x15,0x00,0x25,0x01,
    0x75,0x01,0x95,0x08,0x81,0x02,0x81,0x01,0x19,0x00,0x29,0x65,0x15,0x00,0x25,0x65,
    0x75,0x08,0x95,0x06,0x81,0x00,0xc0];
const RDESC_CTRL: &[u8] = &[ // the real Deck controller, interface 2 (Usage Page 0xFFFF)
    0x06,0xff,0xff,0x09,0x01,0xa1,0x01,0x09,0x02,0x09,0x03,0x15,0x00,0x26,0xff,0x00,
    0x75,0x08,0x95,0x40,0x81,0x02,0x09,0x06,0x09,0x07,0x15,0x00,0x26,0xff,0x00,0x75,
    0x08,0x95,0x40,0xb1,0x02,0xc0];

fn hid_desc(report_len: usize, country: u8) -> Vec<u8> {
    let l = report_len as u16;
    vec![0x09, 0x21, 0x10, 0x01, country, 1, 0x22, (l & 0xff) as u8, (l >> 8) as u8]
}

/// Captured-from-hardware feature replies (the contract Steam's GetControllerInfo reads).
fn feature_reply(last_set: &[u8], serial: &str, unit_id: u32) -> Vec<u8> {
    let cmd = last_set.first().copied().unwrap_or(0xAE);
    let mut r = vec![0u8; 64];
    match cmd {
        0x83 => {
            r[0] = 0x83;
            r[1] = 0x2d;
            let attrs: [(u8, u32); 9] = [
                (0x01, 0x1205), (0x02, 0), (0x0a, unit_id), (0x04, unit_id ^ 0x5555_5555),
                (0x09, 0x2e), (0x0b, 0x0fa0), (0x0d, 0), (0x0c, 0), (0x0e, 0),
            ];
            let mut o = 2;
            for (id, val) in attrs {
                r[o] = id;
                r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                o += 5;
            }
        }
        0xAE => {
            let attr = last_set.get(2).copied().unwrap_or(0x01);
            let b = serial.as_bytes();
            let len = b.len().clamp(1, 20);
            r[0] = 0xAE;
            r[1] = len as u8;
            r[2] = attr;
            r[3..3 + len].copy_from_slice(&b[..len]);
        }
        _ => {
            let n = last_set.len().min(64);
            r[..n].copy_from_slice(&last_set[..n]);
        }
    }
    r
}

/// The Deck controller interface (vendor HID): answers feature reports + streams the 64-byte state.
#[derive(Debug)]
struct ControllerHandler {
    report_desc: Vec<u8>,
    last_set: Vec<u8>,
    seq: u32,
    press_a: bool,
}
impl UsbInterfaceHandler for ControllerHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_desc(self.report_desc.len(), 33)
    }
    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if ep.is_ep0() {
            Ok(match (setup.request_type, setup.request) {
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => self.report_desc.clone(), // GET report descriptor
                (0xA1, 0x01) => feature_reply(&self.last_set, "PFDECK0000", 0x5046_0000), // HID GET_REPORT (feature)
                (0x21, 0x09) => {
                    self.last_set = req.to_vec();
                    vec![]
                } // HID SET_REPORT
                (0x21, 0x0A) | (0x21, 0x0B) => vec![], // SET_IDLE / SET_PROTOCOL
                _ => vec![],
            })
        } else if let Direction::In = ep.direction() {
            self.seq = self.seq.wrapping_add(1);
            let mut r = vec![0u8; 64];
            r[0] = 0x01;
            r[2] = 0x09;
            r[3] = 0x3c;
            r[4..8].copy_from_slice(&self.seq.to_le_bytes());
            if self.press_a {
                r[8] = 0x80; // btn::A
            }
            Ok(r)
        } else {
            Ok(vec![])
        }
    }
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// A minimal HID interface (mouse/keyboard) — serves its report descriptor, sends nothing.
#[derive(Debug)]
struct IdleHidHandler {
    report_desc: Vec<u8>,
}
impl UsbInterfaceHandler for IdleHidHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_desc(self.report_desc.len(), 0)
    }
    fn handle_urb(
        &mut self,
        _i: &UsbInterface,
        ep: UsbEndpoint,
        _l: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if ep.is_ep0() && setup.request == 0x06 && (setup.value >> 8) == 0x22 {
            Ok(self.report_desc.clone())
        } else {
            Ok(vec![])
        }
    }
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

fn handler(h: impl UsbInterfaceHandler + Send + 'static) -> Arc<Mutex<Box<dyn UsbInterfaceHandler + Send>>> {
    Arc::new(Mutex::new(Box::new(h)))
}
fn ep(addr: u8, mps: u16) -> UsbEndpoint {
    UsbEndpoint { address: addr, attributes: 0x03, max_packet_size: mps, interval: 4 }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let press_a = std::env::args().any(|a| a == "pressa");

    let mut dev = UsbDevice::new(0);
    dev.vendor_id = 0x28DE;
    dev.product_id = 0x1205;
    dev.set_manufacturer_name("Valve Software");
    dev.set_product_name("Steam Deck Controller");
    dev.set_serial_number("PFDECK0000");

    // interface 0: mouse, interface 1: keyboard, interface 2: the controller.
    let dev = dev
        .with_interface(0x03, 0x00, 0x02, Some("mouse"), vec![ep(0x81, 8)],
            handler(IdleHidHandler { report_desc: RDESC_MOUSE.to_vec() }))
        .with_interface(0x03, 0x01, 0x01, Some("keyboard"), vec![ep(0x82, 8)],
            handler(IdleHidHandler { report_desc: RDESC_KBD.to_vec() }))
        .with_interface(0x03, 0x00, 0x00, Some("controller"), vec![ep(0x83, 64)],
            handler(ControllerHandler { report_desc: RDESC_CTRL.to_vec(), last_set: vec![], seq: 0, press_a }));

    let server = Arc::new(UsbIpServer::new_simulated(vec![dev]));
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 3240);
    println!("usbip Deck server on {addr} (press_a={press_a}); attach with: usbip attach -r 127.0.0.1 -b 0-0-0");
    usbip::server(addr, server).await;
}
