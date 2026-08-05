//! Session controller: the worker thread runs connect → pump (video pull + decode +
//! stats), a dedicated audio thread pulls + Opus-decodes the audio plane (Apple
//! `SessionAudio` parity — audio never waits behind a video decode), both feeding the GTK
//! main loop / PipeWire over channels. The UI keeps the `Arc<NativeClient>` from the
//! `Connected` event for direct input sends (no extra hop on the input path) —
//! `NativeClient` is `Sync`, planes stay one-consumer-per-thread: video here, audio on
//! its own thread, rumble+hidout on the gamepad thread.

use crate::audio;
use crate::video::{DecodedFrame, DecodedImage, Decoder};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use punktfunk_core::reanchor::{index_gap, GateVerdict, ReanchorGate};
use punktfunk_core::PunktfunkError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct SessionParams {
    pub host: String,
    pub port: u16,
    pub mode: Mode,
    pub compositor: CompositorPref,
    pub gamepad: GamepadPref,
    pub bitrate_kbps: u32,
    /// Requested audio channel count (2/6/8); the host echoes the resolved value.
    pub audio_channels: u8,
    /// The user's preferred video codec (a `quic::CODEC_*` bit, `0` = auto). Soft — the host honors
    /// it when it can emit it, else falls back; the resolved codec drives the decoder.
    pub preferred_codec: u8,
    /// The advertised `quic::VIDEO_CAP_*` bits. Normally 10-bit + HDR (Main10/PQ: the
    /// Vulkan presenter decodes P010 everywhere and presents PQ on an HDR10 swapchain
    /// where the desktop offers one, tonemapping in the CSC shader where it doesn't;
    /// the host still gates the upgrade behind its own PUNKTFUNK_10BIT policy) — `0`
    /// when the user turned HDR off in Settings ("never send me 10-bit").
    pub video_caps: u8,
    /// This display's HDR colour volume (primaries/white/luminance), when the embedder can read
    /// it from the OS. Rides `Hello::display_hdr` → the host's virtual-display EDID, so host apps
    /// tone-map to THIS panel. `None` = unknown/SDR (host EDID defaults). Overridable for testing
    /// via `PUNKTFUNK_CLIENT_PEAK_NITS` (synthesizes a BT.2020 volume at that peak).
    pub display_hdr: Option<punktfunk_core::quic::HdrMeta>,
    /// Stream the default microphone to the host's virtual mic source.
    pub mic_enabled: bool,
    /// Run the uplink through the platform's echo cancellation ([`Settings::echo_cancel`]).
    /// Ignored when `mic_enabled` is false; `PUNKTFUNK_NO_AEC=1` overrides it off.
    pub echo_cancel: bool,
    /// Render the host's per-pad DualSense voice-coil haptics stream (0xD1 kind 0) on a wired
    /// physical DualSense ([`crate::trust::Settings::pad_haptics`]). With `pad_speaker` it
    /// gates the `CLIENT_CAP_PAD_AUDIO` advertisement and the pad-audio renderer thread.
    pub pad_haptics: bool,
    /// Where the DualSense built-in-speaker stream (0xD1 kind 1) goes: `"pad"` | `"mix"` |
    /// `"off"` ([`crate::trust::Settings::pad_speaker`]; `"mix"` is a TODO that renders as
    /// off — see [`crate::pad_audio::speaker_active`]).
    pub pad_speaker: String,
    /// Share the clipboard with this host (the per-host `KnownHost::clipboard_sync`). The
    /// bridge additionally needs the host to advertise `HOST_CAP_CLIPBOARD`.
    pub clipboard: bool,
    /// Advertise `quic::CLIENT_CAP_CURSOR`: this embedder renders the host cursor locally
    /// (the presenter's cursor channel, design/remote-desktop-sweep.md M2), so the host may
    /// stop compositing the pointer into the video. Only set when the embedder actually
    /// draws it (the SDL presenter in desktop mouse mode) — a session that advertises it
    /// without rendering streams with NO visible cursor. The host answers `HOST_CAP_CURSOR`
    /// when its capture can forward (Linux portal, not gamescope/Windows).
    pub cursor_forward: bool,
    /// Video decoder preference (Settings; `PUNKTFUNK_DECODER` overrides — see
    /// `video::Decoder::new`).
    pub decoder: String,
    /// Library id for the host to launch this session (`"steam:570"`, from the library
    /// page); `None` = plain desktop session.
    pub launch: Option<String>,
    /// The presenter's shared Vulkan device, when its stack can run FFmpeg's Vulkan
    /// Video decoder (decode lands as VkImages the presenter samples directly).
    pub vulkan: Option<crate::video::VulkanDecodeDevice>,
    /// Pinned host fingerprint; `None` = trust on first use (caller persists the observed one).
    pub pin: Option<[u8; 32]>,
    pub identity: (String, String),
    /// How long to wait for the handshake. The normal path uses a short budget; the
    /// "request access" (delegated-approval) path uses a long one, because the host PARKS the
    /// connection until the operator clicks Approve in its console (so this must exceed the
    /// host's approval window — see `PENDING_APPROVAL_WAIT`).
    pub connect_timeout: Duration,
    /// Raised by the PRESENTER when hardware frames can't be displayed (GL converter init
    /// failed / dmabuf import rejected): the pump demotes the decoder to software and
    /// re-requests a keyframe. Decode itself succeeds in that state, so nothing else
    /// would recover — without this the stream stays black.
    pub force_software: Arc<AtomicBool>,
    /// Name of the settings profile these params were resolved with (`None` = the global
    /// defaults). Display only — every value it influenced is already baked into the fields
    /// above; it rides along so the stats overlay can answer "which profile am I on?" without
    /// re-reading any store (design/client-settings-profiles.md §5.2).
    pub profile: Option<String>,
    /// Advertise `quic::CLIENT_CAP_PHASE_LOCK`: this embedder's presenter has REAL on-glass
    /// latch stamps (`VK_KHR_present_wait`) and will feed [`latch_grid`](Self::latch_grid),
    /// so the pump sends the ~1 Hz `PhaseReport`s the host phase-locks its capture tick to
    /// (design/phase-locked-capture.md — previously Apple/Android only). Never set without
    /// present timing: the host arms on report receipt, but the Hello should say what the
    /// client actually does.
    pub phase_lock: bool,
    /// The presenter-written latch grid the pump's reports are computed from.
    pub latch_grid: Arc<LatchGrid>,
}

/// The presenter's display-latch grid, shared presenter → pump (the `force_software`
/// pattern in the other direction): the presenter's 1 Hz present-timing fold writes a
/// recent on-glass latch instant plus the panel period; the pump's stats window folds its
/// per-AU arrival stamps against them into the ~1 Hz `PhaseReport`. All zeros until the
/// first fold — and forever when present timing isn't available — so the pump simply
/// stays quiet then.
#[derive(Default)]
pub struct LatchGrid {
    /// A recent on-glass latch instant (client `CLOCK_REALTIME` ns — the same domain as
    /// the AU arrival stamps). Any grid point works; the report extrapolates forward.
    pub anchor_ns: std::sync::atomic::AtomicU64,
    /// The panel's latch period (ns). `0` = no grid yet.
    pub period_ns: std::sync::atomic::AtomicU64,
}

