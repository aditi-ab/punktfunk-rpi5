//! Per-pad DualSense audio topology (Linux): the PipeWire node graph a **physically connected**
//! DualSense presents, minted for a virtual pad — so a game that renders voice-coil haptics or
//! pad-speaker audio finds "the controller's audio device" in the shape it was written against
//! and plays into us. We own the nodes, so `process()` IS the capture: everything written to any
//! of them is mixed into one 4-ch F32 48 kHz quad (ch0/1 = speaker/headphone pair, ch2/3 = voice
//! coils) that feeds the 0xD1 lanes (`native/pad_audio.rs`).
//!
//! ## The specimen
//!
//! Measured 2026-08-15 against a real DS5 (`054c:0ce6`) wired to a SteamOS 3.7 Deck running
//! `alsa-ucm-conf` 1.2.14-2.4, whose `USB-Audio/Sony/DualSense-PS5` UCM splits the pad's single
//! 4-channel PCM into **one card object plus three playback nodes**:
//!
//! | node | `media.class` | format |
//! |---|---|---|
//! | `alsa_output.hw_Controller_0` | `Audio/Sink/Internal` (hidden parent, `api.alsa.split.parent`) | S16LE **4ch AUX0..AUX3** |
//! | `…-00.HiFi__SpeakerHaptic__sink` | `Audio/Sink` | F32LE **4ch POSITIONED FL FR RL RR** |
//! | `…-00.HiFi__Speaker__sink` | `Audio/Sink` | F32LE **1ch MONO** |
//!
//! The UCM also states what the four hardware channels ARE, which nothing else documents:
//! `Headphones` takes `Channel0 0`/`Channel1 1`, `Speaker` takes `Channel0 1`, and both haptic
//! devices take `Channel2 2`/`Channel3 3`. So **ch0 = headphone L, ch1 = headphone R AND the
//! internal mono speaker, ch2/ch3 = the two voice coils** — the layout `split_quad` and the
//! Windows endpoint stamp already assume, now confirmed against hardware rather than inferred.
//!
//! ## Why all three, and not just the AUX quad
//!
//! We used to mint ONE node: a 4-channel AUX quad wearing the mono sink's `Speaker__sink` name.
//! That satisfies GE-Proton's haptic leg (it opens `api.alsa.split.name` as `pipewire:NODE=…`
//! with `aux_channels=1` — the AUX shape is exactly right there), but it is not what a writer
//! aimed at a real pad meets. A real pad's only PUBLIC 4-channel surface is **positioned**
//! FL/FR/RL/RR, and a positioned quad landing on an AUX node is position-remixed: measured on
//! .181, `peak_speaker=0.2441` with `peak_coils=0.0000` — the coil pair is folded away and the
//! haptics are silently discarded. Minting the real split means both routes land correctly:
//! positioned writers take `SpeakerHaptic__sink`, AUX writers take the parent, and the mono
//! controller-speaker takes `Speaker__sink` at the pad's real speaker channel (ch1, not ch0).
//!
//! ## Identity
//!
//! GE-Proton matches layered — pulse proplist (`device.bus == "usb"`,
//! `device.vendor.id == 0x054c`, `device.product.id ∈ {0x0ce6, 0x0df2}`), then name substrings
//! (`Sony_Interactive_Entertainment…Wireless_Controller`, `DualSense`). pipewire-pulse fills a
//! sink's proplist from the node's own props verbatim (`fill_sink_info_proplist`), so every key
//! here reaches a wine/Proton client. The ids carry the **`0x` prefix the specimen publishes**:
//! `strtol(s, _, 16)` and `strtoul(s, _, 0)` both yield 0x054c for `"0x054c"`, while the bare
//! `"054c"` we used to publish is parse-dependent (base 0 reads it as octal `054`, stops at the
//! `c`, and yields 44 — no match at all).
//!
//! What a pure PipeWire graph still cannot satisfy is wine's ContainerId derivation (udev walk to
//! a `usb_device` parent → `GUID_NULL`; our pad is uhid and has no USB parent) and GE's raw-ALSA
//! leg (`snd_pcm_open` on an `api.alsa.path` that must be a real card, which is why we never set
//! that key). Its `pipewire:NODE=` leg we *do* satisfy — see [`split_target`].
//!
//! Deliberate deviations from the specimen, each for a reason that outranks fidelity:
//! - `node.description` stays **"Wireless Controller"**, not the specimen's "DualSense wireless
//!   controller (PS5)": wine hands `node.description` to `PKEY_Device_FriendlyName` and FF14/FF7R
//!   do a case-sensitive `wcsstr(name, L"Wireless Controller")` that a lowercase "wireless"
//!   fails. GE papers over this on real pads via `PROTON_SONY_WINDOWS_DEVICE_NAMES`; we do not
//!   need the workaround if we never present the losing string. `PUNKTFUNK_PAD_SINK_DESC` flips
//!   it for a field A/B.
//! - `priority.session` stays low (specimen: 1100 parent / 90 public). Our nodes appear and
//!   vanish with pad arrival and must never win WirePlumber's default-sink election — games reach
//!   them BY IDENTITY, nothing may auto-route here.
//! - `api.alsa.split.position` is NOT set even though the specimen carries it: that key is
//!   WirePlumber's own `split_nodes_om` trigger, and inviting WirePlumber to manage nodes it did
//!   not create is a different bug. `api.alsa.split.hw-position` (informational) we do carry.
//!
//! Every identity string has an env override for field debugging (`PUNKTFUNK_PAD_SINK_NAME` /
//! `PUNKTFUNK_PAD_SINK_DESC` with `{pad}` / `{mac}` placeholders, `PUNKTFUNK_PAD_SINK_SPLIT_NAME`,
//! `PUNKTFUNK_PAD_SINK_PARENT_CLASS`).

