//! Virtual Steam Deck via the USB **gadget** subsystem (`raw_gadget` + `dummy_hcd`) — the only
//! virtual-Deck transport Steam Input recognizes.
//!
//! The UHID [`super::steam_controller::SteamDeckPad`] binds the kernel `hid-steam` driver, but Steam's
//! own controller driver filters the Deck's controller to USB **interface 2**, and a UHID device has no
//! USB interface number (`Interface: -1`), so Steam enumerates it but never promotes it. This backend
//! instead presents a *real* 3-interface USB Deck (mouse = interface 0, keyboard = 1, **controller =
//! 2**) on a `dummy_hcd` loopback UDC, driven from userspace via `/dev/raw-gadget` so we can answer
//! every control transfer (including the HID feature reports `f_hid` can't). Proven on a real Deck:
//! hid-steam binds it, Steam reserves an XInput slot and emits an X-Box pad. Descriptors are captured
//! verbatim from a physical Deck; see `packaging/linux/steam-deck-gadget/` for the original PoC + the
//! USB-stack gotchas. **SteamOS-host only** (needs `dummy_hcd` + `raw_gadget`, which SteamOS ships).
//!
//! The transport here is self-contained (libc + std); the report bytes it streams are produced by
//! [`super::steam_proto`] in the wrapping backend.

use anyhow::{bail, Context, Result};
use std::mem::size_of;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

// ---- raw_gadget UAPI (mirrors linux/usb/raw_gadget.h; inlined like the C PoC) ----
const UDC_NAME_MAX: usize = 128;

#[repr(C)]
struct UsbRawInit {
    driver_name: [u8; UDC_NAME_MAX],
    device_name: [u8; UDC_NAME_MAX],
    speed: u8,
}

// usb_raw_event { u32 type; u32 length; u8 data[]; } — we read it into a fixed buffer.
const EVENT_HDR: usize = 8; // type + length
const EVENT_BUF: usize = EVENT_HDR + 64; // setup packet (8) fits easily

// usb_raw_ep_io { u16 ep; u16 flags; u32 length; u8 data[]; }
const EPIO_HDR: usize = 8;

// usb_endpoint_descriptor is 9 bytes in the kernel (audio bRefresh/bSynchAddress); EP_ENABLE wants it.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct UsbEndpointDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_endpoint_address: u8,
    bm_attributes: u8,
    w_max_packet_size: u16,
    b_interval: u8,
    b_refresh: u8,
    b_synch_address: u8,
}

const fn ioc(dir: u64, nr: u64, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | nr) as libc::c_ulong
}
const IOCTL_INIT: libc::c_ulong = ioc(1, 0, size_of::<UsbRawInit>());
const IOCTL_RUN: libc::c_ulong = ioc(0, 1, 0);
const IOCTL_EVENT_FETCH: libc::c_ulong = ioc(2, 2, EVENT_HDR); // size is the header; kernel copies more
const IOCTL_EP0_WRITE: libc::c_ulong = ioc(1, 3, EPIO_HDR);
const IOCTL_EP0_READ: libc::c_ulong = ioc(2 | 1, 4, EPIO_HDR); // _IOWR
const IOCTL_EP_ENABLE: libc::c_ulong = ioc(1, 5, size_of::<UsbEndpointDescriptor>());
const IOCTL_EP_WRITE: libc::c_ulong = ioc(1, 7, EPIO_HDR);
const IOCTL_CONFIGURE: libc::c_ulong = ioc(0, 9, 0);
const IOCTL_VBUS_DRAW: libc::c_ulong = ioc(1, 10, 4);
const IOCTL_EP0_STALL: libc::c_ulong = ioc(0, 12, 0);

const USB_RAW_EVENT_CONNECT: u32 = 1;
const USB_RAW_EVENT_CONTROL: u32 = 2;
const USB_SPEED_HIGH: u8 = 3;

