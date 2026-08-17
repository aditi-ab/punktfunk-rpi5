//! Virtual Steam Deck over **USB/IP** (`vhci_hcd`) — the shippable, Secure-Boot-clean, universal
//! alternative to [`super::steam_gadget`] (`raw_gadget` + `dummy_hcd`, SteamOS-only).
//!
//! Like the gadget, this presents a *real* 3-interface USB Steam Deck (mouse = interface 0, keyboard
//! = 1, **controller = 2**) — the interface-2 layout Steam's own driver filters on, so Steam Input
//! promotes it (a UHID Deck, `Interface: -1`, never is). Unlike the gadget it needs no out-of-tree
//! module: `vhci_hcd` is in-tree + signed on SteamOS, Bazzite, and ~every distro, loads under Secure
//! Boot, and needs no MOK. A userspace [`usbip_sim`] server emulates the Deck; the local `vhci_hcd`
//! attaches it. **Validated on Bazzite**: `vhci_hcd` enumerates the 3-interface Deck, `hid-steam`
//! binds it, and Steam reserves an XInput slot — identical recognition to the gadget.
//!
//! The device model + the USB/IP protocol come from the vendored [`usbip_sim`] crate (the upstream
//! `usbip` crate trimmed of its libusb host mode); the captured descriptors + the `0x83`/`0xAE`
//! feature contract come from the shared [`super::steam_proto`] (one source of truth with the gadget).
//!
//! **Attach** is in-process by default (no external `usbip` CLI dependency — the production goal): we
//! run the emulation server on a loopback TCP port, connect to it ourselves, perform the
//! `OP_REQ_IMPORT` handshake, then hand the connected socket fd to `vhci_hcd` via its sysfs `attach`
//! file. If anything in that path fails we fall back to the widely-packaged `usbip` CLI; if *that*
//! also fails, [`open`](SteamDeckUsbip::open) returns `Err` and the caller degrades to UHID.

use super::steam_proto::{
    deck_serial, deck_unit_id, feature_reply, neutral_deck_report, parse_steam_output,
    SteamFeedback, SteamState, RDESC_DECK_CTRL, RDESC_DECK_KBD, RDESC_DECK_MOUSE,
};
use anyhow::{bail, Context, Result};
use std::any::Any;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use usbip_sim::{
    Direction, SetupPacket, UsbDevice, UsbEndpoint, UsbInterface, UsbInterfaceHandler, UsbIpServer,
    Version,
};

const STEAM_VENDOR: u16 = 0x28DE;
const STEAMDECK_PRODUCT: u16 = 0x1205;
/// The single device's USB/IP bus id (one device per server, so the fixed default is fine).
const BUS_ID: &str = "0-0-0";
/// The usbip default TCP port — the server must listen here for the `usbip` CLI fallback to attach.
const USBIP_TCP_PORT: u16 = 3240;

/// Build the 9-byte HID class descriptor inserted between the interface and endpoint descriptors.
fn hid_desc(report_len: usize, country: u8) -> Vec<u8> {
    let l = report_len as u16;
    #[rustfmt::skip]
    let d = vec![0x09, 0x21, 0x10, 0x01, country, 1, 0x22, (l & 0xff) as u8, (l >> 8) as u8];
    d
}

/// The Deck **controller** interface (vendor HID, interface 2): answers the HID feature reports
/// (descriptor / `0x83` attributes / `0xAE` serial), streams the current 64-byte state on the
/// interrupt-IN endpoint, and surfaces rumble written via SET_REPORT.
#[derive(Debug)]
struct ControllerHandler {
    /// The current 64-byte Deck input report, shared with [`SteamDeckUsbip::write_state`].
    report: Arc<Mutex<[u8; 64]>>,
    /// Rumble extracted from the kernel's SET_REPORTs, drained by [`SteamDeckUsbip::service`].
    feedback: Arc<Mutex<SteamFeedback>>,
    /// The host's last SET_REPORT command (drives [`feature_reply`]).
    last_set: Vec<u8>,
    serial: String,
    unit_id: u32,
}