use anyhow::{anyhow, Context, Result};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// Message asking the PipeWire loop thread to quit (sent from `Drop`).
struct Terminate;

/// The pad's fixed channel count — the hardware quad every node is mixed down onto.
const PAD_CHANNELS: u32 = 4;

/// How many pad slots may carry a sink (`PUNKTFUNK_PAD_AUDIO_SLOTS`, default all 4 — a PipeWire
/// stream node is cheap, unlike the Windows devnode mint whose default is 1).
pub(crate) fn pad_audio_slots() -> u8 {
    std::env::var("PUNKTFUNK_PAD_AUDIO_SLOTS")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(4)
        .clamp(1, 4)
}

/// Whether a PipeWire daemon is plausibly reachable from this process — the Linux analogue of
/// "startup provisioning published at least one endpoint" for [`host_cap`]'s existence leg
/// (`native/pad_audio.rs`). A stat, not a connect: the handshake path runs per-Hello and must
/// not block. `PIPEWIRE_REMOTE` names a non-default socket — trust it (the session capturer
/// honors it via libpipewire, and a wrong value degrades to spawn-time failure, pad kept).
pub(crate) fn pipewire_reachable() -> bool {
    if std::env::var_os("PIPEWIRE_REMOTE").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| std::path::Path::new(&dir).join("pipewire-0").exists())
        .unwrap_or(false)
}

/// The pad's virtual MAC as colon-separated display hex — [`ds_pairing_reply`]'s bytes 1..7
/// are LSB-first (the report layout `hid-playstation` adopts as the HID `uniq` via `%pMR`,
/// i.e. printed reversed), so the display form reverses them. The audio nodes no longer carry it
/// (a real pad has no USB `iSerialNumber`, so neither do we — see [`PadSinkIdentity::new`]), but
/// it stays available to the `{mac}` override placeholder for field debugging.
///
/// [`ds_pairing_reply`]: pf_inject::dualsense_proto::ds_pairing_reply
fn pad_mac(pad: u8) -> String {
    let reply = crate::inject::dualsense_proto::ds_pairing_reply(pad);
    let m = &reply[1..7];
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        m[5], m[4], m[3], m[2], m[1], m[0]
    )
}

/// Expand the `{pad}` / `{mac}` placeholders of an identity template. Callers pass the MAC in
/// the form the surrounding string wants: colon display form for proplist values, bare hex for
/// the ALSA-style node name (udev serials carry no colons).
fn expand(template: &str, pad: u8, mac: &str) -> String {
    template
        .replace("{pad}", &pad.to_string())
        .replace("{mac}", mac)
}

/// The full identity the pad's nodes wear, resolved once at open.
struct PadSinkIdentity {
    /// The public **mono** speaker sink — what GE-Proton's `is_dualsense_speaker_sink` binds the
    /// controller-effect stream to, and the node the whole identity hangs off.
    speaker_name: String,
    /// The public **positioned quad** sink (UCM `SpeakerHaptic`) — where a writer aimed at a real
    /// modern pad puts haptics.
    haptic_name: String,
    /// The hidden **AUX quad** parent: our capture point and GE's `pipewire:NODE=` target.
    parent_name: String,
    description: String,
    serial: String,
    product_id: &'static str,
    product_name: &'static str,
    card_name: &'static str,
    long_card_name: String,
    components: &'static str,
    /// GE-Proton's `api.alsa.split.name` — the node name it opens as `pipewire:NODE=…` for the
    /// haptic stream. Empty disables the key. See [`split_target`].
    split_name: String,
}

/// GE-Proton reads `api.alsa.split.name` off the sink it is about to render haptics into and,
/// on its preferred leg, opens *that* node through its bundled pipewire-alsa plugin as
/// `pipewire:NODE=<name>` with `aux_channels=1` (patches 0114/0115/0116 of `proton-ds5-haptic`).
/// On the specimen the key names the hidden 4-channel parent — the public mono `Speaker__sink` is
/// only a 1-channel split of it, so rendering four channels at the public sink would lose the
/// voice-coil pair. We now mint that same parent, so the key names it too (it used to name the
/// sink itself, which was the honest answer while we had no split).
///
/// The parent's OWN copy of the key is self-referential, exactly as on the specimen
/// (`alsa_output.hw_Controller_0` carries `api.alsa.split.name = alsa_output.hw_Controller_0`).
///
/// `PUNKTFUNK_PAD_SINK_SPLIT_NAME` is the field lever: `0`/`false`/`off` drops the key (GE then
/// takes its Pulse leg, which also works for us because the parent's channel positions already
/// match its forced `AUX0..AUX3` map), any other value overrides the target verbatim.
fn split_target(parent_name: &str) -> String {
    resolve_split_target(
        parent_name,
        std::env::var("PUNKTFUNK_PAD_SINK_SPLIT_NAME").ok(),
    )
}

