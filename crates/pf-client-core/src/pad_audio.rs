//! Pad audio (the 0xD1 plane): render the host's per-gamepad DualSense streams — voice-coil
//! haptics (kind 0, the BACK channel pair) and the built-in speaker (kind 1, the FRONT pair) —
//! into a USB-connected physical DualSense's own 4-channel audio device.
//!
//! Tier A only (v1): a WIRED DualSense / DualSense Edge — Bluetooth exposes no audio device, so
//! wired is what makes the 4-ch sibling exist at all. The gamepad worker detects tier A at slot
//! open ([`crate::gamepad`]) and declares the pad's render capabilities to the core
//! ([`punktfunk_core::client::NativeClient::set_pad_audio_caps`]), which rides them on the
//! arrival (flags bits 8/9) toward a `HOST_CAP_PAD_AUDIO` host; the host then emits 0xD1 for
//! exactly those pads. This module owns everything after that:
//!
//! - **Correlation** pad ↔ audio device. Windows: the SDL HID interface path → the devnode's
//!   `ContainerID` (registry) → the active eRender endpoint whose stamped
//!   `PKEY_Device_ContainerId` matches AND whose device format has 4 channels. Linux: the
//!   PipeWire sink whose name/description carries the DualSense signature (one physical DS5 in
//!   v1 — first match wins).
//! - **The renderer worker** ([`spawn`]): drains [`NativeClient::next_pad_audio`] (the plane's
//!   single consumer), Opus-decodes per (pad, kind) with seq-gap PLC (the session audio path's
//!   [`AudioGapTracker`] discipline), interleaves both pairs into one 4-ch stream
//!   (speaker → channels 0/1, haptics → 2/3 — the DS5 device's own layout), and plays it on the
//!   correlated device: WASAPI shared/event-driven on Windows, a PipeWire playback stream with
//!   `target.object` on Linux — both with the session players' 3-quantum prime/cap/re-prime
//!   ring policy at a SMALLER floor (haptics are felt latency). The output device is opened
//!   lazily on the first arriving frame and re-correlated with backoff when it goes away.
//!
//! The `Settings` side: `pad_haptics` (bool) and `pad_speaker` (`"pad"`/`"mix"`/`"off"`) gate
//! the `CLIENT_CAP_PAD_AUDIO` advertisement and the per-pad capability bits; `"mix"` (fold the
//! speaker into the main stream audio) is a declared TODO and renders as `"off"`.

use punktfunk_core::audio::AudioGapTracker;
use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The render layout: 4 interleaved f32 channels — speaker FL/FR on 0/1, haptics (rear pair
/// on the wire layout, the voice coils on the device) on 2/3.
const PAD_CHANNELS: usize = 4;

/// Mixer depth bound (frames @48 kHz — 100 ms). Only guards a wedged/absent output; the live
/// latency bound is the platform ring policy's cap.
const MAX_BUFFER_FRAMES: usize = 4800;

/// Device (re)correlation backoff bounds: a missing DualSense audio device is polled at
/// [`RETRY_MIN`] doubling to [`RETRY_MAX`] — correlation enumerates the audio graph, so it must
/// not run per frame.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(8);

// ---- settings vocabulary --------------------------------------------------------------------

/// Whether the `pad_speaker` setting asks for a renderer: `"pad"` = the physical pad's speaker
/// (the only implemented target). `"mix"` — fold the speaker stream into the main session
/// audio — is a declared TODO: it logs once and renders as `"off"` so the setting name can ship
/// before the mixer leg does. Anything else (including `"off"`) = no renderer.
pub fn speaker_active(mode: &str) -> bool {
    match mode {
        "pad" => true,
        "mix" => {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::info!(
                    "pad_speaker=\"mix\" is not implemented yet (TODO: fold the DS5 speaker \
                     stream into the main session audio) — treating it as \"off\""
                );
            });
            false
        }
        _ => false,
    }
}

// ---- tier-A detection -----------------------------------------------------------------------

/// Tier A = a physical DualSense (`054C:0CE6`) or DualSense Edge (`054C:0DF2`) on a WIRED
/// connection — only USB exposes the pad's 4-ch audio device (Bluetooth pads are tier B/C,
/// out of scope here). Pure so the policy is testable; the wired signal comes from
/// `SDL_GetGamepadConnectionState`, falling back to the audio-sibling probe
/// ([`wired_audio_sibling`]) when SDL answers Unknown.
pub(crate) fn is_tier_a_ds5(vid: u16, pid: u16, wired: bool) -> bool {
    vid == 0x054C && matches!(pid, 0x0CE6 | 0x0DF2) && wired
}

/// The wired fallback when SDL cannot say (`ConnectionState::Unknown`): does the pad's 4-ch
/// audio sibling exist? A Bluetooth DS5 exposes no audio device, so a resolvable device IS the
/// wired signal. Linux ignores the HID path (the sink match is signature-based); Windows
/// resolves the path's container against the render endpoints.
#[cfg(target_os = "linux")]
pub(crate) fn wired_audio_sibling(_hid_path: Option<&str>) -> bool {
    crate::audio::devices()
        .map(|(sinks, _)| sinks.iter().any(|d| is_ds5_sink(&d.name, &d.description)))
        .unwrap_or(false)
}

#[cfg(windows)]
pub(crate) fn wired_audio_sibling(hid_path: Option<&str>) -> bool {
    hid_path.is_some_and(|p| correlate_pad_endpoint(p).is_ok())
}

// ---- tier-A pad registry (gamepad worker → renderer worker) ---------------------------------