impl UsbInterfaceHandler for ControllerHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_desc(RDESC_DECK_CTRL.len(), 33)
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
                // GET report descriptor (standard, interface recipient).
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => RDESC_DECK_CTRL.to_vec(),
                // HID GET_REPORT (feature) — the Deck `0x83`/`0xAE` contract.
                (0xA1, 0x01) => feature_reply(&self.last_set, &self.serial, self.unit_id).to_vec(),
                // HID SET_REPORT — remember the command (for the next feature reply) + surface rumble.
                (0x21, 0x09) => {
                    self.last_set = req.to_vec();
                    // `parse_steam_output` expects `[report-id(0), cmd, …]`; EP0 OUT data is `[cmd, …]`.
                    let mut framed = Vec::with_capacity(req.len() + 1);
                    framed.push(0);
                    framed.extend_from_slice(req);
                    let fb = parse_steam_output(&framed);
                    if fb.rumble.is_some() {
                        if let Ok(mut g) = self.feedback.lock() {
                            *g = fb;
                        }
                    }
                    vec![]
                }
                (0x21, 0x0A) | (0x21, 0x0B) => vec![], // SET_IDLE / SET_PROTOCOL
                _ => vec![],
            })
        } else if let Direction::In = ep.direction() {
            // Interrupt-IN poll: return the current report. The vendored sim paces interrupt-IN by
            // bInterval (vhci_hcd does NOT throttle the server side), so this isn't a busy spin.
            let r = self
                .report
                .lock()
                .map(|g| *g)
                .unwrap_or_else(|_| neutral_deck_report());
            Ok(r.to_vec())
        } else {
            Ok(vec![])
        }
    }
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// A minimal idle HID interface (mouse / keyboard) — serves only its report descriptor.
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

pub(crate) fn boxed(
    h: impl UsbInterfaceHandler + Send + 'static,
) -> Arc<Mutex<Box<dyn UsbInterfaceHandler + Send>>> {
    Arc::new(Mutex::new(Box::new(h)))
}
fn ep(addr: u8, mps: u16) -> UsbEndpoint {
    UsbEndpoint {
        address: addr,
        attributes: 0x03, // interrupt
        max_packet_size: mps,
        interval: 4,
    }
}

/// Assemble the simulated 3-interface USB Deck. The controller handler shares `report` + `feedback`
/// with the owning [`SteamDeckUsbip`].
fn build_device(
    index: u8,
    report: &Arc<Mutex<[u8; 64]>>,
    feedback: &Arc<Mutex<SteamFeedback>>,
) -> UsbDevice {
    let mut dev = UsbDevice::new(0); // one device per server; bus_id stays the default "0-0-0".
    dev.vendor_id = STEAM_VENDOR;
    dev.product_id = STEAMDECK_PRODUCT;
    dev.usb_version = Version::from(0x0200u16); // bcdUSB 2.00
    dev.device_bcd = Version::from(0x0300u16); // bcdDevice 3.00 (matches the gadget)
    dev.set_manufacturer_name("Valve Software");
    dev.set_product_name("Steam Deck Controller");
    dev.set_serial_number(&deck_serial(index));
    dev.with_interface(
        0x03,
        0x00,
        0x02,
        Some("mouse"),
        vec![ep(0x81, 8)],
        boxed(IdleHidHandler {
            report_desc: RDESC_DECK_MOUSE.to_vec(),
        }),
    )
    .with_interface(
        0x03,
        0x01,
        0x01,
        Some("keyboard"),
        vec![ep(0x82, 8)],
        boxed(IdleHidHandler {
            report_desc: RDESC_DECK_KBD.to_vec(),
        }),
    )
    .with_interface(
        0x03,
        0x00,
        0x00,
        Some("controller"),
        vec![ep(0x83, 64)],
        boxed(ControllerHandler {
            report: report.clone(),
            feedback: feedback.clone(),
            last_set: vec![],
            serial: deck_serial(index),
            unit_id: deck_unit_id(index),
        }),
    )
}