/// The session pump's share of the unified stats window (design/stats-unification.md):
/// stream facts plus the two stages measured before the presenter. The frame consumer in
/// `ui_stream` contributes the `display` stage and the end-to-end percentiles.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    /// AUs received (reassembled) per second, actual-elapsed-time denominator.
    pub fps: f32,
    /// Received payload bytes × 8 / elapsed (goodput, excludes FEC overhead).
    pub mbps: f32,
    /// p50 `host+network` stage: capture → received, host-clock corrected (ms).
    pub host_net_ms: f32,
    /// p50 `host` stage: the host's own capture→fully-sent, from the per-AU 0xCF host
    /// timings (design/stats-unification.md Phase 2). Valid only when `split`.
    pub host_ms: f32,
    /// p50 `network` stage: capture→received minus the host-reported share
    /// (`hostnet − host`, per-frame, saturating). Valid only when `split`.
    pub net_ms: f32,
    /// The window had matched host timings — the OSD splits `host+network` into
    /// `host + network`. An old host never emits 0xCF, so this stays false and the
    /// combined stage renders unchanged.
    pub split: bool,
    /// p50 host STAGE split (latency plan T0.1), valid only when `staged`: capture→submit
    /// queue age, encoder submit→bitstream, seal/FEC + send-channel wait (the residual
    /// `host − queue − encode − pace`), and the paced-send spread. Together they tile
    /// `host_ms`, giving per-stage attribution without a host-side log in hand.
    pub host_queue_ms: f32,
    pub host_encode_ms: f32,
    pub host_xfer_ms: f32,
    pub host_pace_ms: f32,
    /// The window had extended (staged) 0xCF timings — a host older than the stage tail
    /// sends the 13-byte form and the OSD keeps the plain `host` figure.
    pub staged: bool,
    /// p50 `decode` stage: received → decode COMPLETE, single-clock client-local (ms).
    /// Hardware paths measure GPU completion via the frame's timeline fence (an async
    /// decoder's submission returning in ~0.1 ms is not "decoded"); software measures
    /// the synchronous CPU decode.
    pub decode_ms: f32,
    /// Unrecoverable network frame drops this window, and their share of
    /// received+lost (%). The OSD renders the counter line only when nonzero.
    pub lost: u32,
    pub lost_pct: f32,
    /// Mic uplink frames this window: handed to the QUIC datagram send, and shed anywhere
    /// client-side (queue-full at the producer + the pump's stale-oldest backlog governor —
    /// see [`NativeClient::mic_stats`]). Both stay 0 while the mic is off OR muted (a mute
    /// stops the sending, not the capture), so the OSD renders the mic line only while voice
    /// is actually going out — the muted case has its own badge, which does not need stats on.
    pub mic_sent: u32,
    pub mic_dropped: u32,
    /// The decode path frames actually took this window (`"vaapi"`/`"software"`, empty
    /// until the first frame) — the OSD's trailing tag; tracks a mid-session fallback.
    pub decoder: &'static str,
    /// The encoder's CURRENT target bitrate (kbps): the Welcome resolve, then live per
    /// `BitrateChanged` ack. What `mbps` (measured goodput) is judged AGAINST — a user
    /// staring at "19 Mb/s" can't otherwise tell "the encoder is capped at 20" from "my
    /// 200 Mb/s ask was honoured and this scene is cheap" (the gap that let the
    /// settings-drop bug ship four releases). `0` = an old host that never reported one.
    pub target_kbps: u32,
    /// Automatic bitrate is armed (ABR moves `target_kbps` on its own) — the OSD tags the
    /// target `(auto)` so a moving figure reads as policy, not a broken setting.
    pub auto_rate: bool,
    /// The host resolved full-chroma 4:4:4 for this session (`Welcome::chroma_format`).
    pub chroma_444: bool,
    /// This session ADVERTISED `VIDEO_CAP_444` (the Settings "Full chroma" opt-in): with
    /// `chroma_444` false, the host declined — the OSD says so instead of leaving the
    /// switch's effect unobservable.
    pub asked_444: bool,
}

/// Frames the pump keeps waiting for their 0xCF host timing (pts → capture→received µs).
/// ~2 s at 120 Hz — a timing arrives within a frame or two of its AU, and against an old
/// host (no 0xCF at all) this just caps the dead-weight ring.
const PENDING_SPLIT_CAP: usize = 256;

/// Sort a window of µs samples in place and return `(p50, p95)` per the spec's index
/// rules (`sorted[len/2]`, `sorted[min(len*95/100, len-1)]`); an empty window reads 0.
pub fn window_percentiles(samples: &mut [u64]) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    (p50, p95)
}

pub enum SessionEvent {
    Connected {
        connector: Arc<NativeClient>,
        mode: Mode,
        fingerprint: [u8; 32],
    },
    /// `trust_rejected` is set when the connect failed the TLS trust check (a `Crypto`
    /// error): for a pinned connect this is the fingerprint-changed signal, so the UI can
    /// offer a re-pair (PIN) path rather than a dead-end error.
    Failed {
        msg: String,
        trust_rejected: bool,
    },
    Ended(Option<String>),
    Stats(Stats),
}

/// The in-stream microphone mute (B4), shared between the embedder's toggle (a keyboard chord
/// in the presenter) and the capture callback that reads it every quantum.
///
/// Two flags, not one, so the indicator can never lie: `live` is raised by the pump only once
/// the uplink is actually running, so a session whose mic is off in Settings — or whose capture
/// device failed to open — reports "no mic here" and the chord is a documented no-op instead of
/// silently latching a mute nothing implements. Per session by design: the mute is a moment
/// ("don't send the doorbell"), not a preference, so it is never persisted and every new
/// session starts unmuted.
#[derive(Clone, Default)]
pub struct MicControl {
    muted: Arc<AtomicBool>,
    live: Arc<AtomicBool>,
}

impl MicControl {
    /// True when this session has a running uplink to mute at all.
    pub fn live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    /// True when the user has muted a uplink that exists — what the OSD indicator draws.
    pub fn muted(&self) -> bool {
        self.live() && self.muted.load(Ordering::Relaxed)
    }

    /// Flip the mute. `Some(now_muted)` when it applied, `None` when this session has no
    /// uplink (the caller says so rather than pretending something happened).
    pub fn toggle(&self) -> Option<bool> {
        if !self.live() {
            return None;
        }
        let next = !self.muted.load(Ordering::Relaxed);
        self.muted.store(next, Ordering::Relaxed);
        Some(next)
    }

    /// The capture side's handle on the flag (the streamer reads it per quantum).
    fn flag(&self) -> Arc<AtomicBool> {
        self.muted.clone()
    }

    /// The pump's report that the uplink came up (or went away).
    fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::Relaxed);
    }
}

pub struct SessionHandle {
    pub events: async_channel::Receiver<SessionEvent>,
    pub frames: async_channel::Receiver<DecodedFrame>,
    pub stop: Arc<AtomicBool>,
    /// The in-stream mic mute. Inert (`live()` false) until the pump has the uplink running,
    /// and for the whole session when the mic is off in Settings.
    pub mic: MicControl,
    /// The pump thread. A Vulkan-Video pump SUBMITS to the shared device's decode
    /// queue — the presenter must join this before any `vkDeviceWaitIdle`/teardown
    /// (external-sync rule over every device queue).
    pub thread: Option<std::thread::JoinHandle<()>>,
}

