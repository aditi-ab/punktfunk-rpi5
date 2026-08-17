//! Virtual Sony DualSense over **USB/IP** (`vhci_hcd`) — a *composite* pad that carries its own
//! USB Audio Class sound card, so the host sees the pad the way a physically-plugged DS5 presents.
//!
//! # Why this exists (the uhid pad is not enough)
//!
//! [`super::dualsense`] creates the pad with `/dev/uhid`. That is enough for the kernel — the HID
//! device claims `bus = BUS_USB` and `hid-playstation` binds it — but the *sysfs topology* is a lie:
//!
//! ```text
//! /sys/devices/virtual/misc/uhid/0003:054C:0CE6.000C  [hid]
//!   -> /sys/devices/virtual/misc/uhid                 [misc]
//!   -> /sys/devices/virtual
//! ```
//!
//! There is no `usb_device` anywhere on that chain, and two things a DualSense game depends on are
//! derived by *walking* it:
//!
//! 1. **Wine's ContainerId.** `winebus`'s `get_container_id_for_usb_udev_device` walks a HID device
//!    up to its `usb_device` parent to synthesise the Windows ContainerId. With no USB ancestor it
//!    logs `Failed to get parent device.` and every endpoint registers as `GUID_NULL`. A
//!    libScePad-style title pairs "my controller" with "my controller's speaker" by ContainerId, so
//!    the pad reads as having no speaker and the game never opens the haptic stream at all.
//! 2. **GE-Proton's raw-ALSA leg.** GE finds the pad's haptics by enumerating real ALSA **cards**
//!    (`snd_card_next` → `snd_card_get_name`/`get_longname` → `snd_ctl_pcm_next_device` →
//!    `snd_pcm_open` demanding 48 kHz / S16 / **4 channels**). Minted PipeWire nodes are not ALSA
//!    cards and `snd_card_next` cannot see them, however faithfully their proplist impersonates one
//!    — which is why [`crate::pad_sink`](../../../punktfunk-host/src/audio/linux/pad_sink.rs) never
//!    sets `api.alsa.path`.
//!
//! Both fall out of the same missing fact, and both are fixed by the same change: make the pad a
//! **real USB device**. `vhci_hcd` gives us one with no out-of-tree module and no Secure Boot
//! trouble — the transport [`super::steam_usbip`] already ships and validates on Bazzite/SteamOS.
//!
//! # What this presents
//!
//! The real DualSense's own 4-interface composite layout, reproduced from a `lsusb -v` capture of a
//! wired `054c:0ce6` (see [`tests::config_descriptor_matches_hardware`], which pins the assembled
//! `wTotalLength` to the hardware's `0x00E3`):
//!
//! | interface | class | contents |
//! |---|---|---|
//! | 0 | Audio **Control** | the topology: USB-streaming IN terminal (4ch) → feature unit → Speaker OUT terminal, plus the headset capture chain |
//! | 1 | Audio **Streaming** (out) | alt 0 = zero-bandwidth, alt 1 = isochronous OUT `0x01`, **S16LE 4ch 48 kHz** — the haptics + speaker |
//! | 2 | Audio **Streaming** (in) | alt 0 = zero-bandwidth, alt 1 = isochronous IN `0x82`, S16LE 2ch 48 kHz — the headset mic |
//! | 3 | **HID** | interrupt IN `0x84` / OUT `0x03`, the same report `0x01`/`0x02` codec the uhid pad uses |
//!
//! Because interfaces 0–2 are a genuine UAC 1.0 device, `snd-usb-audio` binds them and mints a real
//! ALSA card named `DualSense Wireless Controller` — the card GE scans for. PipeWire's ALSA monitor
//! then creates the `…HiFi__Speaker__sink` / `…HiFi__SpeakerHaptic__sink` nodes *itself*, from the
//! distro's DualSense UCM, so the node graph is not impersonated any more: it is the real thing.
//!
//! # Where the audio comes out
//!
//! Whatever the game writes lands on us as isochronous OUT packets on endpoint `0x01`, which is the
//! single point every route converges on — PipeWire-mixed or a raw `hw:X,0` grab alike. The handler
//! converts those S16LE quad frames to `f32` and publishes them on a per-pad channel that
//! `punktfunk-host`'s pad-audio streamer drains ([`take_audio_rx`]); the 0xD1 wire path downstream
//! is unchanged.
//!
//! # Known limitation: URBs are answered one at a time
//!
//! The vendored server handles one URB per connection at a time — read a `USBIP_CMD_SUBMIT`, answer
//! it, read the next — and both the interrupt-IN and the isochronous paths *sleep* for their
//! service interval before answering (they must: `vhci_hcd` does not throttle the server side, and
//! for an audio endpoint the completion rate is the sample clock). So while audio is streaming, a
//! HID input poll can queue behind an isochronous URB and wait up to that URB's duration.
//!
//! In practice that lands the pad's input polling in the same band as a **physical** DualSense,
//! which declares `bInterval 6` = 4 ms; it is nevertheless slower than the uhid pad, which has no
//! polling model at all. Fixing it properly means answering URBs concurrently — USB/IP already
//! permits out-of-order completion, since every `USBIP_RET_SUBMIT` carries its `seqnum` — by
//! splitting the socket and spawning per-URB tasks behind a shared writer. That is a change to the
//! vendored server's concurrency model and is deliberately **not** bundled with the first landing
//! of this transport.
//!
//! # Report descriptor fidelity
//!
//! The hardware declares a 289-byte HID report descriptor; we serve
//! [`DUALSENSE_RDESC`](super::dualsense_proto::DUALSENSE_RDESC) (273 bytes), the descriptor the uhid
//! pad has always served and that `hid-playstation` is proven to bind. `wDescriptorLength` follows
//! whatever we actually serve, so the two stay consistent.