/// One tier-A pad the gamepad worker holds open: its wire index and (Windows correlation) the
/// SDL HID device path. Registered at slot open, dropped at slot close.
struct TierAPad {
    index: u8,
    /// Read by the Windows correlation only — Linux matches the sink by signature.
    #[cfg_attr(not(windows), allow(dead_code))]
    hid_path: Option<String>,
}

/// The tier-A pads currently open, shared between the gamepad worker (writer, at slot
/// open/close) and the session renderer worker (reader, at device correlation). A process-wide
/// static because the two workers meet nowhere else: the gamepad service is app-lifetime, the
/// renderer is per-session.
static TIER_A_PADS: Mutex<Vec<TierAPad>> = Mutex::new(Vec::new());

/// Gamepad worker: a tier-A slot opened on wire index `index` (idempotent per index).
pub(crate) fn register_tier_a(index: u8, hid_path: Option<String>) {
    let mut pads = TIER_A_PADS.lock().unwrap();
    pads.retain(|p| p.index != index);
    pads.push(TierAPad { index, hid_path });
}

/// Gamepad worker: the tier-A slot on `index` closed (no-op for non-tier-A indices).
pub(crate) fn unregister_tier_a(index: u8) {
    TIER_A_PADS.lock().unwrap().retain(|p| p.index != index);
}

/// The first registered tier-A pad's HID path (v1 renders one physical DS5).
#[cfg(windows)]
fn first_tier_a_hid_path() -> Option<String> {
    TIER_A_PADS
        .lock()
        .unwrap()
        .first()
        .and_then(|p| p.hid_path.clone())
}

// ---- correlation: Linux (PipeWire sink signature) -------------------------------------------

/// Does this PipeWire sink look like a wired DualSense's audio device? The ALSA node name
/// carries the USB vendor string (`Sony_Interactive_Entertainment`), descriptions carry the
/// product (`DualSense`), and the kernel's fallback device name is `Wireless Controller` —
/// any of the three identifies the pad's sink.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_ds5_sink(name: &str, description: &str) -> bool {
    let hit = |s: &str| {
        s.contains("Sony_Interactive_Entertainment")
            || s.contains("DualSense")
            || s.starts_with("Wireless Controller")
    };
    hit(name) || hit(description)
}

/// First-match pick over an enumerated sink list (v1 supports ONE physical DS5; more than one
/// match logs once and keeps the first).
#[cfg(target_os = "linux")]
fn find_ds5_sink(sinks: &[crate::audio::AudioDevice]) -> Option<crate::audio::AudioDevice> {
    let mut matches = sinks
        .iter()
        .filter(|d| is_ds5_sink(&d.name, &d.description));
    let first = matches.next()?.clone();
    if matches.next().is_some() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                sink = %first.name,
                "multiple DualSense audio sinks — v1 renders one physical DS5, using the first"
            );
        });
    }
    Some(first)
}

// ---- correlation: Windows (HID container → render endpoint) ---------------------------------

/// One enumerated render endpoint reduced to what the matcher needs. Pure data so the container
/// match is unit-testable off-box.
#[cfg(any(windows, test))]
pub(crate) struct EndpointCandidate {
    /// The `IMMDevice` endpoint id (`{0.0.0.00000000}.{…}`) — what WASAPI's device targeting
    /// takes.
    pub(crate) id: String,
    /// The endpoint's `PKEY_Device_ContainerId` as a braced lowercase GUID string; `None` when
    /// unreadable.
    pub(crate) container: Option<String>,
    /// Channel count of the endpoint's device format.
    pub(crate) channels: u16,
}

/// The endpoint belonging to the pad: container match AND a 4-channel device format (the DS5
/// audio function is the only 4-ch endpoint in its container — the mix-format gate keeps a
/// hypothetical sibling stereo endpoint from winning).
#[cfg(any(windows, test))]
pub(crate) fn pick_pad_endpoint<'a>(
    endpoints: &'a [EndpointCandidate],
    container: &str,
) -> Option<&'a EndpointCandidate> {
    endpoints.iter().find(|e| {
        e.channels == 4
            && e.container
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(container))
    })
}

/// A device INSTANCE id from a device INTERFACE path: strip the `\\?\` (or `\\.\`) prefix,
/// the `#` separators become `\`, and the trailing `{interface-class-guid}` segment drops —
/// `\\?\HID#VID_054C&PID_0CE6#8&2de&0&0000#{4d1e55b2-…}` → `HID\VID_054C&PID_0CE6\8&2de&0&0000`.
/// That instance id is the devnode's key under `HKLM\SYSTEM\CurrentControlSet\Enum`, where its
/// `ContainerID` lives.
#[cfg(any(windows, test))]
pub(crate) fn hid_instance_from_interface_path(path: &str) -> Option<String> {
    let p = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))
        .unwrap_or(path);
    let mut segs: Vec<&str> = p.split('#').collect();
    if let Some(last) = segs.last() {
        if last.starts_with('{') && last.ends_with('}') {
            segs.pop();
        }
    }
    if segs.len() != 3 || segs.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(segs.join("\\"))
}

/// Parse a serialized `VT_CLSID` PROPVARIANT registry blob (the on-disk shape of the MMDevices
/// property store: 8-byte header `[vt, 0, 0, 0, 1, 0, 0, 0]`, then the GUID in registry byte
/// order) into a braced lowercase GUID string. `None` for anything else.
#[cfg(any(windows, test))]
pub(crate) fn container_guid_from_blob(bytes: &[u8]) -> Option<String> {
    const VT_CLSID: u8 = 0x48;
    if bytes.len() < 24 || bytes[0] != VT_CLSID {
        return None;
    }
    let g = &bytes[8..24];
    let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let d2 = u16::from_le_bytes([g[4], g[5]]);
    let d3 = u16::from_le_bytes([g[6], g[7]]);
    Some(format!(
        "{{{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}}}",
        d1, d2, d3, g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    ))
}

