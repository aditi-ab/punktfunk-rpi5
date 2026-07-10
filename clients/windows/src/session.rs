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
    /// AUs received (reassembled) per second — actual-elapsed-time denominator.
    pub fps: f32,
    /// Received payload goodput (excludes FEC overhead).
    pub mbps: f32,
    /// `decode` stage p50 over the last 1 s window: received → decoded, client-local clock.
    pub decode_ms: f32,
    /// `host+network` stage p50 over the last 1 s window: capture (`pts_ns`) → received,
    /// host-clock corrected via `clock_offset_ns`.
    pub hostnet_ms: f32,
    /// `host` stage p50 (host capture→sent, from the per-AU 0xCF host-timing plane). Valid only
    /// when `split` — an old host emits no 0xCF and the HUD keeps the combined stage.
    pub host_ms: f32,
    /// `network` stage p50 (`hostnet − host`, tiled per frame before taking the percentile).
    /// Valid only when `split`.
    pub net_ms: f32,
    /// True when any 0xCF host timings matched received AUs this window — the HUD then renders
    /// `host + network` instead of the combined `host+network` term.
    pub split: bool,
    /// True when `clock_offset_ns == 0` (host didn't answer the skew handshake / same host) —
    /// the HUD appends `(same-host clock)` to the end-to-end line.
    pub same_host: bool,
    /// True when decoding on the GPU (D3D11VA) vs. CPU (software).
    pub hardware: bool,
    /// True when the stream is BT.2020 PQ HDR10 (last decoded frame).
    pub hdr: bool,
    /// The negotiated wire codec (a `quic::CODEC_*` bit) — the HUD's codec chip.
    pub codec: u8,
    /// Frames lost to unrecoverable network drops since session start (reassembler count; each
    /// triggers a keyframe re-request).
    pub dropped: u64,
    /// Seconds since the stream started.
    pub uptime_secs: u32,
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

/// Per-frame measurement points carried with a decoded frame to the render thread: the host
/// capture clock (`pts_ns`) and our local `decoded` stamp (wall-clock ns). Post-`Present()` the
/// render thread derives the `display` stage (displayed − decoded, single-clock) and the
/// end-to-end headline (displayed + clock_offset − pts) from them.
#[derive(Clone, Copy)]
pub struct FrameTimes {
    pub pts_ns: u64,
    pub decoded_ns: u64,
}

/// Decoded frames + their measurement points, session pump → render thread (crossbeam so that
/// thread can block with a timeout — async-channel has no `recv_timeout`).
pub type FrameRx = crossbeam_channel::Receiver<(DecodedFrame, FrameTimes)>;