use super::dualsense_proto::{
    ds_pairing_reply, parse_ds_output, serialize_state, DsFeedback, DsState,
    DS_FEATURE_CALIBRATION, DS_FEATURE_FIRMWARE, DS_INPUT_REPORT_LEN, DS_PRODUCT, DS_VENDOR,
    DUALSENSE_RDESC,
};
use super::steam_usbip::{attach_device, boxed, UsbipAttachment};
use crate::sensor_clock::SensorClock;
use anyhow::Result;
use std::any::Any;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use usbip_sim::{
    Direction, IsoPacket, SetupPacket, UsbAltSetting, UsbDevice, UsbEndpoint, UsbInterface,
    UsbInterfaceHandler, Version,
};

/// The pad's audio quad: ch0/1 = headphone L/R (ch1 doubles as the internal mono speaker),
/// ch2/ch3 = the two haptic voice coils. Same layout `pad_sink`/`split_quad` already assume.
pub const PAD_AUDIO_CHANNELS: usize = 4;

/// Isochronous OUT endpoint (haptics + speaker), from the hardware capture.
const EP_AUDIO_OUT: u8 = 0x01;
/// Isochronous IN endpoint (headset mic).
const EP_AUDIO_IN: u8 = 0x82;
/// Interrupt IN endpoint (HID input report `0x01`).
const EP_HID_IN: u8 = 0x84;
/// Interrupt OUT endpoint (HID output report `0x02`).
const EP_HID_OUT: u8 = 0x03;

/// `bInterval` for the HID endpoints. **Deliberately 4 (1 ms at high speed), not the hardware's 6
/// (4 ms).** Nothing fingerprints the polling interval, and inheriting the hardware's 250 Hz cap
/// would add up to 4 ms of input latency the uhid pad does not have — a real regression on the one
/// axis this product is judged by.
const HID_INTERVAL: u8 = 4;
/// `bInterval` for the isochronous audio endpoints — 4 ⇒ one packet per millisecond at high speed,
/// exactly as the hardware declares.
const AUDIO_INTERVAL: u8 = 4;

/// `wMaxPacketSize` of the audio OUT endpoint: 49 frames × 4 ch × 2 bytes. One millisecond of
/// 48 kHz is 48 frames; the spare frame is the slack an *adaptive* sink needs to absorb the host's
/// clock drift, and the hardware declares the same 392.
const AUDIO_OUT_MPS: u16 = 392;
/// `wMaxPacketSize` of the audio IN endpoint: 49 frames × 2 ch × 2 bytes.
const AUDIO_IN_MPS: u16 = 196;

/// How many decoded audio chunks may queue for the streamer before we drop. The consumer wakes
/// every 5 ms and each chunk is ~1 ms, so this is generous; dropping beats blocking the URB reply,
/// which would stall the kernel's ISO ring and xrun the game's stream.
const AUDIO_QUEUE_DEPTH: usize = 256;

// ---- per-pad audio hand-off to the host's pad-audio streamer ----

/// Receivers published by live usbip pads, indexed by wire pad index.
///
/// The pad is created on the session's input thread while the pad-audio streamer runs on its own
/// thread in another crate, so the two are joined by this registry rather than by a call graph. A
/// `Receiver` is single-consumer by construction, so [`take_audio_rx`] hands it over exactly once;
/// the streamer's existing open-with-backoff loop covers the case where it looks before the pad
/// exists.
static AUDIO_RX: Mutex<Vec<Option<Receiver<Vec<f32>>>>> = Mutex::new(Vec::new());

/// Take the audio receiver for wire pad `pad`, if a usbip DualSense has published one and nobody
/// has claimed it yet. Returns interleaved `f32` chunks of [`PAD_AUDIO_CHANNELS`] channels at
/// 48 kHz — the pad's raw hardware quad.
pub fn take_audio_rx(pad: u8) -> Option<Receiver<Vec<f32>>> {
    let mut g = AUDIO_RX.lock().ok()?;
    g.get_mut(pad as usize).and_then(Option::take)
}

fn publish_audio_rx(pad: u8, rx: Receiver<Vec<f32>>) {
    if let Ok(mut g) = AUDIO_RX.lock() {
        if g.len() <= pad as usize {
            g.resize_with(pad as usize + 1, || None);
        }
        g[pad as usize] = Some(rx);
    }
}

fn clear_audio_rx(pad: u8) {
    if let Ok(mut g) = AUDIO_RX.lock() {
        if let Some(slot) = g.get_mut(pad as usize) {
            *slot = None;
        }
    }
}

// ---- descriptor helpers ----

fn ep(address: u8, attributes: u8, max_packet_size: u16, interval: u8) -> UsbEndpoint {
    UsbEndpoint {
        address,
        attributes,
        max_packet_size,
        interval,
    }
}

/// The 9-byte HID class descriptor for interface 3. `bcdHID 1.11`, country 0 — the hardware's
/// values (the Deck helper in [`super::steam_usbip`] bakes its own 1.10/33, hence a local copy).
fn hid_class_descriptor(report_len: usize) -> Vec<u8> {
    let l = report_len as u16;
    #[rustfmt::skip]
    let d = vec![
        0x09, 0x21,       // bLength, bDescriptorType (HID)
        0x11, 0x01,       // bcdHID 1.11
        0x00,             // bCountryCode
        0x01,             // bNumDescriptors
        0x22,             // bDescriptorType (Report)
        (l & 0xff) as u8, (l >> 8) as u8, // wDescriptorLength
    ];
    d
}