// ---- captured-from-hardware descriptors (a real Steam Deck, 28DE:1205) ----
#[rustfmt::skip]
const RDESC_MOUSE: &[u8] = &[
    0x05,0x01,0x09,0x02,0xa1,0x01,0x09,0x01,0xa1,0x00,0x05,0x09,0x19,0x01,0x29,0x02,
    0x15,0x00,0x25,0x01,0x75,0x01,0x95,0x02,0x81,0x02,0x75,0x06,0x95,0x01,0x81,0x01,
    0x05,0x01,0x09,0x30,0x09,0x31,0x15,0x81,0x25,0x7f,0x75,0x08,0x95,0x02,0x81,0x06,
    0x95,0x01,0x09,0x38,0x81,0x06,0x05,0x0c,0x0a,0x38,0x02,0x95,0x01,0x81,0x06,0xc0,0xc0];
#[rustfmt::skip]
const RDESC_KBD: &[u8] = &[
    0x05,0x01,0x09,0x06,0xa1,0x01,0x05,0x07,0x19,0xe0,0x29,0xe7,0x15,0x00,0x25,0x01,
    0x75,0x01,0x95,0x08,0x81,0x02,0x81,0x01,0x19,0x00,0x29,0x65,0x15,0x00,0x25,0x65,
    0x75,0x08,0x95,0x06,0x81,0x00,0xc0];
#[rustfmt::skip]
const RDESC_CTRL: &[u8] = &[ // the real Deck controller, interface 2 (Usage Page 0xFFFF)
    0x06,0xff,0xff,0x09,0x01,0xa1,0x01,0x09,0x02,0x09,0x03,0x15,0x00,0x26,0xff,0x00,
    0x75,0x08,0x95,0x40,0x81,0x02,0x09,0x06,0x09,0x07,0x15,0x00,0x26,0xff,0x00,0x75,
    0x08,0x95,0x40,0xb1,0x02,0xc0];

const DEV_DESC: [u8; 18] = [
    18, 1, 0x00, 0x02, // bLength, DEVICE, bcdUSB 2.00
    0, 0, 0, 64, // class/sub/proto, bMaxPacketSize0
    0xDE, 0x28, 0x05, 0x12, // idVendor 28DE, idProduct 1205
    0x00, 0x03, // bcdDevice 3.00
    1, 2, 3, 1, // iManufacturer, iProduct, iSerial, bNumConfigurations
];

const HID_DT: u8 = 0x21;
const HID_RPT_DT: u8 = 0x22;

/// Assemble the 84-byte config descriptor: config + 3×(interface + HID + 7-byte endpoint).
fn build_config() -> Vec<u8> {
    let mut c = Vec::with_capacity(84);
    // config descriptor (wTotalLength patched after)
    c.extend_from_slice(&[9, 2, 84, 0, 3, 1, 0, 0x80, 250]);
    // helper closures
    let iface = |n: u8, sub: u8, proto: u8| [9u8, 4, n, 0, 1, 3, sub, proto, 0];
    let hid = |rlen: u16, country: u8| {
        [
            9u8,
            HID_DT,
            0x10,
            0x01,
            country,
            1,
            HID_RPT_DT,
            (rlen & 0xff) as u8,
            (rlen >> 8) as u8,
        ]
    };
    let ep = |addr: u8, mps: u16| [7u8, 5, addr, 0x03, (mps & 0xff) as u8, (mps >> 8) as u8, 4];
    // interface 0: mouse, EP 0x81
    c.extend_from_slice(&iface(0, 0, 2));
    c.extend_from_slice(&hid(RDESC_MOUSE.len() as u16, 0));
    c.extend_from_slice(&ep(0x81, 8));
    // interface 1: keyboard (boot), EP 0x82
    c.extend_from_slice(&iface(1, 1, 1));
    c.extend_from_slice(&hid(RDESC_KBD.len() as u16, 0));
    c.extend_from_slice(&ep(0x82, 8));
    // interface 2: controller, EP 0x83, bCountryCode 33
    c.extend_from_slice(&iface(2, 0, 0));
    c.extend_from_slice(&hid(RDESC_CTRL.len() as u16, 33));
    c.extend_from_slice(&ep(0x83, 64));
    debug_assert_eq!(c.len(), 84);
    c
}