/// Owns the emulation-server thread (a dedicated current-thread tokio runtime) and stops it on drop.
/// Run on its own thread so `SteamDeckUsbip::open` works whether or not the caller is inside a tokio
/// runtime (creating a runtime inside one would panic).
struct ServerThread {
    stop: Arc<tokio::sync::Notify>,
    join: Option<JoinHandle<()>>,
}

impl ServerThread {
    /// Spawn the server on `listener`, serving exactly the one simulated `dev`. `label` names the
    /// device in log lines and in the `PUNKTFUNK_USBIP_TRACE` file names.
    fn spawn(listener: std::net::TcpListener, dev: UsbDevice, label: &str) -> Result<ServerThread> {
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_t = stop.clone();
        let label = label.to_string();
        let join = std::thread::Builder::new()
            .name("pf-deck-usbip".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(error = %e, "usbip server runtime build failed");
                        return;
                    }
                };
                rt.block_on(run_server(
                    listener,
                    Arc::new(UsbIpServer::new_simulated(vec![dev])),
                    stop_t,
                    label,
                ));
            })
            .context("spawn usbip server thread")?;
        Ok(ServerThread {
            stop,
            join: Some(join),
        })
    }
}

impl Drop for ServerThread {
    fn drop(&mut self) {
        self.stop.notify_one();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Accept loop: serve each USB/IP connection with the vendored `usbip_sim::handler` until stopped.
async fn run_server(
    listener: std::net::TcpListener,
    server: Arc<UsbIpServer>,
    stop: Arc<tokio::sync::Notify>,
    label: String,
) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "usbip TcpListener::from_std failed");
            return;
        }
    };
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            r = listener.accept() => match r {
                Ok((mut sock, _)) => {
                    // URB replies are small and interleave with the kernel's next SUBMITs; without
                    // TCP_NODELAY the multi-interface request/response pattern collapses into
                    // ~40 ms Nagle/delayed-ACK stalls (observed as ~22 reports/s on the Puck's
                    // active hidraw against a 266 Hz source).
                    sock.set_nodelay(true).ok();
                    let server = server.clone();
                    let trace = super::usbip_trace::trace_prefix(&label);
                    let label = label.clone();
                    tokio::spawn(async move {
                        // The handler's Err arm used to be discarded. It is the *only* signal that
                        // we tore the connection down rather than the kernel — and the kernel's
                        // side of that (`recv xbuf`, `sendmsg failed`) reads identically either
                        // way, so throwing it away cost days of mis-attributed diagnosis.
                        let sink = trace.and_then(|prefix| {
                            match super::usbip_trace::open_trace(&prefix) {
                                Ok(s) => {
                                    tracing::info!(prefix, "usbip byte trace armed");
                                    Some(s)
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "usbip trace files unopenable — running untraced");
                                    None
                                }
                            }
                        });
                        let res = match sink {
                            Some(s) => {
                                let mut traced = super::usbip_trace::TracedIo::wrap(sock, s);
                                usbip_sim::handler(&mut traced, server).await
                            }
                            None => usbip_sim::handler(&mut sock, server).await,
                        };
                        match res {
                            Ok(()) => tracing::debug!(label, "usbip connection closed by the kernel"),
                            Err(e) => tracing::warn!(
                                label,
                                error = %e,
                                "usbip server dropped the connection — the kernel will report this as a \
                                 transfer error on whatever URB was in flight"
                            ),
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "usbip accept error");
                    break;
                }
            }
        }
    }
}

/// A usbip-attached simulated device: the `vhci_hcd` port plus the socket + emulation server
/// keeping it alive. Dropping it detaches the port FIRST (the kernel closes its socket end and
/// tears the device down — Steam releases its slot), then drops the socket and stops the server —
/// the teardown order the Deck transport shipped with. Shared by every usbip-presented pad
/// (the Deck here, the Steam Controller 2 in [`super::triton_usbip`]).
pub(crate) struct UsbipAttachment {
    /// The `vhci_hcd` port we attached to — written to the sysfs `detach` file on drop.
    vhci_port: u16,
    /// Kept alive so the connected socket fd we handed to `vhci_hcd` stays valid (in-process attach
    /// only; the CLI hands its own fd to the kernel and exits, so this is `None` there).
    _client_sock: Option<TcpStream>,
    /// Emulation-server thread; dropped (stopped) after the detach.
    _server: ServerThread,
}