/// The class-specific **Audio Control** descriptor block for interface 0, verbatim from the
/// hardware capture: the 4-channel USB-streaming input terminal feeding a feature unit and the
/// Speaker output terminal, plus the Headset input terminal feeding the USB-streaming output
/// terminal. `wTotalLength` is `0x0049` (73) — the length of everything below.
#[rustfmt::skip]
fn audio_control_descriptor() -> Vec<u8> {
    vec![
        // HEADER: bcdADC 1.00, wTotalLength 73, 2 streaming interfaces (1, 2)
        0x0A, 0x24, 0x01, 0x00, 0x01, 0x49, 0x00, 0x02, 0x01, 0x02,
        // INPUT_TERMINAL 1: USB Streaming (0x0101), assoc 6, 4 ch, FL|FR|RL|RR (0x0033)
        0x0C, 0x24, 0x02, 0x01, 0x01, 0x01, 0x06, 0x04, 0x33, 0x00, 0x00, 0x00,
        // FEATURE_UNIT 2: source 1, 1-byte controls, master mute+volume then 4 silent channels
        0x0C, 0x24, 0x06, 0x02, 0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
        // OUTPUT_TERMINAL 3: Speaker (0x0301), assoc 4, source 2
        0x09, 0x24, 0x03, 0x03, 0x01, 0x03, 0x04, 0x02, 0x00,
        // INPUT_TERMINAL 4: Headset (0x0402), assoc 3, 2 ch, L|R (0x0003)
        0x0C, 0x24, 0x02, 0x04, 0x02, 0x04, 0x03, 0x02, 0x03, 0x00, 0x00, 0x00,
        // FEATURE_UNIT 5: source 4, master mute+volume then 1 silent channel
        0x09, 0x24, 0x06, 0x05, 0x04, 0x01, 0x03, 0x00, 0x00,
        // OUTPUT_TERMINAL 6: USB Streaming (0x0101), assoc 1, source 5
        0x09, 0x24, 0x03, 0x06, 0x01, 0x01, 0x01, 0x05, 0x00,
    ]
}

/// The class-specific **Audio Streaming** block for one direction: `AS_GENERAL` naming the terminal
/// it links to, then a Type-I `FORMAT_TYPE` fixing PCM S16 at 48 kHz over `channels` channels.
#[rustfmt::skip]
fn audio_streaming_descriptor(terminal_link: u8, channels: u8) -> Vec<u8> {
    vec![
        // AS_GENERAL: bTerminalLink, bDelay 1 frame, wFormatTag PCM (0x0001)
        0x07, 0x24, 0x01, terminal_link, 0x01, 0x01, 0x00,
        // FORMAT_TYPE_I: channels, 2-byte subframes, 16-bit, 1 discrete rate, 48000 (24-bit LE)
        0x0B, 0x24, 0x02, 0x01, channels, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
    ]
}

/// The 2 bytes a UAC 1.0 isochronous endpoint descriptor carries past the standard 7
/// (`bRefresh`, `bSynchAddress`), and the `AS_ENDPOINT` descriptor that follows it.
fn audio_endpoint_extras() -> (Vec<u8>, Vec<u8>) {
    let in_descriptor = vec![0x00, 0x00]; // bRefresh, bSynchAddress
    #[rustfmt::skip]
    let trailer = vec![
        // AS_ENDPOINT (CS_ENDPOINT / EP_GENERAL): no controls, no lock delay
        0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
    ];
    (in_descriptor, trailer)
}

// ---- interface handlers ----

/// Answers the standard per-interface requests every interface must survive: `SET_INTERFACE` (which
/// `snd-usb-audio` uses to arm and disarm a streaming altsetting), `GET_INTERFACE`, and
/// `GET_STATUS`. Returns `None` if the request was not one of those.
fn standard_interface_reply(setup: SetupPacket, current_alt: &mut u8) -> Option<Vec<u8>> {
    match (setup.request_type, setup.request) {
        // SET_INTERFACE — wValue is the alternate setting. ACK with no data.
        (0x01, 0x0B) => {
            *current_alt = setup.value as u8;
            Some(Vec::new())
        }
        // GET_INTERFACE — report whichever setting is armed.
        (0x81, 0x0A) => Some(vec![*current_alt]),
        // GET_STATUS — interfaces are always "0".
        (0x81, 0x00) => Some(vec![0x00, 0x00]),
        _ => None,
    }
}

/// Interface 0 — Audio Control. Serves the topology descriptor and answers the feature-unit
/// mute/volume queries `snd-usb-audio` makes while building its mixer. A refused query only costs
/// the mixer control, but answering keeps `amixer`/`wpctl` showing the pad the way hardware does.
#[derive(Debug, Default)]
struct AudioControlHandler {
    current_alt: u8,
}

