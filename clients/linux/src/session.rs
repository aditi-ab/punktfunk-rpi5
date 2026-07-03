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
    /// Stream the default microphone to the host's virtual mic source.
    pub mic_enabled: bool,
    /// Video decoder preference (Settings; `PUNKTFUNK_DECODER` overrides — see
    /// `video::Decoder::new`).
    pub decoder: String,
    /// Library id for the host to launch this session (`"steam:570"`, from the library
    /// page); `None` = plain desktop session.
    pub launch: Option<String>,
    /// Pinned host fingerprint; `None` = trust on first use (caller persists the observed one).
    pub pin: Option<[u8; 32]>,
    pub identity: (String, String),
    /// How long to wait for the handshake. The normal path uses a short budget; the
    /// "request access" (delegated-approval) path uses a long one, because the host PARKS the
    /// connection until the operator clicks Approve in its console (so this must exceed the
    /// host's approval window — see `PENDING_APPROVAL_WAIT`).
    pub connect_timeout: Duration,
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
    /// p50 `decode` stage: received → decoded, single-clock client-local (ms).
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
}

pub fn start(params: SessionParams) -> SessionHandle {
    let (ev_tx, ev_rx) = async_channel::unbounded();
    // Tiny frame queue, newest wins: force_send displaces the oldest when the UI lags.
    let (frame_tx, frame_rx) = async_channel::bounded(2);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    std::thread::Builder::new()
        .name("punktfunk-session".into())
        .spawn(move || pump(params, ev_tx, frame_tx, stop_w))
        .expect("spawn session thread");
    SessionHandle {
        events: ev_rx,
        frames: frame_rx,
        stop,
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
        0, // video_caps: the Linux client has no 10-bit/HDR present path yet
        params.audio_channels,
        crate::video::decodable_codecs(), // codecs FFmpeg can decode (HEVC/H.264/AV1)
        params.preferred_codec,           // the user's soft codec preference (0 = auto)
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
    let mut decoder = match Decoder::new(codec_id, &params.decoder) {
        Ok(d) => d,
        Err(e) => {
            let _ = ev_tx.send_blocking(SessionEvent::Ended(Some(format!("video decoder: {e}"))));
            return;
        }
    };
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

    let clock_offset = connector.clock_offset_ns;
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
    // Loss recovery: watch the host→client unrecoverable-drop count and ask for an IDR when it climbs.
    let mut last_dropped = connector.frames_dropped();
    // The stats window keeps its own drop cursor — the OSD shows the per-window delta.
    let mut window_dropped = last_dropped;
    let mut last_kf_req: Option<Instant> = None;

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
                match decoder.decode(&frame.data) {
                    Ok(Some(image)) => {
                        total_frames += 1;
                        dec_path = match &image {
                            DecodedImage::Cpu(_) => "software",
                            DecodedImage::Dmabuf(_) => "vaapi",
                        };
                        if total_frames == 1 {
                            let (w, h, path) = match &image {
                                DecodedImage::Cpu(c) => (c.width, c.height, "software"),
                                DecodedImage::Dmabuf(d) => (d.width, d.height, "vaapi-dmabuf"),
                            };
                            tracing::info!(width = w, height = h, path, "first frame decoded");
                        }
                        // The `decoded` point — travels with the frame so the presenter
                        // can measure its `display` stage against it.
                        let decoded_ns = now_ns();
                        // `host+network` stage: received expressed in the host's capture
                        // clock, minus the host-stamped capture pts (clamped (0, 10 s)).
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
                        // `decode` stage: received→decoded, single clock, no skew.
                        decode_us.push(decoded_ns.saturating_sub(received_ns) / 1000);
                        let _ = frame_tx.force_send(DecodedFrame {
                            pts_ns: frame.pts_ns,
                            decoded_ns,
                            image,
                        });
                    }
                    Ok(None) => {}
                    // Survivable (loss until the next IDR/RFI recovery) — keep feeding.
                    Err(e) => tracing::debug!(error = %e, "decode error (recovering)"),
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

        // Loss recovery: under infinite GOP the only recovery keyframe is one we request. The
        // reassembler drops unrecoverable AUs (frames_dropped); the decoder then conceals the
        // reference-missing delta frames that follow and returns Ok, so keying off a decode error
        // rarely fires. Request an IDR when the drop count climbs, throttled — the decode stays
        // wedged for several frames until the IDR lands, so requesting every frame would flood.
        let dropped = connector.frames_dropped();
        if dropped > last_dropped {
            last_dropped = dropped;
            let now = Instant::now();
            if last_kf_req.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                last_kf_req = Some(now);
                let _ = connector.request_keyframe();
                tracing::debug!(dropped, "requested keyframe (loss recovery)");
            }
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
            while !stop.load(Ordering::SeqCst) {
                match connector.next_audio(Duration::from_millis(100)) {
                    Ok(pkt) => match dec.decode_float(&pkt.data, &mut pcm, false) {
                        // `samples` is per-channel; the interleaved frame is `samples * channels`.
                        Ok(samples) => {
                            let n = samples * channels as usize;
                            let mut buf = player.take_buffer();
                            buf.extend_from_slice(&pcm[..n]);
                            player.push(buf);
                        }
                        Err(e) => tracing::debug!(error = %e, "opus decode"),
                    },
                    Err(PunktfunkError::NoFrame) => {}
                    Err(_) => break, // plane closed — the session is ending
                }
            }
            tracing::debug!("audio pull thread exited");
        })
        .map_err(|e| tracing::warn!(error = %e, "audio thread failed to start — audio disabled"))
        .ok()
}