impl Drop for UsbipAttachment {
    fn drop(&mut self) {
        if let Err(e) = vhci_detach(self.vhci_port) {
            tracing::debug!(port = self.vhci_port, error = %e, "vhci detach failed (device may already be gone)");
        }
    }
}

/// Attach a simulated USB device locally via `vhci_hcd`. Requires `vhci_hcd` loaded and root
/// (the sysfs attach / the CLI both need it). Tries the in-process sysfs attach first, then the
/// `usbip` CLI; `PUNKTFUNK_USBIP_ATTACH=inproc|cli` pins one path (for debugging). `build` is
/// invoked once per attempted path (a [`UsbDevice`] isn't reusable across servers); `label`
/// names the device in the attach log lines.
pub(crate) fn attach_device(build: impl Fn() -> UsbDevice, label: &str) -> Result<UsbipAttachment> {
    ensure_modules();
    if vhci_base().is_none() {
        bail!("vhci_hcd unavailable (no /sys/devices/platform/vhci_hcd*/status) — is it loaded?");
    }
    let mode = std::env::var("PUNKTFUNK_USBIP_ATTACH").ok();
    if mode.as_deref() != Some("cli") {
        match attach_in_process(build(), label) {
            Ok(a) => return Ok(a),
            Err(e) if mode.as_deref() == Some("inproc") => return Err(e),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "in-process vhci attach failed — trying the usbip CLI")
            }
        }
    }
    attach_via_cli(build(), label)
}

/// In-process attach: emulate on a loopback port, do the import handshake ourselves, hand the
/// connected socket to `vhci_hcd` via sysfs. No external dependency.
fn attach_in_process(dev: UsbDevice, label: &str) -> Result<UsbipAttachment> {
    // An ephemeral loopback port (avoids contending the usbip default with another pad).
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("bind loopback usbip server")?;
    let port = listener
        .local_addr()
        .context("usbip server local_addr")?
        .port();
    listener
        .set_nonblocking(true)
        .context("usbip listener set_nonblocking")?;
    let server = ServerThread::spawn(listener, dev, label)?;

    // Connect to our own server and run the OP_REQ_IMPORT handshake.
    let mut sock = connect_loopback(port).context("connect to usbip server")?;
    let (devid, speed) = import_handshake(&mut sock).context("usbip import handshake")?;

    // Hand the connected socket to vhci_hcd. Clear BOTH timeouts first: the kernel's vhci rx/tx
    // threads honour SO_RCVTIMEO/SO_SNDTIMEO on this socket, so the 3s handshake timeouts would
    // otherwise tear the device down after 3s idle (rx) or a 3s-blocked send (tx).
    let vhci_port = vhci_find_free_port(speed).context("find a free vhci port")?;
    sock.set_read_timeout(None).ok();
    sock.set_write_timeout(None).ok();
    vhci_attach(vhci_port, sock.as_raw_fd(), devid, speed).context("write vhci_hcd attach")?;

    tracing::info!(
        label,
        vhci_port,
        "attached via usbip (in-process — Steam Input recognizes it)"
    );
    Ok(UsbipAttachment {
        vhci_port,
        _client_sock: Some(sock),
        _server: server,
    })
}

/// Fallback: emulate on the usbip default port and let the `usbip` CLI attach (it picks the vhci
/// port itself; we recover it by diffing the sysfs status).
fn attach_via_cli(dev: UsbDevice, label: &str) -> Result<UsbipAttachment> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", USBIP_TCP_PORT))
        .with_context(|| format!("bind usbip default port {USBIP_TCP_PORT} for CLI attach"))?;
    listener
        .set_nonblocking(true)
        .context("usbip listener set_nonblocking")?;
    let server = ServerThread::spawn(listener, dev, label)?;

    let before = vhci_used_ports();
    usbip_attach_cli().context("usbip CLI attach")?;
    let vhci_port = wait_for_new_port(&before)
        .context("could not determine the vhci port the usbip CLI attached to")?;

    tracing::info!(
        label,
        vhci_port,
        "attached via usbip (CLI — Steam Input recognizes it)"
    );
    Ok(UsbipAttachment {
        vhci_port,
        _client_sock: None,
        _server: server,
    })
}