/// [`split_target`]'s decision, with the environment lifted out so it is testable.
fn resolve_split_target(parent_name: &str, override_var: Option<String>) -> String {
    match override_var.as_deref().map(str::trim) {
        Some("0" | "false" | "off" | "no") => String::new(),
        Some(v) if !v.is_empty() => v.to_string(),
        _ => parent_name.to_string(),
    }
}

impl PadSinkIdentity {
    fn new(pad: u8, edge: bool) -> PadSinkIdentity {
        let mac = pad_mac(pad);
        let mac_bare: String = mac.chars().filter(|c| *c != ':').collect();
        // The pad's USB `iProduct` verbatim. The specimen settles a question this file previously
        // got backwards: a plain DualSense reports **"DualSense Wireless Controller"**, model word
        // included — its ALSA card is `usb-Sony_Interactive_Entertainment_DualSense_Wireless_
        // Controller-00`, so `Sony_Interactive_Entertainment_Wireless_Controller` is NOT contiguous
        // on real hardware either, and dropping the infix to preserve that substring was chasing a
        // property no pad has. GE's own fallback is a two-substring test (`alsa_output.usb-Sony_
        // Interactive_Entertainment_` AND `Wireless_Controller`), which the real form passes.
        let (usb_product, product_id, product_name, card_name, components) = if edge {
            (
                "DualSense_Edge_Wireless_Controller",
                "0x0df2",
                "DualSense Edge Wireless Controller",
                "DualSense Edge Wireless Controller",
                "USB054c:0df2",
            )
        } else {
            (
                "DualSense_Wireless_Controller",
                "0x0ce6",
                "DualSense wireless controller (PS5)",
                "DualSense Wireless Controller",
                "USB054c:0ce6",
            )
        };
        // udev's `ID_SERIAL` is manufacturer_product[_serial]. A real DualSense has **no USB
        // iSerialNumber**, so the specimen's is just manufacturer_product and ALSA disambiguates
        // multiple cards with the trailing index instead. We do the same and put the pad index
        // there — which also drops the invented MAC infix that no real pad's name carries.
        let serial = format!("Sony_Interactive_Entertainment_{usb_product}");
        let card_index = format!("{pad:02}");
        // The `…-<NN>.HiFi__Speaker__sink` suffix is LOAD-BEARING, not decoration. GE-Proton's
        // `is_dualsense_speaker_sink()` is a pure substring test for `Speaker__sink` (plus the USB
        // ids, or the `alsa_output.usb-Sony_Interactive_Entertainment_` + `Wireless_Controller`
        // pair we also carry), and three things hang off it: `apply_windows_sony_audio_format()`
        // forces the wine endpoint to the Windows 4×48 kHz `KSAUDIO_SPEAKER_QUAD` layout DS5
        // titles probe for, the pad-SPEAKER (mono controller-effect) streams will only bind and
        // retarget to a sink it accepts, and the whole controller-audio endpoint lands on the
        // identity Spider-Man's working path used.
        //
        // ⚠ `SpeakerHaptic__sink` deliberately does NOT contain `Speaker__sink` (the `Haptic`
        // infix breaks the substring), which is exactly how the specimen keeps the two apart —
        // GE binds the mono speaker to the mono sink and nothing else.
        let speaker_name = match std::env::var("PUNKTFUNK_PAD_SINK_NAME") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac_bare),
            _ => format!("alsa_output.usb-{serial}-{card_index}.HiFi__Speaker__sink"),
        };
        let haptic_name =
            format!("alsa_output.usb-{serial}-{card_index}.HiFi__SpeakerHaptic__sink");
        // The specimen's parent is named after its ALSA PCM (`hw:Controller,0` →
        // `alsa_output.hw_Controller_0`). Ours names the pseudo-card the pad would be.
        let parent_name = format!("alsa_output.hw_punktfunkpad{pad}_0");
        // What the community WirePlumber rule renames real pads TO — minted that way directly.
        // See the module docs for why this is NOT the specimen's hwdb description.
        let description = match std::env::var("PUNKTFUNK_PAD_SINK_DESC") {
            Ok(t) if !t.trim().is_empty() => expand(&t, pad, &mac),
            _ => "Wireless Controller".to_string(),
        };
        let split_name = split_target(&parent_name);
        PadSinkIdentity {
            long_card_name: format!(
                "Sony Interactive Entertainment {card_name} at usb-punktfunk-pad{pad}, full speed"
            ),
            speaker_name,
            haptic_name,
            parent_name,
            description,
            serial,
            product_id,
            product_name,
            card_name,
            components,
            split_name,
        }
    }
}

/// Which node a buffer arrived on. The discriminant is the mixer's contribution bit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PadNode {
    /// Hidden AUX quad parent — GE's haptic leg and any Pro-Audio-shaped writer.
    Parent = 0,
    /// Public positioned quad (UCM `SpeakerHaptic`).
    Haptic = 1,
    /// Public mono speaker.
    Speaker = 2,
}