fn string_desc(idx: u8, serial: &str) -> Vec<u8> {
    if idx == 0 {
        return vec![4, 3, 0x09, 0x04]; // LANGID en-US
    }
    let s: &str = match idx {
        1 => "Valve Software",
        2 => "Steam Deck Controller",
        3 => serial,
        _ => "",
    };
    let mut v = vec![(2 + s.len() * 2) as u8, 3];
    for ch in s.encode_utf16() {
        v.push((ch & 0xff) as u8);
        v.push((ch >> 8) as u8);
    }
    v
}

// ---- ioctl wrappers (the only unsafe surface for the raw_gadget UAPI; documented once) ----
fn ioctl_ptr<T>(fd: RawFd, req: libc::c_ulong, arg: *const T) -> i32 {
    // SAFETY: `fd` is our open /dev/raw-gadget descriptor; `arg` points to a correctly-sized,
    // initialized argument for `req` (a raw_gadget UAPI struct or an owned usb_raw_ep_io buffer)
    // that lives for the duration of the call. `ioctl` is variadic, so passing a thin pointer is ABI-correct.
    unsafe { libc::ioctl(fd, req as _, arg) as i32 }
}
fn ioctl_mut<T>(fd: RawFd, req: libc::c_ulong, arg: *mut T) -> i32 {
    // SAFETY: as `ioctl_ptr`, but `arg` is a writable buffer the kernel fills for `req` (EVENT_FETCH / EP0_READ).
    unsafe { libc::ioctl(fd, req as _, arg) as i32 }
}
fn ioctl_val(fd: RawFd, req: libc::c_ulong, val: libc::c_ulong) -> i32 {
    // SAFETY: `req` (VBUS_DRAW) takes an integer argument by value; `fd` is our descriptor.
    unsafe { libc::ioctl(fd, req as _, val) as i32 }
}
fn ioctl_none(fd: RawFd, req: libc::c_ulong) -> i32 {
    // SAFETY: `req` (RUN / CONFIGURE / EP0_STALL) takes no argument, but raw_gadget rejects a non-zero
    // `value` with EINVAL — pass an explicit 0 (an omitted vararg would be an indeterminate register).
    unsafe { libc::ioctl(fd, req as _, 0) as i32 }
}

// ---- low-level ep0 helpers (operate on the shared fd) ----
fn ep0_write(fd: RawFd, data: &[u8]) -> i32 {
    let mut buf = vec![0u8; EPIO_HDR + data.len()];
    buf[0..2].copy_from_slice(&0u16.to_ne_bytes()); // ep 0
    buf[4..8].copy_from_slice(&(data.len() as u32).to_ne_bytes());
    buf[EPIO_HDR..].copy_from_slice(data);
    ioctl_ptr(fd, IOCTL_EP0_WRITE, buf.as_ptr())
}
fn ep0_read(fd: RawFd, len: usize) -> (i32, Vec<u8>) {
    let mut buf = vec![0u8; EPIO_HDR + len.max(1)];
    buf[4..8].copy_from_slice(&(len as u32).to_ne_bytes());
    let r = ioctl_mut(fd, IOCTL_EP0_READ, buf.as_mut_ptr());
    let n = if r > 0 { r as usize } else { 0 };
    (r, buf[EPIO_HDR..EPIO_HDR + n.min(len.max(1))].to_vec())
}
/// Complete a no-data OUT control (status stage is an IN, handled by a zero-length read).
fn ep0_ack(fd: RawFd) {
    ep0_read(fd, 0);
}
fn ep0_stall(fd: RawFd) {
    ioctl_none(fd, IOCTL_EP0_STALL);
}

/// Owns the `/dev/raw-gadget` fd; closing it tears the device down.
struct GadgetFd(RawFd);
impl Drop for GadgetFd {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the fd we opened in `SteamDeckGadget::open` and own uniquely here.
        unsafe { libc::close(self.0) };
    }
}