/// A virtual Steam Deck presented over USB/IP. Dropping it detaches the `vhci_hcd` port (the device
/// disappears, Steam releases its slot) and stops the emulation server.
pub struct SteamDeckUsbip {
    report: Arc<Mutex<[u8; 64]>>,
    feedback: Arc<Mutex<SteamFeedback>>,
    _attach: UsbipAttachment,
    seq: u32,
}

impl SteamDeckUsbip {
    /// Bind a virtual Deck and attach it locally via `vhci_hcd`. `index` varies only the serial.
    pub fn open(index: u8) -> Result<SteamDeckUsbip> {
        let report = Arc::new(Mutex::new(neutral_deck_report()));
        let feedback = Arc::new(Mutex::new(SteamFeedback::default()));
        let attach = attach_device(
            || build_device(index, &report, &feedback),
            &format!("virtual Steam Deck {index}"),
        )?;
        Ok(SteamDeckUsbip {
            report,
            feedback,
            _attach: attach,
            seq: 0,
        })
    }

    /// Serialize `st` into the 64-byte Deck report streamed on the controller interrupt-IN endpoint.
    pub fn write_state(&mut self, st: &SteamState) {
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; 64];
        super::steam_proto::serialize_deck_state(&mut r, st, self.seq);
        if let Ok(mut g) = self.report.lock() {
            *g = r;
        }
    }

    /// Drain any rumble feedback the kernel/Steam wrote to the device.
    pub fn service(&mut self) -> SteamFeedback {
        self.feedback
            .lock()
            .map(|mut f| std::mem::take(&mut *f))
            .unwrap_or_default()
    }
}

// ---- USB/IP import handshake (we act as the usbip *client* before handing the fd to the kernel) ----

const USBIP_VERSION: u16 = 0x0111;
const OP_REQ_IMPORT: u16 = 0x8003;