pub fn start(params: SessionParams) -> SessionHandle {
    let (ev_tx, ev_rx) = async_channel::unbounded();
    // Tiny frame queue, newest wins: force_send displaces the oldest when the UI lags.
    let (frame_tx, frame_rx) = async_channel::bounded(2);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let mic = MicControl::default();
    let mic_w = mic.clone();
    let thread = std::thread::Builder::new()
        .name("punktfunk-session".into())
        .spawn(move || pump(params, ev_tx, frame_tx, stop_w, mic_w))
        .expect("spawn session thread");
    SessionHandle {
        events: ev_rx,
        frames: frame_rx,
        stop,
        mic,
        thread: Some(thread),
    }
}

pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Opus decoder for the audio plane: a plain stereo decoder (the validated path) or a multistream
/// decoder for 5.1/7.1, both behind one `decode_float`. Built from the host-RESOLVED channel count
/// via the shared layout table.
enum AudioDec {
    Stereo(opus::Decoder),
    Surround(opus::MSDecoder),
}

impl AudioDec {
    fn new(channels: u8) -> Result<AudioDec, opus::Error> {
        if channels == 2 {
            Ok(AudioDec::Stereo(opus::Decoder::new(
                48_000,
                opus::Channels::Stereo,
            )?))
        } else {
            let l = punktfunk_core::audio::layout_for(channels, false);
            Ok(AudioDec::Surround(opus::MSDecoder::new(
                48_000, l.streams, l.coupled, l.mapping,
            )?))
        }
    }

    fn decode_float(
        &mut self,
        input: &[u8],
        out: &mut [f32],
        fec: bool,
    ) -> Result<usize, opus::Error> {
        match self {
            AudioDec::Stereo(d) => d.decode_float(input, out, fec),
            AudioDec::Surround(d) => d.decode_float(input, out, fec),
        }
    }
}

