//! The audio data plane (UDP 48000). On RTSP PLAY we learn the client's audio endpoint from
//! its port-learning ping, capture the default-sink monitor at the negotiated channel count,
//! Opus-encode fixed frames (stereo: plain Opus; 5.1/7.1: libopus *multistream*), and send
//! each as a GameStream RTP audio packet.
//!
//! Wire format (moonlight-common-c `AudioStream.c`/`RtpAudioQueue.c`, verified verbatim
//! 2026-06-10): a 12-byte big-endian `RTP_PACKET` (`packetType = 97`, `sequenceNumber++`,
//! `timestamp += packetDuration`, `ssrc = 0`) followed by the AES-128-CBC-encrypted Opus
//! payload. Like the control stream, modern Moonlight always AES-CBC-decrypts audio (it
//! reports "Failed to decrypt audio packet" on plaintext), so we encrypt the payload under
//! the `/launch` `rikey` with a per-packet IV `BE32(rikeyid + seq)` (PKCS7 padding, RTP
//! header left in the clear).
//!
//! Surround sessions additionally carry Sunshine-style audio FEC: every aligned block of 4
//! data packets is followed by 2 Reed–Solomon parity packets (`packetType = 127`, an
//! `AUDIO_FEC_HEADER` after the RTP header). FEC is opportunistic on the client — in-order
//! data packets are consumed immediately and missing parity only costs loss recovery — so
//! the validated stereo path stays byte-identical (data packets only, exactly as before).

#[cfg(any(target_os = "linux", target_os = "windows", test))]
use crate::audio::SAMPLE_RATE;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use {
    super::AUDIO_PORT,
    crate::audio::{self, AudioCapturer},
    anyhow::{Context, Result},
    cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit},
    std::net::UdpSocket,
    std::sync::atomic::{AtomicBool, Ordering},
    std::sync::Arc,
    std::time::{Duration, Instant},
};

#[cfg(any(target_os = "linux", target_os = "windows"))]
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// RTP payload types (moonlight-common-c `RtpAudioQueue.c`: `RTP_PAYLOAD_TYPE_AUDIO  97`,
/// `RTP_PAYLOAD_TYPE_FEC  127`).
const AUDIO_PACKET_TYPE: u8 = 97;
const AUDIO_FEC_PACKET_TYPE: u8 = 127;

/// Audio FEC geometry (moonlight-common-c `RtpAudioQueue.h`: `RTPA_DATA_SHARDS 4`,
/// `RTPA_FEC_SHARDS 2`). Blocks are aligned: the client synthesizes the block base as
/// `(seq / 4) * 4`, so parity always covers data seqs `[base, base+4)`.
const FEC_DATA_SHARDS: usize = 4;
const FEC_PARITY_SHARDS: usize = 2;
/// The RS(4,2) parity rows both ends hardcode (Sunshine `stream.cpp` and moonlight-common-c
/// `RtpAudioQueue.c` patch their nanors context with these same bytes — the OpenFEC matrix
/// NVIDIA used, NOT nanors' own Cauchy matrix). nanors `reed_solomon_encode` (gemm over the
/// row-major `m×k` matrix, GF(2⁸) poly 0x11d) computes
/// `parity[j] = XOR_i gfmul(M[j][i], data[i])` — replicated in [`audio_parity`].
const FEC_MATRIX: [[u8; FEC_DATA_SHARDS]; FEC_PARITY_SHARDS] =
    [[0x77, 0x40, 0x38, 0x0e], [0xc7, 0xa7, 0x0d, 0x6c]];

/// Per-session audio parameters from the RTSP ANNOUNCE (`x-nv-audio.surround.numChannels`,
/// `x-nv-audio.surround.AudioQuality`, `x-nv-aqos.packetDuration` — the attribute set
/// moonlight-common-c `SdpGenerator.c` emits). Defaults match Moonlight's: stereo, normal
/// quality, 5 ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioParams {
    /// Negotiated channel count: 2, 6 (5.1) or 8 (7.1).
    pub channels: u8,
    /// `AudioQuality == 1` — uncoupled high-bitrate multistream (client opted in, only
    /// offered when our DESCRIBE advertises a second surround-params line).
    pub high_quality: bool,
    /// Opus frame duration in ms; Moonlight sends 5 (default) or 10 (slow decoder/link).
    pub packet_duration_ms: u8,
}

