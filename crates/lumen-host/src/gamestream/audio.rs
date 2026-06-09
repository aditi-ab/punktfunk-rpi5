//! The audio data plane (UDP 48000). On RTSP PLAY we learn the client's audio endpoint from
//! its port-learning ping, capture the default-sink monitor, Opus-encode 5 ms stereo frames,
//! and send each as a GameStream RTP audio packet.
//!
//! Wire format (moonlight-common-c `AudioStream.c`): a 12-byte big-endian `RTP_PACKET`
//! (`packetType = 97`, `sequenceNumber++`, `timestamp += packetDuration`, `ssrc = 0`)
//! followed by the AES-128-CBC-encrypted Opus payload. Stereo Opus is a single coupled
//! multistream, so a plain `opus_encode` bitstream is what the client's multistream decoder
//! expects. Like the control stream, modern Moonlight always AES-CBC-decrypts audio (it
//! reports "Failed to decrypt audio packet" on plaintext), so we encrypt the payload under the
//! `/launch` `rikey` with a per-packet IV `BE32(rikeyid + seq)` (PKCS7 padding, RTP header
//! left in the clear). Reed-Solomon audio FEC is layered on top in P1.5.

use super::AUDIO_PORT;
use crate::audio::{self, CHANNELS, SAMPLE_RATE};
use anyhow::{Context, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use opus::{Application, Bitrate, Channels, Encoder};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// Opus frame duration; 5 ms is moonlight's default (`x-nv-aqos.packetDuration`).
const FRAME_MS: usize = 5;
/// Samples per channel per Opus frame (48 kHz · 5 ms = 240).
const SAMPLES_PER_FRAME: usize = SAMPLE_RATE as usize * FRAME_MS / 1000;
/// RTP payload type for audio (moonlight `AudioStream.c` checks `packetType == 97`).
const AUDIO_PACKET_TYPE: u8 = 97;
const OPUS_BITRATE: i32 = 128_000;

/// Spawn the audio stream thread (idempotent via `running`). Stops when `running` clears.
/// `gcm_key`/`rikeyid` come from `/launch` and key the AES-CBC payload encryption.
pub fn start(running: Arc<AtomicBool>, gcm_key: [u8; 16], rikeyid: i32) {
    let _ = std::thread::Builder::new()
        .name("lumen-audio".into())
        .spawn(move || {
            tracing::info!("audio stream starting");
            if let Err(e) = run(&running, &gcm_key, rikeyid) {
                tracing::error!(error = %format!("{e:#}"), "audio stream failed");
            }
            running.store(false, Ordering::SeqCst);
            tracing::info!("audio stream stopped");
        });
}

fn run(running: &AtomicBool, gcm_key: &[u8; 16], rikeyid: i32) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", AUDIO_PORT)).context("bind audio UDP")?;
    // The client pings the audio port (~every 500ms) so we learn where to send.
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    tracing::info!(port = AUDIO_PORT, "audio: awaiting client ping");
    let mut probe = [0u8; 256];
    let (_, client) = sock
        .recv_from(&mut probe)
        .context("audio: no client ping within 10s")?;
    sock.connect(client)
        .context("connect client audio endpoint")?;
    tracing::info!(%client, "audio: client endpoint learned");

    let mut cap = audio::open_audio_capture().context("open audio capture")?;
    // RESTRICTED_LOWDELAY + CBR, matching Sunshine — CBR keeps the Opus TOC byte constant,
    // which the client asserts per stream.
    let mut enc = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::LowDelay)
        .context("create Opus encoder")?;
    enc.set_bitrate(Bitrate::Bits(OPUS_BITRATE)).ok();
    enc.set_vbr(false).ok();

    let frame_len = SAMPLES_PER_FRAME * CHANNELS; // interleaved samples per Opus frame
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    let mut out = vec![0u8; 1400];
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let mut sent: u64 = 0;
    // Pacing anchor: PipeWire hands us large capture buffers (~1024 frames), so we'd otherwise
    // emit packets in bursts the client's low-latency jitter buffer hears as glitching. Emit
    // each frame at its 5 ms slot instead. Production is real-time, so the backlog stays small.
    let start = Instant::now();
    let mut frame_no: u64 = 0;
    // Optional linear gain for quiet capture sources (LUMEN_AUDIO_GAIN, default 1.0).
    let gain: f32 = std::env::var("LUMEN_AUDIO_GAIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);

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
            let n = enc.encode_float(&frame, &mut out).context("opus encode")?;
            // AES-128-CBC the Opus payload (RTP header stays plaintext). Per-packet IV =
            // BE32(rikeyid + seq) in [0..4], zero elsewhere; PKCS7 padding.
            let iv_seq = (rikeyid as u32).wrapping_add(seq as u32);
            let mut iv = [0u8; 16];
            iv[0..4].copy_from_slice(&iv_seq.to_be_bytes());
            let ct = Aes128CbcEnc::new(gcm_key.into(), (&iv).into())
                .encrypt_padded_vec_mut::<Pkcs7>(&out[..n]);
            let pkt = build_rtp(seq, timestamp, &ct);
            if sock.send(&pkt).is_err() {
                tracing::info!(sent, "audio: client unreachable — stopping");
                return Ok(());
            }
            seq = seq.wrapping_add(1);
            // GameStream's audio RTP timestamp ticks by packetDuration (ms), not by samples.
            timestamp = timestamp.wrapping_add(FRAME_MS as u32);
            sent += 1;
            if sent % 400 == 0 {
                tracing::info!(sent, "audio: streaming");
            }

            // Hold each frame to its 5 ms slot (skip if we've fallen behind a burst).
            frame_no += 1;
            let scheduled = start + Duration::from_millis(5 * frame_no);
            let now = Instant::now();
            if scheduled > now {
                std::thread::sleep((scheduled - now).min(Duration::from_millis(20)));
            }
        }
    }
    Ok(())
}

/// Build a GameStream RTP audio packet: 12-byte BE `RTP_PACKET` header + Opus payload.
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
        assert_eq!(SAMPLES_PER_FRAME, 240);
    }
}