fn pump(
    params: SessionParams,
    ev_tx: async_channel::Sender<SessionEvent>,
    frame_tx: async_channel::Sender<DecodedFrame>,
    stop: Arc<AtomicBool>,
    mic: MicControl,
) {
    // PUNKTFUNK_PREFER_PYROWAVE=1 — the Phase-2 lab opt-in for the wired-LAN wavelet codec
    // (a Settings toggle is the Phase-3 productization). Riding `preferred_codec` is exactly
    // the plan-§3 contract: the host only ever picks PyroWave when the client names it.
    #[allow(unused_mut)]
    let mut preferred = params.preferred_codec;
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    if std::env::var("PUNKTFUNK_PREFER_PYROWAVE").as_deref() == Ok("1") {
        if params.vulkan.as_ref().is_some_and(|v| v.pyrowave_decode) {
            preferred = punktfunk_core::quic::CODEC_PYROWAVE;
        } else {
            tracing::warn!(
                "PUNKTFUNK_PREFER_PYROWAVE=1 but the presenter device failed the pyrowave probe — keeping the normal codec preference"
            );
        }
    }
    // Pad audio (0xD1): advertise only when the settings could render a stream — the per-pad
    // tier-A detection at slot open (gamepad.rs) still decides which pads declare render caps
    // on their arrivals, so this bit alone changes nothing without a wired DualSense.
    let pad_speaker_on = crate::pad_audio::speaker_active(&params.pad_speaker);
    let pad_audio_on = params.pad_haptics || pad_speaker_on;
    let connector = match NativeClient::connect(
        &params.host,
        params.port,
        params.mode,
        params.compositor,
        params.gamepad,
        params.bitrate_kbps,
        params.video_caps,
        params.audio_channels,
        // FFmpeg's codecs plus CODEC_PYROWAVE when the presenter device passed the probe.
        crate::video::decodable_codecs_for(params.vulkan.as_ref()),
        preferred, // the user's soft codec preference (0 = auto; see the pyrowave opt-in above)
        // This display's HDR volume → the host's virtual-display EDID. The env hatch wins so an
        // A/B run can pin an exact peak (PUNKTFUNK_CLIENT_PEAK_NITS=600).
        punktfunk_core::client::display_hdr_env_override().or(params.display_hdr),
        // CURSOR: this embedder renders the host cursor locally in desktop mouse mode.
        // PHASE_LOCK: the presenter has real latch stamps and the pump reports them below.
        (if params.cursor_forward {
            punktfunk_core::quic::CLIENT_CAP_CURSOR
        } else {
            0
        }) | (if params.phase_lock {
            punktfunk_core::quic::CLIENT_CAP_PHASE_LOCK
        } else {
            0
            // PAD_AUDIO: the embedder can render per-pad DualSense haptics/speaker (see above).
        }) | (if pad_audio_on {
            punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO
        } else {
            0
        }),
        // Slice-progressive delivery: off — this presenter feeds FFmpeg whole AUs; a partial
        // avcodec feed path can flip it later.
        false,
        params.launch.clone(),
        // The host's approval-list / trust-store label for this client. Without it every no-PIN
        // "request access" knock showed up as the fingerprint placeholder "device abcd1234".
        Some(crate::trust::device_name()),
        params.pin,
        Some(params.identity),
        params.connect_timeout,
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            let trust_rejected = matches!(e, PunktfunkError::Crypto);
            let msg = match e {
                PunktfunkError::Crypto => {
                    "Host identity rejected — wrong fingerprint, or the host requires pairing"
                        .to_string()
                }
                PunktfunkError::Timeout => "Connection timed out".to_string(),
                // The host said WHY it turned us away (typed application close) — show that
                // verbatim instead of a generic failure: "the request was denied on the host"
                // and "connection timed out" call for very different next steps.
                PunktfunkError::Rejected(reason) => crate::trust::connect_reject_message(reason),
                other => format!("Connect failed: {other:?}"),
            };
            let _ = ev_tx.send_blocking(SessionEvent::Failed {
                msg,
                trust_rejected,
            });
            return;
        }
    };
    let _ = ev_tx.send_blocking(SessionEvent::Connected {
        connector: connector.clone(),
        mode: connector.mode(),
        fingerprint: connector.host_fingerprint,
    });

    // Build the decoder for the codec the host resolved (never assume HEVC), honoring the
    // Settings backend preference (auto/vaapi/software).
    let codec_id = crate::video::ffmpeg_codec_id(connector.codec);
    // The WIRE codec is the negotiated truth; the FFmpeg id is meaningful only where
    // FFmpeg decodes it. `ffmpeg_codec_id`'s fallthrough maps every unknown wire bit —
    // PyroWave included — to HEVC, so logging it unconditionally claimed
    // `codec_id=HEVC` for wavelet sessions that never touch FFmpeg at all.
    let codec = match connector.codec {
        punktfunk_core::quic::CODEC_H264 => "H264",
        punktfunk_core::quic::CODEC_HEVC => "HEVC",
        punktfunk_core::quic::CODEC_AV1 => "AV1",
        punktfunk_core::quic::CODEC_PYROWAVE => "PyroWave",
        _ => "unknown",
    };
    if connector.codec == punktfunk_core::quic::CODEC_PYROWAVE {
        tracing::info!(
            codec,
            welcome_codec = connector.codec,
            "negotiated video codec"
        );
    } else {
        tracing::info!(
            codec,
            ?codec_id,
            welcome_codec = connector.codec,
            "negotiated video codec"
        );
    }
    // A negotiated PyroWave session decodes on the presenter's device, no FFmpeg —
    // reachable only through the explicit preference above (resolve_codec never
    // auto-picks the bit), so failing loudly here is failing an opted-in experiment.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    let built = if connector.codec == punktfunk_core::quic::CODEC_PYROWAVE {
        let mode = connector.mode();
        // The wavelet bitstream has no VUI: the negotiated Welcome colour signalling IS
        // the session's colour contract (BT.709 limited SDR today, BT.2020 PQ once the
        // HDR leg lands), and the chroma the host resolved sizes the plane ring.
        let color = crate::video::ColorDesc {
            primaries: connector.color.primaries,
            transfer: connector.color.transfer,
            matrix: connector.color.matrix,
            full_range: connector.color.full_range != 0,
        };
        match params.vulkan.as_ref() {
            Some(vk) => Decoder::new_pyrowave(
                vk,
                mode.width,
                mode.height,
                connector.shard_payload as usize,
                connector.chroma_format == punktfunk_core::quic::CHROMA_IDC_444,
                color,
                connector.bit_depth >= 10,
            ),
            None => Err(anyhow::anyhow!(
                "pyrowave session without a presenter device"
            )),
        }
    } else {
        Decoder::new(codec_id, &params.decoder, params.vulkan.as_ref())
    };
    #[cfg(not(all(any(target_os = "linux", windows), feature = "pyrowave")))]
    let built = Decoder::new(codec_id, &params.decoder, params.vulkan.as_ref());
    let mut decoder = match built {
        Ok(d) => d,
        Err(e) => {
            let _ = ev_tx.send_blocking(SessionEvent::Ended(Some(format!("video decoder: {e}"))));
            return;
        }
    };
    let force_software = params.force_software.clone();
    // Session-constant stats facts (design/stats-unification.md): what the target figure is
    // judged against and whether the 4:4:4 opt-in was honoured. `target_kbps` itself is read
    // live per window — an Automatic session's ABR moves it.
    let auto_rate = connector.wants_decode_latency();
    let chroma_444 = connector.chroma_format == punktfunk_core::quic::CHROMA_IDC_444;
    let asked_444 = params.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
    // Audio is best-effort: a session without it still streams. Gamepads are the
    // app-lifetime service's job (the UI attaches it on Connected). Audio runs on its own
    // thread (one puller per plane), blocking on the audio queue like the Apple client.
    let audio_thread = spawn_audio(connector.clone(), stop.clone());
    // Pad audio (0xD1): its own drain thread (that plane's single consumer), spawned whenever
    // the settings could render. The output device is opened LAZILY once frames actually
    // arrive — which only happens after a tier-A pad declared render caps on its arrival — so
    // a session without a wired DualSense costs one idle 10 ms poll loop.
    let pad_audio_thread = pad_audio_on
        .then(|| {
            crate::pad_audio::spawn(
                connector.clone(),
                stop.clone(),
                params.pad_haptics,
                pad_speaker_on,
            )
        })
        .flatten();
    // The shared clipboard (design/clipboard-and-file-transfer.md §5): its own thread, since
    // `next_clip` blocks and the OS clipboard calls can wait on other apps. Returns straight
    // away when the host has no clipboard capability, so spawning is unconditional.
    let clipboard_thread = params
        .clipboard
        .then(|| {
            let c = connector.clone();
            let s = stop.clone();
            std::thread::Builder::new()
                .name("pf-clipboard".into())
                .spawn(move || crate::clipboard::run(c, s))
                .ok()
        })
        .flatten();
    // The uplink, and with it the mute the embedder's chord drives. `set_live` is what makes
    // the chord (and its indicator) real: a mic turned off in Settings, or a capture device
    // that wouldn't open, leaves it false and the chord stays an honest no-op.
    let _mic = params
        .mic_enabled
        .then(|| {
            audio::MicStreamer::spawn(connector.clone(), mic.flag(), params.echo_cancel)
                .map_err(|e| tracing::warn!(error = %e, "mic uplink disabled"))
                .ok()
        })
        .flatten();
    mic.set_live(_mic.is_some());

    // Live host↔client clock offset: loaded per frame (Relaxed) so mid-stream re-syncs (an NTP
    // step, drift) keep the capture-clock latency stats honest — never cached at session start.
    let clock_offset_live = connector.clock_offset_shared();
    // Phase-lock (advertised above): every received AU's arrival stamp, folded per stats
    // window against the presenter's latch grid into the ~1 Hz PhaseReport. Desktop
    // sessions receive whole AUs only (no frame parts), so every arrival counts — the
    // reference reporters (Apple/Android) sample the same signal. 256 ≈ 2 s at 120 Hz.
    let latch_grid = params.latch_grid.clone();
    let mut phase_arrivals: Vec<u64> = Vec::new();
    let mut last_applied_phase: Option<i32> = None;
    // PUNKTFUNK_DEBUG_RECONFIGURE=WxH@HZ:SECS — lab lever: request ONE mid-stream mode
    // switch N seconds in, so a headless session (no window manager to drag a window in)
    // can exercise the resize path deterministically — host pipeline rebuild, decoder
    // follow-through (e.g. the PyroWave in-place rebuild), overlay/aspect handling.
    let pump_start = Instant::now();
    let mut debug_reconfig = std::env::var("PUNKTFUNK_DEBUG_RECONFIGURE")
        .ok()
        .and_then(|s| {
            let parsed = parse_debug_reconfigure(&s);
            if parsed.is_none() {
                tracing::warn!(value = %s, "PUNKTFUNK_DEBUG_RECONFIGURE not understood (want WxH@HZ:SECS) — ignored");
            }
            parsed
        });
    let mut total_frames = 0u64;
    // Newest frame index handed to the decoder — the staleness bar for late partials.
    let mut newest_decoded_idx: Option<u32> = None;
    let mut window_start = Instant::now();
    let mut frames_n = 0u32;
    let mut bytes_n = 0u64;
    // Stage windows (µs samples): `host+network` = capture→received (host-clock
    // corrected), `decode` = received→decoded (client-local). p50 per 1 s window.
    let mut hostnet_us: Vec<u64> = Vec::with_capacity(256);
    let mut decode_us: Vec<u64> = Vec::with_capacity(256);
    // Adaptive bitrate: report the decode stage back to the core controller only when it's armed
    // (Automatic, non-PyroWave). Constant for the session — resolve once, gate the per-frame call.
    let wants_decode = connector.wants_decode_latency();
    // Host/network split (Phase 2): frames awaiting their per-AU 0xCF host timing,
    // correlated by pts_ns. Bounded — an old host never sends any, so entries just age out.
    let mut pending_split: std::collections::VecDeque<(u64, u64)> =
        std::collections::VecDeque::with_capacity(PENDING_SPLIT_CAP);
    let mut host_us_win: Vec<u64> = Vec::with_capacity(256);
    let mut net_us_win: Vec<u64> = Vec::with_capacity(256);
    // T0.1 host-stage windows (extended 0xCF only; empty against an older host).
    let mut queue_us_win: Vec<u64> = Vec::with_capacity(256);
    let mut enc_us_win: Vec<u64> = Vec::with_capacity(256);
    let mut xfer_us_win: Vec<u64> = Vec::with_capacity(256);
    let mut pace_us_win: Vec<u64> = Vec::with_capacity(256);
    // What actually decoded the last frame — a VAAPI failure demotes mid-session, so
    // this is read off each frame's image variant rather than fixed at startup.
    let mut dec_path: &'static str = "";
    // The stats window keeps its own drop cursor — the OSD shows the per-window delta.
    let mut window_dropped = connector.frames_dropped();
    // Mic uplink cursor (same per-window diffing): a healthy 10 ms-frame mic reads ~100
    // sent/s; a nonzero drop delta is the queue shedding backlog (see NativeClient::mic_stats).
    let mut window_mic = connector.mic_stats();
    let mut last_kf_req: Option<Instant> = None;
    // Freeze-until-reanchor: the shared post-loss gate ([`punktfunk_core::reanchor::ReanchorGate`]).
    // Armed on any loss signal (frame-index gap, dropped-count climb, decoder wedge/demotion), it
    // withholds the decoder's concealed frames from the presenter — which then redraws the last good
    // picture — until a proven clean re-anchor (IDR / RFI anchor / second recovery mark) lifts it. It
    // also owns the no-output streak and the overdue-freeze backstop; the client keeps its own
    // `last_kf_req` request throttle and routes the gate's keyframe intents through it. Seeded with the
    // current drop count so the first `poll` doesn't read the baseline as a loss.
    let mut gate = ReanchorGate::new(connector.frames_dropped());
    // The frame_index we expect next (the host numbers frames consecutively). A jump means a frame
    // went missing — the earliest, most reliable signal that the decoder is about to conceal, ~120 ms
    // ahead of `frames_dropped` (the reassembler only declares a straggler lost once it ages out of
    // the loss window, by which point the concealment already reached the screen).
    let mut next_expected_index: Option<u32> = None;
    // Fixture capture for the native-decode program: every AU exactly as it reaches
    // `decode_frame`, plus a boundary/flags index — see `au_dump.rs` for the format.
    let mut au_dump = crate::au_dump::AuDump::from_env(connector.codec);

    let end: Option<String> = loop {
        if stop.load(Ordering::SeqCst) {
            break None;
        }
        if let Some((mode, delay)) = debug_reconfig {
            if pump_start.elapsed() >= delay {
                tracing::info!(
                    ?mode,
                    "PUNKTFUNK_DEBUG_RECONFIGURE: requesting mid-stream mode switch"
                );
                if let Err(e) = connector.request_mode(mode) {
                    tracing::warn!(error = ?e, "debug mode switch request failed");
                }
                debug_reconfig = None;
            }
        }
        // 20 ms wait: audio has its own thread now, so this only bounds stop-flag
        // responsiveness and the per-iteration keyframe-recovery check (a frame arrives
        // every ~8–16 ms at 60–120 Hz anyway, so this rarely times out mid-stream).
        match connector.next_frame(Duration::from_millis(20)) {
            Ok(frame) => {
                // The `received` point: reassembly COMPLETION, stamped by the core session as
                // the AU crossed poll_frame (ABI v9). Stamping here at the hand-off pull instead
                // would fold the pre-decode queue wait into `host+network` — a client-side
                // standing backlog masquerading as network latency (the 2026-07 two-pair
                // investigation). 0 = a core predating the stamp; fall back to the pull instant.
                let received_ns = if frame.received_ns > 0 {
                    frame.received_ns
                } else {
                    now_ns()
                };
                if params.phase_lock && phase_arrivals.len() < 256 {
                    phase_arrivals.push(received_ns);
                }
                // fps / goodput count every received AU (spec), decoded or not.
                frames_n += 1;
                bytes_n += frame.data.len() as u64;
                // Reference-continuity gate: the host numbers frames consecutively, so a jump in
                // frame_index means a frame is missing (lost, or an out-of-order straggler the
                // reassembler emitted a newer frame ahead of) and this AU references a picture we
                // never decoded. On RADV the decoder conceals that as a gray plate with the new
                // motion on top — the reported artifact, and it shows most on high-motion frames (a
                // full-screen pan bursts far more packets than a static desktop or a UFO-test's small
                // moving sprite, so it is the frame that loses shards). Arm the freeze at the FIRST
                // such frame — ~120 ms before `frames_dropped` would — so the gray never reaches the
                // screen; recovery IDRs stay on the existing throttled path (see the arm below).
                match next_expected_index {
                    Some(exp) if frame.frame_index == exp => {
                        next_expected_index = Some(exp.wrapping_add(1)); // contiguous
                    }
                    // A forward gap: hold the last good frame — but DO NOT ask for a keyframe here.
                    // Hiding the concealment is free (the presenter redraws the last picture); an IDR
                    // is not — at 4K120 it is a multi-megabyte frame and a visible stutter, and it can
                    // re-trigger the very burst loss that caused this. The existing loss recovery below
                    // (`frames_dropped`, host-coalesced + throttled) still requests it at exactly the
                    // cadence it did before this change, so we add zero IDR pressure per pan. A
                    // straggler behind us (`index_gap` → None) leaves the expectation put so the real
                    // gap still trips.
                    Some(exp) => {
                        if let Some(gap) = index_gap(exp, frame.frame_index) {
                            let now = Instant::now();
                            gate.arm(now);
                            next_expected_index = Some(frame.frame_index.wrapping_add(1));
                            // The gap carries the PRECISE lost range — [first missing, newest
                            // received - 1] — so this is the one recovery signal that can drive true
                            // reference-frame invalidation. Prefer an RFI request over a keyframe: an
                            // RFI-capable host (AMD LTR / NVENC) re-references a known-good picture and
                            // emits a clean P-frame tagged USER_FLAG_RECOVERY_ANCHOR (the freeze lifts
                            // on ONE frame, no 20-40× IDR spike); an incapable/old host forces a
                            // host-coalesced IDR instead, or ignores it (then the frames_dropped /
                            // overdue keyframe paths below are the backstop). Throttled with those
                            // paths (one recovery ask per 100 ms) so a burst of gaps — a full-screen
                            // pan shedding shards — can't storm the control stream. This fires ~120 ms
                            // before frames_dropped would, so recovery also starts sooner.
                            //
                            // A gap wider than RFI_MAX_RANGE is beyond any encoder's reference
                            // history (a seconds-long outage — or a phantom index jump, e.g. the
                            // first real AU after an old host's speed-test burst consumed video
                            // indexes): RFI is hopeless there, so ask for the IDR resync directly.
                            if last_kf_req
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                            {
                                last_kf_req = Some(now);
                                if gap > punktfunk_core::packet::RFI_MAX_RANGE {
                                    let _ = connector.request_keyframe();
                                } else {
                                    let _ = connector
                                        .request_rfi(exp, frame.frame_index.wrapping_sub(1));
                                }
                            }
                            tracing::trace!(
                                gap,
                                "frame gap — RFI recovery, holding last frame until re-anchor"
                            );
                        }
                    }
                    None => next_expected_index = Some(frame.frame_index.wrapping_add(1)),
                }
                // A PARTIAL that lost the race (a newer frame already decoded) is pure
                // time travel — skip it; each PyroWave frame is independent, so nothing
                // downstream needs it. Completes keep the normal path (reorder is handled
                // by the continuity gate).
                if !frame.complete
                    && newest_decoded_idx
                        .is_some_and(|n: u32| n.wrapping_sub(frame.frame_index) <= u32::MAX / 2)
                {
                    continue;
                }
                newest_decoded_idx = Some(match newest_decoded_idx {
                    Some(n) if frame.frame_index.wrapping_sub(n) > u32::MAX / 2 => n,
                    _ => frame.frame_index,
                });
                if let Some(d) = au_dump.as_mut() {
                    if !d.write(&frame.data, frame.flags, frame.complete) {
                        au_dump = None;
                    }
                }
                match decoder.decode_frame(&frame.data, frame.flags, frame.complete) {
                    Ok(Some(image)) => {
                        // Fold this decoded frame through the shared freeze gate: it reads the AU's
                        // re-anchor wire flags (FLAG_SOF IDR marker / RECOVERY_ANCHOR / RECOVERY_POINT),
                        // takes `image.is_keyframe()` as the ffmpeg keyframe belt, applies the two-mark
                        // rule + the mark-patience backstop, clears the no-output streak, and returns
                        // whether to present this frame or withhold it as a post-loss concealment.
                        let present =
                            gate.on_decoded(frame.flags, image.is_keyframe(), Instant::now())
                                == GateVerdict::Present;
                        total_frames += 1;
                        dec_path = match &image {
                            DecodedImage::Cpu(_) => "software",
                            #[cfg(target_os = "linux")]
                            DecodedImage::Dmabuf(_) => "vaapi",
                            DecodedImage::VkFrame(_) => "vulkan",
                            #[cfg(windows)]
                            DecodedImage::D3d11(_) => "d3d11va",
                            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
                            DecodedImage::PyroWave(_) => "pyrowave",
                            DecodedImage::NativeVk(_) => "native-vulkan",
                        };
                        if total_frames == 1 {
                            let (w, h, path) = match &image {
                                DecodedImage::Cpu(c) => (c.width, c.height, "software"),
                                #[cfg(target_os = "linux")]
                                DecodedImage::Dmabuf(d) => (d.width, d.height, "vaapi-dmabuf"),
                                DecodedImage::VkFrame(v) => (v.width, v.height, "vulkan-video"),
                                #[cfg(windows)]
                                DecodedImage::D3d11(d) => (d.width, d.height, "d3d11va"),
                                #[cfg(all(
                                    any(target_os = "linux", windows),
                                    feature = "pyrowave"
                                ))]
                                DecodedImage::PyroWave(f) => (f.width, f.height, "pyrowave"),
                                DecodedImage::NativeVk(f) => (f.width, f.height, "native-vulkan"),
                            };
                            tracing::info!(width = w, height = h, path, "first frame decoded");
                        }
                        // The `decoded` point — travels with the frame so the presenter
                        // can measure its `display` stage against it.
                        let decoded_ns = now_ns();
                        // `host+network` stage: received expressed in the host's capture
                        // clock, minus the host-stamped capture pts (clamped (0, 10 s)).
                        let clock_offset =
                            clock_offset_live.load(std::sync::atomic::Ordering::Relaxed);
                        let hn = (received_ns as i128 + clock_offset as i128 - frame.pts_ns as i128)
                            .max(0) as u64;
                        if hn > 0 && hn < 10_000_000_000 {
                            hostnet_us.push(hn / 1000);
                            // Remember the sample for the host/network split — matched
                            // against the AU's 0xCF host timing when it arrives.
                            if pending_split.len() >= PENDING_SPLIT_CAP {
                                pending_split.pop_front();
                            }
                            pending_split.push_back((frame.pts_ns, hn / 1000));
                        }
                        // Ship the frame FIRST, then settle the decode stat: on the
                        // Vulkan path receive_frame returns at SUBMISSION (~0.1 ms) and
                        // the hardware decodes asynchronously — the frame's timeline
                        // fence measures true received→decode-complete. But the fence
                        // wait BLOCKS this thread, and per-frame that serializes the
                        // pipeline to 1/decode_latency (observed: an APU's 19 ms decode
                        // capping a 5120×1440 stream at ~51 fps while the engine could
                        // pipeline several frames — and drivers may spin-wait, burning
                        // CPU). So sample ONE frame per stats window: the p50 the OSD
                        // shows becomes that sample — honest, at zero pipeline cost on
                        // every other frame. Software keeps the synchronous stamp on
                        // every frame (its decode really is done by now).
                        let hw_fence = match &image {
                            DecodedImage::VkFrame(v) => Some((v.timeline_sem, v.decode_done_value)),
                            // DecodedImage::NativeVk carries the same (semaphore,
                            // value) pair and COULD feed this sampled decode-time
                            // stat identically — deliberately deferred to WP-D:
                            // the native rung's field A/B should measure the same
                            // stats surface the FFmpeg rung had at parity time,
                            // and grow new ones after the verdict, not during it.
                            _ => None,
                        };
                        if present {
                            let _ = frame_tx.force_send(DecodedFrame {
                                pts_ns: frame.pts_ns,
                                decoded_ns,
                                image,
                            });
                        } else {
                            // Post-loss concealment: withhold this frame (it references a lost/gray
                            // reference) so the presenter keeps redrawing the last good picture rather
                            // than flashing the decoder's gray plate. Dropped here — the hw-decode stat
                            // below still samples via `hw_fence` (raw handle + value, valid past the
                            // guard). The gate lifts the freeze on the next clean re-anchor / backstop.
                            tracing::trace!("holding last frame — awaiting post-loss re-anchor");
                        }
                        // `decode` stage: received→decode COMPLETE, single clock.
                        match hw_fence {
                            Some((sem, value)) => {
                                if decode_us.is_empty()
                                    && decoder.wait_hw_decoded(sem, value, 50_000_000)
                                {
                                    decode_us.push(now_ns().saturating_sub(received_ns) / 1000);
                                }
                            }
                            None => {
                                decode_us.push(decoded_ns.saturating_sub(received_ns) / 1000);
                            }
                        }
                        // Adaptive bitrate: feed the decoder-backlog signal every frame (the network
                        // signals can't see the client's decoder). Uses the CPU-side decoded stamp:
                        // exact for the synchronous D3D11VA/software path; received→submit for the
                        // async Vulkan-Video path — still the decoder-input backpressure the rate
                        // controller needs, without the per-frame fence wait the HUD stat avoids.
                        if wants_decode {
                            let us = decoded_ns.saturating_sub(received_ns) / 1000;
                            connector.report_decode_us(us.min(u32::MAX as u64) as u32);
                        }
                    }
                    // The decoder produced nothing — under zero-reorder LOW_DELAY (one-in/one-out) that
                    // means it's wedged on missing references with no reassembler drop to trigger
                    // recovery. The gate counts the streak and, once it trips, arms the freeze and tells
                    // us to (throttled) request a fresh IDR to re-anchor. Both the empty-output and the
                    // survivable-decode-error arms feed it; a decoded frame resets the streak in
                    // `on_decoded`.
                    Ok(None) => {
                        let now = Instant::now();
                        if gate.on_no_output(now)
                            && last_kf_req
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                        {
                            last_kf_req = Some(now);
                            let _ = connector.request_keyframe();
                            tracing::debug!("requested keyframe (decoder produced no output)");
                        }
                    }
                    // Survivable (loss until the next IDR/RFI recovery) — keep feeding.
                    Err(e) => {
                        tracing::debug!(error = %e, "decode error (recovering)");
                        let now = Instant::now();
                        if gate.on_no_output(now)
                            && last_kf_req
                                .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                        {
                            last_kf_req = Some(now);
                            let _ = connector.request_keyframe();
                            tracing::debug!("requested keyframe (decode error recovery)");
                        }
                    }
                }
                // The presenter's verdict: hardware frames can't be displayed (GL converter
                // init failed / dmabuf import rejected) — demote to software here, on the
                // decoder's own thread. Decode succeeds in that state, so the error-streak
                // demotion above never fires.
                if force_software.swap(false, Ordering::Relaxed) {
                    if let Err(e) = decoder.force_software() {
                        break Some(format!("software decoder rebuild: {e}"));
                    }
                }
                // A decode error / VAAPI→software demotion asks for a fresh IDR: the infinite
                // GOP has no periodic keyframe, so a rebuilt/erroring decoder would stay
                // gray/frozen until an unrelated packet drop happened to request one. Route it
                // through the same throttle as loss recovery below.
                if decoder.take_keyframe_request() {
                    let now = Instant::now();
                    gate.arm(now);
                    if last_kf_req
                        .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
                    {
                        last_kf_req = Some(now);
                        let _ = connector.request_keyframe();
                        tracing::debug!("requested keyframe (decoder recovery)");
                    }
                }
            }
            Err(PunktfunkError::NoFrame) => {}
            Err(PunktfunkError::Closed) => break Some("Host ended the session".to_string()),
            Err(e) => break Some(format!("session: {e:?}")),
        }

        // Drain the per-AU host timings (0xCF) non-blockingly and match them to received
        // frames by pts: host = the host's own capture→sent, network = our
        // capture→received minus it (the two tile per frame by construction). An old
        // host never emits any — the deque fills to its cap and the OSD keeps the
        // combined `host+network` stage.
        while let Ok(t) = connector.next_host_timing(Duration::ZERO) {
            // Phase-lock closed loop: the host's applied grid offset rides the 0xCF tail.
            // Log transitions so an on-glass run can watch the controller engage/settle
            // (the Android reporter's parity log).
            if params.phase_lock
                && t.applied_phase_ns.is_some()
                && t.applied_phase_ns != last_applied_phase
            {
                last_applied_phase = t.applied_phase_ns;
                tracing::info!(
                    applied_phase_ns = t.applied_phase_ns.unwrap_or(0),
                    "host phase-lock: applied capture-grid offset"
                );
            }
            if let Some(i) = pending_split.iter().position(|(p, _)| *p == t.pts_ns) {
                let (_, hn_us) = pending_split.remove(i).unwrap();
                host_us_win.push(t.host_us as u64);
                net_us_win.push(hn_us.saturating_sub(t.host_us as u64));
                // Extended 0xCF (T0.1): per-stage host split; the seal/FEC + channel-wait
                // residual is derived so the four stages tile host_us exactly.
                if let Some(s) = t.stages {
                    queue_us_win.push(s.queue_us as u64);
                    enc_us_win.push(s.encode_us as u64);
                    pace_us_win.push(s.pace_us as u64);
                    xfer_us_win.push(
                        (t.host_us as u64).saturating_sub(
                            s.queue_us as u64 + s.encode_us as u64 + s.pace_us as u64,
                        ),
                    );
                }
            }
        }

        // Loss recovery + overdue backstop, folded through the shared gate. A climb in the
        // reassembler's unrecoverable-drop count (`frames_dropped`) means the AUs after the lost one
        // reference a picture we never decoded — the decoder conceals them (gray on RADV) and returns
        // Ok, so a decode-error trigger rarely fires; the gate arms the freeze on the climb instead. An
        // overdue freeze (held a full REANCHOR_FREEZE_MAX with no clean re-anchor — a lost recovery IDR,
        // or a benign reorder that produced no `frames_dropped`) re-asks while it keeps holding: NEVER
        // resume to gray — a genuinely dead stream is the QUIC idle-timeout watchdog's job. Both route
        // the gate's keyframe intent through the shared 100 ms throttle; under infinite GOP the only
        // recovery keyframe is one we request.
        let dropped = connector.frames_dropped();
        let now = Instant::now();
        if gate.poll(dropped, now)
            && last_kf_req.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
        {
            last_kf_req = Some(now);
            let _ = connector.request_keyframe();
            tracing::debug!(
                dropped,
                "requested keyframe (loss recovery / overdue re-anchor)"
            );
        }

        if window_start.elapsed() >= Duration::from_secs(1) {
            // Phase-lock report (~1 Hz, riding the stats window — the reference reporters'
            // cadence): this window's arrival leads before the presenter's latch grid,
            // folded with the SHARED circular statistic (the host controller was tuned
            // against it). Quiet until the presenter has a grid (period 0 — no
            // present-timing samples yet) or the window is thin (< 8 arrivals —
            // `circular_latch` declines). 1 ms uncertainty = Apple/Android parity.
            if params.phase_lock {
                let period = latch_grid.period_ns.load(Ordering::Relaxed);
                let anchor = latch_grid.anchor_ns.load(Ordering::Relaxed);
                if period > 0 && anchor > 0 {
                    let leads_us: Vec<u64> = phase_arrivals
                        .iter()
                        .map(|a| {
                            ((anchor as i128 - *a as i128).rem_euclid(period as i128) / 1000) as u64
                        })
                        .collect();
                    if let Some((lead_ns, coherence)) =
                        punktfunk_core::phase::circular_latch(&leads_us, period as i64)
                    {
                        // Extrapolate the (possibly ~1 s old) anchor to the next latch at
                        // or after now, then express it on the host clock.
                        let (now, p, a) = (now_ns() as i128, period as i128, anchor as i128);
                        let k = ((now - a).max(0) + p - 1) / p;
                        let offset = clock_offset_live.load(Ordering::Relaxed) as i128;
                        connector.report_phase(
                            (a + k * p + offset).max(0) as u64,
                            period.min(u32::MAX as u64) as u32,
                            1_000_000,
                            lead_ns.min(u32::MAX as u64) as u32,
                            coherence,
                        );
                    }
                }
                phase_arrivals.clear();
            }
            let secs = window_start.elapsed().as_secs_f32();
            let (hn_p50, _) = window_percentiles(&mut hostnet_us);
            let (dec_p50, _) = window_percentiles(&mut decode_us);
            // Host/network split — present only when this window matched 0xCF timings.
            let split = !host_us_win.is_empty();
            let (host_p50, _) = window_percentiles(&mut host_us_win);
            let (net_p50, _) = window_percentiles(&mut net_us_win);
            let staged = !queue_us_win.is_empty();
            let (queue_p50, _) = window_percentiles(&mut queue_us_win);
            let (enc_p50, _) = window_percentiles(&mut enc_us_win);
            let (xfer_p50, _) = window_percentiles(&mut xfer_us_win);
            let (pace_p50, _) = window_percentiles(&mut pace_us_win);
            let lost = dropped.saturating_sub(window_dropped) as u32;
            window_dropped = dropped;
            let mic_now = connector.mic_stats();
            let mic_sent = mic_now.sent.saturating_sub(window_mic.sent) as u32;
            let mic_dropped = (mic_now.dropped_full + mic_now.dropped_stale)
                .saturating_sub(window_mic.dropped_full + window_mic.dropped_stale)
                as u32;
            window_mic = mic_now;
            tracing::debug!(
                fps = frames_n,
                hostnet_p50_us = hn_p50,
                host_p50_us = host_p50,
                net_p50_us = net_p50,
                queue_p50_us = queue_p50,
                encode_p50_us = enc_p50,
                xfer_p50_us = xfer_p50,
                pace_p50_us = pace_p50,
                decode_p50_us = dec_p50,
                lost,
                mic_sent,
                mic_dropped,
                total_frames,
                "stream window"
            );
            let _ = ev_tx.try_send(SessionEvent::Stats(Stats {
                fps: frames_n as f32 / secs,
                mbps: bytes_n as f32 * 8.0 / 1e6 / secs,
                host_net_ms: hn_p50 as f32 / 1000.0,
                host_ms: host_p50 as f32 / 1000.0,
                net_ms: net_p50 as f32 / 1000.0,
                split,
                host_queue_ms: queue_p50 as f32 / 1000.0,
                host_encode_ms: enc_p50 as f32 / 1000.0,
                host_xfer_ms: xfer_p50 as f32 / 1000.0,
                host_pace_ms: pace_p50 as f32 / 1000.0,
                staged,
                decode_ms: dec_p50 as f32 / 1000.0,
                lost,
                lost_pct: if lost > 0 {
                    lost as f32 * 100.0 / (frames_n + lost) as f32
                } else {
                    0.0
                },
                mic_sent,
                mic_dropped,
                decoder: dec_path,
                target_kbps: connector.current_bitrate_kbps(),
                auto_rate,
                chroma_444,
                asked_444,
            }));
            window_start = Instant::now();
            frames_n = 0;
            bytes_n = 0;
            hostnet_us.clear();
            decode_us.clear();
            host_us_win.clear();
            net_us_win.clear();
            queue_us_win.clear();
            enc_us_win.clear();
            xfer_us_win.clear();
            pace_us_win.clear();
        }
    };

    tracing::info!(
        total_frames,
        reason = end.as_deref().unwrap_or("user"),
        "session ended"
    );
    stop.store(true, Ordering::SeqCst);
    // The uplink is about to be dropped with the rest of this frame — stop claiming a mute
    // surface, so an embedder still holding the handle through its end path (browse mode
    // returns to the console with it) can't draw a muted mic that no longer exists.
    mic.set_live(false);
    if let Some(t) = audio_thread {
        let _ = t.join(); // exits within its 100 ms pull timeout once `stop` is set
    }
    if let Some(t) = pad_audio_thread {
        let _ = t.join(); // exits within its 10 ms pull timeout once `stop` is set
    }
    if let Some(t) = clipboard_thread {
        let _ = t.join(); // exits within its next_clip wait once `stop` is set
    }
    let _ = ev_tx.send_blocking(SessionEvent::Ended(end));
}