impl UsbInterfaceHandler for AudioControlHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        audio_control_descriptor()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if let Some(r) = standard_interface_reply(setup, &mut self.current_alt) {
            return Ok(r);
        }
        // Class requests carry the control selector in wValue's high byte: 0x01 = MUTE (1 byte),
        // 0x02 = VOLUME (2 bytes, signed 1/256 dB).
        let selector = (setup.value >> 8) as u8;
        Ok(match (setup.request_type, setup.request, selector) {
            // GET_CUR / GET_MIN / GET_MAX / GET_RES on MUTE.
            (0xA1, 0x81..=0x84, 0x01) => vec![0x00], // never muted
            // GET_CUR on VOLUME — 0 dB.
            (0xA1, 0x81, 0x02) => 0i16.to_le_bytes().to_vec(),
            // GET_MIN — −60 dB.
            (0xA1, 0x82, 0x02) => (-60i16 * 256).to_le_bytes().to_vec(),
            // GET_MAX — 0 dB.
            (0xA1, 0x83, 0x02) => 0i16.to_le_bytes().to_vec(),
            // GET_RES — 3/16 dB, the step the hardware advertises.
            (0xA1, 0x84, 0x02) => 0x0030i16.to_le_bytes().to_vec(),
            // SET_CUR and anything else: ACK silently, exactly as the hardware tolerates.
            _ => Vec::new(),
        })
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Interface 1 — Audio Streaming OUT. **This is the capture point**: every isochronous packet the
/// kernel pushes here is one service interval of the game's haptic/speaker quad.
#[derive(Debug)]
struct SpeakerStreamHandler {
    tx: SyncSender<Vec<f32>>,
    current_alt: u8,
    /// Packets dropped because the streamer fell behind — logged once per burst rather than per
    /// packet so a stalled consumer cannot itself become the log flood.
    dropped: u64,
}

impl UsbInterfaceHandler for SpeakerStreamHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        // Alt 0 is the zero-bandwidth setting and carries no class-specific descriptor; the real
        // ones live on alt 1 (see `build_device`).
        Vec::new()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        Ok(standard_interface_reply(setup, &mut self.current_alt).unwrap_or_default())
    }

    fn handle_iso_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        packets: &[IsoPacket<'_>],
    ) -> std::io::Result<Vec<Vec<u8>>> {
        let total: usize = packets.iter().map(|p| p.data.len()).sum();
        if total >= 2 {
            let mut pcm = Vec::with_capacity(total / 2);
            for p in packets {
                // Trailing odd byte can only mean a truncated frame; `chunks_exact` drops it.
                for s in p.data.chunks_exact(2) {
                    pcm.push(i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0);
                }
            }
            if !pcm.is_empty() && self.tx.try_send(pcm).is_err() {
                self.dropped += 1;
                if self.dropped.is_power_of_two() {
                    tracing::debug!(
                        dropped = self.dropped,
                        "pad usb audio queue full — dropping (streamer behind?)"
                    );
                }
            }
        }
        // OUT packets are acknowledged with no payload; the caller fills in actual_length.
        Ok(vec![Vec::new(); packets.len()])
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Interface 2 — Audio Streaming IN (the headset mic). Declared for topology fidelity so the ALSA
/// card looks like the hardware's; it streams silence until pad-mic capture exists.
#[derive(Debug, Default)]
struct MicStreamHandler {
    current_alt: u8,
}

impl UsbInterfaceHandler for MicStreamHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        Vec::new()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        Ok(standard_interface_reply(setup, &mut self.current_alt).unwrap_or_default())
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Interface 3 — the controller itself. Mirrors [`super::dualsense::DualSensePad`]'s codec: report
/// `0x01` out of the interrupt-IN endpoint, report `0x02` in on the interrupt-OUT endpoint, and the
/// `0x05`/`0x09`/`0x20` feature reports `hid-playstation` demands during init (without them no
/// input devices ever appear).
struct HidHandler {
    report: Arc<Mutex<[u8; DS_INPUT_REPORT_LEN]>>,
    feedback: Arc<Mutex<DsFeedback>>,
    pad: u8,
    current_alt: u8,
}

// Hand-written because `DsFeedback` is not `Debug` (it carries `HidOutput`s), and the trait bound
// on `UsbInterfaceHandler` only ever wants a name for tracing.
impl std::fmt::Debug for HidHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HidHandler")
            .field("pad", &self.pad)
            .field("current_alt", &self.current_alt)
            .finish()
    }
}