/// Resolve the SDL HID interface path to the matching 4-ch render endpoint id — the Windows
/// correlation chain: interface path → instance id → devnode `ContainerID` (registry) → the
/// active eRender endpoint whose stamped `PKEY_Device_ContainerId` matches with a 4-channel
/// device format. Registry-only for the property reads (the MMDevices ACL denies writes, never
/// reads); the endpoint enumeration runs on its own MTA thread like [`crate::audio::devices`].
#[cfg(windows)]
pub(crate) fn correlate_pad_endpoint(hid_path: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let instance = hid_instance_from_interface_path(hid_path)
        .with_context(|| format!("unrecognised HID interface path shape: {hid_path}"))?;
    let container = hid_container_id(&instance)
        .with_context(|| format!("no ContainerID on devnode {instance}"))?;
    let endpoints = render_endpoints()?;
    pick_pad_endpoint(&endpoints, &container)
        .map(|e| e.id.clone())
        .with_context(|| {
            format!(
                "no active 4-ch render endpoint in container {container} \
                 ({} endpoints inspected)",
                endpoints.len()
            )
        })
}

/// The devnode's `ContainerID` value (a braced GUID string) under
/// `HKLM\SYSTEM\CurrentControlSet\Enum\<instance>`.
#[cfg(windows)]
fn hid_container_id(instance: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SYSTEM\CurrentControlSet\Enum\{instance}"))
        .with_context(|| format!(r"open Enum\{instance}"))?;
    key.get_value::<String, _>("ContainerID")
        .context("read ContainerID")
}

/// Enumerate the active eRender endpoints with the container + channel facts the matcher needs.
/// Its own short-lived MTA thread (the caller may sit in an STA — the [`crate::audio::devices`]
/// discipline); one broken endpoint must not hide the rest.
#[cfg(windows)]
fn render_endpoints() -> anyhow::Result<Vec<EndpointCandidate>> {
    use anyhow::{anyhow, Context};
    std::thread::Builder::new()
        .name("pf-pad-audio-enum".into())
        .spawn(|| -> anyhow::Result<Vec<EndpointCandidate>> {
            wasapi::initialize_mta()
                .ok()
                .context("CoInitializeEx (MTA)")?;
            let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
            let coll = enumerator
                .get_device_collection(&wasapi::Direction::Render)
                .context("render endpoint collection")?;
            let mut out = Vec::new();
            for i in 0..coll.get_nbr_devices().context("endpoint count")? {
                let Ok(dev) = coll.get_device_at_index(i) else {
                    continue;
                };
                let Ok(id) = dev.get_id() else {
                    continue;
                };
                let channels = dev
                    .get_device_format()
                    .map(|f| f.get_nchannels())
                    .unwrap_or(0);
                out.push(EndpointCandidate {
                    container: endpoint_container_id(&id),
                    id,
                    channels,
                });
            }
            Ok(out)
        })
        .context("spawn pad-audio enumeration thread")?
        .join()
        .map_err(|_| anyhow!("pad-audio enumeration thread panicked"))?
}

/// The endpoint's stamped `PKEY_Device_ContainerId`, read from its MMDevices property store in
/// the registry (`…\MMDevices\Audio\Render\{ep-guid}\Properties`, value
/// `"{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2"`, a serialized VT_CLSID blob).
#[cfg(windows)]
fn endpoint_container_id(endpoint_id: &str) -> Option<String> {
    let guid = endpoint_id.rfind('{').map(|i| &endpoint_id[i..])?;
    let key = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(format!(
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{guid}\Properties"
        ))
        .ok()?;
    let v = key
        .get_raw_value("{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2")
        .ok()?;
    container_guid_from_blob(&v.bytes)
}

// ---- the 4-channel mixer --------------------------------------------------------------------

/// Interleave the two independent stereo streams into one 4-ch frame stream: speaker
/// ([`PAD_AUDIO_KIND_SPEAKER`]) on channels 0/1, haptics ([`PAD_AUDIO_KIND_HAPTICS`]) on 2/3.
/// Each kind has its own write cursor (they arrive on different cadences — 10 ms vs 5 ms);
/// [`pop`](Self::pop) emits every frame the FURTHER-ahead kind has filled, with the lagging
/// (or absent) kind's pair reading zeros — so a haptics-only session plays voice-coil audio
/// with a silent speaker pair and vice versa. Pure logic (unit-tested); pacing and latency
/// bounds live in the platform ring downstream.
pub(crate) struct QuadMixer {
    /// Interleaved 4-ch samples; the front is the next frame to output. Length is always
    /// `ready_frames() * 4` (pushes zero-extend for their own cursor only).
    ring: std::collections::VecDeque<f32>,
    /// Per-kind write cursor in FRAMES relative to the ring front (`[haptics, speaker]` —
    /// indexed by the wire `kind`).
    written: [usize; 2],
}

impl QuadMixer {
    pub(crate) fn new() -> QuadMixer {
        QuadMixer {
            ring: std::collections::VecDeque::new(),
            written: [0; 2],
        }
    }

    /// Write one decoded stereo chunk (interleaved L/R) for `kind` at that kind's cursor,
    /// zero-extending the ring as needed. Depth is bounded by [`MAX_BUFFER_FRAMES`] — overflow
    /// drops the oldest frames (both kinds shift together, so the interleave never skews).
    pub(crate) fn push(&mut self, kind: u8, stereo: &[f32]) {
        let k = (kind as usize).min(1);
        let off = if kind == PAD_AUDIO_KIND_SPEAKER { 0 } else { 2 };
        let frames = stereo.len() / 2;
        let base = self.written[k];
        let need = (base + frames) * PAD_CHANNELS;
        if self.ring.len() < need {
            self.ring.resize(need, 0.0);
        }
        for (i, fr) in stereo.chunks_exact(2).enumerate() {
            let at = (base + i) * PAD_CHANNELS + off;
            self.ring[at] = fr[0];
            self.ring[at + 1] = fr[1];
        }
        self.written[k] = base + frames;
        let over = self.ready_frames().saturating_sub(MAX_BUFFER_FRAMES);
        if over > 0 {
            self.drop_front(over);
        }
    }