/// Connect to our own loopback server, retrying briefly while the server thread comes up.
fn connect_loopback(port: u16) -> Result<TcpStream> {
    let addr = ("127.0.0.1", port);
    let mut last = None;
    for _ in 0..50 {
        match TcpStream::connect(addr) {
            Ok(s) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    Err(anyhow::anyhow!(
        "connect 127.0.0.1:{port}: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Send `OP_REQ_IMPORT` for [`BUS_ID`] and read `OP_REP_IMPORT`, returning `(devid, speed)` parsed
/// from the device record (the same `devid = bus_num<<16 | dev_num` + speed `vhci_hcd` wants). The
/// whole 320-byte reply MUST be consumed here so the socket starts clean at the kernel's first
/// `USBIP_CMD_SUBMIT`.
fn import_handshake(sock: &mut TcpStream) -> Result<(u32, u32)> {
    // Bounded so a non-responsive server can't head-block the per-session input thread (this talks
    // to our own in-process loopback server, so a working handshake completes in well under a ms).
    sock.set_read_timeout(Some(Duration::from_secs(1))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(1))).ok();

    let mut req = Vec::with_capacity(40);
    req.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    req.extend_from_slice(&OP_REQ_IMPORT.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes()); // status
    let mut busid = [0u8; 32];
    let b = BUS_ID.as_bytes();
    busid[..b.len()].copy_from_slice(b);
    req.extend_from_slice(&busid);
    sock.write_all(&req).context("send OP_REQ_IMPORT")?;

    // Reply: version(2) code(2) status(4), then the 312-byte device record on success.
    let mut header = [0u8; 8];
    sock.read_exact(&mut header)
        .context("read OP_REP_IMPORT header")?;
    let status = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if status != 0 {
        bail!("OP_REP_IMPORT refused (status={status}) — device {BUS_ID} not exported?");
    }
    let mut dev = [0u8; 312];
    sock.read_exact(&mut dev)
        .context("read OP_REP_IMPORT device record")?;
    // Device record layout: path[256], bus_id[32], bus_num(4 BE)@288, dev_num(4 BE)@292, speed(4)@296.
    let be = |o: usize| u32::from_be_bytes([dev[o], dev[o + 1], dev[o + 2], dev[o + 3]]);
    let bus_num = be(288);
    let dev_num = be(292);
    let speed = be(296);
    Ok(((bus_num << 16) | dev_num, speed))
}

// ---- vhci_hcd sysfs plumbing ----

/// Best-effort load of `vhci_hcd` (in-tree + signed on SteamOS/Bazzite/most distros).
pub fn ensure_modules() {
    let _ = Command::new("modprobe").arg("vhci_hcd").status();
}

/// Run `usbip attach -r 127.0.0.1 -b 0-0-0`, bounded by a deadline so a hung CLI can't head-block
/// the per-session input thread indefinitely (the caller runs this inline on that thread).
fn usbip_attach_cli() -> Result<()> {
    let mut child = Command::new("usbip")
        .args(["attach", "-r", "127.0.0.1", "-b", BUS_ID])
        .spawn()
        .context("spawn `usbip attach` (is usbip-utils installed?)")?;
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        match child.try_wait().context("wait on `usbip attach`")? {
            Some(st) if st.success() => return Ok(()),
            Some(st) => bail!("`usbip attach` exited with {st}"),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("`usbip attach` timed out (>6s) — killed");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Whether a usbip attach should be attempted at all. Default on (the universal Steam-promotable
/// transport on non-SteamOS hosts); `PUNKTFUNK_STEAM_USBIP=0` forces it off, `=1` forces it on.
/// [`open`](SteamDeckUsbip::open) still degrades gracefully if `vhci_hcd` turns out to be absent.
pub fn usbip_preferred() -> bool {
    !matches!(
        std::env::var("PUNKTFUNK_STEAM_USBIP").ok().as_deref(),
        Some("0") | Some("false")
    )
}

/// The `vhci_hcd.0` (or legacy `vhci_hcd`) platform sysfs directory, if present.
/// `pub(crate)` so the diagnostics probe ([`crate::vhci_probe`]) asks the same question the attach
/// path asks, rather than growing a second copy of these paths that can drift from it.
pub(crate) fn vhci_base() -> Option<PathBuf> {
    for p in [
        "/sys/devices/platform/vhci_hcd.0",
        "/sys/devices/platform/vhci_hcd",
    ] {
        let base = Path::new(p);
        if base.join("status").exists() {
            return Some(base.to_path_buf());
        }
    }
    None
}

fn read_status() -> Result<String> {
    let base = vhci_base().context("vhci_hcd sysfs not present")?;
    std::fs::read_to_string(base.join("status")).context("read vhci_hcd status")
}

/// One parsed `status` row: `(port, hub_is_superspeed, sta)`. Handles both the modern
/// `hub port sta …` and the legacy `port sta …` column layouts; returns `None` for header/blank rows.
fn parse_status_row(line: &str) -> Option<(u16, bool, u32)> {
    let t: Vec<&str> = line.split_whitespace().collect();
    if t.is_empty() {
        return None;
    }
    let (hub_ss, port_str, sta_str) = if t[0] == "hs" || t[0] == "ss" {
        (Some(t[0] == "ss"), *t.get(1)?, *t.get(2)?)
    } else if t[0].chars().all(|c| c.is_ascii_digit()) {
        (None, t[0], *t.get(1)?) // legacy: port sta …
    } else {
        return None; // header ("hub"/"prt"/"port" …)
    };
    let port = port_str.parse::<u16>().ok()?;
    let sta = sta_str.parse::<u32>().ok()?;
    Some((port, hub_ss.unwrap_or(false), sta))
}

/// `sta == 4` is `VDEV_ST_NULL` (a free port).
const VDEV_ST_NULL: u32 = 4;

/// Pick a free `vhci_hcd` port matching the device speed (`usbip_speed >= 5` ⇒ SuperSpeed hub).
fn vhci_find_free_port(usbip_speed: u32) -> Result<u16> {
    let want_ss = usbip_speed >= 5;
    let status = read_status()?;
    for line in status.lines() {
        if let Some((port, is_ss, sta)) = parse_status_row(line) {
            if sta == VDEV_ST_NULL && is_ss == want_ss {
                return Ok(port);
            }
        }
    }
    // Speed-class match failed (legacy single-hub status): take any free port.
    for line in status.lines() {
        if let Some((port, _, sta)) = parse_status_row(line) {
            if sta == VDEV_ST_NULL {
                return Ok(port);
            }
        }
    }
    bail!("no free vhci_hcd port (all ports in use?)")
}

/// Ports currently in use (`sta != VDEV_ST_NULL`) — snapshotted around a CLI attach to recover its port.
fn vhci_used_ports() -> HashSet<u16> {
    read_status()
        .unwrap_or_default()
        .lines()
        .filter_map(parse_status_row)
        .filter(|&(_, _, sta)| sta != VDEV_ST_NULL)
        .map(|(port, _, _)| port)
        .collect()
}

/// Poll the status file (briefly) for a port that became used since `before` — the one the CLI attached.
fn wait_for_new_port(before: &HashSet<u16>) -> Result<u16> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(p) = vhci_used_ports().difference(before).copied().min() {
            return Ok(p);
        }
        if Instant::now() >= deadline {
            bail!("no newly-attached vhci port appeared after `usbip attach`");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn vhci_attach(port: u16, sockfd: i32, devid: u32, speed: u32) -> Result<()> {
    let base = vhci_base().context("vhci_hcd sysfs not present")?;
    let line = format!("{port} {sockfd} {devid} {speed}");
    std::fs::write(base.join("attach"), line)
        .with_context(|| format!("write vhci_hcd attach (port {port}) — root?"))
}

fn vhci_detach(port: u16) -> Result<()> {
    let base = vhci_base().context("vhci_hcd sysfs not present")?;
    std::fs::write(base.join("detach"), format!("{port}")).context("write vhci_hcd detach")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `status` parser handles the modern `hub port sta …` layout, the legacy `port sta …`
    /// layout, and skips header/blank lines — a slip here would mean attaching to a busy port.
    #[test]
    fn status_parser_handles_both_layouts() {
        // modern
        assert_eq!(
            parse_status_row("hs  0000 004 000 00000000 000000 0-0"),
            Some((0, false, 4))
        );
        assert_eq!(
            parse_status_row("ss  0008 006 000 00000000 000000 0-0"),
            Some((8, true, 6))
        );
        // legacy (no hub column)
        assert_eq!(
            parse_status_row("0001 004 000 00000000 000000 0-0"),
            Some((1, false, 4))
        );
        // header / blank
        assert_eq!(
            parse_status_row("hub port sta spd dev      sockfd local_busid"),
            None
        );
        assert_eq!(parse_status_row(""), None);
    }

    /// A free HS port is preferred for an HS device; a free SS port for an SS device.
    #[test]
    fn free_port_selection_matches_speed() {
        let status = "hub port sta spd dev      sockfd local_busid\n\
                      hs  0000 006 000 00000000 000000 0-0\n\
                      hs  0001 004 000 00000000 000000 0-0\n\
                      ss  0008 004 000 00000000 000000 0-0\n";
        // Reuse the parser directly (vhci_find_free_port reads sysfs; test the selection logic).
        let hs = status
            .lines()
            .filter_map(parse_status_row)
            .find(|&(_, is_ss, sta)| sta == VDEV_ST_NULL && !is_ss)
            .map(|(p, _, _)| p);
        let ss = status
            .lines()
            .filter_map(parse_status_row)
            .find(|&(_, is_ss, sta)| sta == VDEV_ST_NULL && is_ss)
            .map(|(p, _, _)| p);
        assert_eq!(hs, Some(1));
        assert_eq!(ss, Some(8));
    }

    /// On-box smoke test (needs root + `vhci_hcd`): attach a virtual Deck, confirm `hid-steam` binds
    /// it (the `Steam Deck` evdev appears) and that it tears down on drop. `#[ignore]`d in CI.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root + vhci_hcd"]
    fn usbip_deck_binds_and_tears_down() {
        ensure_modules();
        let mut pad = SteamDeckUsbip::open(0).expect("open SteamDeckUsbip (root + vhci_hcd?)");
        let st = SteamState::from_gamepad(punktfunk_core::input::gamepad::BTN_A, 0, 0, 0, 0, 0, 0);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(800) {
            pad.write_state(&st);
            let _ = pad.service();
            std::thread::sleep(Duration::from_millis(8));
        }
        let devs = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            devs.contains("Steam Deck"),
            "hid-steam did not bind the usbip Deck"
        );
        drop(pad);
        std::thread::sleep(Duration::from_millis(300));
        let devs = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        assert!(
            !devs.contains("Steam Deck Motion Sensors"),
            "device not torn down on drop"
        );
    }

    /// On-box smoke test (needs root + `vhci_hcd`): rumble the attached virtual Deck exactly like
    /// Steam does — a `0xEB` feature SET_REPORT on the hid-steam hidraw node — and confirm
    /// [`SteamDeckUsbip::service`] surfaces `(left, right)` for the 0xCA plane. The Deck presents
    /// 3 interfaces (0 mouse / 1 kbd / 2 controller); only the CONTROLLER interface's EP0 handler
    /// parses feedback (the idle interfaces ACK silently, like real hardware), and Steam filters
    /// on interface 2 — so the write must land there. `#[ignore]`d in CI.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root + vhci_hcd"]
    fn usbip_deck_rumble_flows_via_controller_interface() {
        use super::super::steam_proto::ID_TRIGGER_RUMBLE_CMD;
        ensure_modules();
        let mut pad = SteamDeckUsbip::open(0).expect("open SteamDeckUsbip (root + vhci_hcd?)");
        let st = SteamState::from_gamepad(0, 0, 0, 0, 0, 0, 0);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            pad.write_state(&st);
            let _ = pad.service();
            std::thread::sleep(Duration::from_millis(8));
        }
        // The hid-steam hidraw node on USB interface 2 (bInterfaceNumber is the HID device's
        // parent attribute).
        let node = std::fs::read_dir("/sys/class/hidraw")
            .expect("/sys/class/hidraw")
            .flatten()
            .find_map(|e| {
                let ue =
                    std::fs::read_to_string(e.path().join("device/uevent")).unwrap_or_default();
                let iface = std::fs::read_to_string(e.path().join("device/../bInterfaceNumber"))
                    .ok()
                    .and_then(|s| u8::from_str_radix(s.trim(), 16).ok());
                (ue.lines().any(|l| l == "DRIVER=hid-steam") && iface == Some(2))
                    .then(|| format!("/dev/{}", e.file_name().to_string_lossy()))
            })
            .expect("no hid-steam hidraw on interface 2");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .expect("open hidraw");
        // steam_haptic_rumble: [report-id 0, 0xEB, len 9, 0, intensity(2), left(2), right(2), gain(2)]
        let mut buf = [0u8; 12];
        buf[1] = ID_TRIGGER_RUMBLE_CMD;
        buf[2] = 0x09;
        buf[6..8].copy_from_slice(&0xC000u16.to_le_bytes());
        buf[8..10].copy_from_slice(&0x4000u16.to_le_bytes());
        // HIDIOCSFEATURE(12)
        let req: libc::c_ulong =
            (3 << 30) | ((buf.len() as libc::c_ulong) << 16) | (0x48 << 8) | 0x06;
        // SAFETY: HIDIOCSFEATURE reads the 12-byte report from the live `buf` behind the valid
        // hidraw fd `f`; the length is encoded in the request, so nothing is written past it.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, buf.as_mut_ptr()) };
        assert!(
            rc >= 0,
            "HIDIOCSFEATURE: {}",
            std::io::Error::last_os_error()
        );
        let start = Instant::now();
        let mut got = None;
        while got.is_none() && start.elapsed() < Duration::from_millis(1500) {
            got = pad.service().rumble;
            pad.write_state(&st);
            std::thread::sleep(Duration::from_millis(8));
        }
        assert_eq!(
            got,
            Some((0xC000, 0x4000)),
            "Deck rumble never surfaced from the interface-2 SET_REPORT"
        );
    }
}