impl PadNode {
    /// Source-channel → hardware-quad-channel map for this node.
    ///
    /// The two quads are identity: the parent already speaks the hardware order, and the
    /// positioned sink's FL/FR/RL/RR line up with it one-for-one — which is precisely the UCM's
    /// `HeadphonesHaptic` mapping (`Channel0 0, Channel1 1, Channel2 2, Channel3 3`). We take
    /// that over `SpeakerHaptic`'s own `Channel0 1, Channel1 1` fold because summing FL+FR into
    /// ch1 exists only to feed a physical mono speaker; we are shipping the quad onward to a
    /// client that may have headphones in the pad's jack, and destroying the stereo pair here
    /// could not be undone there.
    ///
    /// The mono speaker maps to **ch1**, per the UCM's `Speaker` device (`Channel0 1`) — the pad's
    /// built-in speaker is hardware channel 1, not channel 0. Landing it on ch0 (which is what a
    /// mono stream into a bare AUX node does) would put controller-effect audio in the headphone
    /// LEFT channel and leave the speaker silent.
    fn channel_map(self) -> &'static [usize] {
        match self {
            PadNode::Parent | PadNode::Haptic => &[0, 1, 2, 3],
            PadNode::Speaker => &[1],
        }
    }

    /// How many channels the node's own format carries.
    fn src_channels(self) -> usize {
        match self {
            PadNode::Parent | PadNode::Haptic => 4,
            PadNode::Speaker => 1,
        }
    }
}

/// Sums what the three nodes receive into one hardware quad.
///
/// A real pad needs no mixer — its public sinks are ALSA SplitPCM views that the kernel sums into
/// one PCM. We have three independent PipeWire nodes, and GE drives two of them AT ONCE by design
/// (the haptic leg on the parent, the controller-effect leg on the mono sink), so summing is not
/// optional: emitting each node's buffers straight into the chunk channel would interleave them
/// and gap both halves ~50%.
///
/// Alignment is by contribution round, not by timestamp: a node that contributes twice before its
/// peers have contributed once closes the window. With every node on the same graph quantum that
/// is sample-exact; when quanta differ it costs sub-quantum skew, which haptics cannot resolve.
/// The cost is one quantum of buffering (~5 ms at the usual 240-frame quantum).
struct Mixer {
    /// Accumulation buffer, 4-ch interleaved.
    buf: Vec<f32>,
    /// Frames currently accumulated (the longest contribution this round).
    frames: usize,
    /// Bitmask of [`PadNode`]s that have contributed to the open window.
    contributed: u8,
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    /// Lossy-drop counter: a full channel means the 0xD1 encode thread stalled. Invisible drops
    /// cost a field investigation on the desktop plane once — count and warn, power-of-two
    /// throttled (this runs at the graph quantum).
    dropped: u64,
}

impl Mixer {
    fn new(tx: std::sync::mpsc::SyncSender<Vec<f32>>) -> Mixer {
        Mixer {
            buf: Vec::new(),
            frames: 0,
            contributed: 0,
            tx,
            dropped: 0,
        }
    }

    fn add(&mut self, node: PadNode, samples: &[f32]) {
        let src = node.src_channels();
        let bit = 1u8 << (node as u8);
        // This node is starting its next round — close the one its peers already summed into.
        if self.contributed & bit != 0 {
            self.flush();
        }
        self.contributed |= bit;
        let quad = PAD_CHANNELS as usize;
        let n = samples.len() / src;
        if self.buf.len() < n * quad {
            self.buf.resize(n * quad, 0.0);
        }
        self.frames = self.frames.max(n);
        for (f, frame) in samples.chunks_exact(src).enumerate() {
            for (s, &out) in node.channel_map().iter().enumerate() {
                self.buf[f * quad + out] += frame[s];
            }
        }
    }

    fn flush(&mut self) {
        self.contributed = 0;
        if self.frames == 0 {
            return;
        }
        let used = self.frames * PAD_CHANNELS as usize;
        let out = self.buf[..used].to_vec();
        self.buf[..used].fill(0.0);
        self.frames = 0;
        if self.tx.try_send(out).is_err() {
            self.dropped += 1;
            if self.dropped.is_power_of_two() {
                tracing::warn!(
                    dropped = self.dropped,
                    "pad-audio encode thread not keeping up — captured pad audio dropped \
                     (haptics will click)"
                );
            }
        }
    }
}

/// Per-stream callback state.
struct PadUd {
    mix: Rc<RefCell<Mixer>>,
    node: PadNode,
}

/// A live per-pad node graph + its capture. Same next-chunk contract as every
/// [`AudioCapturer`](crate::audio::AudioCapturer): empty chunk = quiet pad (keep me), `Err` =
/// dead loop thread (reopen me). Dropping tears the nodes down promptly via the Terminate
/// channel (a wedged PipeWire link head-blocks the daemon — see the session capturer's docs).
pub struct PadSinkCapturer {
    chunks: Receiver<Vec<f32>>,
    quit: pipewire::channel::Sender<Terminate>,
    /// The public mono speaker sink — the identity a title has to match. Kept as `node_name` for
    /// the devtest and logs.
    pub node_name: String,
    /// The public positioned-quad sink (UCM `SpeakerHaptic`).
    pub haptic_name: String,
    /// The hidden AUX parent, and what GE-Proton reads as `api.alsa.split.name`; empty when the
    /// key is suppressed.
    pub split_name: String,
}