impl Default for AudioParams {
    fn default() -> Self {
        AudioParams {
            channels: 2,
            high_quality: false,
            packet_duration_ms: 5,
        }
    }
}

// The Opus surround layout table (channel order FL FR FC LFE RL RR [SL SR], identity mapping,
// Sunshine's per-config bitrates) now lives in `punktfunk_core::audio`, shared with the native
// `punktfunk/1` path and every client decoder. Re-export the pieces the GameStream module + its
// RTSP SDP (`rtsp.rs`) reference; the GFE-specific `surround_params` SDP rotation stays below.
pub use punktfunk_core::audio::{
    OpusLayout, LAYOUT_51, LAYOUT_51_HQ, LAYOUT_71, LAYOUT_71_HQ, LAYOUT_STEREO,
};

/// Pick the encoder layout for the negotiated session parameters. Thin wrapper over the shared
/// [`punktfunk_core::audio::layout_for`] keyed on this module's [`AudioParams`] (unknown channel
/// counts fall back to stereo; the client can only request 2/6/8 — `AUDIO_CONFIGURATION_*` in
/// Limelight.h).
pub fn layout_for(params: &AudioParams) -> &'static OpusLayout {
    punktfunk_core::audio::layout_for(params.channels, params.high_quality)
}

/// The `a=fmtp:97 surround-params=` digit string for a layout: channelCount, streams,
/// coupledStreams, then one mapping digit per channel.
///
/// moonlight-common-c (`RtspConnection.c parseOpusConfigurations`, verified verbatim
/// 2026-06-10) post-processes the NORMAL-quality mapping it parses — GFE advertised the
/// FL FR C RL RR SL SR LFE channel order, the client wants FL FR C LFE RL RR SL SR — by
/// moving the *last* digit to index 3 and sliding the rest up. We therefore pre-rotate the
/// encoder mapping (`adv[3..ch-1] = enc[4..ch]`, `adv[ch-1] = enc[3]`) so the client's
/// post-swap decoder mapping equals our encoder mapping exactly. The HIGH-quality string
/// (the second surround-params match for a channel count) is used verbatim — no swap.
///
/// NB: Sunshine pre-rotates only indices `[3, 6)` (`audio::MAX_STREAM_CONFIG`, a config
/// count, not a channel index), which leaves 7.1 LFE/SL/SR scrambled after the client's
/// swap; we rotate over `[3, channels)` so 7.1 round-trips correctly.
pub fn surround_params(layout: &OpusLayout, high_quality: bool) -> String {
    let ch = layout.channels as usize;
    let mut mapping = layout.mapping.to_vec();
    if !high_quality && ch > 2 {
        mapping[3..ch - 1].copy_from_slice(&layout.mapping[4..ch]);
        mapping[ch - 1] = layout.mapping[3];
    }
    let mut s = format!("{}{}{}", layout.channels, layout.streams, layout.coupled);
    for m in mapping {
        s.push((b'0' + m) as char);
    }
    s
}

/// GF(2⁸) multiply, reduction poly 0x11d (the nanors/oblas field both wire ends use).
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1d;
        }
        b >>= 1;
    }
    p
}

/// RS(4,2) parity over one aligned block of 4 encrypted payloads, exactly as nanors
/// `reed_solomon_encode` with the patched [`FEC_MATRIX`] computes it. Returns `None` if the
/// shard lengths differ — impossible under hard-CBR Opus + PKCS7 of equal input, but FEC is
/// opportunistic so skipping a block is always safe (the client just loses recovery for it).
fn audio_parity(data: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    debug_assert_eq!(data.len(), FEC_DATA_SHARDS);
    let len = data[0].len();
    if data.iter().any(|d| d.len() != len) {
        return None;
    }
    let mut parity = vec![vec![0u8; len]; FEC_PARITY_SHARDS];
    for (j, row) in FEC_MATRIX.iter().enumerate() {
        for (i, shard) in data.iter().enumerate() {
            let coef = row[i];
            for (p, &d) in parity[j].iter_mut().zip(shard.iter()) {
                *p ^= gf_mul(coef, d);
            }
        }
    }
    Some(parity)
}

