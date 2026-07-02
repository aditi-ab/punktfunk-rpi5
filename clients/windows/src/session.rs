//! Session controller: one worker thread runs connect → pump (video pull + decode, audio
//! pull + Opus decode, stats), feeding the UI over channels. The UI keeps the
//! `Arc<NativeClient>` from the `Connected` event for direct input sends (no extra hop on
//! the input path) — `NativeClient` is `Sync`, planes stay one-consumer-per-thread:
//! video+audio here, rumble+hidout on the gamepad thread.
//!
//! Ported from the GTK Linux client; the platform-specific pieces are the video decoder
//! (software-only here) and the audio backend (WASAPI). The pump body is identical.

use crate::audio;
use crate::video::{DecodedFrame, Decoder, DecoderPref};
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
    /// Stream the default microphone to the host's virtual mic source.
    pub mic_enabled: bool,
    /// Advertise 10-bit + HDR10 so the host may upgrade HDR content to a Main10/PQ stream.
    pub hdr_enabled: bool,
    /// Which video decode backend to use (auto/hardware/software).
    pub decoder: DecoderPref,
    /// The user's preferred video codec (a `quic::CODEC_*` bit, `0` = auto). Soft — the host honors
    /// it when it can emit it, else falls back; the resolved codec drives the decoder.
    pub preferred_codec: u8,
    /// Pinned host fingerprint; `None` = trust on first use (caller persists the observed one).
    pub pin: Option<[u8; 32]>,
    pub identity: (String, String),
    /// How long to wait for the handshake. The normal path uses a short budget; the
    /// "request access" (delegated-approval) path uses a long one, because the host PARKS the
    /// connection until the operator clicks Approve in its console (so this must exceed the
    /// host's approval window — see `PENDING_APPROVAL_WAIT`).
    pub connect_timeout: Duration,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub fps: f32,
    pub mbps: f32,
    pub decode_ms: f32,
    /// Median capture→decoded latency over the last window (host-clock corrected).
    pub latency_ms: f32,
    /// True when decoding on the GPU (D3D11VA zero-copy) vs. CPU (software).
    pub hardware: bool,
    /// True when the stream is BT.2020 PQ HDR10 (last decoded frame).
    pub hdr: bool,
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

/// Blocking speed-test probe (the GUI's per-host "Test" and the `--headless --speed-test` CLI):
/// a minimal identified connect (720p60 — the host builds a virtual output, but nothing is
/// decoded), then `request_probe` (a 2 s burst up to the host's 3 Gbps ceiling) polled to
/// completion. Run on a worker thread.
pub fn run_speed_probe(
    addr: &str,
    port: u16,
    fp_hex: Option<&str>,
    identity: (String, String),
) -> Result<punktfunk_core::client::ProbeOutcome, String> {
    // Pin the saved/advertised fingerprint when we have one; a manual host measures over TOFU.
    let pin = fp_hex.and_then(crate::trust::parse_hex32);
    let c = NativeClient::connect(
        addr,
        port,
        Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        },
        CompositorPref::Auto,
        GamepadPref::Auto,
        0, // bitrate_kbps: host default
        0, // video_caps: probe connect, nothing is decoded
        2, // audio_channels: stereo baseline
        crate::video::decodable_codecs(),
        0,    // preferred_codec: no preference
        None, // launch: no game
        pin,
        Some(identity),
        Duration::from_secs(15),
    )
    .map_err(|e| format!("connect: {e:?}"))?;
    c.request_probe(3_000_000, 2_000)
        .map_err(|e| format!("probe: {e:?}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if c.probe_result().done {
            // Let the last UDP shards land before tearing down.
            std::thread::sleep(Duration::from_millis(400));
            return Ok(c.probe_result());
        }
        if Instant::now() > deadline {
            return Err("probe timed out".to_string());
        }
    }
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

