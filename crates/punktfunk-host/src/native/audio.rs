//! The native audio plane (plan §W1 — carved out of the [`super`] module): desktop capture → Opus
//! (48 kHz, 5 ms, constrained VBR at the configured [`AudioTier`](punktfunk_core::audio::AudioTier))
//! → `AUDIO_MAGIC` QUIC datagrams — or `AUDIO_RED_MAGIC` when the session negotiated redundancy —
//! at the negotiated channel count. The encoder ([`NativeAudioEnc`]) and the capture/encode/send
//! loop ([`audio_thread`]) are gated to linux/windows (libopus + a real capturer); other targets
//! get the stub, so a dev build streams video-only rather than failing to compile.
//!
//! Two things here deliberately DIVERGE from the GameStream plane, which used to share this
//! tuning: hard CBR (its audio FEC needs fixed-size packets; this plane has no FEC, so CBR was a
//! pure quality tax) and the fixed 128 kbps stereo bitrate. See [`NativeAudioEnc::new`].

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
    /// Build the encoder for `channels` (2/6/8) at `tier`, RESTRICTED_LOWDELAY like the GameStream
    /// path but — unlike it — in CONSTRAINED VBR.
    ///
    /// **Why not hard CBR (WP1.2).** The layout table's comment justifies `set_vbr(false)` with
    /// "constant packet size, which GameStream's audio FEC relies on" — true of the GameStream
    /// plane, and irrelevant here: the native `punktfunk/1` audio plane has no FEC at all (see
    /// `punktfunk_core::audio::AudioGapTracker`, which exists precisely because a lost packet has
    /// nothing to rebuild it from). So this path was paying a pure quality tax for a constraint
    /// that does not apply to it. Constrained VBR keeps the same average bitrate and the same
    /// bounded packet size, and spends the bits where the signal needs them.
    ///
    /// The GameStream encoder (`crate::gamestream::audio`) is deliberately NOT changed: its FEC
    /// really does need fixed-size packets.
    fn new(
        channels: u8,
        tier: punktfunk_core::audio::AudioTier,
    ) -> Result<NativeAudioEnc, opus::Error> {
        let l = punktfunk_core::audio::layout_for(channels, false);
        let bitrate = l.bitrate_for(tier);
        if channels == 2 {
            let mut e = opus::Encoder::new(
                crate::audio::SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::LowDelay,
            )?;
            e.set_bitrate(opus::Bitrate::Bits(bitrate)).ok();
            e.set_vbr(true).ok();
            e.set_vbr_constraint(true).ok();
            Ok(NativeAudioEnc::Stereo(e))
        } else {
            let mut e = opus::MSEncoder::new(
                crate::audio::SAMPLE_RATE,
                l.streams,
                l.coupled,
                l.mapping,
                opus::Application::LowDelay,
            )?;
            e.set_bitrate(opus::Bitrate::Bits(bitrate)).ok();
            e.set_vbr(true).ok();
            e.set_vbr_constraint(true).ok();
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

/// The audio thread: desktop capture → Opus (48 kHz, 5 ms, constrained VBR at the configured
/// tier) → `AUDIO_MAGIC` (or `AUDIO_RED_MAGIC`) datagrams, at the negotiated `channels` (2 stereo / 6 = 5.1 / 8 = 7.1,
/// canonical wire order FL FR FC LFE RL RR SL SR). QUIC already encrypts; no extra layer. The
/// capturer comes from (and returns to) the persistent slot — see [`AudioCapSlot`].
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(super) fn audio_thread(
    conn: quinn::Connection,
    stop: Arc<AtomicBool>,
    audio_cap: AudioCapSlot,
    channels: u8,
    budget: punktfunk_core::audio::AudioBudget,
) {
    use crate::audio::SAMPLE_RATE;
    const FRAME_MS: usize = 5;
    const SAMPLES_PER_FRAME: usize = SAMPLE_RATE as usize * FRAME_MS / 1000; // 240
    let want = punktfunk_core::audio::normalize_channels(channels);
    // Tier and redundancy are ONE decision, budgeted against the session's video bitrate — see
    // `handshake::audio_budget`. An unparseable `audio.quality` was already warned about there
    // and fell back to the default, so nothing here can silently downgrade someone's audio.
    let (tier, redundancy) = (budget.tier, budget.redundancy);

    // Reuse the cached capturer ONLY when its channel count matches this session's; a stereo
    // capturer left by a prior session must not feed a 5.1/7.1 session (the encoder + the client's
    // decoder are sized for `want`, so a mismatched capturer would garble/desync the audio).
    // A FAILED first open does not end the session's audio: session start is peak endpoint churn
    // on Windows (the virtual-display attach and the wiring plan's own default-device flips race
    // the WASAPI activate — 0x80070002 mid-re-registration), so it enters the same
    // reopen-with-backoff loop a mid-session capture death does; audio then starts a few seconds
    // late instead of never.
    let capturer = match audio_cap.lock().unwrap().take() {
        Some(mut c) if c.channels() == want as u32 => {
            c.drain(); // discard audio captured between sessions (also re-claims routing)
            Some(c)
        }
        prev => {
            drop(prev); // wrong channel count (or none): clean teardown, open fresh at `want`
            match crate::audio::open_audio_capture(want as u32) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "punktfunk/1 audio failed to open — retrying in the background until it comes up");
                    None
                }
            }
        }
    };
    let mut enc = match NativeAudioEnc::new(want, tier) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "opus encoder init failed — session continues without audio");
            if let Some(mut c) = capturer {
                c.idle(); // parked, not streaming — release the routing claim
                crate::audio::park_audio_capture(&audio_cap, c);
            }
            return;
        }
    };

    let frame_len = SAMPLES_PER_FRAME * want as usize;
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    // Sized for the largest surround frame (7.1 HQ ≈ 1.3 KB at 5 ms); ample for normal quality.
    let mut opus_buf = vec![0u8; 4096];
    let mut seq: u32 = 0;
    // Reopen-with-backoff: hold the capturer in an Option so a mid-session capture-thread death
    // (device unplug, daemon restart) — or a first open lost to session-start churn above —
    // reopens instead of muting the rest of a multi-hour session. A quiet sink is NOT a death —
    // `next_chunk` returns an empty chunk on its idle timeout — so only a genuine thread-ended
    // Err drops the capturer. Reopens are throttled by INJECTOR_REOPEN_BACKOFF. The Opus encoder
    // and the monotonic `seq` are kept across reopens (the client sees a gap, not a restart).
    let mut last_failed = capturer.is_none().then(std::time::Instant::now);
    let mut capturer = capturer;
    // A stuck Opus encoder would fail on every 5 ms frame (~200/s); power-of-two throttle the
    // warn so it can't flood stderr + the log ring while still surfacing that it's failing.
    let mut opus_encode_errs: u64 = 0;
    // WP3.1 — the previous frame's Opus bytes, for the redundant `0xD2` plane. Cleared whenever
    // continuity breaks (a capture reopen), so we never advertise a predecessor the client's
    // sequence numbering does not agree with.
    let mut prev_frame: Vec<u8> = Vec::new();
    if capturer.is_some() {
        tracing::info!(
            channels = want,
            tier = tier.as_str(),
            kbps = budget.kbps,
            redundancy,
            "punktfunk/1 audio streaming (Opus 48 kHz, 5 ms datagrams)"
        );
    }
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
                                 // The next frame has no valid predecessor across the gap: sending the
                                 // pre-gap frame as "the previous one" would hand the client audio from
                                 // before the discontinuity to splice in.
                    prev_frame.clear();
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
                    let opus = &opus_buf[..n];
                    let d = if redundancy {
                        punktfunk_core::quic::encode_audio_red_datagram(
                            seq,
                            pts_ns,
                            opus,
                            &prev_frame,
                        )
                    } else {
                        punktfunk_core::quic::encode_audio_datagram(seq, pts_ns, opus)
                    };
                    if conn.send_datagram(d.into()).is_err() {
                        break 'session; // connection gone
                    }
                    if redundancy {
                        prev_frame.clear();
                        prev_frame.extend_from_slice(opus);
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
    // Park the live capturer for the next session (None if it died and never reopened),
    // releasing its session-scoped routing claim (Linux: the default sink moves back;
    // Windows: dropped, restoring the operator's default playback device).
    if let Some(mut c) = capturer {
        c.idle();
        crate::audio::park_audio_capture(&audio_cap, c);
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
    _budget: punktfunk_core::audio::AudioBudget,
) {
    tracing::warn!("punktfunk/1 audio requires Linux or Windows — session continues without it");
}