/// Build a GameStream RTP audio data packet: 12-byte BE `RTP_PACKET` header + Opus payload.
fn build_rtp(seq: u16, timestamp: u32, opus: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + opus.len());
    p.push(0x80); // RTP version 2, no padding/extension/CSRC
    p.push(AUDIO_PACKET_TYPE);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // ssrc
    p.extend_from_slice(opus);
    p
}

/// Build a GameStream audio FEC packet: 12-byte RTP header (`packetType = 127`,
/// `timestamp = 0`, like Sunshine) + 12-byte `AUDIO_FEC_HEADER { fecShardIndex u8,
/// payloadType = 97, baseSequenceNumber BE u16, baseTimestamp BE u32, ssrc = 0 }`
/// (moonlight-common-c `RtpAudioQueue.h`) + the parity bytes.
fn build_fec_rtp(
    rtp_seq: u16,
    shard_index: u8,
    base_seq: u16,
    base_ts: u32,
    parity: &[u8],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + parity.len());
    p.push(0x80);
    p.push(AUDIO_FEC_PACKET_TYPE);
    p.extend_from_slice(&rtp_seq.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // timestamp (Sunshine leaves it 0)
    p.extend_from_slice(&0u32.to_be_bytes()); // ssrc
    p.push(shard_index);
    p.push(AUDIO_PACKET_TYPE); // fecHeader.payloadType — stamped onto recovered packets
    p.extend_from_slice(&base_seq.to_be_bytes());
    p.extend_from_slice(&base_ts.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes()); // fecHeader.ssrc
    p.extend_from_slice(parity);
    p
}

/// Slot for the persistent audio capturer, reused across streams (no leaked PipeWire
/// thread). A surround session that needs a different channel count drops the cached
/// capturer (clean RAII teardown) and opens a fresh one.
#[cfg(target_os = "linux")]
pub type AudioCapSlot = Arc<std::sync::Mutex<Option<Box<dyn AudioCapturer>>>>;
#[cfg(not(target_os = "linux"))]
pub type AudioCapSlot =
    std::sync::Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>;

/// Spawn the audio stream thread (idempotent via `running`). Stops when `running` clears.
/// `gcm_key`/`rikeyid` come from `/launch` and key the AES-CBC payload encryption;
/// `params` is the negotiated [`AudioParams`] from the RTSP ANNOUNCE.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn start(
    running: Arc<AtomicBool>,
    gcm_key: [u8; 16],
    rikeyid: i32,
    params: AudioParams,
    audio_cap: AudioCapSlot,
    on_lost: super::OnSessionLost,
) {
    let _ = std::thread::Builder::new()
        .name("punktfunk-audio".into())
        .spawn(move || {
            tracing::info!(?params, "audio stream starting");
            if let Err(e) = run(&running, &gcm_key, rikeyid, params, &audio_cap, &on_lost) {
                tracing::error!(error = %format!("{e:#}"), "audio stream failed");
            }
            running.store(false, Ordering::SeqCst);
            tracing::info!("audio stream stopped");
        });
}