fn now_ns() -> u64 {
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
        // Advertise 10-bit + HDR10 only when the user enabled HDR AND a display is actually in HDR
        // mode: the host then upgrades HDR content to a Main10/PQ stream (its own 10-bit gate still
        // applies). On an SDR display we advertise `0` so the host sends a proper 8-bit BT.709 stream
        // rather than PQ the panel would mis-tone-map (washed-out/dark). An HDR display self-tone-maps
        // from the mastering metadata we apply. The presenter handles BT.2020 PQ frames (P010 / X2BGR10).
        if params.hdr_enabled && crate::present::display_supports_hdr() {
            punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR
        } else {
            if params.hdr_enabled {
                tracing::info!(
                    "HDR enabled in settings but no HDR display detected — requesting SDR"
                );
            }
            0
        },
        params.audio_channels,
        crate::video::decodable_codecs(), // codecs FFmpeg can decode (HEVC/H.264/AV1)
        params.preferred_codec,           // the user's soft codec preference (0 = auto)
        None,                             // launch: the Windows client has no library picker yet
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

    // Build the decoder for the codec the host resolved (never assume HEVC).
    let codec_id = crate::video::ffmpeg_codec_id(connector.codec);
    tracing::info!(
        ?codec_id,
        welcome_codec = connector.codec,
        "negotiated video codec"
    );
    let mut decoder = match Decoder::new(params.decoder, codec_id) {
        Ok(d) => d,
        Err(e) => {
            let _ = ev_tx.send_blocking(SessionEvent::Ended(Some(format!("video decoder: {e}"))));
            return;
        }
    };
    let mut hardware = decoder.is_hardware();
    let mut hdr = false;
    // Audio is best-effort: a session without it still streams. Gamepads are the
    // app-lifetime service's job (the UI attaches it on Connected). Build the decoder + playback
    // from the host-RESOLVED channel count (never the request), so an older/clamping host that
    // resolves stereo is decoded as stereo.
    let channels = connector.audio_channels;
    let player = audio::AudioPlayer::spawn(channels)
        .map_err(|e| tracing::warn!(error = %e, "audio disabled"))
        .ok();
    let mut opus_dec = AudioDec::new(channels)
        .map_err(|e| tracing::warn!(error = %e, "opus decoder failed — audio disabled"))
        .ok();
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
    let mut decode_us_sum = 0u64;
    let mut lat_us: Vec<u64> = Vec::with_capacity(256);
    let mut pcm = vec![0f32; 5760 * channels as usize]; // scratch: max Opus frame (120 ms) × channels
                                                        // Loss recovery: watch the host→client unrecoverable-drop count and ask for an IDR when it climbs.
    let mut last_dropped = connector.frames_dropped();
    let mut last_kf_req: Option<Instant> = None;

    let end: Option<String> = loop {
        if stop.load(Ordering::SeqCst) {
            break None;
        }
        match connector.next_frame(Duration::from_millis(4)) {
            Ok(frame) => {
                let t0 = Instant::now();
                match decoder.decode(&frame.data) {
                    Ok(Some(decoded)) => {
                        total_frames += 1;
                        hdr = decoded.hdr();
                        // The backend can demote D3D11VA → software mid-session on a hardware error.
                        hardware = decoder.is_hardware();
                        if total_frames == 1 {
                            let (w, h) = decoded.dims();
                            tracing::info!(
                                width = w,
                                height = h,
                                path = if hardware { "d3d11va" } else { "software" },
                                hdr,
                                "first frame decoded"
                            );
                        }
                        // Latency: our wall clock expressed in the host's capture clock,
                        // minus the host-stamped capture pts (same math as client-rs).
                        let lat = (now_ns() as i128 + clock_offset as i128 - frame.pts_ns as i128)
                            .max(0) as u64;
                        if lat > 0 && lat < 10_000_000_000 {
                            lat_us.push(lat / 1000);
                        }
                        decode_us_sum += t0.elapsed().as_micros() as u64;
                        frames_n += 1;
                        bytes_n += frame.data.len() as u64;
                        let _ = frame_tx.force_send(decoded);
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

        // Loss recovery: under infinite GOP the only recovery keyframe is one we request. The
        // reassembler drops unrecoverable AUs (frames_dropped); the decoder conceals the
        // reference-missing delta frames that follow and returns Ok, so keying off a decode error
        // rarely fires. Request an IDR when the drop count climbs, throttled.
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

        // Drain audio between frames (packets land every 5 ms; the queue holds 320 ms).
        while let Ok(pkt) = connector.next_audio(Duration::ZERO) {
            if let (Some(player), Some(dec)) = (&player, opus_dec.as_mut()) {
                match dec.decode_float(&pkt.data, &mut pcm, false) {
                    // `samples` is per-channel; the interleaved frame is `samples * channels`.
                    Ok(samples) => player.push(pcm[..samples * channels as usize].to_vec()),
                    Err(e) => tracing::debug!(error = %e, "opus decode"),
                }
            }
        }

        // Drain the HDR static-metadata plane (0xCE): the source's real mastering display + content
        // light level. Stash the latest for the UI-thread presenter to apply via SetHDRMetaData —
        // this pump is the sole consumer of the plane. Rare (start + on change/keyframe).
        while let Ok(meta) = connector.next_hdr_meta(Duration::ZERO) {
            *crate::present::LATEST_HDR_META.lock().unwrap() = Some(meta);
        }

        if window_start.elapsed() >= Duration::from_secs(1) {
            let secs = window_start.elapsed().as_secs_f32();
            lat_us.sort_unstable();
            let p50 = lat_us.get(lat_us.len() / 2).copied().unwrap_or(0);
            tracing::debug!(
                fps = frames_n,
                lat_p50_us = p50,
                total_frames,
                "stream window"
            );
            let _ = ev_tx.try_send(SessionEvent::Stats(Stats {
                fps: frames_n as f32 / secs,
                mbps: bytes_n as f32 * 8.0 / 1e6 / secs,
                decode_ms: if frames_n > 0 {
                    decode_us_sum as f32 / frames_n as f32 / 1000.0
                } else {
                    0.0
                },
                latency_ms: p50 as f32 / 1000.0,
                hardware,
                hdr,
            }));
            window_start = Instant::now();
            frames_n = 0;
            bytes_n = 0;
            decode_us_sum = 0;
            lat_us.clear();
        }
    };

    tracing::info!(
        total_frames,
        reason = end.as_deref().unwrap_or("user"),
        "session ended"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = ev_tx.send_blocking(SessionEvent::Ended(end));
}