impl PadSinkCapturer {
    /// Mint the node graph for wire pad `pad` (`edge` = DualSense Edge identity) and start
    /// capturing. Fails if PipeWire is unreachable — the caller's reopen-with-backoff owns the
    /// retry.
    pub fn open(pad: u8, edge: bool) -> Result<PadSinkCapturer> {
        let identity = PadSinkIdentity::new(pad, edge);
        let node_name = identity.speaker_name.clone();
        let haptic_name = identity.haptic_name.clone();
        let parent_name = identity.parent_name.clone();
        let split_name = identity.split_name.clone();
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        // Bring-up handshake (the session capturer's discipline): a PipeWire that isn't running
        // must surface as an open ERROR, engaging the caller's backoff — not a zombie thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        thread::Builder::new()
            .name(format!("punktfunk-pw-pad{pad}"))
            .spawn(move || {
                if let Err(e) = pad_sink_thread(tx, quit_rx, identity, ready_tx) {
                    tracing::warn!(pad, error = %format!("{e:#}"), "pipewire pad-sink thread failed");
                }
            })
            .context("spawn pipewire pad-sink thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("pipewire pad-sink init timed out")),
        }
        // The identity a title has to match, in the log a field report will carry. Cheap once per
        // pad, and it is the only place the negotiated strings are visible without a live `pactl`.
        let split_log = if split_name.is_empty() {
            "(suppressed)"
        } else {
            split_name.as_str()
        };
        tracing::info!(
            pad,
            edge,
            speaker_sink = %node_name,
            haptic_sink = %haptic_name,
            parent = %parent_name,
            split_name = %split_log,
            "pad-audio nodes minted (real-pad split: mono Speaker__sink + positioned \
             SpeakerHaptic__sink + hidden AUX parent; ch0/1 speaker, ch2/3 coils)"
        );
        Ok(PadSinkCapturer {
            chunks: rx,
            quit: quit_tx,
            node_name,
            haptic_name,
            split_name,
        })
    }
}

impl Drop for PadSinkCapturer {
    fn drop(&mut self) {
        // A failed send means the loop thread already exited — nothing to tear down.
        let _ = self.quit.send(Terminate);
    }
}

impl crate::audio::AudioCapturer for PadSinkCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            // A quiet pad (no game rendering pad audio — the common case) is NOT a failure; the
            // per-pad streamer keeps us and its silence gate stays closed.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pipewire pad-sink thread ended")),
        }
    }

    fn channels(&self) -> u32 {
        PAD_CHANNELS
    }
}

/// SPA channel positions for the parent's AUX quad (`enum spa_audio_channel`:
/// `SPA_AUDIO_CHANNEL_START_Aux` = 0x1000), NOT a positioned layout. This is the shape the
/// specimen's hidden parent exposes and the one GE-Proton's haptics were built against: its
/// `open_dualsense_haptic_pcm` targets the node through the bundled pipewire-alsa plugin with
/// `aux_channels=1` — "the hidden PipeWire parent for a DualSense output exposes AUX0 through
/// AUX3" (proton-ds5-haptic patch 0115) — and its pulse fallback forces a
/// `PA_CHANNEL_POSITION_AUX0..3` map. Aux positions carry no spatial meaning, so nothing in the
/// graph position-remixes into or out of the parent: writers land by INDEX, exactly the raw quad
/// the pad speaks.
fn aux_positions() -> [u32; 64] {
    const AUX0: u32 = 0x1000;
    let mut pos = [0u32; 64];
    pos[..4].copy_from_slice(&[AUX0, AUX0 + 1, AUX0 + 2, AUX0 + 3]);
    pos
}

/// SPA positions for the public quad: FL, FR, RL, RR — the specimen's `SpeakerHaptic__sink`
/// layout, and the only 4-channel surface a real pad publishes. A writer aimed at real hardware
/// sends exactly this, so accepting it unfolded is the whole point of minting the node.
fn positioned_quad() -> [u32; 64] {
    let mut pos = [0u32; 64];
    // spa_audio_channel: FL = 3, FR = 4, RL = 12, RR = 13.
    pos[..4].copy_from_slice(&[3, 4, 12, 13]);
    pos
}

/// SPA positions for the public mono speaker sink (`SPA_AUDIO_CHANNEL_MONO` = 2).
fn mono_position() -> [u32; 64] {
    let mut pos = [0u32; 64];
    pos[0] = 2;
    pos
}

/// Serialize an `EnumFormat` pod for one node's F32LE 48 kHz layout.
fn format_pod(channels: u32, positions: [u32; 64]) -> Result<Vec<u8>> {
    use pipewire as pw;
    use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(crate::audio::SAMPLE_RATE);
    info.set_channels(channels);
    info.set_position(positions);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    Ok(pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize pad-sink format pod")?
    .0
    .into_inner())
}

/// The shared `process()` body, as a closure — a macro rather than a function so the PipeWire
/// stream type never has to be named (it is only reachable through the builder's inference).
macro_rules! pad_process {
    () => {
        |stream, ud: &mut PadUd| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let d = &mut datas[0];
                let (offset, size) = {
                    let c = d.chunk();
                    (c.offset() as usize, c.size() as usize)
                };
                let Some(buf) = d.data() else { return };
                if offset > buf.len() {
                    return;
                }
                let region = &buf[offset..(offset + size).min(buf.len())];
                // Negotiated as F32LE; reinterpret the byte region as interleaved f32.
                let n = region.len() / 4;
                let mut samples = Vec::with_capacity(n);
                for i in 0..n {
                    samples.push(f32::from_le_bytes([
                        region[i * 4],
                        region[i * 4 + 1],
                        region[i * 4 + 2],
                        region[i * 4 + 3],
                    ]));
                }
                ud.mix.borrow_mut().add(ud.node, &samples);
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire pad-sink callback — chunk dropped");
            }
        }
    };
}