/// A virtual Steam Deck presented over the USB gadget subsystem. Dropping it stops the threads and
/// closes the gadget (the kernel tears down the device).
pub struct SteamDeckGadget {
    report: Arc<Mutex<[u8; 64]>>,
    feedback: Arc<Mutex<super::steam_proto::SteamFeedback>>,
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    _fd: Arc<GadgetFd>,
    seq: u32,
}

impl SteamDeckGadget {
    /// Bind a virtual Deck on a fresh `dummy_hcd` UDC. `index` only varies the serial. Requires
    /// `dummy_hcd` + `raw_gadget` loaded and write access to `/dev/raw-gadget` (root on SteamOS).
    pub fn open(index: u8) -> Result<SteamDeckGadget> {
        // SAFETY: opening a constant NUL-terminated device path with O_RDWR; returns a fd or -1.
        let fd = unsafe { libc::open(c"/dev/raw-gadget".as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            bail!(
                "open /dev/raw-gadget ({}) — is raw_gadget+dummy_hcd loaded and are we root?",
                std::io::Error::last_os_error()
            );
        }
        let fd = Arc::new(GadgetFd(fd));
        let raw = fd.0;

        // INIT against the dummy UDC, then RUN.
        // SAFETY: `UsbRawInit` is a plain-old-data struct (byte arrays + u8); all-zero is a valid value.
        let mut init: UsbRawInit = unsafe { std::mem::zeroed() };
        copy_cstr(&mut init.driver_name, "dummy_udc");
        copy_cstr(&mut init.device_name, "dummy_udc.0");
        init.speed = USB_SPEED_HIGH;
        if ioctl_ptr(raw, IOCTL_INIT, &init as *const _) < 0 {
            bail!("raw_gadget INIT: {}", std::io::Error::last_os_error());
        }
        if ioctl_none(raw, IOCTL_RUN) < 0 {
            bail!("raw_gadget RUN: {}", std::io::Error::last_os_error());
        }

        let serial = format!("PFDECK{index:04}");
        let unit_id = 0x5046_0000u32 | index as u32; // "PF" + index — a synthetic per-instance device id
        let report = Arc::new(Mutex::new(neutral_report()));
        let feedback = Arc::new(Mutex::new(Default::default()));
        let running = Arc::new(AtomicBool::new(true));
        let ctrl_ep = Arc::new(std::sync::atomic::AtomicI32::new(-1));
        let configured = Arc::new(AtomicBool::new(false));

        // Control thread: enumerate + answer every control transfer.
        let control = {
            let fd = fd.clone();
            let running = running.clone();
            let ctrl_ep = ctrl_ep.clone();
            let configured = configured.clone();
            let feedback = feedback.clone();
            std::thread::Builder::new()
                .name("pf-deck-gadget-ctrl".into())
                .spawn(move || {
                    control_loop(fd, running, ctrl_ep, configured, feedback, serial, unit_id)
                })
                .context("spawn gadget control thread")?
        };
        // Stream thread: push the current report on the controller interrupt-IN endpoint.
        let stream = {
            let fd = fd.clone();
            let running = running.clone();
            let ctrl_ep = ctrl_ep.clone();
            let configured = configured.clone();
            let report = report.clone();
            std::thread::Builder::new()
                .name("pf-deck-gadget-stream".into())
                .spawn(move || stream_loop(fd, running, ctrl_ep, configured, report))
                .context("spawn gadget stream thread")?
        };

        Ok(SteamDeckGadget {
            report,
            feedback,
            running,
            threads: vec![control, stream],
            _fd: fd,
            seq: 0,
        })
    }

    /// Serialize `st` into the 64-byte Deck state report streamed to the kernel.
    pub fn write_state(&mut self, st: &super::steam_proto::SteamState) {
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; 64];
        super::steam_proto::serialize_deck_state(&mut r, st, self.seq);
        if let Ok(mut g) = self.report.lock() {
            *g = r;
        }
    }

    /// Drain any feedback (rumble) the kernel/Steam wrote to the device.
    pub fn service(&mut self) -> super::steam_proto::SteamFeedback {
        self.feedback
            .lock()
            .map(|mut f| std::mem::take(&mut *f))
            .unwrap_or_default()
    }
}