/// Stub — the audio plane needs an audio-capture backend (PipeWire on Linux, WASAPI on
/// Windows) + libopus; this keeps the remaining targets (e.g. macOS) compiling (crate doc:
/// "the crate compiles everywhere"). Reports failure the same way the real stream thread
/// does: clears `running`.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn start(
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _gcm_key: [u8; 16],
    _rikeyid: i32,
    _params: AudioParams,
    _audio_cap: AudioCapSlot,
    _on_lost: super::OnSessionLost,
) {
    tracing::error!("GameStream audio requires Linux (PipeWire) or Windows (WASAPI) + libopus");
    running.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn run(
    running: &AtomicBool,
    gcm_key: &[u8; 16],
    rikeyid: i32,
    params: AudioParams,
    audio_cap: &std::sync::Mutex<Option<Box<dyn AudioCapturer>>>,
    on_lost: &super::OnSessionLost,
) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", AUDIO_PORT)).context("bind audio UDP")?;
    // Grow SO_SNDBUF/RCVBUF; the opt-in DSCP/QoS tag happens after connect below (Windows
    // qWAVE derives the flow from the connected 5-tuple).
    punktfunk_core::transport::grow_socket_buffers(&sock);
    // The client pings the audio port (~every 500ms) so we learn where to send.
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    tracing::debug!(port = AUDIO_PORT, "audio: awaiting client ping");
    let mut probe = [0u8; 256];
    let (_, client) = sock
        .recv_from(&mut probe)
        .context("audio: no client ping within 10s")?;
    sock.connect(client)
        .context("connect client audio endpoint")?;
    // Opt-in DSCP/QoS-tag this as the audio class (PUNKTFUNK_DSCP=1); the guard keeps the
    // Windows qWAVE flow alive for the whole stream (this function's scope IS the stream).
    let _qos_flow = punktfunk_core::transport::set_media_qos(
        &sock,
        punktfunk_core::transport::MediaClass::Audio,
    );
    tracing::debug!(%client, "audio: client endpoint learned");

    // Reuse the persistent capturer when its channel count still matches (drain stale
    // buffered audio); otherwise drop it (clean PipeWire teardown) and open at the new count.
    let want = layout_for(&params).channels as u32;
    let mut cap = match audio_cap.lock().unwrap().take() {
        Some(mut c) if c.channels() == want => {
            c.drain();
            c
        }
        Some(c) => {
            tracing::info!(
                have = c.channels(),
                want,
                "audio capturer channel count changed — reopening"
            );
            drop(c);
            audio::open_audio_capture(want).context("open audio capture")?
        }
        None => audio::open_audio_capture(want).context("open audio capture")?,
    };
    let result = audio_body(&mut *cap, &sock, gcm_key, rikeyid, params, running, on_lost);
    cap.idle(); // parked between sessions — release the routing claim (Linux stream sink)
    audio::park_audio_capture(audio_cap, cap); // drop on Windows (restores the default), keep on Linux
    result
}