impl UsbInterfaceHandler for HidHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_class_descriptor(DUALSENSE_RDESC.len())
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
            if let Some(r) = standard_interface_reply(setup, &mut self.current_alt) {
                return Ok(r);
            }
            return Ok(match (setup.request_type, setup.request) {
                // GET_DESCRIPTOR(Report) — standard, interface recipient.
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => DUALSENSE_RDESC.to_vec(),
                // HID GET_REPORT(Feature) — wValue low byte is the report id.
                (0xA1, 0x01) => {
                    let pairing = ds_pairing_reply(self.pad);
                    match setup.value as u8 {
                        0x05 => DS_FEATURE_CALIBRATION.to_vec(),
                        0x09 => pairing.to_vec(),
                        0x20 => DS_FEATURE_FIRMWARE.to_vec(),
                        _ => Vec::new(),
                    }
                }
                // HID SET_REPORT — every known DualSense writer sends feedback as an OUTPUT report
                // on the interrupt endpoint, but parse it anyway so a SET_REPORT writer is not
                // silently ignored.
                (0x21, 0x09) => {
                    self.absorb_output(req);
                    Vec::new()
                }
                // SET_IDLE / SET_PROTOCOL.
                (0x21, 0x0A) | (0x21, 0x0B) => Vec::new(),
                _ => Vec::new(),
            });
        }
        match ep.direction() {
            // Interrupt IN: hand over the current state report. The sim paces this by bInterval.
            Direction::In => Ok(self
                .report
                .lock()
                .map(|g| g.to_vec())
                .unwrap_or_else(|_| vec![0u8; DS_INPUT_REPORT_LEN])),
            // Interrupt OUT: report 0x02 — rumble / lightbar / player LEDs / adaptive triggers.
            Direction::Out => {
                self.absorb_output(req);
                Ok(Vec::new())
            }
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

impl HidHandler {
    /// Fold one HID output report into the pending feedback. Merged rather than replaced: a writer
    /// may split rumble and LED updates across reports, and `service` drains at its own cadence.
    fn absorb_output(&mut self, data: &[u8]) {
        let mut fb = DsFeedback::default();
        parse_ds_output(self.pad, data, &mut fb);
        if let Ok(mut g) = self.feedback.lock() {
            if fb.rumble.is_some() {
                g.rumble = fb.rumble;
            }
            g.hidout.extend(fb.hidout);
        }
    }
}

// ---- device assembly ----

/// Assemble the 4-interface composite DualSense. `index` is the wire pad index; it varies nothing
/// in the descriptors (the hardware has no `iSerialNumber`, and `hid-playstation` takes the per-pad
/// identity from the `0x09` pairing feature report instead).
fn build_device(
    index: u8,
    report: &Arc<Mutex<[u8; DS_INPUT_REPORT_LEN]>>,
    feedback: &Arc<Mutex<DsFeedback>>,
    audio_tx: &SyncSender<Vec<f32>>,
) -> UsbDevice {
    let (ep_extra, ep_trailer) = audio_endpoint_extras();

    let mut dev = UsbDevice::new(0); // one device per server, so the default bus_id "0-0-0" stands.
    dev.vendor_id = DS_VENDOR as u16;
    dev.product_id = DS_PRODUCT as u16;
    dev.usb_version = Version::from(0x0200u16); // bcdUSB 2.00
    dev.device_bcd = Version::from(0x0100u16); // bcdDevice 1.00
    dev.configuration_attributes = 0xC0; // self-powered, as the hardware reports
    dev.configuration_max_power = 250; // 500 mA in 2 mA units
    dev.set_manufacturer_name("Sony Interactive Entertainment");
    dev.set_product_name("DualSense Wireless Controller");

    dev
        // Interface 0 — Audio Control (no endpoints).
        .with_interface(
            0x01,
            0x01,
            0x00,
            None,
            vec![],
            boxed(AudioControlHandler::default()),
        )
        // Interface 1 — Audio Streaming OUT: alt 0 idle, alt 1 carries the isochronous endpoint.
        .with_interface(
            0x01,
            0x02,
            0x00,
            None,
            vec![],
            boxed(SpeakerStreamHandler {
                tx: audio_tx.clone(),
                current_alt: 0,
                dropped: 0,
            }),
        )
        .with_alt_settings(vec![UsbAltSetting {
            alternate_setting: 1,
            interface_class: 0x01,
            interface_subclass: 0x02,
            interface_protocol: 0x00,
            class_specific_descriptor: audio_streaming_descriptor(1, 4),
            // 0x09 = isochronous, adaptive, data — the sink must ride the host's clock.
            endpoints: vec![ep(EP_AUDIO_OUT, 0x09, AUDIO_OUT_MPS, AUDIO_INTERVAL)],
            endpoint_extra: vec![ep_extra.clone()],
            endpoint_trailers: vec![ep_trailer.clone()],
        }])
        // Interface 2 — Audio Streaming IN (headset mic).
        .with_interface(
            0x01,
            0x02,
            0x00,
            None,
            vec![],
            boxed(MicStreamHandler::default()),
        )
        .with_alt_settings(vec![UsbAltSetting {
            alternate_setting: 1,
            interface_class: 0x01,
            interface_subclass: 0x02,
            interface_protocol: 0x00,
            class_specific_descriptor: audio_streaming_descriptor(6, 2),
            // 0x05 = isochronous, asynchronous, data — a source runs on its own clock.
            endpoints: vec![ep(EP_AUDIO_IN, 0x05, AUDIO_IN_MPS, AUDIO_INTERVAL)],
            endpoint_extra: vec![ep_extra],
            endpoint_trailers: vec![ep_trailer],
        }])
        // Interface 3 — HID.
        .with_interface(
            0x03,
            0x00,
            0x00,
            None,
            vec![
                ep(EP_HID_IN, 0x03, 64, HID_INTERVAL),
                ep(EP_HID_OUT, 0x03, 64, HID_INTERVAL),
            ],
            boxed(HidHandler {
                report: report.clone(),
                feedback: feedback.clone(),
                pad: index,
                current_alt: 0,
            }),
        )
}

/// A virtual DualSense presented over USB/IP, carrying its own USB Audio Class sound card.
///
/// Dropping it detaches the `vhci_hcd` port — the pad and its ALSA card disappear together, exactly
/// as unplugging the hardware would — and withdraws the audio receiver.
pub struct DualSenseUsbip {
    report: Arc<Mutex<[u8; DS_INPUT_REPORT_LEN]>>,
    feedback: Arc<Mutex<DsFeedback>>,
    clock: SensorClock,
    pad: u8,
    seq: u8,
    _attach: UsbipAttachment,
}

impl DualSenseUsbip {
    /// Bind a virtual DualSense for wire pad `index` and attach it locally via `vhci_hcd`.
    ///
    /// Fails (so the caller can degrade to uhid) when `vhci_hcd` is absent or its sysfs `attach` is
    /// not writable — see [`super::steam_usbip::attach_device`].
    pub fn open(index: u8) -> Result<DualSenseUsbip> {
        let report = Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN]));
        let feedback = Arc::new(Mutex::new(DsFeedback::default()));
        let (tx, rx) = sync_channel::<Vec<f32>>(AUDIO_QUEUE_DEPTH);

        let attach = attach_device(
            || build_device(index, &report, &feedback, &tx),
            &format!("virtual DualSense {index}"),
        )?;

        // Publish only once the device is actually attached, so a failed attach leaves no stale
        // receiver for the streamer to drain forever.
        publish_audio_rx(index, rx);
        tracing::info!(
            index,
            "virtual DualSense created (usbip — real USB topology, so wine derives a ContainerId \
             and GE finds a real ALSA card)"
        );
        Ok(DualSenseUsbip {
            report,
            feedback,
            clock: SensorClock::dualsense(),
            pad: index,
            seq: 0,
            _attach: attach,
        })
    }

    /// Serialize `st` into report `0x01`, ready for the next interrupt-IN poll.
    pub fn write_state(&mut self, st: &DsState) {
        self.seq = self.seq.wrapping_add(1);
        let ts = self.clock.ds_ticks(Instant::now());
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.seq, ts);
        if let Ok(mut g) = self.report.lock() {
            *g = r;
        }
    }

    /// Drain the feedback the kernel/game has written to the pad since the last call.
    pub fn service(&mut self) -> DsFeedback {
        self.feedback
            .lock()
            .map(|mut f| std::mem::take(&mut *f))
            .unwrap_or_default()
    }
}