    /// Frames ready to output: the further-ahead kind's cursor (the other pair reads zeros).
    pub(crate) fn ready_frames(&self) -> usize {
        self.written[0].max(self.written[1])
    }

    /// Append every ready frame (interleaved 4-ch) to `out`; returns the frame count. Both
    /// cursors move back together, so a kind that lagged simply resumes at the new front.
    pub(crate) fn pop(&mut self, out: &mut Vec<f32>) -> usize {
        let frames = self.ready_frames();
        let n = frames * PAD_CHANNELS;
        debug_assert_eq!(self.ring.len(), n);
        out.extend(self.ring.drain(..n.min(self.ring.len())));
        for w in &mut self.written {
            *w = w.saturating_sub(frames);
        }
        frames
    }

    /// Throw the ready frames away (no output device right now).
    pub(crate) fn discard(&mut self) {
        let f = self.ready_frames();
        self.drop_front(f);
    }

    fn drop_front(&mut self, frames: usize) {
        let n = (frames * PAD_CHANNELS).min(self.ring.len());
        self.ring.drain(..n);
        let f = n / PAD_CHANNELS;
        for w in &mut self.written {
            *w = w.saturating_sub(f);
        }
    }
}

// ---- decode + PLC ---------------------------------------------------------------------------

/// Per-(pad, kind) decode state: a stereo 48 kHz Opus decoder, the seq-gap tracker, and the
/// last decoded frame size (the PLC synthesis unit — session.rs's audio-thread discipline).
struct KindStream {
    dec: opus::Decoder,
    gaps: AudioGapTracker,
    frame_samples: usize,
}

/// How many concealment frames to synthesize before decoding `seq`: the tracker's capped gap
/// count — but 0 until a first frame decoded (`frame_samples == 0`; there is nothing to size
/// the PLC from). The tracker is ALWAYS fed, so a pre-first-frame gap can't replay later as a
/// phantom gap. Pure (unit-tested).
fn plc_frames(gaps: &mut AudioGapTracker, seq: u32, frame_samples: usize) -> u32 {
    let missing = gaps.missing_before(seq);
    if frame_samples == 0 {
        0
    } else {
        missing
    }
}

// ---- the renderer worker --------------------------------------------------------------------

/// Spawn the pad-audio renderer thread — the 0xD1 plane's single consumer, started by the
/// session pump whenever the settings could render (`pad_haptics` / `pad_speaker == "pad"`).
/// The output device is opened LAZILY on the first arriving frame: frames only flow once a
/// tier-A pad declared render caps on its arrival, so a session without a wired DualSense
/// costs one idle 10 ms poll loop and never touches the audio graph. Exits on the session
/// stop flag (join it like the audio thread) or the plane closing.
pub(crate) fn spawn(
    connector: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    haptics: bool,
    speaker: bool,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pf-pad-audio".into())
        .spawn(move || run(&connector, &stop, haptics, speaker))
        .map_err(|e| tracing::warn!(error = %e, "pad-audio thread failed to start"))
        .ok()
}

