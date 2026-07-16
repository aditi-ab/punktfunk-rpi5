//! The native audio plane (plan §W1 — carved out of the [`super`] module): desktop capture → Opus
//! (48 kHz, 5 ms, CBR — the same tuning as the GameStream path) → `AUDIO_MAGIC` QUIC datagrams, at
//! the negotiated channel count. The encoder ([`NativeAudioEnc`]) and the capture/encode/send loop
//! ([`audio_thread`]) are gated to linux/windows (libopus + a real capturer); other targets get the
//! stub, so a dev build streams video-only rather than failing to compile.

use super::*;

/// Opus encoder for the native audio plane: a plain stereo encoder (the live-validated,
/// byte-identical path) or a libopus *multistream* encoder for 5.1/7.1, both behind one
/// `encode_float`. Surround uses the safe `opus::MSEncoder` (no `audiopus_sys`).
#[cfg(any(target_os = "linux", target_os = "windows"))]
enum NativeAudioEnc {
    Stereo(opus::Encoder),
    Surround(opus::MSEncoder),
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl NativeAudioEnc {
    /// Build the encoder for `channels` (2/6/8), hard-CBR + RESTRICTED_LOWDELAY like the
    /// GameStream path; bitrate from the shared layout table (stereo keeps the validated 128 kbps).
    fn new(channels: u8) -> Result<NativeAudioEnc, opus::Error> {
        if channels == 2 {
            let mut e = opus::Encoder::new(
                crate::audio::SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::LowDelay,
            )?;
            e.set_bitrate(opus::Bitrate::Bits(128_000)).ok();
            e.set_vbr(false).ok();
            Ok(NativeAudioEnc::Stereo(e))
        } else {
            let l = punktfunk_core::audio::layout_for(channels, false);
            let mut e = opus::MSEncoder::new(
                crate::audio::SAMPLE_RATE,
                l.streams,
                l.coupled,
                l.mapping,
                opus::Application::LowDelay,
            )?;
            e.set_bitrate(opus::Bitrate::Bits(l.bitrate)).ok();
            e.set_vbr(false).ok();
            Ok(NativeAudioEnc::Surround(e))
        }
    }

    fn encode_float(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize, opus::Error> {
        match self {
            NativeAudioEnc::Stereo(e) => e.encode_float(frame, out),
            NativeAudioEnc::Surround(e) => e.encode_float(frame, out),
        }
    }
}

/// The audio thread: desktop capture → Opus (48 kHz, 5 ms, CBR — same tuning as the GameStream
/// path) → `AUDIO_MAGIC` datagrams, at the negotiated `channels` (2 stereo / 6 = 5.1 / 8 = 7.1,
/// canonical wire order FL FR FC LFE RL RR SL SR). QUIC already encrypts; no extra layer. The
/// capturer comes from (and returns to) the persistent slot — see [`AudioCapSlot`].
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(super) fn audio_thread(
    conn: quinn::Connection,
    stop: Arc<AtomicBool>,
    audio_cap: AudioCapSlot,
    channels: u8,
) {
    use crate::audio::SAMPLE_RATE;
    const FRAME_MS: usize = 5;
    const SAMPLES_PER_FRAME: usize = SAMPLE_RATE as usize * FRAME_MS / 1000; // 240
    let want = punktfunk_core::audio::normalize_channels(channels);

    // Reuse the cached capturer ONLY when its channel count matches this session's; a stereo
    // capturer left by a prior session must not feed a 5.1/7.1 session (the encoder + the client's
    // decoder are sized for `want`, so a mismatched capturer would garble/desync the audio).
    let capturer = match audio_cap.lock().unwrap().take() {
        Some(mut c) if c.channels() == want as u32 => {
            c.drain(); // discard audio captured between sessions
            c
        }
        prev => {
            drop(prev); // wrong channel count (or none): clean teardown, open fresh at `want`
            match crate::audio::open_audio_capture(want as u32) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "punktfunk/1 audio unavailable — session continues without it");
                    return;
                }
            }
        }
    };
    let mut enc = match NativeAudioEnc::new(want) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "opus encoder init failed — session continues without audio");
            *audio_cap.lock().unwrap() = Some(capturer);
            return;
        }
    };

    let frame_len = SAMPLES_PER_FRAME * want as usize;
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    // Sized for the largest surround frame (7.1 HQ ≈ 1.3 KB at 5 ms); ample for normal quality.
    let mut opus_buf = vec![0u8; 4096];
    let mut seq: u32 = 0;
    // Reopen-with-backoff: hold the capturer in an Option so a mid-session capture-thread death
    // (device unplug, daemon restart) reopens instead of muting the rest of a multi-hour session.
    // A quiet sink is NOT a death — `next_chunk` returns an empty chunk on its idle timeout — so only
    // a genuine thread-ended Err drops the capturer. Reopens are throttled by INJECTOR_REOPEN_BACKOFF.
    // The Opus encoder and the monotonic `seq` are kept across reopens (the client sees a gap, not a
    // restart). The first open already happened above; failing THAT still ends the session quietly.
    let mut capturer = Some(capturer);
    let mut last_failed: Option<std::time::Instant> = None;
    // A stuck Opus encoder would fail on every 5 ms frame (~200/s); power-of-two throttle the
    // warn so it can't flood stderr + the log ring while still surfacing that it's failing.
    let mut opus_encode_errs: u64 = 0;
    tracing::info!(
        channels = want,
        "punktfunk/1 audio streaming (Opus 48 kHz, 5 ms datagrams)"
    );
    'session: while !stop.load(Ordering::SeqCst) {
        if capturer.is_none() {
            if last_failed.is_some_and(|t| t.elapsed() < INJECTOR_REOPEN_BACKOFF) {
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            match crate::audio::open_audio_capture(want as u32) {
                Ok(c) => {
                    tracing::info!("punktfunk/1 audio capture reopened");
                    capturer = Some(c);
                    last_failed = None;
                    acc.clear(); // drop the partial frame straddling the gap
                }
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "audio reopen failed — will retry");
                    last_failed = Some(std::time::Instant::now());
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            }
        }
        let chunk = match capturer.as_mut().unwrap().next_chunk() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture lost — reopening");
                capturer = None;
                last_failed = Some(std::time::Instant::now());
                continue;
            }
        };
        acc.extend_from_slice(&chunk);
        while acc.len() >= frame_len {
            let frame: Vec<f32> = acc.drain(..frame_len).collect();
            let pts_ns = now_ns();
            match enc.encode_float(&frame, &mut opus_buf) {
                Ok(n) => {
                    let d =
                        punktfunk_core::quic::encode_audio_datagram(seq, pts_ns, &opus_buf[..n]);
                    if conn.send_datagram(d.into()).is_err() {
                        break 'session; // connection gone
                    }
                    seq = seq.wrapping_add(1);
                }
                Err(e) => {
                    opus_encode_errs += 1;
                    if opus_encode_errs.is_power_of_two() {
                        tracing::warn!(
                            error = %e,
                            count = opus_encode_errs,
                            "opus encode failed — dropping audio frame"
                        );
                    }
                }
            }
        }
    }
    // Return the live capturer for the next session (None if it died and never reopened).
    if let Some(c) = capturer {
        *audio_cap.lock().unwrap() = Some(c);
    }
}

/// Stub — punktfunk/1 audio needs Linux (PipeWire capture + libopus); non-Linux dev builds
/// run sessions without it, same as when the capturer fails to open.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(super) fn audio_thread(
    _conn: quinn::Connection,
    _stop: Arc<AtomicBool>,
    _audio_cap: AudioCapSlot,
    _channels: u8,
) {
    tracing::warn!("punktfunk/1 audio requires Linux or Windows — session continues without it");
}