impl Drop for DualSenseUsbip {
    fn drop(&mut self) {
        clear_audio_rx(self.pad);
    }
}

/// The sysfs path of an attached virtual DualSense's `usb_device` node, plus the udev properties
/// wine turns into a Windows ContainerId.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbTopology {
    /// e.g. `/sys/devices/platform/vhci_hcd.0/usb11/11-2` — the `usb_device` both container-id
    /// derivations must land on.
    pub sysfs_path: std::path::PathBuf,
    /// `PRODUCT` (`vendor/product/bcd`), `BUSNUM` and `DEVNUM`, the fields wine packs into the GUID.
    pub busnum: String,
    pub devnum: String,
}

/// Locate the attached virtual DualSense's USB device in sysfs.
///
/// Used to *prove* the fix rather than to implement it: with a real USB device the audio sinks get
/// their `device.sysfs.path` from PipeWire's own ALSA/udev path, and wine walks the HID device's own
/// tree — neither needs us to tell it anything. This just lets the devtest print the node both
/// derivations will land on, so a mismatch is visible without launching a game.
///
/// **Assumes a single virtual DualSense.** It matches on vendor/product under a `vhci_hcd` path,
/// which cannot tell two virtual pads apart (the hardware has no `iSerialNumber`, so neither can
/// anything else); with several attached it returns the first. Good enough for the devtest, which
/// attaches exactly one.
pub fn find_usb_topology() -> Option<UsbTopology> {
    let attr = |dir: &std::path::Path, name: &str| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    };
    for entry in std::fs::read_dir("/sys/bus/usb/devices").ok()?.flatten() {
        let dir = entry.path();
        // Interfaces (`11-2:1.0`) have no idVendor; only usb_device nodes do.
        if attr(&dir, "idVendor").as_deref() != Some("054c")
            || attr(&dir, "idProduct").as_deref() != Some("0ce6")
        {
            continue;
        }
        let real = std::fs::canonicalize(&dir).unwrap_or(dir);
        if !real.to_string_lossy().contains("vhci_hcd") {
            continue; // a physically plugged pad, not ours
        }
        return Some(UsbTopology {
            busnum: attr(&real, "busnum").unwrap_or_default(),
            devnum: attr(&real, "devnum").unwrap_or_default(),
            sysfs_path: real,
        });
    }
    None
}