fn run(connector: &NativeClient, stop: &AtomicBool, haptics: bool, speaker: bool) {
    // Per-kind decode state for the ONE rendered pad (v1: the first pad that streams; the
    // spec's per-(pad, kind) fan-out degenerates to per-kind once the pad is latched).
    let mut streams: [Option<KindStream>; 2] = [None, None];
    let mut mixer = QuadMixer::new();
    let mut pcm = vec![0f32; 5760 * 2]; // scratch: max Opus frame (120 ms) × stereo
    let mut out: Option<PadOut> = None;
    let mut active_pad: Option<u8> = None;
    let mut other_pad_logged = false;
    let mut open_fail_logged = false;
    let mut retry_at = Instant::now();
    let mut backoff = RETRY_MIN;
    while !stop.load(Ordering::SeqCst) {
        let Some(f) = connector.next_pad_audio(Duration::from_millis(10)) else {
            if connector.is_session_ended() {
                break;
            }
            continue;
        };
        // The host only emits kinds the arrival declared, but the settings gate is re-checked
        // here so a stale host can never force an undeclared renderer.
        if f.kind > 1
            || (f.kind == PAD_AUDIO_KIND_HAPTICS && !haptics)
            || (f.kind == PAD_AUDIO_KIND_SPEAKER && !speaker)
        {
            continue;
        }
        // v1 renders ONE physical DualSense: latch the first streaming pad, drop the rest.
        match active_pad {
            None => active_pad = Some(f.pad),
            Some(p) if p != f.pad => {
                if !other_pad_logged {
                    other_pad_logged = true;
                    tracing::info!(
                        rendered = p,
                        ignored = f.pad,
                        "pad audio from a second pad — v1 renders one physical DualSense"
                    );
                }
                continue;
            }
            _ => {}
        }
        let k = f.kind as usize;
        if streams[k].is_none() {
            match opus::Decoder::new(48_000, opus::Channels::Stereo) {
                Ok(dec) => {
                    streams[k] = Some(KindStream {
                        dec,
                        gaps: AudioGapTracker::new(),
                        frame_samples: 0,
                    })
                }
                Err(e) => {
                    tracing::warn!(error = %e, kind = f.kind, "pad-audio opus decoder failed");
                    continue;
                }
            }
        }
        let st = streams[k].as_mut().expect("inserted above");
        // Conceal lost packets (a seq gap) with libopus PLC before decoding the arrival —
        // the session audio thread's exact discipline. A frozen seq (the host paused the
        // stream) produces no packets at all, which is silence by construction.
        for _ in 0..plc_frames(&mut st.gaps, f.seq, st.frame_samples) {
            let n = st.frame_samples * 2;
            if let Ok(samples) = st.dec.decode_float(&[], &mut pcm[..n], false) {
                mixer.push(f.kind, &pcm[..samples * 2]);
            }
        }
        if !f.opus.is_empty() {
            match st.dec.decode_float(&f.opus, &mut pcm, false) {
                Ok(samples) => {
                    st.frame_samples = samples;
                    mixer.push(f.kind, &pcm[..samples * 2]);
                }
                Err(e) => tracing::debug!(error = %e, kind = f.kind, "pad-audio opus decode"),
            }
        }
        // Output: open lazily (frames flowing prove a tier-A pad exists), drop + re-correlate
        // with backoff when the device goes away (USB unplug kills the sink/endpoint).
        if out.as_ref().is_some_and(PadOut::finished) {
            tracing::info!("pad-audio output ended (device gone?) — re-correlating");
            out = None;
            retry_at = Instant::now() + backoff;
            backoff = (backoff * 2).min(RETRY_MAX);
        }
        if out.is_none() && Instant::now() >= retry_at {
            match PadOut::open() {
                Ok(o) => {
                    tracing::info!("pad-audio output opened on the DualSense audio device");
                    out = Some(o);
                    backoff = RETRY_MIN;
                    open_fail_logged = false;
                }
                Err(e) => {
                    if !open_fail_logged {
                        open_fail_logged = true;
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "no DualSense audio device — pad audio parked (retrying with backoff)"
                        );
                    }
                    retry_at = Instant::now() + backoff;
                    backoff = (backoff * 2).min(RETRY_MAX);
                }
            }
        }
        match &out {
            Some(o) => {
                let mut chunk = o.take_buffer();
                if mixer.pop(&mut chunk) > 0 {
                    o.push(chunk);
                }
            }
            None => mixer.discard(),
        }
    }
    tracing::debug!("pad-audio pull thread exited");
}

// ---- platform output: Linux (PipeWire) ------------------------------------------------------