impl Drop for SteamDeckGadget {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn neutral_report() -> [u8; 64] {
    let mut r = [0u8; 64];
    r[0] = 0x01;
    r[2] = 0x09; // ID_CONTROLLER_DECK_STATE
    r[3] = 0x3C;
    r
}

fn copy_cstr(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

fn control_loop(
    fd: Arc<GadgetFd>,
    running: Arc<AtomicBool>,
    ctrl_ep: Arc<std::sync::atomic::AtomicI32>,
    configured: Arc<AtomicBool>,
    feedback: Arc<Mutex<super::steam_proto::SteamFeedback>>,
    serial: String,
    unit_id: u32,
) {
    let raw = fd.0;
    let cfg = build_config();
    let mut last_set: Vec<u8> = Vec::new();
    let mut evbuf = [0u8; EVENT_BUF];
    while running.load(Ordering::SeqCst) {
        // EVENT_FETCH: type(4) length(4) data[].
        evbuf[4..8].copy_from_slice(&(8u32).to_ne_bytes()); // request setup-sized payload
        let r = ioctl_mut(raw, IOCTL_EVENT_FETCH, evbuf.as_mut_ptr());
        if r < 0 {
            if running.load(Ordering::SeqCst) {
                // transient; brief backoff
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            continue;
        }
        let etype = u32::from_ne_bytes([evbuf[0], evbuf[1], evbuf[2], evbuf[3]]);
        match etype {
            USB_RAW_EVENT_CONNECT => {}
            USB_RAW_EVENT_CONTROL => {
                let s = &evbuf[EVENT_HDR..EVENT_HDR + 8];
                let ctrl = Setup {
                    bm_request_type: s[0],
                    b_request: s[1],
                    w_value: u16::from_le_bytes([s[2], s[3]]),
                    w_index: u16::from_le_bytes([s[4], s[5]]),
                    w_length: u16::from_le_bytes([s[6], s[7]]),
                };
                handle_control(
                    raw,
                    &ctrl,
                    &cfg,
                    &serial,
                    unit_id,
                    &ctrl_ep,
                    &configured,
                    &mut last_set,
                    &feedback,
                );
            }
            _ => {}
        }
    }
}

struct Setup {
    bm_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    w_length: u16,
}

#[allow(clippy::too_many_arguments)]
fn handle_control(
    raw: RawFd,
    ctrl: &Setup,
    cfg: &[u8],
    serial: &str,
    unit_id: u32,
    ctrl_ep: &std::sync::atomic::AtomicI32,
    configured: &AtomicBool,
    last_set: &mut Vec<u8>,
    feedback: &Mutex<super::steam_proto::SteamFeedback>,
) {
    let idx = (ctrl.w_index & 0xff) as u8;
    let type_class = ctrl.bm_request_type & 0x60;
    let wl = ctrl.w_length as usize;
    if type_class == 0x00 {
        // standard
        match ctrl.b_request {
            0x06 => {
                // GET_DESCRIPTOR
                let dt = (ctrl.w_value >> 8) as u8;
                let di = (ctrl.w_value & 0xff) as u8;
                let resp: Vec<u8> = match dt {
                    1 => DEV_DESC.to_vec(),
                    2 => cfg.to_vec(),
                    3 => string_desc(di, serial),
                    HID_RPT_DT => match idx {
                        0 => RDESC_MOUSE.to_vec(),
                        1 => RDESC_KBD.to_vec(),
                        _ => RDESC_CTRL.to_vec(),
                    },
                    HID_DT => {
                        // re-emit the interface's HID descriptor from the config blob (best effort)
                        hid_desc_for(cfg, idx)
                    }
                    _ => {
                        ep0_stall(raw);
                        return;
                    }
                };
                let n = resp.len().min(wl);
                ep0_write(raw, &resp[..n]);
            }
            0x09 => {
                // SET_CONFIGURATION
                ioctl_val(raw, IOCTL_VBUS_DRAW, 0x32);
                ioctl_none(raw, IOCTL_CONFIGURE);
                enable_endpoints(raw, ctrl_ep);
                ep0_ack(raw);
                configured.store(true, Ordering::SeqCst);
            }
            0x0b => ep0_ack(raw), // SET_INTERFACE
            0x00 => {
                let st = 0u16;
                ep0_write(raw, &st.to_le_bytes());
            }
            _ => ep0_stall(raw),
        }
    } else if type_class == 0x20 {
        // HID class
        match ctrl.b_request {
            0x01 => {
                // GET_REPORT — serve the Deck feature reply for the last requested command.
                let resp = feature_reply(last_set, serial, unit_id);
                let n = resp.len().min(wl);
                ep0_write(raw, &resp[..n]);
            }
            0x09 => {
                // SET_REPORT — read the host's data; remember it + extract feedback.
                let (r, data) = ep0_read(raw, wl);
                if r > 0 {
                    *last_set = data.clone();
                    // parse_steam_output expects [report-id(0), cmd, …]; EP0 OUT data is [cmd, …].
                    let mut framed = Vec::with_capacity(data.len() + 1);
                    framed.push(0);
                    framed.extend_from_slice(&data);
                    let fb = super::steam_proto::parse_steam_output(&framed);
                    if fb.rumble.is_some() {
                        if let Ok(mut g) = feedback.lock() {
                            *g = fb;
                        }
                    }
                }
            }
            0x0a | 0x0b => ep0_ack(raw), // SET_IDLE / SET_PROTOCOL
            0x03 => {
                ep0_write(raw, &[0u8]);
            } // GET_PROTOCOL
            _ => ep0_stall(raw),
        }
    } else {
        ep0_stall(raw);
    }
}

/// Build the HID feature GET_REPORT reply for the host's last SET_REPORT command. Steam's
/// `GetControllerInfo` reads the `0x83` attributes + the `0xAE` serial; **serving the real `0x83`
/// blob is what stops Steam re-probing** (the gamepad-evdev churn). The contract (`0x83` 9-attribute
/// layout + the `0xAE` string format) was captured from a physical Steam Deck via hidraw. `unit_id`
/// stamps a per-instance value into the device-id attributes (`0x0a`/`0x04`) so a gadget never
/// collides with a real Deck or another gadget.
fn feature_reply(last_set: &[u8], serial: &str, unit_id: u32) -> [u8; 64] {
    let cmd = last_set.first().copied().unwrap_or(0xAE);
    let mut r = [0u8; 64];
    match cmd {
        0x83 => {
            // GET_ATTRIBUTES_VALUES: [0x83, 0x2d, then 9× (attr-id, value u32-LE)].
            r[0] = 0x83;
            r[1] = 0x2d;
            let attrs: [(u8, u32); 9] = [
                (0x01, 0x1205), // product id
                (0x02, 0),
                (0x0a, unit_id), // unit serial number (per-instance)
                (0x04, unit_id ^ 0x5555_5555),
                (0x09, 0x2e),
                (0x0b, 0x0fa0),
                (0x0d, 0),
                (0x0c, 0),
                (0x0e, 0),
            ];
            let mut o = 2;
            for (id, val) in attrs {
                r[o] = id;
                r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                o += 5;
            }
        }
        0xAE => {
            // GET_STRING_ATTRIBUTE: [0xAE, len, attr, ascii…]. The kernel validates the serial (attr
            // 0x01) wants reply[2]==0x01 and 1<=len<=21; for other attrs we echo the requested id.
            let attr = last_set.get(2).copied().unwrap_or(0x01);
            let b = serial.as_bytes();
            let len = b.len().clamp(1, 20);
            r[0] = 0xAE;
            r[1] = len as u8;
            r[2] = attr;
            r[3..3 + len].copy_from_slice(&b[..len]);
        }
        _ => {
            // Settings read-back (e.g. 0x87): echo the host's last command + data.
            let n = last_set.len().min(64);
            r[..n].copy_from_slice(&last_set[..n]);
        }
    }
    r
}

fn hid_desc_for(cfg: &[u8], idx: u8) -> Vec<u8> {
    // The HID descriptors live right after each interface descriptor in the config blob.
    // Offsets: cfg(9) | i0(9) h0(9) e0(7) | i1(9) h1(9) e1(7) | i2(9) h2(9) e2(7)
    let off = match idx {
        0 => 9 + 9,
        1 => 9 + 25 + 9,
        _ => 9 + 50 + 9,
    };
    cfg.get(off..off + 9)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}

fn enable_endpoints(raw: RawFd, ctrl_ep: &std::sync::atomic::AtomicI32) {
    let mk = |addr: u8, mps: u16| UsbEndpointDescriptor {
        b_length: 7,
        b_descriptor_type: 5,
        b_endpoint_address: addr,
        bm_attributes: 0x03,
        w_max_packet_size: mps,
        b_interval: 4,
        ..Default::default()
    };
    let e0 = mk(0x81, 8);
    let e1 = mk(0x82, 8);
    let e2 = mk(0x83, 64);
    ioctl_ptr(raw, IOCTL_EP_ENABLE, &e0 as *const _);
    ioctl_ptr(raw, IOCTL_EP_ENABLE, &e1 as *const _);
    let h2 = ioctl_ptr(raw, IOCTL_EP_ENABLE, &e2 as *const _);
    ctrl_ep.store(h2, Ordering::SeqCst);
}

fn stream_loop(
    fd: Arc<GadgetFd>,
    running: Arc<AtomicBool>,
    ctrl_ep: Arc<std::sync::atomic::AtomicI32>,
    configured: Arc<AtomicBool>,
    report: Arc<Mutex<[u8; 64]>>,
) {
    let raw = fd.0;
    while running.load(Ordering::SeqCst) {
        let ep = ctrl_ep.load(Ordering::SeqCst);
        if configured.load(Ordering::SeqCst) && ep >= 0 {
            let r = report
                .lock()
                .map(|g| *g)
                .unwrap_or_else(|_| neutral_report());
            let mut buf = [0u8; EPIO_HDR + 64];
            buf[0..2].copy_from_slice(&(ep as u16).to_ne_bytes());
            buf[4..8].copy_from_slice(&(64u32).to_ne_bytes());
            buf[EPIO_HDR..].copy_from_slice(&r);
            // Blocks until the host polls the interrupt-IN endpoint; that's fine on its own thread.
            ioctl_ptr(raw, IOCTL_EP_WRITE, buf.as_ptr());
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

/// Best-effort load of the gadget modules (SteamOS ships `dummy_hcd` + `raw_gadget`). Failures are
/// ignored — the caller falls back to UHID if `/dev/raw-gadget` is then still unusable.
pub fn ensure_modules() {
    for m in ["dummy_hcd", "raw_gadget"] {
        let _ = std::process::Command::new("modprobe").arg(m).status();
    }
}

/// Whether to prefer the USB-gadget Deck over the UHID `SteamDeckPad` — the only transport Steam Input
/// promotes (validated glass-to-glass on a Deck). Defaults **on for SteamOS** hosts (which ship the
/// gadget modules + run Steam Input); off elsewhere, where the universal UHID path stays the default.
/// `PUNKTFUNK_STEAM_GADGET=1`/`0` forces it on/off. A Deck-as-host with a *physical* Deck never reaches
/// here: `resolve_gamepad`'s conflict gate degrades `SteamDeck` → DualSense before the manager is built.
pub fn gadget_preferred() -> bool {
    if let Ok(v) = std::env::var("PUNKTFUNK_STEAM_GADGET") {
        return v == "1" || v.eq_ignore_ascii_case("true");
    }
    is_steamos()
}

/// True on SteamOS-class hosts (`/etc/os-release` `ID=steamos`, or `ID_LIKE` naming it).
fn is_steamos() -> bool {
    std::fs::read_to_string("/etc/os-release")
        .map(|s| {
            s.lines()
                .any(|l| l == "ID=steamos" || (l.starts_with("ID_LIKE=") && l.contains("steamos")))
        })
        .unwrap_or(false)
}
