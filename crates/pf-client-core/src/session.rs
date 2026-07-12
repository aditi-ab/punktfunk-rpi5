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
    /// p50 `decode` stage: received → decode COMPLETE, single-clock client-local (ms).
    /// Hardware paths measure GPU completion via the frame's timeline fence (an async
    /// decoder's submission returning in ~0.1 ms is not "decoded"); software measures
    /// the synchronous CPU decode.
    pub decode_ms: f32,
    /// Unrecoverable network frame drops this window, and their share of
    /// received+lost (%). The OSD renders the counter line only when nonzero.
    pub lost: u32,
    pub lost_pct: f32,
    /// The decode path frames actually took this window (`"vaapi"`/`"software"`, empty
    /// until the first frame) — the OSD's trailing tag; tracks a mid-session fallback.
    pub decoder: &'static str,
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

pub struct SessionHandle {
    pub events: async_channel::Receiver<SessionEvent>,
    pub frames: async_channel::Receiver<DecodedFrame>,
    pub stop: Arc<AtomicBool>,
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
    let thread = std::thread::Builder::new()
        .name("punktfunk-session".into())
        .spawn(move || pump(params, ev_tx, frame_tx, stop_w))
        .expect("spawn session thread");
    SessionHandle {
        events: ev_rx,
        frames: frame_rx,
        stop,
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
) {
    let connector = match NativeClient::connect(
        &params.host,
        params.port,
        params.mode,
        params.compositor,
        params.gamepad,
        params.bitrate_kbps,
        params.video_caps,
        params.audio_channels,
        crate::video::decodable_codecs(), // codecs FFmpeg can decode (HEVC/H.264/AV1)
        params.preferred_codec,           // the user's soft codec preference (0 = auto)
        // This display's HDR volume → the host's virtual-display EDID. The env hatch wins so an
        // A/B run can pin an exact peak (PUNKTFUNK_CLIENT_PEAK_NITS=600).
        punktfunk_core::client::display_hdr_env_override().or(params.display_hdr),
        params.launch.clone(),
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
    tracing::info!(
        ?codec_id,
        welcome_codec = connector.codec,
        "negotiated video codec"
    );
    let mut decoder = match Decoder::new(codec_id, &params.decoder, params.vulkan.as_ref()) {
        Ok(d) => d,
        Err(e) => {
            let _ = ev_tx.send_blocking(SessionEvent::Ended(Some(format!("video decoder: {e}"))));
            return;
        }
    };
    let force_software = params.force_software.clone();
    // Audio is best-effort: a session without it still streams. Gamepads are the
    // app-lifetime service's job (the UI attaches it on Connected). Audio runs on its own
    // thread (one puller per plane), blocking on the audio queue like the Apple client.
    let audio_thread = spawn_audio(connector.clone(), stop.clone());
    let _mic = params
        .mic_enabled
        .then(|| {
            audio::MicStreamer::spawn(connector.clone())
                .map_err(|e| tracing::warn!(error = %e, "mic uplink disabled"))
                .ok()
        })
        .flatten();

    // Live host↔client clock offset: loaded per frame (Relaxed) so mid-stream re-syncs (an NTP
    // step, drift) keep the capture-clock latency stats honest — never cached at session start.
    let clock_offset_live = connector.clock_offset_shared();
    let mut total_frames = 0u64;
    let mut window_start = Instant::now();
    let mut frames_n = 0u32;
    let mut bytes_n = 0u64;
    // Stage windows (µs samples): `host+network` = capture→received (host-clock
    // corrected), `decode` = received→decoded (client-local). p50 per 1 s window.
    let mut hostnet_us: Vec<u64> = Vec::with_capacity(256);
    let mut decode_us: Vec<u64> = Vec::with_capacity(256);
    // Host/network split (Phase 2): frames awaiting their per-AU 0xCF host timing,
    // correlated by pts_ns. Bounded — an old host never sends any, so entries just age out.
    let mut pending_split: std::collections::VecDeque<(u64, u64)> =
        std::collections::VecDeque::with_capacity(PENDING_SPLIT_CAP);
    let mut host_us_win: Vec<u64> = Vec::with_capacity(256);
    let mut net_us_win: Vec<u64> = Vec::with_capacity(256);
    // What actually decoded the last frame — a VAAPI failure demotes mid-session, so
    // this is read off each frame's image variant rather than fixed at startup.
    let mut dec_path: &'static str = "";
    // The stats window keeps its own drop cursor — the OSD shows the per-window delta.
    let mut window_dropped = connector.frames_dropped();
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

    let end: Option<String> = loop {
        if stop.load(Ordering::SeqCst) {
            break None;
        }
        // 20 ms wait: audio has its own thread now, so this only bounds stop-flag
        // responsiveness and the per-iteration keyframe-recovery check (a frame arrives
        // every ~8–16 ms at 60–120 Hz anyway, so this rarely times out mid-stream).
        match connector.next_frame(Duration::from_millis(20)) {
            Ok(frame) => {
                // The `received` point: AU fully reassembled, in hand, before decode.
                let received_ns = now_ns();
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
                match decoder.decode(&frame.data) {
                    Ok(Some(image)) => {
                        // Fold this decoded frame through the shared freeze gate: it reads the AU's
                        // re-anchor wire flags (FLAG_SOF IDR marker / RECOVERY_ANCHOR / RECOVERY_POINT),
                        // takes `image.is_keyframe()` as the ffmpeg keyframe belt, applies the two-mark
                        // rule + the mark-patience backstop, clears the no-output streak, and returns
                        // whether to present this frame or withhold it as a post-loss concealment.
                        let present = gate
                            .on_decoded(frame.flags, image.is_keyframe(), Instant::now())
                            == GateVerdict::Present;
                        total_frames += 1;
                        dec_path = match &image {
                            DecodedImage::Cpu(_) => "software",
                            #[cfg(target_os = "linux")]
                            DecodedImage::Dmabuf(_) => "vaapi",
                            DecodedImage::VkFrame(_) => "vulkan",
                            #[cfg(windows)]
                            DecodedImage::D3d11(_) => "d3d11va",
                        };
                        if total_frames == 1 {
                            let (w, h, path) = match &image {
                                DecodedImage::Cpu(c) => (c.width, c.height, "software"),
                                #[cfg(target_os = "linux")]
                                DecodedImage::Dmabuf(d) => (d.width, d.height, "vaapi-dmabuf"),
                                DecodedImage::VkFrame(v) => (v.width, v.height, "vulkan-video"),
                                #[cfg(windows)]
                                DecodedImage::D3d11(d) => (d.width, d.height, "d3d11va"),
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
            if let Some(i) = pending_split.iter().position(|(p, _)| *p == t.pts_ns) {
                let (_, hn_us) = pending_split.remove(i).unwrap();
                host_us_win.push(t.host_us as u64);
                net_us_win.push(hn_us.saturating_sub(t.host_us as u64));
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
            tracing::debug!(dropped, "requested keyframe (loss recovery / overdue re-anchor)");
        }

        if window_start.elapsed() >= Duration::from_secs(1) {
            let secs = window_start.elapsed().as_secs_f32();
            let (hn_p50, _) = window_percentiles(&mut hostnet_us);
            let (dec_p50, _) = window_percentiles(&mut decode_us);
            // Host/network split — present only when this window matched 0xCF timings.
            let split = !host_us_win.is_empty();
            let (host_p50, _) = window_percentiles(&mut host_us_win);
            let (net_p50, _) = window_percentiles(&mut net_us_win);
            let lost = dropped.saturating_sub(window_dropped) as u32;
            window_dropped = dropped;
            tracing::debug!(
                fps = frames_n,
                hostnet_p50_us = hn_p50,
                host_p50_us = host_p50,
                net_p50_us = net_p50,
                decode_p50_us = dec_p50,
                lost,
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
                decode_ms: dec_p50 as f32 / 1000.0,
                lost,
                lost_pct: if lost > 0 {
                    lost as f32 * 100.0 / (frames_n + lost) as f32
                } else {
                    0.0
                },
                decoder: dec_path,
            }));
            window_start = Instant::now();
            frames_n = 0;
            bytes_n = 0;
            hostnet_us.clear();
            decode_us.clear();
            host_us_win.clear();
            net_us_win.clear();
        }
    };

    tracing::info!(
        total_frames,
        reason = end.as_deref().unwrap_or("user"),
        "session ended"
    );
    stop.store(true, Ordering::SeqCst);
    if let Some(t) = audio_thread {
        let _ = t.join(); // exits within its 100 ms pull timeout once `stop` is set
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
                            Err(e) => tracing::debug!(error = %e, "opus decode"),
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