/// The platform output half: a dedicated device thread fed interleaved 4-ch f32 chunks over a
/// bounded channel with a recycle pool (the `AudioPlayer` shape), targeting the correlated
/// DualSense device. `finished()` is the device-gone signal — the worker drops the handle and
/// re-correlates with backoff.
#[cfg(target_os = "linux")]
struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    quit_tx: pipewire::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl PadOut {
    /// Correlate (sink signature match) and open the PipeWire playback stream on it.
    fn open() -> anyhow::Result<PadOut> {
        use anyhow::Context;
        let (sinks, _) = crate::audio::devices().context("enumerate sinks")?;
        let sink =
            find_ds5_sink(&sinks).ok_or_else(|| anyhow::anyhow!("no DualSense sink found"))?;
        tracing::info!(sink = %sink.name, description = %sink.description, "pad-audio sink matched");
        // 64 × 5 ms of slack between the renderer worker and the PipeWire loop, with the
        // recycle pool keeping the steady state allocation-free (the AudioPlayer shape).
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
        let target = sink.name;
        let thread = std::thread::Builder::new()
            .name("pf-pad-audio-out".into())
            .spawn(move || {
                if let Err(e) = pad_pw_thread(pcm_rx, recycle_tx, quit_rx, target) {
                    tracing::warn!(error = %format!("{e:#}"), "pad-audio playback thread ended");
                }
            })
            .context("spawn pad-audio playback thread")?;
        Ok(PadOut {
            pcm_tx,
            recycle_rx,
            quit_tx,
            thread: Some(thread),
        })
    }

    fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

#[cfg(target_os = "linux")]
impl Drop for PadOut {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The PipeWire playback thread on the DualSense sink: 4 channels positioned FL FR RL RR (the
/// pad's speaker pair + the voice-coil pair), a 5 ms quantum, and the session player's
/// adaptive ring policy at a SMALLER floor — haptics are felt latency, so the prime target is
/// 3 quanta bounded to [240, 2400] frames instead of the main player's [720, 9600].
#[cfg(target_os = "linux")]
fn pad_pw_thread(
    pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    recycle_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<()>,
    target: String,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Playback",
        *pw::keys::MEDIA_ROLE       => "Game",
        *pw::keys::NODE_NAME        => "punktfunk-pad-audio",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Pad Audio",
        // ~5 ms quantum (one haptics Opus frame) keeps the ring — and the felt latency — small.
        *pw::keys::NODE_LATENCY     => "240/48000",
        // The correlated DualSense sink (raw key — the `keys::TARGET_OBJECT` constant is
        // feature-gated on a newer libpipewire than we require; the wire name is stable).
        "target.object"             => target.as_str(),
        // The pad unplugging must END this stream (the worker re-correlates), not let the
        // session manager re-route 4-ch haptics onto the desktop speakers.
        "node.dont-reconnect"       => "true",
    };
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-pad-audio", props).context("pw Stream")?;

    struct PadPlayData {
        rx: std::sync::mpsc::Receiver<Vec<f32>>,
        recycle: std::sync::mpsc::SyncSender<Vec<f32>>,
        ring: std::collections::VecDeque<f32>,
        primed: bool,
    }
    let ud = PadPlayData {
        rx: pcm_rx,
        recycle: recycle_tx,
        ring: std::collections::VecDeque::new(),
        primed: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed({
            let mainloop = mainloop.clone();
            move |_s, _ud, old, new| {
                tracing::debug!(?old, ?new, "pipewire pad-audio stream state");
                // Device gone (USB unplug) with dont-reconnect: the stream errors out — end
                // the thread so the worker's `finished()` check re-correlates with backoff.
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    mainloop.quit();
                }
            }
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                while let Ok(mut chunk) = ud.rx.try_recv() {
                    ud.ring.extend(chunk.iter().copied());
                    chunk.clear();
                    let _ = ud.recycle.try_send(chunk);
                }
                let stride = 4 * PAD_CHANNELS; // F32LE interleaved
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let want_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                let want = want_frames * PAD_CHANNELS;

                // The adaptive jitter buffer at the pad floor: prime to ~3 quanta within
                // [240, 2400] frames, cap ~1 quantum of slack beyond, re-prime after a drain.
                let target = (3 * want).clamp(240 * PAD_CHANNELS, 2400 * PAD_CHANNELS);
                while ud.ring.len() > target.max(want) + want {
                    ud.ring.pop_front();
                }
                if !ud.primed && ud.ring.len() >= target {
                    ud.primed = true;
                }

                let n_frames = if let Some(slice) = data.data() {
                    for k in 0..want {
                        let s = if ud.primed {
                            ud.ring.pop_front().unwrap_or(0.0)
                        } else {
                            0.0
                        };
                        let off = k * 4;
                        slice[off..off + 4].copy_from_slice(&s.to_le_bytes());
                    }
                    want_frames
                } else {
                    0
                };
                if ud.ring.is_empty() {
                    ud.primed = false;
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire pad-audio callback");
            }
        })
        .register()
        .context("register pad-audio listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(PAD_CHANNELS as u32);
    // FL FR RL RR (SPA ids 3 4 12 13): the front pair is the pad's speaker, the rear pair the
    // voice coils — the DS5 device's own channel order, identity-routed (no remix wanted).
    let mut positions = [0u32; 64];
    positions[..4].copy_from_slice(&[3, 4, 12, 13]);
    info.set_position(positions);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize pad format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("pad pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw pad stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire pad-audio loop exited");
    Ok(())
}

// ---- platform output: Windows (WASAPI) ------------------------------------------------------

#[cfg(windows)]
struct PadOut {
    pcm_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    recycle_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl PadOut {
    /// Correlate (HID container → endpoint id) and open a shared event-driven render stream ON
    /// that endpoint (`audio_wasapi::render_thread`'s shape — autoconvert, default period).
    fn open() -> anyhow::Result<PadOut> {
        use anyhow::{anyhow, Context};
        let hid_path =
            first_tier_a_hid_path().ok_or_else(|| anyhow!("no tier-A pad registered"))?;
        let endpoint = correlate_pad_endpoint(&hid_path)?;
        tracing::info!(endpoint = %endpoint, "pad-audio endpoint correlated");
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<anyhow::Result<()>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pf-pad-audio-out".into())
            .spawn(move || {
                if let Err(e) = pad_render_thread(pcm_rx, recycle_tx, stop_t, ready_tx, &endpoint) {
                    tracing::warn!(error = %format!("{e:#}"), "pad-audio render thread ended");
                }
            })
            .context("spawn pad-audio render thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(PadOut {
                pcm_tx,
                recycle_rx,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("pad-audio render init timed out")),
        }
    }

    fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    fn push(&self, pcm: Vec<f32>) {
        let _ = self.pcm_tx.try_send(pcm); // never block the renderer; drops are concealed
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

#[cfg(windows)]
impl Drop for PadOut {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The WASAPI render thread ON the correlated endpoint: shared event-driven, autoconvert, 4 ch
/// f32 masked FL|FR|BL|BR (0x33 — the DS5 endpoint's own layout, so the map is identity), and
/// the session player's ring policy at the pad floor ([240, 2400] frames instead of
/// [720, 9600] — haptics are felt latency). Any device error (unplug) ends the thread; the
/// worker's `finished()` check re-correlates with backoff.
#[cfg(windows)]
fn pad_render_thread(
    pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    recycle_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
    endpoint_id: &str,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use wasapi::{Direction, SampleType, StreamMode, WaveFormat};
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    let res = (|| -> anyhow::Result<()> {
        const BLOCK_ALIGN: usize = PAD_CHANNELS * 4; // f32 interleaved
        let enumerator = wasapi::DeviceEnumerator::new().context("DeviceEnumerator")?;
        let device = enumerator
            .get_device(endpoint_id)
            .map_err(|e| anyhow!("correlated endpoint not found: {e}"))?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // FL|FR|BL|BR: front pair = the pad's speaker, back pair = the voice coils.
        let desired = WaveFormat::new(32, 32, &SampleType::Float, 48_000, PAD_CHANNELS, Some(0x33));
        let (default_period, _min_period) =
            audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Render, &mode)
            .context("initialize pad render client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("IAudioRenderClient")?;
        audio_client
            .start_stream()
            .context("start pad render stream")?;
        let _ = ready.send(Ok(()));

        // The adaptive jitter buffer in f32-byte units, at the pad floor (see the module doc).
        let mut ring: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut primed = false;
        let mut out = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            if h_event.wait_for_event(100).is_err() {
                continue;
            }
            while let Ok(mut chunk) = pcm_rx.try_recv() {
                for s in chunk.iter() {
                    ring.extend(s.to_le_bytes());
                }
                chunk.clear();
                let _ = recycle_tx.try_send(chunk);
            }
            let avail_frames = audio_client
                .get_available_space_in_frames()
                .context("available space")? as usize;
            if avail_frames == 0 {
                continue;
            }
            let want_bytes = avail_frames * BLOCK_ALIGN;

            // Prime to ~3 quanta within [240, 2400] frames; cap ~1 quantum of slack beyond;
            // instant re-prime on a genuine drain.
            let target = (3 * want_bytes).clamp(240 * BLOCK_ALIGN, 2400 * BLOCK_ALIGN);
            let cap = target.max(want_bytes) + want_bytes;
            if ring.len() > cap {
                ring.drain(..ring.len() - cap);
            }
            if !primed && ring.len() >= target {
                primed = true;
            }

            out.clear();
            out.resize(want_bytes, 0);
            if primed {
                let n = ring.len().min(want_bytes);
                for (dst, b) in out.iter_mut().zip(ring.drain(..n)) {
                    *dst = b;
                }
            }
            if ring.is_empty() {
                primed = false;
            }
            render_client
                .write_to_device(avail_frames, &out, None)
                .context("write_to_device")?;
        }
        audio_client.stop_stream().ok();
        Ok(())
    })();
    if let Err(ref e) = res {
        let _ = ready.send(Err(anyhow::anyhow!("{e:#}")));
    }
    res
}

// ---- tests ----------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The speaker mode gate: only `"pad"` renders today; `"mix"` is the declared TODO and
    /// reads as off; unknown values (a future store, a typo) fail safe to off.
    #[test]
    fn speaker_mode_gates() {
        assert!(speaker_active("pad"));
        assert!(!speaker_active("off"));
        assert!(!speaker_active("mix")); // TODO leg — off until the mixer exists
        assert!(!speaker_active(""));
        assert!(!speaker_active("Pad")); // stored names are lowercase; anything else is off
    }

    /// Tier A needs all three facts: the DualSense/Edge VID:PID and a wired connection.
    #[test]
    fn tier_a_is_wired_ds5_or_edge_only() {
        assert!(is_tier_a_ds5(0x054C, 0x0CE6, true)); // DualSense
        assert!(is_tier_a_ds5(0x054C, 0x0DF2, true)); // DualSense Edge
        assert!(!is_tier_a_ds5(0x054C, 0x0CE6, false)); // Bluetooth → no audio device
        assert!(!is_tier_a_ds5(0x054C, 0x05C4, true)); // DualShock 4
        assert!(!is_tier_a_ds5(0x045E, 0x0CE6, true)); // wrong vendor, right product id
        assert!(!is_tier_a_ds5(0x28DE, 0x1205, true)); // Steam Deck
    }

    /// The Linux sink signature: USB vendor string in the node name, product in the
    /// description, or the kernel's bare "Wireless Controller" fallback — and nothing else.
    #[test]
    fn ds5_sink_signature_matching() {
        assert!(is_ds5_sink(
            "alsa_output.usb-Sony_Interactive_Entertainment_Wireless_Controller-00.analog-stereo",
            "Wireless Controller Analog Stereo"
        ));
        assert!(is_ds5_sink(
            "alsa_output.usb-054c_0ce6-00",
            "DualSense Wireless Controller"
        ));
        assert!(is_ds5_sink("Wireless Controller", ""));
        assert!(is_ds5_sink("", "Wireless Controller Audio"));
        assert!(!is_ds5_sink(
            "alsa_output.pci-0000_0a_00.4.analog-stereo",
            "Built-in Audio Analog Stereo"
        ));
        // "Wireless Controller" must LEAD the string — a headset description mentioning
        // "... for Wireless Controller" is not the pad.
        assert!(!is_ds5_sink("headset", "Adapter for Wireless Controller"));
    }

    /// The Windows container matcher: container equality (case-insensitive — registry GUIDs
    /// come in both cases) AND the 4-channel format gate.
    #[test]
    fn endpoint_pick_needs_container_and_four_channels() {
        let cands = [
            EndpointCandidate {
                id: "{0.0.0.00000000}.{aaaa}".into(),
                container: Some("{11111111-2222-3333-4444-555555555555}".into()),
                channels: 2, // right container, stereo — not the pad function
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{bbbb}".into(),
                container: Some("{99999999-2222-3333-4444-555555555555}".into()),
                channels: 4, // 4-ch but another container
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{cccc}".into(),
                container: Some("{11111111-2222-3333-4444-555555555555}".into()),
                channels: 4, // the pad
            },
            EndpointCandidate {
                id: "{0.0.0.00000000}.{dddd}".into(),
                container: None,
                channels: 4,
            },
        ];
        let hit = pick_pad_endpoint(&cands, "{11111111-2222-3333-4444-555555555555}").unwrap();
        assert_eq!(hit.id, "{0.0.0.00000000}.{cccc}");
        // Case-insensitive (Enum stores uppercase, MMDevices lowercase).
        let hit = pick_pad_endpoint(
            &cands,
            "{11111111-2222-3333-4444-555555555555}"
                .to_uppercase()
                .as_str(),
        )
        .unwrap();
        assert_eq!(hit.id, "{0.0.0.00000000}.{cccc}");
        assert!(pick_pad_endpoint(&cands, "{00000000-0000-0000-0000-000000000000}").is_none());
    }

    /// Interface path → instance id: prefix stripped, `#` → `\`, interface-class GUID dropped;
    /// garbage shapes are rejected rather than mis-keyed into the registry.
    #[test]
    fn hid_interface_path_to_instance_id() {
        assert_eq!(
            hid_instance_from_interface_path(
                r"\\?\HID#VID_054C&PID_0CE6#8&2de99099&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}"
            )
            .as_deref(),
            Some(r"HID\VID_054C&PID_0CE6\8&2de99099&0&0000")
        );
        // hidapi paths come lowercase and sometimes without the class GUID — both parse.
        assert_eq!(
            hid_instance_from_interface_path(r"\\?\hid#vid_054c&pid_0df2#7&1a2b3c4d&1&0000")
                .as_deref(),
            Some(r"hid\vid_054c&pid_0df2\7&1a2b3c4d&1&0000")
        );
        for bad in ["", "/dev/hidraw3", r"\\?\HID#VID_054C", "a#b#c#d#e"] {
            assert_eq!(
                hid_instance_from_interface_path(bad),
                None,
                "{bad:?} parsed"
            );
        }
    }

    /// The serialized VT_CLSID blob (8-byte PROPVARIANT header + registry-order GUID) parses to
    /// the braced string; short/foreign blobs don't.
    #[test]
    fn container_blob_parses_vt_clsid() {
        // {11223344-5566-7788-99aa-bbccddeeff00}: data1/2/3 little-endian on disk.
        let mut blob = vec![0x48, 0, 0, 0, 1, 0, 0, 0];
        blob.extend_from_slice(&[
            0x44, 0x33, 0x22, 0x11, 0x66, 0x55, 0x88, 0x77, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ]);
        assert_eq!(
            container_guid_from_blob(&blob).as_deref(),
            Some("{11223344-5566-7788-99aa-bbccddeeff00}")
        );
        assert_eq!(container_guid_from_blob(&blob[..20]), None); // truncated
        let mut wrong_vt = blob.clone();
        wrong_vt[0] = 0x41; // VT_BLOB — a format value, not a container
        assert_eq!(container_guid_from_blob(&wrong_vt), None);
    }

    /// The 4-ch interleave: speaker frames land on channels 0/1 at the speaker cursor, haptics
    /// on 2/3 at theirs, and the pop emits exactly the further-ahead kind's frame count with
    /// the lagging kind's tail zeroed.
    #[test]
    fn mixer_interleaves_kinds_into_quad_frames() {
        let mut m = QuadMixer::new();
        // 2 speaker frames, 1 haptics frame.
        m.push(PAD_AUDIO_KIND_SPEAKER, &[1.0, 2.0, 3.0, 4.0]);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[5.0, 6.0]);
        assert_eq!(m.ready_frames(), 2);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 2);
        assert_eq!(out, vec![1.0, 2.0, 5.0, 6.0, 3.0, 4.0, 0.0, 0.0]);
        assert_eq!(m.ready_frames(), 0);
        // After the pop both cursors are back at the front: the next haptics frame starts a
        // fresh quad frame with a silent speaker pair.
        m.push(PAD_AUDIO_KIND_HAPTICS, &[7.0, 8.0]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 1);
        assert_eq!(out, vec![0.0, 0.0, 7.0, 8.0]);
    }

    /// A single flowing kind never conjures data on the other pair (kind 1 → 0/1, kind 0 → 2/3).
    #[test]
    fn mixer_missing_kind_stays_zero() {
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_HAPTICS, &[0.5, -0.5, 0.25, -0.25]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 2);
        assert_eq!(out, vec![0.0, 0.0, 0.5, -0.5, 0.0, 0.0, 0.25, -0.25]);
        let mut m = QuadMixer::new();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[0.5, -0.5]);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out), 1);
        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.0]);
    }

    /// The depth bound: a wedged output can't grow the mixer past [`MAX_BUFFER_FRAMES`]; the
    /// oldest frames drop and both cursors shift together (no interleave skew).
    #[test]
    fn mixer_caps_depth_dropping_oldest() {
        let mut m = QuadMixer::new();
        let chunk = vec![1.0f32; 480 * 2]; // 480 frames per push
        for _ in 0..12 {
            m.push(PAD_AUDIO_KIND_HAPTICS, &chunk); // 5760 frames pushed
        }
        assert_eq!(m.ready_frames(), MAX_BUFFER_FRAMES);
        // A late speaker push still lands at ITS cursor (0 after the drops) — front of ring.
        m.push(PAD_AUDIO_KIND_SPEAKER, &[9.0, 9.0]);
        let mut out = Vec::new();
        m.pop(&mut out);
        assert_eq!(&out[..4], &[9.0, 9.0, 1.0, 1.0]);
        // `discard` empties without output.
        m.push(PAD_AUDIO_KIND_HAPTICS, &chunk);
        m.discard();
        assert_eq!(m.ready_frames(), 0);
    }

    /// Seq-gap PLC counting mirrors the session audio thread: nothing for the first packet or
    /// in-order flow, the exact gap for a loss, the tracker's cap for a burst — and 0 before a
    /// first decode (`frame_samples == 0`) while STILL consuming the gap (no phantom replay).
    #[test]
    fn plc_counts_gaps_like_the_session_audio_path() {
        let mut gaps = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut gaps, 0, 480), 0); // first packet
        assert_eq!(plc_frames(&mut gaps, 1, 480), 0); // in-order
        assert_eq!(plc_frames(&mut gaps, 5, 480), 3); // 2,3,4 lost
        assert_eq!(plc_frames(&mut gaps, 5, 480), 0); // duplicate
        assert_eq!(plc_frames(&mut gaps, 4, 480), 0); // reorder — nothing to conceal
        assert_eq!(plc_frames(&mut gaps, 1000, 480), 10); // burst, capped (MAX_CONCEAL_PACKETS)
                                                          // Before the first decode there is no frame size to synthesize from — but the tracker
                                                          // must still advance, or this gap would replay against the next packet.
        let mut gaps = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut gaps, 7, 0), 0);
        assert_eq!(plc_frames(&mut gaps, 12, 0), 0); // gap consumed silently
        assert_eq!(plc_frames(&mut gaps, 13, 480), 0); // in-order once decoding starts
    }
}