/// Whether a usbip DualSense should be attempted before the uhid pad.
///
/// **Opt-in for now** (`PUNKTFUNK_DUALSENSE_USBIP=1`). The uhid pad is the long-validated default
/// and this changes the pad's whole kernel presentation — including minting a real ALSA card that
/// supersedes the pad-audio sinks — so it stays behind a flag until it has been through on-glass
/// verification. Flip the default here once it has.
pub fn usbip_preferred() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_DUALSENSE_USBIP").ok().as_deref(),
        Some("1") | Some("true")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sum the descriptor bytes the device would emit for its configuration, the way
    /// `UsbDevice::handle_urb` assembles them. Kept in lockstep with that assembly by the
    /// `wTotalLength` assertion below, which is the number the hardware publishes.
    fn assembled_config_len() -> usize {
        let (ep_extra, ep_trailer) = audio_endpoint_extras();
        let iso_ep_len = 7 + ep_extra.len() + ep_trailer.len();
        9                                          // configuration descriptor
            + 9 + audio_control_descriptor().len() // interface 0 + its AC block
            + 9                                    // interface 1 alt 0
            + 9 + audio_streaming_descriptor(1, 4).len() + iso_ep_len // interface 1 alt 1
            + 9                                    // interface 2 alt 0
            + 9 + audio_streaming_descriptor(6, 2).len() + iso_ep_len // interface 2 alt 1
            + 9 + hid_class_descriptor(DUALSENSE_RDESC.len()).len() + 7 + 7 // interface 3
    }

    /// The assembled configuration descriptor must be exactly as long as the hardware's, which
    /// `lsusb -v` on a wired `054c:0ce6` reports as `wTotalLength 0x00e3`. This is the cheapest
    /// possible check that the whole descriptor set — terminal topology, both streaming interfaces
    /// with their alt settings, the 9-byte isochronous endpoints, the HID interface — is shaped
    /// like the real pad rather than merely self-consistent.
    #[test]
    fn config_descriptor_matches_hardware() {
        assert_eq!(assembled_config_len(), 0x00E3);
    }

    /// The Audio Control block's own `wTotalLength` (bytes 5..7 of the HEADER descriptor) must
    /// equal the block's real length, or `snd-usb-audio` walks off the end of the topology and
    /// creates no PCM at all.
    #[test]
    fn audio_control_total_length_is_self_consistent() {
        let d = audio_control_descriptor();
        let stated = u16::from_le_bytes([d[5], d[6]]) as usize;
        assert_eq!(stated, d.len(), "AC header wTotalLength vs actual block");
        assert_eq!(stated, 0x49, "hardware publishes 73");
    }

    /// Every descriptor in the Audio Control block must declare its own `bLength` correctly —
    /// the kernel walks the block by hopping `bLength` bytes at a time, so one wrong byte
    /// desynchronises everything after it.
    #[test]
    fn audio_control_sub_descriptors_walk_cleanly() {
        let d = audio_control_descriptor();
        let mut off = 0;
        let mut seen = 0;
        while off < d.len() {
            let len = d[off] as usize;
            assert!(len >= 3, "descriptor at {off} has absurd bLength {len}");
            assert_eq!(d[off + 1], 0x24, "CS_INTERFACE type at {off}");
            off += len;
            seen += 1;
        }
        assert_eq!(off, d.len(), "descriptor walk overran the block");
        assert_eq!(seen, 7, "header + 2 input + 2 feature + 2 output terminals");
    }

    /// The streaming block fixes the format the whole design depends on: 48 kHz, 16-bit, and
    /// **4 channels** on the OUT side. GE-Proton rejects any haptic PCM that is not exactly that,
    /// so a slip here reproduces the original bug with a real ALSA card in place.
    #[test]
    fn streaming_format_is_48k_s16_quad() {
        let d = audio_streaming_descriptor(1, 4);
        let fmt = &d[7..]; // skip AS_GENERAL
        assert_eq!(fmt[0] as usize, fmt.len(), "FORMAT_TYPE bLength");
        assert_eq!(fmt[3], 0x01, "FORMAT_TYPE_I");
        assert_eq!(fmt[4], 4, "bNrChannels");
        assert_eq!(fmt[5], 2, "bSubframeSize");
        assert_eq!(fmt[6], 16, "bBitResolution");
        let rate = u32::from_le_bytes([fmt[8], fmt[9], fmt[10], 0]);
        assert_eq!(rate, 48_000);
    }

    /// The OUT endpoint must hold a whole millisecond of the quad with a frame to spare, or an
    /// adaptive sink underruns the moment the host's clock drifts.
    #[test]
    fn audio_out_packet_holds_a_millisecond_of_quad() {
        let bytes_per_frame = PAD_AUDIO_CHANNELS * 2;
        assert_eq!(AUDIO_OUT_MPS as usize, 49 * bytes_per_frame);
        assert!(AUDIO_OUT_MPS as usize >= 48 * bytes_per_frame);
    }

    /// Isochronous OUT packets must turn into interleaved `f32` quad frames, and the URB must be
    /// acknowledged packet-for-packet whatever the consumer does.
    #[test]
    fn iso_out_decodes_s16_quad_to_f32() {
        let (tx, rx) = sync_channel::<Vec<f32>>(4);
        let mut h = SpeakerStreamHandler {
            tx,
            current_alt: 1,
            dropped: 0,
        };
        // Two frames: full-scale positive on ch0, full-scale negative on ch3.
        let mut raw = Vec::new();
        for s in [i16::MAX, 0, 0, i16::MIN, 0, 0, 0, 0] {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        let packets = [IsoPacket {
            data: &raw,
            requested_len: raw.len(),
        }];
        let intf = probe_interface();
        let replies = h
            .handle_iso_urb(&intf, ep(EP_AUDIO_OUT, 0x09, AUDIO_OUT_MPS, 4), &packets)
            .expect("iso urb");
        assert_eq!(replies.len(), 1, "one reply per packet");
        assert!(replies[0].is_empty(), "OUT packets carry no reply payload");

        let pcm = rx.try_recv().expect("decoded chunk");
        assert_eq!(pcm.len(), 8, "2 frames x 4 channels");
        assert!(
            (pcm[0] - 0.999_97).abs() < 1e-4,
            "ch0 full scale: {}",
            pcm[0]
        );
        assert_eq!(pcm[3], -1.0, "ch3 full scale negative");
        assert_eq!(pcm[4], 0.0);
    }

    /// A wedged consumer must not wedge the URB path: packets are dropped, the reply still comes.
    #[test]
    fn iso_out_drops_rather_than_blocking_when_the_streamer_stalls() {
        let (tx, _rx) = sync_channel::<Vec<f32>>(1);
        let mut h = SpeakerStreamHandler {
            tx,
            current_alt: 1,
            dropped: 0,
        };
        let raw = vec![0u8; 8 * PAD_AUDIO_CHANNELS * 2];
        let packets = [IsoPacket {
            data: &raw,
            requested_len: raw.len(),
        }];
        let intf = probe_interface();
        for _ in 0..8 {
            let replies = h
                .handle_iso_urb(&intf, ep(EP_AUDIO_OUT, 0x09, AUDIO_OUT_MPS, 4), &packets)
                .expect("iso urb");
            assert_eq!(replies.len(), 1);
        }
        assert!(h.dropped > 0, "a full queue must register drops");
    }

    /// `SET_INTERFACE` is how `snd-usb-audio` arms alt 1 to start streaming and returns to alt 0 to
    /// stop. Losing it would leave the endpoint permanently idle.
    #[test]
    fn set_interface_is_tracked_and_reported() {
        let mut alt = 0u8;
        let set = SetupPacket {
            request_type: 0x01,
            request: 0x0B,
            value: 1,
            index: 1,
            length: 0,
        };
        assert_eq!(standard_interface_reply(set, &mut alt), Some(Vec::new()));
        assert_eq!(alt, 1);
        let get = SetupPacket {
            request_type: 0x81,
            request: 0x0A,
            value: 0,
            index: 1,
            length: 1,
        };
        assert_eq!(standard_interface_reply(get, &mut alt), Some(vec![1]));
    }

    /// `hid-playstation` will not publish any input device until the `0x05`/`0x09`/`0x20` feature
    /// reports answer, and the `0x09` MAC must differ per pad or SDL/Steam merge the two pads.
    #[test]
    fn hid_feature_reports_answer_and_the_mac_is_per_pad() {
        let feature = |h: &mut HidHandler, id: u8| {
            let setup = SetupPacket {
                request_type: 0xA1,
                request: 0x01,
                value: 0x0300 | id as u16,
                index: 3,
                length: 64,
            };
            h.handle_urb(
                &probe_interface(),
                UsbEndpoint {
                    address: 0x80,
                    attributes: 0x00,
                    max_packet_size: 64,
                    interval: 0,
                },
                64,
                setup,
                &[],
            )
            .expect("feature")
        };
        let mk = |pad| HidHandler {
            report: Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN])),
            feedback: Arc::new(Mutex::new(DsFeedback::default())),
            pad,
            current_alt: 0,
        };
        let (mut a, mut b) = (mk(0), mk(1));
        assert_eq!(feature(&mut a, 0x05), DS_FEATURE_CALIBRATION.to_vec());
        assert_eq!(feature(&mut a, 0x20), DS_FEATURE_FIRMWARE.to_vec());
        let (m0, m1) = (feature(&mut a, 0x09), feature(&mut b, 0x09));
        assert_eq!(m0[0], 0x09, "reply keeps its report id");
        assert_ne!(m0[1..7], m1[1..7], "per-pad MAC must differ");
    }

    /// A rumble output report on the interrupt-OUT endpoint must reach `service`, which is the
    /// entire feedback path back to the client's pad.
    #[test]
    fn interrupt_out_report_becomes_feedback() {
        let feedback = Arc::new(Mutex::new(DsFeedback::default()));
        let mut h = HidHandler {
            report: Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN])),
            feedback: feedback.clone(),
            pad: 0,
            current_alt: 0,
        };
        // Report 0x02 with the compatible-vibration flag and both motors set.
        let mut out = vec![0u8; 48];
        out[0] = 0x02;
        out[1] = 0x01; // valid_flag0: compatible vibration
        out[3] = 0x80; // right (high-frequency) motor
        out[4] = 0x40; // left (low-frequency) motor
        h.handle_urb(
            &probe_interface(),
            ep(EP_HID_OUT, 0x03, 64, HID_INTERVAL),
            0,
            SetupPacket {
                request_type: 0,
                request: 0,
                value: 0,
                index: 0,
                length: 0,
            },
            &out,
        )
        .expect("interrupt out");
        let got = feedback.lock().unwrap().rumble;
        assert_eq!(got, Some((0x4000, 0x8000)));
    }

    /// The interrupt-IN endpoint must hand back exactly the report `write_state` last serialized,
    /// report id included.
    #[test]
    fn interrupt_in_serves_the_current_state_report() {
        let report = Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN]));
        let mut h = HidHandler {
            report: report.clone(),
            feedback: Arc::new(Mutex::new(DsFeedback::default())),
            pad: 0,
            current_alt: 0,
        };
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &DsState::neutral(), 7, 0);
        *report.lock().unwrap() = r;
        let got = h
            .handle_urb(
                &probe_interface(),
                ep(EP_HID_IN, 0x03, 64, HID_INTERVAL),
                64,
                SetupPacket {
                    request_type: 0,
                    request: 0,
                    value: 0,
                    index: 0,
                    length: 0,
                },
                &[],
            )
            .expect("interrupt in");
        assert_eq!(got.len(), DS_INPUT_REPORT_LEN);
        assert_eq!(got[0], 0x01, "report id");
        assert_eq!(got[7], 7, "sequence number");
    }

    /// The audio hand-off is take-once and per-pad, so two pads never cross streams and a second
    /// consumer cannot silently steal the first one's samples.
    #[test]
    fn audio_receiver_handoff_is_take_once_per_pad() {
        let (tx, rx) = sync_channel::<Vec<f32>>(1);
        publish_audio_rx(9, rx);
        tx.send(vec![0.25; 4]).expect("send");
        let taken = take_audio_rx(9).expect("first take wins");
        assert!(take_audio_rx(9).is_none(), "second take must find nothing");
        assert_eq!(taken.try_recv().expect("chunk"), vec![0.25; 4]);
        assert!(take_audio_rx(8).is_none(), "other pads unaffected");
        clear_audio_rx(9);
    }

    /// `UsbInterfaceHandler::handle_iso_urb` takes an interface it never reads; build a throwaway.
    fn probe_interface() -> UsbInterface {
        UsbInterface {
            interface_class: 0,
            interface_subclass: 0,
            interface_protocol: 0,
            endpoints: vec![],
            string_interface: 0,
            class_specific_descriptor: vec![],
            alt_settings: vec![],
            handler: boxed(MicStreamHandler::default()),
        }
    }
}