/// The dedicated audio thread: owns the Opus decoder, the PCM scratch, and the PipeWire
/// player, and blocks on `next_audio` (the plane's single consumer — packets land every
/// 5 ms). Decoded chunks are pushed in Vecs recycled from the player's pool, so the
/// steady state allocates nothing. Best-effort like before: any setup failure logs and
/// the session streams video-only. Exits on the stop flag or a closed plane.
fn spawn_audio(
    connector: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    // Decoder + playback are built from the host-RESOLVED channel count (never the
    // request), so an older/clamping host that resolves stereo is decoded as stereo.
    let channels = connector.audio_channels;
    let player = audio::AudioPlayer::spawn(channels as u32)
        .map_err(|e| tracing::warn!(error = %e, "audio disabled"))
        .ok()?;
    let mut dec = AudioDec::new(channels)
        .map_err(|e| tracing::warn!(error = %e, "opus decoder failed — audio disabled"))
        .ok()?;
    std::thread::Builder::new()
        .name("punktfunk-audio-rx".into())
        .spawn(move || {
            let mut pcm = vec![0f32; 5760 * channels as usize]; // scratch: max Opus frame (120 ms) × channels
            let mut gaps = punktfunk_core::audio::AudioGapTracker::new();
            let mut frame_samples = 0usize; // per-channel samples of the last decoded frame — the PLC unit
            while !stop.load(Ordering::SeqCst) {
                match connector.next_audio(Duration::from_millis(100)) {
                    Ok(pkt) => {
                        // Conceal lost packets (a seq gap) with libopus PLC before decoding the one
                        // that arrived: empty input synthesizes `frame_samples` of interpolation per
                        // missing packet — an inaudible fade instead of the click a hard gap makes.
                        for _ in 0..gaps.missing_before(pkt.seq) {
                            let plc = frame_samples * channels as usize;
                            if plc == 0 {
                                break; // no decoded frame yet to size the concealment from
                            }
                            if let Ok(samples) = dec.decode_float(&[], &mut pcm[..plc], false) {
                                let mut buf = player.take_buffer();
                                buf.extend_from_slice(&pcm[..samples * channels as usize]);
                                player.push(buf);
                            }
                        }
                        match dec.decode_float(&pkt.data, &mut pcm, false) {
                            // `samples` is per-channel; the interleaved frame is `samples * channels`.
                            Ok(samples) => {
                                frame_samples = samples;
                                let n = samples * channels as usize;
                                let mut buf = player.take_buffer();
                                buf.extend_from_slice(&pcm[..n]);
                                player.push(buf);
                            }
                            Err(e) => tracing::debug!(error = %e, "opus decode failed"),
                        }
                    }
                    Err(PunktfunkError::NoFrame) => {}
                    Err(_) => break, // plane closed — the session is ending
                }
            }
            tracing::debug!("audio pull thread exited");
        })
        .map_err(|e| tracing::warn!(error = %e, "audio thread failed to start — audio disabled"))
        .ok()
}