/// The `!Send` MainLoop/Stream thread: mint the three nodes, hand mixed capture chunks over, run
/// until Terminate / daemon death.
fn pad_sink_thread(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    identity: PadSinkIdentity,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::AudioInfoRaw;
    use spa::pod::Pod;

    let result = (|| -> Result<()> {
        pf_capture::pwinit::ensure_init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw pad-sink MainLoop")?;
        let context =
            pw::context::ContextRc::new(&mainloop, None).context("pw pad-sink Context")?;
        let core = context
            .connect_rc(None)
            .context("pw pad-sink connect (is PipeWire running in this session?)")?;

        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });

        // Daemon death ends this thread → the chunk channel disconnects → `next_chunk` errors →
        // the per-pad streamer reopens with backoff (the session capturer's zombie-thread fix).
        let _core_listener = core
            .add_listener_local()
            .error({
                let mainloop = mainloop.clone();
                move |id, _seq, res, message| {
                    tracing::warn!(id, res, message, "pipewire core error — pad sink ends");
                    mainloop.quit();
                }
            })
            .register();

        let mix = Rc::new(RefCell::new(Mixer::new(tx)));

        // Every node wears the same card identity — on the specimen they are three views of ONE
        // USB card, and every matcher (GE's proplist leg included) reads these off whichever node
        // it happens to be holding.
        let base_props = |class: &str, name: &str, channels: &str, position: &str| {
            let mut p = properties! {
                *pw::keys::MEDIA_TYPE   => "Audio",
                // One Opus-haptics frame (~5 ms) per quantum, like the session sink — haptics are
                // felt latency; bursty delivery would ride through to the client's jitter buffer.
                *pw::keys::NODE_LATENCY => "240/48000",
                // Must NEVER win WirePlumber's default election against real hardware — games
                // reach these nodes BY IDENTITY, nothing auto-routes here.
                "priority.session"      => "50",
                "device.bus"            => "usb",
                "device.vendor.id"      => "0x054c",
                "device.vendor.name"    => "Sony Corp.",
                "device.form_factor"    => "gamepad",
                "device.icon_name"      => "audio-card-analog",
                "api.alsa.pcm.stream"   => "playback",
                "api.alsa.open.ucm"     => "true",
                "alsa.driver_name"      => "snd_usb_audio",
                "node.virtual"          => "false",
                // Informational on the specimen — the parent's raw layout, carried by every node.
                // NOT `api.alsa.split.position`, which is WirePlumber's own management trigger.
                "api.alsa.split.hw-position" => "[AUX0,AUX1,AUX2,AUX3]",
            };
            p.insert(*pw::keys::MEDIA_CLASS, class);
            p.insert(*pw::keys::NODE_NAME, name);
            p.insert(*pw::keys::NODE_DESCRIPTION, identity.description.as_str());
            p.insert("audio.channels", channels);
            p.insert("audio.position", position);
            p.insert("device.serial", identity.serial.as_str());
            p.insert("device.product.id", identity.product_id);
            p.insert("device.product.name", identity.product_name);
            p.insert("device.description", identity.product_name);
            p.insert("alsa.card_name", identity.card_name);
            p.insert("alsa.long_card_name", identity.long_card_name.as_str());
            p.insert("api.alsa.card.name", identity.card_name);
            p.insert("api.alsa.card.longname", identity.long_card_name.as_str());
            p.insert("alsa.components", identity.components);
            p.insert("alsa.id", "Controller");
            // GE-Proton's preferred haptic leg; see `split_target`. Omitted (not empty) when the
            // field lever turns it off, so `pa_proplist_gets` misses rather than returning "".
            if !identity.split_name.is_empty() {
                p.insert("api.alsa.split.name", identity.split_name.as_str());
            }
            p
        };

        // ---- the hidden AUX parent: our capture point and GE's `pipewire:NODE=` target --------
        // `Audio/Sink/Internal` is the specimen's class — hidden from pactl/pulse sink lists while
        // still openable by name. `PUNKTFUNK_PAD_SINK_PARENT_CLASS` flips it to a plain
        // `Audio/Sink` if a session manager ever refuses to let an Internal node take clients.
        let parent_class = std::env::var("PUNKTFUNK_PAD_SINK_PARENT_CLASS")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Audio/Sink/Internal".to_string());
        let mut parent_props = base_props(
            &parent_class,
            &identity.parent_name,
            "4",
            "AUX0,AUX1,AUX2,AUX3",
        );
        parent_props.insert("api.alsa.split.parent", "true");
        parent_props.insert(
            *pw::keys::NODE_NICK,
            "Internal Mono Speaker + Haptic Feedback",
        );
        let parent = pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", parent_props)
            .context("pw pad-sink parent Stream")?;
        let _parent_listener = parent
            .add_local_listener_with_user_data(PadUd {
                mix: mix.clone(),
                node: PadNode::Parent,
            })
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, _ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire pad-sink parent state");
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(move |_stream, _ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    // We own the node, so this IS the format games render into.
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        "pad-sink parent format negotiated"
                    );
                }
            })
            .process(pad_process!())
            .register()
            .context("register pad-sink parent listener")?;

        // ---- the public positioned quad (UCM `SpeakerHaptic`) --------------------------------
        let mut haptic_props = base_props("Audio/Sink", &identity.haptic_name, "4", "FL,FR,RL,RR");
        haptic_props.insert("device.profile.name", "HiFi: SpeakerHaptic: sink");
        haptic_props.insert(
            "device.profile.description",
            "Internal Mono Speaker + Haptic Feedback",
        );
        haptic_props.insert(
            *pw::keys::NODE_NICK,
            "Internal Mono Speaker + Haptic Feedback",
        );
        let haptic = pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", haptic_props)
            .context("pw pad-sink haptic Stream")?;
        let _haptic_listener = haptic
            .add_local_listener_with_user_data(PadUd {
                mix: mix.clone(),
                node: PadNode::Haptic,
            })
            .process(pad_process!())
            .register()
            .context("register pad-sink haptic listener")?;

        // ---- the public mono speaker ---------------------------------------------------------
        let mut speaker_props = base_props("Audio/Sink", &identity.speaker_name, "1", "MONO");
        speaker_props.insert("device.profile.name", "HiFi: Speaker: sink");
        speaker_props.insert("device.profile.description", "Internal Mono Speaker");
        speaker_props.insert(*pw::keys::NODE_NICK, "Internal Mono Speaker");
        let speaker = pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", speaker_props)
            .context("pw pad-sink speaker Stream")?;
        let _speaker_listener = speaker
            .add_local_listener_with_user_data(PadUd {
                mix: mix.clone(),
                node: PadNode::Speaker,
            })
            .process(pad_process!())
            .register()
            .context("register pad-sink speaker listener")?;

        // RT_PROCESS for the same reason as every host-owned stream node here: a sink must be a
        // synchronous graph member that joins its producers' driver group, or `process()` never
        // fires on a busy graph (see the mic's connect comment in mod.rs).
        let flags = pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS;

        let parent_fmt = format_pod(PAD_CHANNELS, aux_positions())?;
        let mut parent_params = [Pod::from_bytes(&parent_fmt).context("parent pod from bytes")?];
        parent
            .connect(
                spa::utils::Direction::Input,
                None,
                flags,
                &mut parent_params,
            )
            .context("pw pad-sink parent connect")?;

        let haptic_fmt = format_pod(PAD_CHANNELS, positioned_quad())?;
        let mut haptic_params = [Pod::from_bytes(&haptic_fmt).context("haptic pod from bytes")?];
        haptic
            .connect(
                spa::utils::Direction::Input,
                None,
                flags,
                &mut haptic_params,
            )
            .context("pw pad-sink haptic connect")?;

        let speaker_fmt = format_pod(1, mono_position())?;
        let mut speaker_params = [Pod::from_bytes(&speaker_fmt).context("speaker pod from bytes")?];
        speaker
            .connect(
                spa::utils::Direction::Input,
                None,
                flags,
                &mut speaker_params,
            )
            .context("pw pad-sink speaker connect")?;

        let _ = ready.send(Ok(()));
        mainloop.run();
        tracing::debug!("pipewire pad-sink loop exited (capturer dropped)");
        Ok(())
    })();
    if let Err(e) = &result {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_mac_is_reversed_display_form_and_per_pad_unique() {
        // DS_FEATURE_PAIRING bytes 1..7 are 74 E7 D6 3A 53 35 LSB-first → display reverses.
        assert_eq!(pad_mac(0), "35:53:3A:D6:E7:74");
        // The pad index offsets the LOW octet — the LAST display octet.
        assert_eq!(pad_mac(1), "35:53:3A:D6:E7:75");
        assert_ne!(pad_mac(2), pad_mac(3));
    }

    #[test]
    fn identity_carries_every_match_surface() {
        let id = PadSinkIdentity::new(0, false);
        // GE-Proton's `string_contains_dualsense_name` legs, each checked separately.
        assert!(id.speaker_name.contains("Sony_Interactive_Entertainment"));
        assert!(id.speaker_name.contains("Wireless_Controller"));
        assert!(id.speaker_name.contains("DualSense"));
        // The specimen's exact prefix: a real DS5's ALSA card is
        // `usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00`.
        assert!(id.speaker_name.starts_with(
            "alsa_output.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00."
        ));
        // The suffix GE's `is_dualsense_speaker_sink` substring-tests for — the pad-speaker
        // binding and the Windows 4ch format forcing both hang off it.
        assert!(id.speaker_name.ends_with("-00.HiFi__Speaker__sink"));
        assert!(id.speaker_name.contains("Speaker__sink"));
        // …and the positioned sibling must NOT satisfy that test, or GE would bind the mono
        // controller-effect stream to the quad. The `Haptic` infix is what keeps them apart.
        assert!(!id.haptic_name.contains("Speaker__sink"));
        assert!(id.haptic_name.ends_with("-00.HiFi__SpeakerHaptic__sink"));
        // No colons in a udev-style serial/name, and no invented MAC infix (a real pad has no
        // USB iSerialNumber, so the trailing card index disambiguates instead).
        assert!(!id.speaker_name.contains(':'));
        assert!(!id.speaker_name.contains("35533AD6E774"));
        // Case-sensitive `wcsstr(FriendlyName, L"Wireless Controller")` (FF14, FF7R).
        assert_eq!(id.description, "Wireless Controller");
        // The ids carry the `0x` prefix the specimen publishes — parseable under base 16 AND
        // base 0, unlike the bare form.
        assert_eq!(id.product_id, "0x0ce6");
        assert_eq!(id.components, "USB054c:0ce6");
        assert_eq!(id.card_name, "DualSense Wireless Controller");
        assert!(id.long_card_name.contains("Sony Interactive Entertainment"));

        let edge = PadSinkIdentity::new(1, true);
        // GE tests the Edge with the full `DualSense_Edge_Wireless_Controller` substring.
        assert!(edge
            .speaker_name
            .contains("DualSense_Edge_Wireless_Controller"));
        assert!(edge.speaker_name.contains("Speaker__sink"));
        assert_eq!(edge.product_id, "0x0df2");
        assert_eq!(edge.components, "USB054c:0df2");
        // Distinct pads mint distinct names (the ALSA-style card index, as on real hardware).
        assert_ne!(id.speaker_name, PadSinkIdentity::new(1, false).speaker_name);
        assert!(PadSinkIdentity::new(1, false)
            .speaker_name
            .ends_with("-01.HiFi__Speaker__sink"));
        // Each pad's parent is distinct too — it is the capture point.
        assert_ne!(id.parent_name, PadSinkIdentity::new(1, false).parent_name);
    }

    #[test]
    fn split_target_points_at_the_parent_unless_overridden() {
        // The specimen's public sinks name the hidden parent, and the parent names itself.
        let id = PadSinkIdentity::new(0, false);
        assert_eq!(id.split_name, id.parent_name);
        assert_ne!(id.split_name, id.speaker_name);
        // The field lever, both ways — through the pure form, so no test mutates the process
        // environment out from under a parallel test runner.
        assert_eq!(resolve_split_target("n", None), "n");
        assert_eq!(resolve_split_target("n", Some(" ".into())), "n");
        assert!(resolve_split_target("n", Some("0".into())).is_empty());
        assert!(resolve_split_target("n", Some("off".into())).is_empty());
        assert_eq!(resolve_split_target("n", Some(" other ".into())), "other");
    }

    #[test]
    fn template_expansion() {
        assert_eq!(expand("pad{pad}-{mac}", 2, "AABB"), "pad2-AABB");
        assert_eq!(expand("static", 0, "x"), "static");
    }

    /// The UCM's channel arithmetic, which is the whole reason the mono sink exists separately:
    /// the pad's built-in speaker is hardware channel **1**, not 0.
    #[test]
    fn channel_maps_follow_the_ucm() {
        assert_eq!(PadNode::Parent.channel_map(), &[0, 1, 2, 3]);
        assert_eq!(PadNode::Haptic.channel_map(), &[0, 1, 2, 3]);
        assert_eq!(PadNode::Speaker.channel_map(), &[1]);
        assert_eq!(PadNode::Speaker.src_channels(), 1);
        assert_eq!(PadNode::Haptic.src_channels(), 4);
    }

    /// One node alone: a round closes when that node contributes again, so the quad comes out
    /// exactly as written (index-exact — the property the .41 on-glass run proved for the parent).
    #[test]
    fn mixer_passes_a_lone_quad_through_unchanged() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        m.add(PadNode::Parent, &[0.1, 0.2, 0.3, 0.4]);
        // Nothing emitted yet — the window is open for peers to sum into.
        assert!(rx.try_recv().is_err());
        m.add(PadNode::Parent, &[0.5, 0.6, 0.7, 0.8]);
        assert_eq!(rx.try_recv().unwrap(), vec![0.1, 0.2, 0.3, 0.4]);
    }

    /// The case a real pad gets for free and we must do by hand: GE drives the haptic leg and the
    /// controller-effect leg AT ONCE, and both must survive in one quad.
    #[test]
    fn mixer_sums_concurrent_nodes_into_one_quad() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        // Haptics on the positioned sink: coils only.
        m.add(PadNode::Haptic, &[0.0, 0.0, 0.5, 0.25]);
        // Controller-effect audio on the mono sink, same round.
        m.add(PadNode::Speaker, &[0.75]);
        m.flush();
        // ch1 carries the mono speaker (UCM `Channel0 1`), ch2/3 the coils, ch0 untouched.
        assert_eq!(rx.try_recv().unwrap(), vec![0.0, 0.75, 0.5, 0.25]);
    }

    /// Ragged buffer lengths must not truncate the longer contributor or read out of bounds.
    #[test]
    fn mixer_handles_unequal_frame_counts() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        m.add(PadNode::Parent, &[1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0]);
        m.add(PadNode::Speaker, &[0.5]);
        m.flush();
        let out = rx.try_recv().unwrap();
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 1.0);
        assert_eq!(out[1], 0.5); // mono summed into frame 0's ch1
        assert_eq!(out[4], 2.0); // frame 1 survived
        assert_eq!(out[5], 0.0);
    }

    /// A flush must leave no residue behind for the next round to inherit.
    #[test]
    fn mixer_clears_between_rounds() {
        let (tx, rx) = sync_channel::<Vec<f32>>(8);
        let mut m = Mixer::new(tx);
        m.add(PadNode::Parent, &[1.0, 1.0, 1.0, 1.0]);
        m.flush();
        assert_eq!(rx.try_recv().unwrap(), vec![1.0, 1.0, 1.0, 1.0]);
        m.add(PadNode::Parent, &[0.25, 0.0, 0.0, 0.0]);
        m.flush();
        assert_eq!(rx.try_recv().unwrap(), vec![0.25, 0.0, 0.0, 0.0]);
        // An empty flush emits nothing at all (a quiet pad must not manufacture chunks).
        m.flush();
        assert!(rx.try_recv().is_err());
    }
}
