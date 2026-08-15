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
    /// One protocol frame of wall time — the cadence paced sends aim for.
    const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(FRAME_MS as u64);
    /// Ceiling on a single pacing sleep. The capture channel is finite and `next_chunk` has to be
    /// serviced; sleeping past a couple of frames would trade a burst on the wire for a drop at
    /// the capturer, which is strictly worse (a drop is a click AND a permanent shift).
    const PACE_MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(10);
    /// How far behind schedule the pacer may fall before it stops trying to catch up and simply
    /// re-anchors. Chasing an old schedule after a stall would send a burst — the exact thing
    /// pacing exists to prevent — so past this point the debt is forgiven, not repaid.
    const PACE_REANCHOR: std::time::Duration = std::time::Duration::from_millis(100);
    let want = punktfunk_core::audio::normalize_channels(channels);
    // Same boost the video capture/encode loop takes, and this thread needs it MORE: it paces
    // 5 ms datagrams, so a scheduling stall here is directly audible where a late video frame
    // is one presentation slip. The 2026-08-14 field log's stutter was exactly this thread
    // descheduled by fresh-game-launch shader storms — it carried no priority at all.
    pf_frame::thread_qos::boost_thread_priority(true);
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
    // Operator capture gain, soft-limited (`PUNKTFUNK_AUDIO_GAIN`, default 1.0 = untouched). This
    // plane had NO gain at all until now, so `PUNKTFUNK_AUDIO_GAIN` silently did nothing on
    // punktfunk/1 while working on GameStream — and since WASAPI loopback taps upstream of the
    // endpoint's master volume, there was no other host-side way to lift a quiet desktop mix.
    // Read once per session rather than per frame: this is an operator setting, not a live control.
    let gain = crate::audio::capture_gain();
    if gain != 1.0 {
        tracing::info!(
            gain,
            "audio: applying operator capture gain (soft-limited above \
             {}; headroom, not loudness)",
            punktfunk_core::audio::SOFT_LIMIT_KNEE
        );
    }
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    // The frame currently being encoded. Reused rather than collected fresh each time: it is
    // filled from `acc` on the normal path and padded out with silence on the infill path, and
    // one buffer covers both without allocating 200 times a second.
    let mut frame_buf: Vec<f32> = Vec::with_capacity(frame_len);
    // Sized for the largest surround frame (7.1 HQ ≈ 1.3 KB at 5 ms); ample for normal quality.
    let mut opus_buf = vec![0u8; 4096];
    let mut seq: u32 = 0;
    // W-B1 — whether the wire covers a capture hole with silence, and for how long. See
    // [`InfillPolicy`]: before this, a hole meant the loop simply blocked in `next_chunk` and
    // NOTHING left the host for its duration, so the client's ring drained → underran →
    // de-primed → re-primed, and a 30 ms hole became a much longer audible artifact.
    let mut infill = crate::audio::capture_policy::InfillPolicy::default();
    let mut last_chunk_at = std::time::Instant::now();
    // Nothing may be synthesized before the first real frame: there is no continuity to protect
    // yet, and the wire clock has no anchor to continue from.
    let mut sent_any = false;
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
    // W1.1/W1.2 — the audio SAMPLE clock, and the schedule frames leave on.
    //
    // `pts_ns` used to be `now_ns()` evaluated inside the drain loop below, which made it the
    // instant we got round to ENCODING rather than the instant the samples were CAPTURED. Every
    // frame carved out of one capture chunk therefore carried a near-identical timestamp, and the
    // value drifted with encoder scheduling. That was harmless only for as long as nothing
    // consumed it; a client-side A/V sync loop regulating against it would be regulating against
    // a fiction, so this is a prerequisite for the whole overhaul, not a tidy-up.
    //
    // `pace_due` exists because a chunk is not a frame. A capture callback hands us a whole
    // quantum (5 ms when the graph honours our ask, 21.3 ms on a VM that clamps it to 1024 —
    // see `audio::linux`'s quantum warning), and the old loop drained all of it into
    // back-to-back `send_datagram` calls. The wire then carried a 4-5 frame burst followed by
    // ~21 ms of nothing, and a client ring can only absorb that by standing at least a burst
    // period deep. Releasing frames on the audio clock instead costs no AVERAGE latency — the
    // client was buffering those frames anyway — and removes the burst the ring was sized for.
    // Uninitialised on purpose: every read is preceded by the re-anchor at the top of the chunk
    // loop, and seeding it with a placeholder would just be a value the compiler correctly points
    // out is never read.
    //
    // Seeded rather than left uninitialised now that infilled frames advance it too: it is the
    // pts of the NEXT frame to leave, real or synthesized, and every send advances it by one
    // frame. `sent_any` is what keeps the seed from ever reaching the wire.
    let mut next_pts_ns: u64 = 0;
    let mut pace_due: Option<std::time::Instant> = None;
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
        // Wake on whichever comes first: a capture chunk, or the moment the wire next has
        // something to say. Waiting only on capture is what made a hole cost more than the audio
        // it swallowed — see [`InfillPolicy`].
        let waited = if infill.exhausted() || !sent_any {
            // Nothing is owed until real audio returns: either the infill budget is spent (the
            // host is not glitching, it is QUIET) or nothing has ever been sent, so there is no
            // continuity to hold. Block the way this loop always did rather than waking two
            // hundred times a second to decide to stay silent — a session that starts on a quiet
            // desktop would otherwise spin until the first sound.
            capturer.as_mut().unwrap().next_chunk()
        } else {
            let now = std::time::Instant::now();
            // A frame that is due but has no audio behind it cannot be acted on until the hole is
            // old enough to be worth covering, so wait for the LATER of the two — waiting only for
            // the due time would spin through the window between them.
            let ready_at = match pace_due {
                Some(due) if acc.len() >= frame_len => due,
                Some(due) => due.max(last_chunk_at + crate::audio::capture_policy::INFILL_AFTER),
                None => now + FRAME_INTERVAL,
            };
            let budget = ready_at.saturating_duration_since(now).min(PACE_MAX_SLEEP);
            capturer.as_mut().unwrap().next_chunk_within(budget)
        };
        let chunk = match waited {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture lost — reopening");
                capturer = None;
                last_failed = Some(std::time::Instant::now());
                continue;
            }
        };
        if !chunk.is_empty() {
            if infill.chunk_arrived() {
                // The wire fell silent across that hole. The partial frame in `acc` and the
                // redundancy predecessor both describe audio from before a discontinuity, so
                // splicing either onto what follows is a click plus a pts that lies about it.
                acc.clear();
                prev_frame.clear();
            }
            last_chunk_at = std::time::Instant::now();
            // Anchor the sample clock on THIS chunk's arrival. PipeWire hands us a buffer of
            // already captured audio, so the newest sample in `acc` is ~now and the oldest is one
            // whole buffer-occupancy earlier. Re-deriving the anchor every chunk (rather than
            // free-running a counter) keeps the stamp tied to the capture device's own cadence, so
            // a drifting or resampling graph corrects itself instead of accumulating error over a
            // long session.
            let arrival_ns = now_ns();
            acc.extend_from_slice(&chunk);
            let queued_frames = (acc.len() / want as usize) as u64;
            let anchor =
                arrival_ns.saturating_sub(queued_frames * 1_000_000_000 / SAMPLE_RATE as u64);
            // Never step backwards. Infilled frames advanced the wire clock while capture was
            // away, and an anchor re-derived from this chunk's arrival can land at or before the
            // last frame we already sent.
            next_pts_ns = anchor.max(next_pts_ns);
        }
        // Everything the wire owes for the slots that have come due — real or synthesized, one
        // schedule, one encoder, one `seq`. A schedule that has fallen more than one frame behind
        // is re-anchored rather than chased, so a scheduling hiccup cannot turn into a permanent
        // send-time debt.
        loop {
            let now = std::time::Instant::now();
            match pace_due {
                Some(due) if due > now => break, // this frame's slot has not arrived yet
                Some(due) if now.duration_since(due) > PACE_REANCHOR => pace_due = None,
                _ => {}
            }
            frame_buf.clear();
            if acc.len() >= frame_len {
                frame_buf.extend(acc.drain(..frame_len));
            } else if !sent_any {
                break;
            } else {
                match infill.decide(last_chunk_at.elapsed()) {
                    crate::audio::capture_policy::Infill::Silence => {
                        // Pad the partial frame out with silence and send THAT, rather than
                        // leaving it for post-gap samples to complete: one frame carrying audio
                        // from both sides of a hole is a click, and its pts is a lie about when
                        // half of it was captured.
                        frame_buf.append(&mut acc);
                        frame_buf.resize(frame_len, 0.0);
                    }
                    crate::audio::capture_policy::Infill::Wait
                    | crate::audio::capture_policy::Infill::Quiet => break,
                }
            }
            pace_due = Some(pace_due.unwrap_or_else(std::time::Instant::now) + FRAME_INTERVAL);
            if gain != 1.0 {
                punktfunk_core::audio::apply_gain(&mut frame_buf, gain);
            }
            let pts_ns = next_pts_ns;
            next_pts_ns += FRAME_MS as u64 * 1_000_000;
            match enc.encode_float(&frame_buf, &mut opus_buf) {
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
                    // From here there is a continuity worth protecting, and `next_pts_ns` has a
                    // real anchor to continue from — both preconditions for synthesizing anything.
                    sent_any = true;
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