/// Parse the `PUNKTFUNK_DEBUG_RECONFIGURE` lab lever: `WxH@HZ:SECS` → request that mode
/// SECS seconds into the stream (e.g. `1280x720@60:5`).
fn parse_debug_reconfigure(s: &str) -> Option<(Mode, Duration)> {
    let (mode_s, secs_s) = s.split_once(':')?;
    let (res, hz) = mode_s.split_once('@')?;
    let (w, h) = res.split_once('x')?;
    let mode = Mode {
        width: w.trim().parse().ok()?,
        height: h.trim().parse().ok()?,
        refresh_hz: hz.trim().parse().ok()?,
    };
    Some((mode, Duration::from_secs(secs_s.trim().parse().ok()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_reconfigure_parses_the_documented_shape() {
        let (mode, delay) = parse_debug_reconfigure("1280x720@60:5").unwrap();
        assert_eq!((mode.width, mode.height, mode.refresh_hz), (1280, 720, 60));
        assert_eq!(delay, Duration::from_secs(5));
    }

    #[test]
    fn debug_reconfigure_rejects_garbage() {
        for bad in [
            "",
            "1280x720",
            "1280x720@60",
            "x@:",
            "ax b@c:d",
            "1280x720@60:x",
        ] {
            assert!(parse_debug_reconfigure(bad).is_none(), "{bad:?} parsed");
        }
    }

    /// The mute is inert until the pump reports a live uplink — a session without a mic must
    /// answer "nothing to mute" rather than latching a mute and drawing the indicator.
    #[test]
    fn mic_mute_is_a_no_op_without_an_uplink() {
        let mic = MicControl::default();
        assert!(!mic.live());
        assert_eq!(mic.toggle(), None, "no uplink, nothing to toggle");
        assert!(!mic.muted(), "and nothing to show");

        mic.set_live(true);
        assert_eq!(mic.toggle(), Some(true));
        assert!(mic.muted());
        // The capture side reads the same flag the toggle writes.
        assert!(mic.flag().load(Ordering::Relaxed));
        assert_eq!(mic.toggle(), Some(false));
        assert!(!mic.muted());

        // A mute that outlives its uplink stops being shown (session end clears `live`).
        assert_eq!(mic.toggle(), Some(true));
        mic.set_live(false);
        assert!(!mic.muted());
        assert_eq!(mic.toggle(), None);
    }
}