pub struct SessionHandle {
    pub events: async_channel::Receiver<SessionEvent>,
    pub frames: FrameRx,
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
        None, // display_hdr: probe connect, nothing presents
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
    // Tiny frame queue, newest wins: the pump displaces the oldest when the renderer lags (it
    // keeps a Receiver clone for exactly that).
    let (frame_tx, frame_rx) = crossbeam_channel::bounded(2);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let frame_rx_pump = frame_rx.clone();
    std::thread::Builder::new()
        .name("punktfunk-session".into())
        .spawn(move || pump(params, ev_tx, frame_tx, frame_rx_pump, stop_w))
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
    frame_tx: crossbeam_channel::Sender<(DecodedFrame, FrameTimes)>,
    frame_rx: FrameRx,
    stop: Arc<AtomicBool>,
) {
    // Advertise 10-bit + HDR10 only when the user enabled HDR AND a display is actually in HDR
    // mode: the host then upgrades HDR content to a Main10/PQ stream (its own 10-bit gate still
    // applies). On an SDR display we advertise `0` so the host sends a proper 8-bit BT.709 stream
    // rather than PQ the panel would mis-tone-map (washed-out/dark). The presenter handles BT.2020
    // PQ frames (P010 / X2BGR10).
    let hdr_active = params.hdr_enabled && crate::present::display_supports_hdr();
    if params.hdr_enabled && !hdr_active {
        tracing::info!("HDR enabled in settings but no HDR display detected — requesting SDR");
    }
    // With HDR active, also report the panel's real colour volume (GetDesc1): the host writes it
    // into its virtual display's EDID, so host apps tone-map to THIS panel and the PQ stream
    // arrives already inside its volume — the client presents it untouched.
    // PUNKTFUNK_CLIENT_PEAK_NITS pins a synthetic volume for A/B runs.
    let display_hdr = if hdr_active {
        let vol = punktfunk_core::client::display_hdr_env_override()
            .or_else(crate::present::display_hdr_volume);
        if let Some(m) = vol {
            tracing::info!(
                max_nits = m.max_display_mastering_luminance / 10_000,
                min_millinits = m.min_display_mastering_luminance / 10,
                max_fall = m.max_fall,
                "advertising this display's HDR volume to the host"
            );
        }
        vol
    } else {
        None
    };
    let connector = match NativeClient::connect(
        &params.host,
        params.port,
        params.mode,
        params.compositor,
        params.gamepad,
        params.bitrate_kbps,
        if hdr_active {
            punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR
        } else {
            0
        },
        params.audio_channels,
        crate::video::decodable_codecs(), // codecs FFmpeg can decode (HEVC/H.264/AV1)
        params.preferred_codec,           // the user's soft codec preference (0 = auto)
        display_hdr,
        None, // launch: the Windows client has no library picker yet
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

    // Force an immediate IDR (with in-band parameter sets) rather than waiting for the host's own
    // first keyframe — under infinite GOP a late/missed IDR means the decoder sits on
    // "PPS id out of range" (a black screen) until one arrives.
    let _ = connector.request_keyframe();

    // Live host↔client clock offset: loaded per use (Relaxed) so mid-stream re-syncs (an NTP
    // step, drift) keep the capture-clock latency stats honest — never cached at session start.
    let clock_offset_live = connector.clock_offset_shared();
    let mut total_frames = 0u64;
    let session_start = Instant::now();
    let mut window_start = Instant::now();
    let mut frames_n = 0u32;
    let mut bytes_n = 0u64;
    // 1 s tumbling stage windows (spec: design/stats-unification.md — percentiles, never means).
    let mut hostnet_us: Vec<u64> = Vec::with_capacity(256);
    let mut decode_us: Vec<u64> = Vec::with_capacity(256);
    // Host/network split (Phase 2): received AUs awaiting their 0xCF host timing, `(pts_ns,
    // hostnet_us)`, matched as the datagrams arrive. Bounded — an old host never sends any.
    let mut pending_split: std::collections::VecDeque<(u64, u64)> =
        std::collections::VecDeque::with_capacity(256);
    let mut host_us_w: Vec<u64> = Vec::with_capacity(256);
    let mut net_us_w: Vec<u64> = Vec::with_capacity(256);
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
                // The `received` point: AU fully reassembled, handed to us, before decode.
                let received_ns = now_ns();
                // fps = AUs received per second, Mb/s = received goodput (spec: counted at the
                // received point, not the decoded one).
                frames_n += 1;
                bytes_n += frame.data.len() as u64;
                // `host+network` stage: capture → received, host-clock corrected. Clamped (0, 10 s).
                let clock_offset = clock_offset_live.load(Ordering::Relaxed);
                let hostnet = (received_ns as i128 + clock_offset as i128 - frame.pts_ns as i128)
                    .max(0) as u64;
                if hostnet > 0 && hostnet < 10_000_000_000 {
                    hostnet_us.push(hostnet / 1000);
                    // Remember this AU for the 0xCF match below (host/network split).
                    pending_split.push_back((frame.pts_ns, hostnet / 1000));
                    if pending_split.len() > 256 {
                        pending_split.pop_front();
                    }
                }
                // A D3D11VA→software demotion (see `Decoder::decode`) starts a FRESH decoder that
                // has none of the stream's parameter sets; under infinite GOP it would sit on
                // "PPS id out of range" forever. Detect the transition and force a new IDR so the
                // rebuilt decoder resynchronizes immediately.
                let was_hw = decoder.is_hardware();
                let decoded = decoder.decode(&frame.data);
                if was_hw && !decoder.is_hardware() {
                    tracing::info!("decoder demoted to software — requesting keyframe to resync");
                    let _ = connector.request_keyframe();
                }
                match decoded {
                    Ok(Some(decoded)) => {
                        // The `decoded` point: decoder output frame available.
                        let decoded_ns = now_ns();
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
                        // `decode` stage: received → decoded, single-clock client-local.
                        decode_us.push(decoded_ns.saturating_sub(received_ns) / 1000);
                        // Newest wins: displace the oldest queued frame when the renderer lags.
                        if let Err(crossbeam_channel::TrySendError::Full(item)) =
                            frame_tx.try_send((
                                decoded,
                                FrameTimes {
                                    pts_ns: frame.pts_ns,
                                    decoded_ns,
                                },
                            ))
                        {
                            let _ = frame_rx.try_recv();
                            let _ = frame_tx.try_send(item);
                        }
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

        // Drain the per-AU host-timing plane (0xCF) and match by pts: `host` = the host's own
        // capture→sent, `network` = our capture→received minus it — the two tile per frame
        // (design/stats-unification.md Phase 2). An old host never emits any; `split` stays false
        // and the HUD keeps the combined `host+network` stage.
        while let Ok(t) = connector.next_host_timing(Duration::ZERO) {
            if let Some(i) = pending_split.iter().position(|(p, _)| *p == t.pts_ns) {
                let (_, hn_us) = pending_split.remove(i).unwrap();
                host_us_w.push(t.host_us as u64);
                net_us_w.push(hn_us.saturating_sub(t.host_us as u64));
            }
        }

        if window_start.elapsed() >= Duration::from_secs(1) {
            let secs = window_start.elapsed().as_secs_f32();
            hostnet_us.sort_unstable();
            decode_us.sort_unstable();
            host_us_w.sort_unstable();
            net_us_w.sort_unstable();
            let p50 = |v: &[u64]| v.get(v.len() / 2).copied().unwrap_or(0);
            let (hostnet_p50, decode_p50) = (p50(&hostnet_us), p50(&decode_us));
            let (host_p50, net_p50) = (p50(&host_us_w), p50(&net_us_w));
            let split = !host_us_w.is_empty();
            tracing::debug!(
                fps = frames_n,
                hostnet_p50_us = hostnet_p50,
                host_p50_us = host_p50,
                net_p50_us = net_p50,
                split,
                decode_p50_us = decode_p50,
                total_frames,
                "stream window"
            );
            let _ = ev_tx.try_send(SessionEvent::Stats(Stats {
                fps: frames_n as f32 / secs,
                mbps: bytes_n as f32 * 8.0 / 1e6 / secs,
                decode_ms: decode_p50 as f32 / 1000.0,
                hostnet_ms: hostnet_p50 as f32 / 1000.0,
                host_ms: host_p50 as f32 / 1000.0,
                net_ms: net_p50 as f32 / 1000.0,
                split,
                same_host: clock_offset_live.load(Ordering::Relaxed) == 0,
                hardware,
                hdr,
                codec: connector.codec,
                dropped: last_dropped,
                uptime_secs: session_start.elapsed().as_secs() as u32,
            }));
            window_start = Instant::now();
            frames_n = 0;
            bytes_n = 0;
            hostnet_us.clear();
            decode_us.clear();
            host_us_w.clear();
            net_us_w.clear();
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