/// Opus encoder for one session: the plain stereo encoder (the live-validated path, byte
/// identical) or the safe `opus::MSEncoder` multistream encoder for 5.1/7.1. Both are
/// cross-platform (Linux + Windows) — surround no longer needs `audiopus_sys`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
enum SessionEncoder {
    Stereo(opus::Encoder),
    Surround(opus::MSEncoder),
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl SessionEncoder {
    fn new(layout: &'static OpusLayout) -> Result<SessionEncoder> {
        // RESTRICTED_LOWDELAY (`opus::Application::LowDelay`) + hard CBR, matching Sunshine — CBR
        // keeps the Opus packet size constant, which the GameStream audio FEC (equal-length shards)
        // relies on, and the client asserts a constant per-stream TOC.
        if layout.channels == 2 {
            let mut enc = opus::Encoder::new(
                SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::LowDelay,
            )
            .context("create Opus encoder")?;
            enc.set_bitrate(opus::Bitrate::Bits(layout.bitrate)).ok();
            enc.set_vbr(false).ok();
            Ok(SessionEncoder::Stereo(enc))
        } else {
            let mut enc = opus::MSEncoder::new(
                SAMPLE_RATE,
                layout.streams,
                layout.coupled,
                layout.mapping,
                opus::Application::LowDelay,
            )
            .map_err(|e| anyhow::anyhow!("create Opus multistream encoder: {e}"))?;
            enc.set_bitrate(opus::Bitrate::Bits(layout.bitrate)).ok();
            enc.set_vbr(false).ok();
            Ok(SessionEncoder::Surround(enc))
        }
    }

    /// Encode one interleaved frame into `out`, returning the packet length. Both encoders infer
    /// the per-channel sample count from `frame.len()` and their channel count.
    fn encode_float(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize> {
        match self {
            SessionEncoder::Stereo(enc) => enc.encode_float(frame, out).context("opus encode"),
            SessionEncoder::Surround(enc) => enc
                .encode_float(frame, out)
                .context("opus multistream encode"),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[allow(clippy::too_many_arguments)]
fn audio_body(
    cap: &mut dyn AudioCapturer,
    sock: &UdpSocket,
    gcm_key: &[u8; 16],
    rikeyid: i32,
    params: AudioParams,
    running: &AtomicBool,
    // Whole-session teardown for the client-unreachable send errors below — video would
    // otherwise keep streaming at the dead endpoint (see `AppState::end_session`).
    on_lost: &super::OnSessionLost,
) -> Result<()> {
    let layout = layout_for(&params);
    let mut enc = SessionEncoder::new(layout)?;
    // Opus frame duration; Moonlight negotiates 5 ms (default) or 10 ms via
    // `x-nv-aqos.packetDuration` and sizes its decoder at `48 * duration` samples.
    // Already snapped to {5, 10} at parse time; guard here too so only legal Opus frame
    // sizes (48 kHz × {5,10} ms = 240/480 samples) ever reach the encoder.
    let frame_ms = if params.packet_duration_ms >= 10 {
        10
    } else {
        5
    } as usize;
    let samples_per_channel = SAMPLE_RATE as usize * frame_ms / 1000;
    let frame_len = samples_per_channel * layout.channels as usize; // interleaved samples
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    let mut out = vec![0u8; 1400];
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let mut sent: u64 = 0;
    // Surround sessions carry RS(4,2) FEC; the stereo wire stays exactly as validated.
    let fec = layout.channels > 2;
    let mut fec_block: Vec<Vec<u8>> = Vec::with_capacity(FEC_DATA_SHARDS);
    let (mut fec_base_seq, mut fec_base_ts) = (0u16, 0u32);
    let mut fec_skipped = false;
    // Pacing anchor: PipeWire hands us large capture buffers (~1024 frames), so we'd otherwise
    // emit packets in bursts the client's low-latency jitter buffer hears as glitching. Emit
    // each frame at its packet-duration slot instead. Production is real-time, so the backlog
    // stays small.
    let start = Instant::now();
    let mut frame_no: u64 = 0;
    // Optional linear gain for quiet capture sources (PUNKTFUNK_AUDIO_GAIN, default 1.0).
    let gain: f32 = std::env::var("PUNKTFUNK_AUDIO_GAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    tracing::info!(
        channels = layout.channels,
        streams = layout.streams,
        coupled = layout.coupled,
        bitrate = layout.bitrate,
        frame_ms,
        fec,
        "audio: encoder configured"
    );

    while running.load(Ordering::SeqCst) {
        let chunk = cap.next_chunk().context("capture audio chunk")?;
        acc.extend_from_slice(&chunk);
        while acc.len() >= frame_len {
            let mut frame: Vec<f32> = acc.drain(..frame_len).collect();
            if gain != 1.0 {
                for s in &mut frame {
                    *s = (*s * gain).clamp(-1.0, 1.0);
                }
            }
            let n = enc.encode_float(&frame, &mut out)?;
            // AES-128-CBC the Opus payload (RTP header stays plaintext). Per-packet IV =
            // BE32(rikeyid + seq) in [0..4], zero elsewhere; PKCS7 padding.
            let iv_seq = (rikeyid as u32).wrapping_add(seq as u32);
            let mut iv = [0u8; 16];
            iv[0..4].copy_from_slice(&iv_seq.to_be_bytes());
            let ct = Aes128CbcEnc::new(gcm_key.into(), (&iv).into())
                .encrypt_padded_vec_mut::<Pkcs7>(&out[..n]);
            let pkt = build_rtp(seq, timestamp, &ct);
            if sock.send(&pkt).is_err() {
                tracing::info!(sent, "audio: client unreachable — ending session");
                on_lost();
                return Ok(());
            }
            // Surround FEC: accumulate the encrypted payloads of the aligned 4-packet block;
            // close the block with 2 parity packets (RTP seqs continue past the block, like
            // Sunshine — the client places parity by the FEC header, not the RTP seq).
            if fec {
                if seq % FEC_DATA_SHARDS as u16 == 0 {
                    fec_block.clear();
                    fec_base_seq = seq;
                    fec_base_ts = timestamp;
                }
                fec_block.push(ct);
                if fec_block.len() == FEC_DATA_SHARDS {
                    match audio_parity(&fec_block) {
                        Some(parity) => {
                            for (x, par) in parity.iter().enumerate() {
                                let rtp_seq =
                                    fec_base_seq.wrapping_add((FEC_DATA_SHARDS + x) as u16);
                                let fp =
                                    build_fec_rtp(rtp_seq, x as u8, fec_base_seq, fec_base_ts, par);
                                if sock.send(&fp).is_err() {
                                    tracing::info!(
                                        sent,
                                        "audio: client unreachable — ending session"
                                    );
                                    on_lost();
                                    return Ok(());
                                }
                            }
                        }
                        None if !fec_skipped => {
                            // Shouldn't happen under hard CBR; log once and keep streaming.
                            tracing::warn!("audio: unequal packet sizes — FEC block skipped");
                            fec_skipped = true;
                        }
                        None => {}
                    }
                    fec_block.clear();
                }
            }
            seq = seq.wrapping_add(1);
            // GameStream's audio RTP timestamp ticks by packetDuration (ms), not by samples.
            timestamp = timestamp.wrapping_add(frame_ms as u32);
            sent += 1;
            if sent % 400 == 0 {
                tracing::debug!(sent, "audio: streaming");
            }

            // Hold each frame to its packet-duration slot (skip if we've fallen behind a burst).
            frame_no += 1;
            let scheduled = start + Duration::from_millis(frame_ms as u64 * frame_no);
            let now = Instant::now();
            if scheduled > now {
                std::thread::sleep((scheduled - now).min(Duration::from_millis(20)));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_layout() {
        let p = build_rtp(0x0102, 0x03040506, &[0xaa, 0xbb]);
        assert_eq!(p[0], 0x80);
        assert_eq!(p[1], 97);
        assert_eq!(&p[2..4], &[0x01, 0x02]); // seq BE
        assert_eq!(&p[4..8], &[0x03, 0x04, 0x05, 0x06]); // timestamp BE
        assert_eq!(&p[8..12], &[0, 0, 0, 0]); // ssrc
        assert_eq!(&p[12..], &[0xaa, 0xbb]); // opus payload
    }

    #[test]
    fn frame_sizing() {
        // 48 kHz · 5 ms = 240 samples/channel (the validated stereo default).
        assert_eq!(SAMPLE_RATE as usize * 5 / 1000, 240);
        assert_eq!(SAMPLE_RATE as usize * 10 / 1000, 480);
    }

    /// FEC datagram layout: RTP(12, packetType 127) + AUDIO_FEC_HEADER(12) + parity.
    #[test]
    fn fec_packet_layout() {
        let p = build_fec_rtp(0x1234, 1, 0x1230, 0xAABBCCDD, &[0xEE, 0xFF]);
        assert_eq!(p[0], 0x80);
        assert_eq!(p[1], 127); // RTP_PAYLOAD_TYPE_FEC
        assert_eq!(&p[2..4], &[0x12, 0x34]); // RTP seq BE
        assert_eq!(&p[4..8], &[0, 0, 0, 0]); // RTP timestamp (Sunshine: 0)
        assert_eq!(&p[8..12], &[0, 0, 0, 0]); // RTP ssrc
        assert_eq!(p[12], 1); // fecShardIndex
        assert_eq!(p[13], 97); // fecHeader.payloadType (stamped on recovered packets)
        assert_eq!(&p[14..16], &[0x12, 0x30]); // baseSequenceNumber BE
        assert_eq!(&p[16..20], &[0xAA, 0xBB, 0xCC, 0xDD]); // baseTimestamp BE
        assert_eq!(&p[20..24], &[0, 0, 0, 0]); // fecHeader.ssrc
        assert_eq!(&p[24..], &[0xEE, 0xFF]); // parity payload
    }

    /// The advertised surround-params strings, locked: any change breaks what stock
    /// Moonlight clients derive their decoder mapping from.
    #[test]
    fn surround_params_strings() {
        assert_eq!(surround_params(&LAYOUT_STEREO, false), "21101");
        assert_eq!(surround_params(&LAYOUT_51, false), "642012453");
        assert_eq!(surround_params(&LAYOUT_51_HQ, true), "660012345");
        assert_eq!(surround_params(&LAYOUT_71, false), "85301245673");
        assert_eq!(surround_params(&LAYOUT_71_HQ, true), "88001234567");
    }

    /// moonlight-common-c's normal-quality mapping swap (RtspConnection.c: `mapping[3] =
    /// old[ch-1]; mapping[4..] = old[3..ch-1]`), replicated byte-for-byte.
    fn client_swap(adv: &[u8]) -> Vec<u8> {
        let ch = adv.len();
        let mut m = adv.to_vec();
        m[3] = adv[ch - 1];
        m[4..ch].copy_from_slice(&adv[3..ch - 1]);
        m
    }

    /// Protocol math, end to end: the mapping a stock client computes from our advertised
    /// surround-params must equal the mapping we encode with — for the normal-quality
    /// layouts after the client's GFE-order swap, for HQ layouts verbatim.
    #[test]
    fn client_derived_mapping_matches_encoder() {
        for (layout, hq) in [
            (&LAYOUT_51, false),
            (&LAYOUT_51_HQ, true),
            (&LAYOUT_71, false),
            (&LAYOUT_71_HQ, true),
        ] {
            let s = surround_params(layout, hq);
            let digits: Vec<u8> = s.bytes().map(|b| b - b'0').collect();
            assert_eq!(digits[0], layout.channels);
            assert_eq!(digits[1], layout.streams);
            assert_eq!(digits[2], layout.coupled);
            let adv = &digits[3..];
            let client = if hq { adv.to_vec() } else { client_swap(adv) };
            assert_eq!(
                client, layout.mapping,
                "layout {}ch hq={hq}",
                layout.channels
            );
        }
    }

    /// GF(2⁸) inverse by brute force (test-only).
    fn gf_inv(a: u8) -> u8 {
        (1..=255u8).find(|&b| gf_mul(a, b) == 1).unwrap()
    }

    /// Self-consistency of the RS(4,2) code: erase any 2 data shards, solve the remaining
    /// 2×2 system over GF(2⁸) with the same matrix, and recover the originals. (Matrix bytes
    /// and gemm semantics are from moonlight-common-c `RtpAudioQueue.c` and `nanors/rs.c`;
    /// this proves our parity is consistent with that generator matrix.)
    #[test]
    fn fec_parity_recovers_two_losses() {
        let data: Vec<Vec<u8>> = vec![
            vec![0x11, 0x22, 0x33],
            vec![0x44, 0x55, 0x66],
            vec![0x77, 0x88, 0x99],
            vec![0xaa, 0xbb, 0xcc],
        ];
        let parity = audio_parity(&data).unwrap();
        for (e0, e1) in [(0usize, 1usize), (1, 3), (0, 3), (2, 3)] {
            // parity[j] = sum_i M[j][i] d[i]  →  with d[e0], d[e1] unknown:
            //   M[j][e0]·x + M[j][e1]·y = parity[j] ^ sum_{i∉{e0,e1}} M[j][i]·d[i]
            let mut rhs = [parity[0].clone(), parity[1].clone()];
            for j in 0..2 {
                for (i, d) in data.iter().enumerate() {
                    if i != e0 && i != e1 {
                        for (r, &b) in rhs[j].iter_mut().zip(d.iter()) {
                            *r ^= gf_mul(FEC_MATRIX[j][i], b);
                        }
                    }
                }
            }
            // Cramer over GF(2⁸): det = a·d ^ b·c (addition = XOR).
            let (a, b) = (FEC_MATRIX[0][e0], FEC_MATRIX[0][e1]);
            let (c, d) = (FEC_MATRIX[1][e0], FEC_MATRIX[1][e1]);
            let det = gf_mul(a, d) ^ gf_mul(b, c);
            assert_ne!(det, 0, "erasures {e0},{e1} must be solvable");
            let det_inv = gf_inv(det);
            for k in 0..data[0].len() {
                let (r0, r1) = (rhs[0][k], rhs[1][k]);
                let x = gf_mul(det_inv, gf_mul(d, r0) ^ gf_mul(b, r1));
                let y = gf_mul(det_inv, gf_mul(c, r0) ^ gf_mul(a, r1));
                assert_eq!(x, data[e0][k], "shard {e0} byte {k}");
                assert_eq!(y, data[e1][k], "shard {e1} byte {k}");
            }
        }
    }

    /// Unequal shard sizes must skip the block, never panic or emit bogus parity.
    #[test]
    fn fec_parity_rejects_unequal_shards() {
        let data = vec![vec![0u8; 10], vec![0u8; 10], vec![0u8; 9], vec![0u8; 10]];
        assert!(audio_parity(&data).is_none());
    }

    /// Real-codec proof of the 5.1 mapping math: encode with our encoder layout, decode with
    /// the mapping a stock Moonlight client derives from our advertised surround-params
    /// (parse → GFE swap), and verify a tone fed into each input channel comes out on the
    /// same output channel. Cross-platform via the safe `opus` crate — this also guards the
    /// (now un-gated) Windows GameStream surround build.
    #[test]
    fn multistream_51_roundtrip_channel_identity() {
        let layout = &LAYOUT_51;
        let samples = 240; // 5 ms
        let ch = layout.channels as usize;

        // Client-side decoder mapping derived exactly as moonlight-common-c does (GFE swap).
        let s = surround_params(layout, false);
        let digits: Vec<u8> = s.bytes().map(|b| b - b'0').collect();
        let client_mapping = client_swap(&digits[3..]);

        let mut dec =
            opus::MSDecoder::new(SAMPLE_RATE, layout.streams, layout.coupled, &client_mapping)
                .expect("multistream decoder");

        for tone_ch in 0..ch {
            let mut enc = opus::MSEncoder::new(
                SAMPLE_RATE,
                layout.streams,
                layout.coupled,
                layout.mapping,
                opus::Application::LowDelay,
            )
            .expect("multistream encoder");
            let mut out = vec![0u8; 1400];
            let mut energy = vec![0f64; ch];
            // A few frames so the codec converges past its startup transient.
            for f in 0..8 {
                let mut frame = vec![0f32; samples * ch];
                for t in 0..samples {
                    let phase = (f * samples + t) as f32 * 440.0 * 2.0 * std::f32::consts::PI
                        / SAMPLE_RATE as f32;
                    frame[t * ch + tone_ch] = 0.5 * phase.sin();
                }
                let n = enc.encode_float(&frame, &mut out).unwrap();
                assert!(n > 0);
                let mut decoded = vec![0f32; samples * ch];
                let got = dec.decode_float(&out[..n], &mut decoded, false).unwrap();
                assert_eq!(got, samples);
                if f >= 4 {
                    for t in 0..samples {
                        for (c, e) in energy.iter_mut().enumerate() {
                            *e += (decoded[t * ch + c] as f64).powi(2);
                        }
                    }
                }
            }
            let loudest = (0..ch)
                .max_by(|&a, &b| energy[a].total_cmp(&energy[b]))
                .unwrap();
            assert_eq!(
                loudest, tone_ch,
                "tone in input channel {tone_ch} must come out on output channel {tone_ch} \
                 (energies: {energy:?})"
            );
        }
    }

    /// Live 5.1 capture → multistream encode → decode, against a real PipeWire session.
    /// Manual validation (needs `pactl load-module module-null-sink sink_name=pf51
    /// channels=6 rate=48000` as the default sink):
    ///   cargo test -p punktfunk-host --lib -- --ignored surround_capture
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn surround_capture_live() {
        let mut cap = crate::audio::open_audio_capture(6).expect("open 6ch capture");
        let layout = &LAYOUT_51;
        let mut enc = opus::MSEncoder::new(
            SAMPLE_RATE,
            layout.streams,
            layout.coupled,
            layout.mapping,
            opus::Application::LowDelay,
        )
        .unwrap();
        enc.set_vbr(false).ok(); // hard CBR so packet sizes are constant (audio FEC relies on it)
        let mut out = vec![0u8; 1400];
        let mut acc: Vec<f32> = Vec::new();
        let frame_len = 240 * 6;
        let mut packets = 0;
        let mut sizes = std::collections::BTreeSet::new();
        while packets < 100 {
            let chunk = cap.next_chunk().expect("capture chunk");
            acc.extend_from_slice(&chunk);
            while acc.len() >= frame_len && packets < 100 {
                let frame: Vec<f32> = acc.drain(..frame_len).collect();
                let n = enc.encode_float(&frame, &mut out).unwrap();
                sizes.insert(n);
                packets += 1;
            }
        }
        // Hard CBR: every multistream packet must be the same size (audio FEC relies on it).
        assert_eq!(sizes.len(), 1, "CBR sizes: {sizes:?}");
        // And a stock client's GFE-derived decoder must accept them.
        let s = surround_params(layout, false);
        let digits: Vec<u8> = s.bytes().map(|b| b - b'0').collect();
        let client_mapping = client_swap(&digits[3..]);
        let mut dec =
            opus::MSDecoder::new(SAMPLE_RATE, layout.streams, layout.coupled, &client_mapping)
                .unwrap();
        let mut pcm = vec![0f32; 240 * 6];
        let got = dec
            .decode_float(&out[..*sizes.first().unwrap()], &mut pcm, false)
            .unwrap();
        assert_eq!(got, 240);
    }
}
