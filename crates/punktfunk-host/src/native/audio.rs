//! The native audio plane (plan §W1 — carved out of the [`super`] module): desktop capture →
//! either
//!
//! - **Opus** (48 kHz, 5 ms, constrained VBR at the configured
//!   [`AudioTier`](punktfunk_core::audio::AudioTier)) → `AUDIO_MAGIC` QUIC datagrams, or
//!   `AUDIO_RED_MAGIC` when the session negotiated redundancy — the default and the fallback; or
//! - **lossless PCM** (both rate families — 44.1/48/88.2/96/176.4 kHz — 16/24-bit, any negotiated
//!   channel count, a negotiated frame duration) → `AUDIO_PCM_MAGIC` datagrams, when the
//!   handshake's §8.4 gate resolved the hi-res plane (`design/hi-res-audio.md`).
//!
//! …at the negotiated channel count. Which plane a session runs is decided ONCE, at handshake,
//! and never switches underneath the client: its output device is open at a fixed rate, so a
//! change would mean a re-open (§6). The encoder ([`NativeAudioEnc`]) and the
//! capture/encode/send loop ([`audio_thread`]) are gated to linux/windows (libopus + a real
//! capturer); other targets get the stub, so a dev build streams video-only rather than failing
//! to compile.
//!
//! Two things here deliberately DIVERGE from the GameStream plane, which used to share this
//! tuning: hard CBR (its audio FEC needs fixed-size packets; this plane has no FEC, so CBR was a
//! pure quality tax) and the fixed 128 kbps stereo bitrate. See [`NativeAudioEnc::new`].

use super::*;

#[cfg(any(target_os = "linux", target_os = "windows"))]
use punktfunk_core::audio::pcm;

/// The audio plane's wire clock — the `pts_ns` every datagram is stamped with.
///
/// An **anchor plus a running total of interleaved samples**, not an accumulator advanced once per
/// frame. That shape is the whole point:
///
/// ⚠⚠ **The negotiated `frame_us` is a LABEL, not a duration.** A frame carries a whole number of
/// samples PER CHANNEL, and `pcm::samples_per_frame` floors — so a rung is a real duration only
/// when the rate divides it. Every rung divides the 48 kHz family; **none of the seven divides
/// 44 100 Hz**, 88 200 divides only 5 000 µs and 176 400 only 5 000 and 2 500. A "5 ms" frame at
/// 44 100 Hz is 220 samples per channel — `frame_duration_ns` of it is 4 988 662 ns, not
/// 5 000 000. Stamping `pts += frame_us * 1000` therefore runs the clock **2 268 ppm fast**: 2.3
/// ms of invented time every second, for the life of the session.
///
/// ⚠ And it is not self-correcting. [`reanchor`](Self::reanchor) only ever moves the clock
/// FORWARD (a capture arrival must not un-send frames already on the wire), so a clock running
/// slow is pulled up by the next chunk while a clock running fast is never pulled back by
/// anything. The client's A/V sync loop then chases a drift manufactured at the source, and every
/// stat agrees with it, because the timestamps are self-consistent and simply wrong.
///
/// Counting samples cannot drift: the sample count IS the frame. A running total beats even a sum
/// of per-frame `frame_duration_ns` values — that sum accumulates just under 1 ns per frame
/// (~0.2 µs/s, irrelevant next to 2.3 ms/s, but not nothing), while this accumulates zero.
#[cfg(any(target_os = "linux", target_os = "windows"))]
struct PtsClock {
    /// Wall-clock nanoseconds the current run of samples is measured from.
    base_ns: u64,
    /// Interleaved samples charged to the clock since [`base_ns`](Self::base_ns).
    samples: usize,
    rate_hz: u32,
    channels: u8,
    /// Interleaved samples in one second — the fold factor [`advance`](Self::advance) uses to keep
    /// `samples` bounded. Precomputed: it is a property of the plane, not of a frame.
    samples_per_sec: usize,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl PtsClock {
    fn new(rate_hz: u32, channels: u8) -> PtsClock {
        PtsClock {
            base_ns: 0,
            samples: 0,
            rate_hz,
            channels,
            samples_per_sec: rate_hz as usize * channels as usize,
        }
    }

    /// The pts of the NEXT frame to leave, real or synthesized.
    fn pts_ns(&self) -> u64 {
        self.base_ns + pcm::frame_duration_ns(self.samples, self.rate_hz, self.channels)
    }

    /// Charge one frame of `samples` interleaved samples: it has left the host.
    fn advance(&mut self, samples: usize) {
        self.samples += samples;
        // Fold whole seconds into the base so a long session cannot grow the total without bound
        // (`frame_duration_ns` takes a `usize`, which is 32 bits on some targets, and 176 400 Hz
        // 7.1 is 1.4 M samples a second). EXACT, so the fold is invisible to `pts_ns`:
        // `rate_hz × channels` interleaved samples are 1 000 000 000 ns at every rate this plane
        // carries, with no remainder to round away.
        if self.samples_per_sec > 0 && self.samples >= self.samples_per_sec {
            let secs = self.samples / self.samples_per_sec;
            self.base_ns += secs as u64 * 1_000_000_000;
            self.samples -= secs * self.samples_per_sec;
        }
    }

    /// Re-anchor on a capture arrival — **forward only**.
    ///
    /// Infilled frames advanced the wire clock while capture was away, and an anchor re-derived
    /// from a chunk's arrival can land at or before the last frame already sent; moving back would
    /// re-issue a pts the client has already played. Re-anchoring restarts the running total,
    /// because the new base already describes everything the old base plus its samples did.
    fn reanchor(&mut self, anchor_ns: u64) {
        if anchor_ns > self.pts_ns() {
            self.base_ns = anchor_ns;
            self.samples = 0;
        }
    }
}

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

/// The audio thread: desktop capture → the session's resolved audio plane (Opus on
/// `AUDIO_MAGIC`/`AUDIO_RED_MAGIC`, or lossless PCM on `AUDIO_PCM_MAGIC`) at the negotiated
/// `channels` (2 stereo / 6 = 5.1 / 8 = 7.1, canonical wire order FL FR FC LFE RL RR SL SR).
/// QUIC already encrypts; no extra layer. The capturer comes from (and returns to) the persistent
/// slot — see [`AudioCapSlot`].
///
/// `plane` is the format the `Welcome` states — read back off it by the caller, never recomputed,
/// so the wire the client was promised and the wire we send cannot disagree.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(super) fn audio_thread(
    conn: quinn::Connection,
    stop: Arc<AtomicBool>,
    audio_cap: AudioCapSlot,
    channels: u8,
    budget: punktfunk_core::audio::AudioBudget,
    plane: super::handshake::AudioPlane,
) {
    use crate::audio::SAMPLE_RATE;
    const FRAME_MS: usize = 5;
    /// Ceiling on a single pacing sleep. The capture channel is finite and `next_chunk` has to be
    /// serviced; sleeping past a couple of frames would trade a burst on the wire for a drop at
    /// the capturer, which is strictly worse (a drop is a click AND a permanent shift).
    const PACE_MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(10);
    /// How far behind schedule the pacer may fall before it stops trying to catch up and simply
    /// re-anchors. Chasing an old schedule after a stall would send a burst — the exact thing
    /// pacing exists to prevent — so past this point the debt is forgiven, not repaid.
    const PACE_REANCHOR: std::time::Duration = std::time::Duration::from_millis(100);
    /// How much audio is faded at each edge of a capture hole — out into the hole, in out of
    /// it — in µs. One millisecond: long enough to be a slope rather than an edge, far too short
    /// to read as a swell. See `last_real` / `resume_fade` below.
    const EDGE_FADE_US: u64 = 1_000;
    let want = punktfunk_core::audio::normalize_channels(channels);
    // The three session values that used to be compile-time constants. Every one of them is now
    // a property of the negotiated plane, and everything downstream — the pacer, the sample
    // clock, the capture open, the scratch buffers — is derived from these rather than from
    // `SAMPLE_RATE`/`FRAME_MS` directly.
    let pcm_plane = plane.is_pcm();
    let rate_hz = if pcm_plane {
        plane.rate_hz
    } else {
        SAMPLE_RATE
    };
    let bits = plane.bits;
    // Opus is the fixed 5 ms of `0xC9`; the PCM plane's duration was negotiated from the
    // connection's datagram size. The `max(1)` is a floor against a malformed plane rather than a
    // real case — the §8.4 gate never produces a PCM plane with a zero frame — but a zero here
    // would divide the pacer by nothing and produce empty frames at infinite rate.
    let frame_us: u32 = if pcm_plane {
        (plane.frame_us as u32).max(1)
    } else {
        FRAME_MS as u32 * 1000
    };
    // Interleaved samples in one protocol frame, derived from the negotiated rate and frame
    // duration rather than from a constant. THE single source of truth for how long a frame is —
    // the client's ring drains exactly this many, so both ends agree by construction rather than
    // by re-deriving `rate × µs` and hoping they round the same way (`pcm::samples_per_frame`).
    //
    // ⚠ Exact only on the 48 kHz family. Every ladder rung divides 48 000 and 96 000 into whole
    // samples per channel; **none of them divides 44 100**, and 88 200/176 400 divide only the
    // longest one or two. On those rates this FLOORS, so a frame is up to one sample per channel
    // shorter than the rung it is labelled with — safe for sizing (the payload can only shrink
    // inside its datagram) and wrong for timing, which is why the pts below is stamped from
    // `frame_duration_ns` and not from `frame_us`.
    let frame_len = pcm::samples_per_frame(rate_hz, frame_us, want);
    // One protocol frame of wall time — the cadence paced sends aim for. A `let`, not a const:
    // the PCM plane's frames are shorter than 5 ms whenever the format does not fit a datagram
    // at that length (§4.2), and a 96/24 session paces 500 of them a second.
    //
    // Measured from the frame's REAL sample count for the same reason the pts is, even though the
    // consequence here is far smaller: this only decides when the loop next looks for work, and
    // every release is additionally gated on `acc` actually holding a frame, so a rung-length
    // interval would produce a slot the pacer waits out rather than time it invents. Cosmetic or
    // not, two clocks describing the same frame must not disagree — a 0.23 % gap between them is
    // exactly the kind of thing a later reader reconciles in the wrong direction. On the 48 kHz
    // family this is bit-identical to `from_micros(frame_us)`.
    //
    // The `max(1)` guards the same malformed-plane case `frame_us` does above: a sub-microsecond
    // rung would floor `frame_len` to zero samples and leave the pacer with a zero interval to
    // spin on. The §8.4 gate never produces one (the shortest ladder rung is 1 000 µs).
    let frame_interval =
        std::time::Duration::from_nanos(pcm::frame_duration_ns(frame_len, rate_hz, want).max(1));
    // Same boost the video capture/encode loop takes, and this thread needs it MORE: it paces
    // 5 ms datagrams, so a scheduling stall here is directly audible where a late video frame
    // is one presentation slip. The 2026-08-14 field log's stutter was exactly this thread
    // descheduled by fresh-game-launch shader storms — it carried no priority at all.
    pf_frame::thread_qos::boost_thread_priority(true);
    // Tier and redundancy are ONE decision, budgeted against the session's video bitrate — see
    // `handshake::audio_budget`. An unparseable `audio.quality` was already warned about there
    // and fell back to the default, so nothing here can silently downgrade someone's audio.
    //
    // Redundancy is FORCED off on the lossless plane (§4.5): `0xD2` is not defined for `0xD3`,
    // there is no PCM-side decoder that would receive it, and doubling a 1.4–33.9 Mbps plane is
    // absurd on its face. The handshake already refuses to grant both bits together, so this is
    // a second lock on the same door — cheap, and it means no future change to the budget ladder
    // can switch redundancy on behind this branch.
    let (tier, redundancy) = (budget.tier, budget.redundancy && !pcm_plane);

    // Reuse the cached capturer ONLY when its channel count AND rate match this session's; a
    // stereo capturer left by a prior session must not feed a 5.1/7.1 session (the encoder + the
    // client's decoder are sized for `want`, so a mismatched capturer would garble/desync the
    // audio), and a 48 kHz one must not feed a 96 kHz session — the frame arithmetic below is
    // denominated in the negotiated rate, so a mismatch there is a pitch shift plus a sample
    // clock that drifts against the wire clock at exactly the ratio of the two rates.
    // A FAILED first open does not end the session's audio: session start is peak endpoint churn
    // on Windows (the virtual-display attach and the wiring plan's own default-device flips race
    // the WASAPI activate — 0x80070002 mid-re-registration), so it enters the same
    // reopen-with-backoff loop a mid-session capture death does; audio then starts a few seconds
    // late instead of never.
    let capturer = match audio_cap.lock().unwrap().take() {
        Some(mut c) if c.channels() == want as u32 && c.sample_rate() == rate_hz => {
            c.drain(); // discard audio captured between sessions (also re-claims routing)
            Some(c)
        }
        prev => {
            drop(prev); // wrong channel count/rate (or none): clean teardown, open fresh
            match crate::audio::open_audio_capture(want as u32, rate_hz) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "punktfunk/1 audio failed to open — retrying in the background until it comes up");
                    None
                }
            }
        }
    };
    // No Opus encoder at all on the PCM plane — there is nothing for it to do, and building one
    // would make a libopus failure able to kill a session that does not use libopus.
    let mut enc = if pcm_plane {
        None
    } else {
        match NativeAudioEnc::new(want, tier) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(error = %e, "opus encoder init failed — session continues without audio");
                if let Some(mut c) = capturer {
                    c.idle(); // parked, not streaming — release the routing claim
                    crate::audio::park_audio_capture(&audio_cap, c);
                }
                return;
            }
        }
    };

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
    // Empty on the PCM plane, which has no Opus encoder to write into it.
    let mut opus_buf = vec![0u8; if pcm_plane { 0 } else { 4096 }];
    // The PCM plane's own scratch. Sized EXACTLY — `frame_payload_bytes` is not an estimate but
    // the size every frame has, and the 4 KB Opus guess would be both too small for 96/24 at the
    // longest rungs and meaningless as a bound. `from_f32` appends, so this is cleared per frame
    // and never reallocates after the first.
    let mut pcm_wire: Vec<u8> = if pcm_plane {
        Vec::with_capacity(pcm::frame_payload_bytes(rate_hz, bits, want, frame_us))
    } else {
        Vec::new()
    };
    let mut seq: u32 = 0;
    // W-B1 — whether the wire covers a capture hole with silence, and for how long. See
    // [`InfillPolicy`]: before this, a hole meant the loop simply blocked in `next_chunk` and
    // NOTHING left the host for its duration, so the client's ring drained → underran →
    // de-primed → re-primed, and a 30 ms hole became a much longer audible artifact.
    //
    // Built from THIS session's frame, like the pacer above it. Both of the policy's figures used
    // to be written against the Opus 5 ms frame — correct for as long as 5 ms was the only frame
    // there was, and off by up to 5× on the lossless plane, whose frames are shorter by
    // construction (§4.2).
    let mut infill = crate::audio::capture_policy::InfillPolicy::new(frame_us);
    let mut last_chunk_at = std::time::Instant::now();
    // Nothing may be synthesized before the first real frame: there is no continuity to protect
    // yet, and the wire clock has no anchor to continue from.
    let mut sent_any = false;
    // The two EDGES of a hole. The infill used to be digital zero from its first sample, and
    // zero is not the absence of sound — it is a step from whatever level the last real sample
    // sat at down to nothing, which the listener hears as a click at the front of every hole
    // (and which the codec faithfully encodes); the audio then resumes mid-waveform, a second
    // step. The Skynet field host opens ~2 such holes a second. So the first synthesized frame
    // fades the audio OUT over `EDGE_FADE_US` and the first real frame after the hole fades it
    // back IN over the same span, both on the same raised cosine — a crossfade through silence
    // rather than a step at each end. `last_real` is what the fade-out is built from when a hole
    // opens on an empty partial (nothing of the pre-hole audio left in hand to fade).
    let mut last_real: Vec<f32> = Vec::with_capacity(frame_len);
    let mut resume_fade = false;
    // In interleaved samples: `frames × channels`, so the curve spans whole frames. (Adjacent
    // channels of one frame land one step apart on the curve — a 1/n gain difference, nothing at
    // any fade longer than a few frames.)
    let edge_fade_samples = (rate_hz as u64 * EDGE_FADE_US / 1_000_000) as usize * want as usize;
    // Backlog above which a slot sends TWO frames — see the bonus arm in the loop below. Sized
    // from the largest capture chunk this session has actually seen (the graph's quantum, or a
    // VM's clamped 1024 frames), plus one full protocol frame: anything up to that is a chunk
    // being paced out, anything past it is audio that arrived faster than the schedule sends.
    let mut max_chunk_len: usize = 0;
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
    // frame. `sent_any` is what keeps the seed from ever reaching the wire. See [`PtsClock`] for
    // why it counts SAMPLES rather than accumulating the negotiated frame length.
    let mut clock = PtsClock::new(rate_hz, want);
    let mut pace_due: Option<std::time::Instant> = None;
    // WP-C — what the wire actually did, as opposed to what the tap handed us. See [`SendStats`]:
    // until this existed the send path was the one stage of the audio pipeline that could not be
    // ruled in or out from a field log.
    //
    // Built from this session's frame, like the pacer and the infill policy: its `late` counter is
    // "missed its slot by a whole protocol frame", and the lossless plane's frame is as short as
    // 1 ms. Against the Opus constant this session could miss every slot and still report zero.
    let mut send_stats = crate::audio::capture_policy::SendStats::new(frame_us);
    let mut last_send_stats = std::time::Instant::now();
    let mut last_departure: Option<std::time::Instant> = None;
    // §4.8 — datagrams the wire refused. Counted, not merely survived: an uncounted drop is what
    // makes a field report un-triageable (the lesson WP0.2 wrote down for the capture side), and
    // on the PCM plane there is no PLC to hide one.
    let mut oversized_drops: u64 = 0;
    // The Opus plane's capture-rate warning fires at most once per session — the condition is
    // re-tested a few hundred times a second, and it is a statement about the capturer, not an
    // event.
    let rate_mismatch_warned = std::sync::Once::new();
    if capturer.is_some() {
        tracing::info!(
            channels = want,
            // The plane this session actually runs, stated rather than assumed. The old line
            // said "Opus 48 kHz, 5 ms datagrams" as a literal, which was true of every session
            // that existed when it was written and is now one of four possible answers.
            plane = if pcm_plane { "0xD3 PCM" } else { "0xC9 Opus" },
            lossless = pcm_plane,
            rate_hz,
            bits,
            frame_us,
            // Meaningful only on the Opus plane — PCM has no tier and no rate control; its cost
            // is exactly `rate × bits × channels`.
            tier = tier.as_str(),
            kbps = if pcm_plane {
                pcm::bitrate_kbps(rate_hz, bits, want)
            } else {
                budget.kbps
            },
            redundancy,
            "punktfunk/1 audio streaming"
        );
    }
    'session: while !stop.load(Ordering::SeqCst) {
        if capturer.is_none() {
            if last_failed.is_some_and(|t| t.elapsed() < INJECTOR_REOPEN_BACKOFF) {
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            match crate::audio::open_audio_capture(want as u32, rate_hz) {
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
        // ⚠ Never ship a rate we did not get (`design/hi-res-audio.md` §9: "never claim a rate
        // you did not get" — the rule is the same at both ends).
        //
        // **BELT AND BRACES, not the mechanism.** §8.4 condition 4 now asks the capture path what
        // rate it can honestly deliver BEFORE the `Welcome` is built
        // (`handshake::resolve_audio_plane` + `crate::audio::probe_capture_rate`), so the Windows
        // §8.2 decline and the Linux §8.3 one both land as an ordinary "the session uses Opus
        // 48 kHz" with a logged reason. This check is no longer how either of those is reached,
        // and it is no longer expected to fire at all.
        //
        // What is left for it is the race the gate structurally cannot close: the probe reads the
        // device, and the capture opens some milliseconds later. An endpoint whose format an
        // operator changes in between, a hotplug that re-plans onto a different endpoint, or a
        // graph that renegotiates mid-session all land here. That is a much smaller and much
        // rarer set than "every 96 kHz request on a 48 kHz box", which is what it used to catch.
        //
        // The ACTION stays the same, because there is still only one correct one. The `Welcome`
        // has already promised the client `rate_hz` and its output device is open at exactly
        // that; every pts, every frame length and the client's whole de-jitter ring are
        // denominated in it. We cannot switch the wire to Opus mid-session (§6 — a session runs
        // one plane, and changing it means re-opening the client's device), and sending
        // wrongly-clocked samples under the promised label is precisely the "label right, content
        // wrong" failure this feature exists to avoid. So the lossless plane ENDS. What changed
        // is the message: it no longer tells the operator to go and set a device rate, because
        // the gate says that up front now — reaching here means something moved underneath a
        // session the gate had already vetted.
        //
        // The Opus plane only WARNS, and deliberately: it is 48 kHz by definition, every
        // capturer has always been asked for 48 kHz, and both backends resample to it rather
        // than hand back something else. If one ever did, that has been true (and mis-clocked)
        // for the plane's whole life — turning it into a session-ending condition here would
        // risk silencing hosts that work today in order to police a case this pass did not
        // introduce. Say it out loud instead; making it fatal is a decision for whoever has a
        // reproduction.
        //
        // Checked every iteration rather than once at open, because the capturers do not all know
        // their rate at open time: PipeWire's `param_changed` may land a moment after the ready
        // handshake, and either backend can renegotiate mid-session. An atomic load a few hundred
        // times a second costs nothing next to the encode.
        let live_rate = capturer.as_ref().unwrap().sample_rate();
        if live_rate != rate_hz {
            if pcm_plane {
                tracing::warn!(
                    promised_hz = rate_hz,
                    capture_hz = live_rate,
                    "the capture path changed rate under a session the hi-res gate had already \
                     vetted — ending the lossless audio plane rather than sending samples under \
                     a label that is not theirs (video continues). Reconnect: the gate re-runs \
                     against the capture path as it now is, and resolves either the real rate or \
                     Opus 48 kHz"
                );
                break 'session;
            }
            rate_mismatch_warned.call_once(|| {
                tracing::warn!(
                    promised_hz = rate_hz,
                    capture_hz = live_rate,
                    "the capture path reports a rate other than the session's — the Opus plane \
                     continues (it is 48 kHz by definition and the capturer resamples), but the \
                     sample clock and the wire clock will not agree if this is real"
                );
            });
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
                Some(due) => due.max(last_chunk_at + infill.after()),
                None => now + frame_interval,
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
            max_chunk_len = max_chunk_len.max(chunk.len());
            // How big the graph's buffers really are, so the infill threshold is one chunk plus
            // one frame and never the middle of a legitimately long cycle — see
            // `InfillPolicy::after`.
            infill.note_quantum(std::time::Duration::from_nanos(pcm::frame_duration_ns(
                chunk.len(),
                rate_hz,
                want,
            )));
            let queued_frames = (acc.len() / want as usize) as u64;
            // The session's rate, not the module constant: at 96 kHz a 48 000 divisor would put
            // the anchor twice as far into the past as the queued audio really is, so every pts
            // would be early by the whole buffer occupancy and A/V sync would chase it.
            let anchor = arrival_ns.saturating_sub(queued_frames * 1_000_000_000 / rate_hz as u64);
            // Never step backwards — and see [`PtsClock::reanchor`] for why that asymmetry is
            // exactly what makes a fast clock unrecoverable and the sample-exact stamp mandatory.
            clock.reanchor(anchor);
        }
        // Everything the wire owes for the slots that have come due — real or synthesized, one
        // schedule, one encoder, one `seq`. A schedule that has fallen more than
        // `PACE_REANCHOR` behind is re-anchored rather than chased, so a scheduling hiccup cannot
        // turn into a permanent send-time debt.
        //
        // **The schedule is wall clock; the source is not, and the two disagree in both
        // directions.** What this loop owes the client is a wire that carries one frame per
        // frame-time of WALL clock — the client's ring is drained on its own device clock, so
        // any frame this schedule fails to send is a frame the ring goes without, and any frame
        // it sends over is one the ring holds forever:
        //
        // - **Source behind** (capture lost audio, or the graph clock is slow): a slot comes due
        //   with no complete frame in `acc`. That is a hole. The ≥ 10 ms kind was already covered
        //   by the infill policy, measured from the last chunk; the SHORT kind — a missed
        //   2.7 ms graph cycle or two, invisible to the capture gap counter — was not, and it
        //   left the schedule permanently behind by the loss. The 2026-08-17 field log shows the
        //   shape: 33–72 % of departures "late", `max_late_ms` climbing to 99, `reanchors` ≈ 0 —
        //   a lag that only ever accumulated, and was only ever repaid when the next long hole's
        //   infill burst out (lag + 10 ms) / 5 ms silence frames back to back. On the client
        //   that is a ring drained a frame at a time and then refilled with a burst it has to
        //   trim. So the infill decision now looks at the LAG as well as the time since the
        //   last chunk: a schedule `after()` behind with no audio to send is owed a frame of
        //   cover exactly as a hole that long is, and the lag stays bounded at that (a couple
        //   of frames at a frame-sized quantum, one chunk plus a frame on a clamped one).
        // - **Source ahead** (the graph clock is fast, or a chunk landed after a stall): `acc`
        //   holds more than a chunk's worth beyond the frame being sent. Left alone the surplus
        //   is never sent — one frame per slot, forever — so a source a hundred ppm fast grows
        //   the backlog, and the latency, by five milliseconds every fifty seconds for the life
        //   of the session, and only a hole ever takes it back. So a slot whose remaining
        //   backlog exceeds one chunk plus one frame sends a second frame in the same slot
        //   (`bonus`): at most two per slot, so a burst never exceeds what a 10 ms quantum
        //   sends every cycle anyway, and the surplus drains at twice real time until it is gone.
        //
        // Both together bound what `late` can accumulate: a lag can no longer grow past
        // `after()` without being covered, so a `max_late_ms` beyond that band is this thread's
        // own scheduling — the reading WP-C built the counter for — and not the source's.
        let mut bonus_taken = false;
        loop {
            let now = std::time::Instant::now();
            // How far past its slot this frame is leaving. Measured before the re-anchor arm can
            // erase the evidence — that arm is the one that forgives debt silently (WP-C).
            let mut late = std::time::Duration::ZERO;
            match pace_due {
                Some(due) if due > now => break, // this frame's slot has not arrived yet
                Some(due) if now.duration_since(due) > PACE_REANCHOR => {
                    send_stats.observe_reanchor();
                    pace_due = None;
                }
                Some(due) => late = now.duration_since(due),
                None => {}
            }
            frame_buf.clear();
            let mut infilled = false;
            if acc.len() >= frame_len {
                frame_buf.extend(acc.drain(..frame_len));
            } else if !sent_any {
                break;
            } else {
                // The hole is the LATER of "since the last chunk" and "how far the schedule is
                // behind": the first is a graph that stopped feeding us, the second is a graph
                // that fed us less than wall clock — see the loop comment.
                match infill.decide(last_chunk_at.elapsed().max(late)) {
                    crate::audio::capture_policy::Infill::Silence => {
                        infilled = true;
                        // Pad the partial frame out with silence and send THAT, rather than
                        // leaving it for post-gap samples to complete: one frame carrying audio
                        // from both sides of a hole is a click, and its pts is a lie about when
                        // half of it was captured. (And it is not dropped either: those samples
                        // are audio the wire owes, and every sample dropped here would come
                        // straight back as schedule lag — see the loop comment.)
                        let partial = acc.len();
                        frame_buf.append(&mut acc);
                        if infill.covered() == infill.frame() {
                            // First frame of the hole: the audio does not stop at a step. Fade
                            // the tail of what we have over up to `EDGE_FADE_US` — the partial
                            // frame if there is one, else the same slice of the last real frame
                            // again, so the fade is a slope from the level the listener was at
                            // down to nothing rather than an edge there.
                            if partial == 0 && !last_real.is_empty() {
                                let n = edge_fade_samples.min(last_real.len());
                                frame_buf.extend_from_slice(&last_real[..n]);
                            }
                            let n = frame_buf.len().min(edge_fade_samples);
                            pcm::raised_cosine_tail(&mut frame_buf, n);
                        }
                        frame_buf.resize(frame_len, 0.0);
                        // Whatever follows this hole starts mid-waveform.
                        resume_fade = true;
                    }
                    crate::audio::capture_policy::Infill::Wait
                    | crate::audio::capture_policy::Infill::Quiet => break,
                }
            }
            // A slot whose remaining backlog exceeds one chunk plus one whole frame sends a
            // second frame in the same slot — see the loop comment. Decided on what is left
            // AFTER this frame, so the frame that merely completes a chunk never triggers it,
            // and never twice in a row, so a big surplus drains at 2× and not in one burst.
            let bonus = !infilled && !bonus_taken && acc.len() >= max_chunk_len + frame_len;
            pace_due = match pace_due {
                Some(due) if bonus => Some(due), // same slot again for the next frame
                other => Some(other.unwrap_or_else(std::time::Instant::now) + frame_interval),
            };
            bonus_taken = bonus;
            if !infilled {
                if gain != 1.0 {
                    punktfunk_core::audio::apply_gain(&mut frame_buf, gain);
                }
                if std::mem::take(&mut resume_fade) {
                    // First real frame after a hole: fade it in from silence, the mirror of the
                    // fade the hole started with.
                    pcm::raised_cosine_head(&mut frame_buf, edge_fade_samples);
                }
                // What the next hole's fade is built from when it opens on an empty partial —
                // recorded post-gain, so the fade sits at the level the listener was hearing.
                last_real.clear();
                last_real.extend_from_slice(&frame_buf);
            }
            // W1.1 — the wire clock. ⚠ Charged the frame's REAL sample count, never the negotiated
            // `frame_us`, which is a label on the 44.1 kHz family and would run this 2.3 ms/s fast
            // forever; see [`PtsClock`]. `frame_buf.len()` rather than `frame_len` because both
            // fill paths above produce exactly `frame_len` today and taking the count off the
            // buffer we are about to send keeps that an observation rather than an assumption.
            let pts_ns = clock.pts_ns();
            clock.advance(frame_buf.len());
            // Build this frame's datagram. Two planes, ONE send path below: the pacing, the
            // telemetry and the send-error handling are properties of the wire, not of the codec,
            // and duplicating them per plane is how the two would drift.
            //
            // `None` = nothing to send for this slot (an Opus encoder error, already counted).
            // The PCM path cannot fail: `from_f32` is scale-and-clamp over a buffer of known
            // length, with no encoder state to get stuck in.
            let datagram: Option<Vec<u8>> = if pcm_plane {
                pcm_wire.clear();
                pcm::from_f32(&frame_buf, bits, &mut pcm_wire);
                Some(punktfunk_core::quic::encode_audio_pcm_datagram(
                    seq, pts_ns, &pcm_wire,
                ))
            } else {
                // Hoisted out of the `match` scrutinee on purpose: the arms below take an
                // IMMUTABLE slice of `opus_buf`, and a `&mut opus_buf` sitting in a scrutinee
                // temporary is live for the whole match statement. Binding the result first ends
                // that borrow at this semicolon and leaves nothing for the reader (or the borrow
                // checker) to reason about.
                let encoded = enc
                    .as_mut()
                    .expect("opus plane has an encoder")
                    .encode_float(&frame_buf, &mut opus_buf);
                match encoded {
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
                        if redundancy {
                            // The predecessor this frame just became. Recorded BEFORE the send
                            // rather than after it, so the drop arm below can clear it: a frame
                            // that never left must not be advertised as frame `seq`'s
                            // predecessor when the client's numbering has already moved past it.
                            prev_frame.clear();
                            prev_frame.extend_from_slice(opus);
                        }
                        Some(d)
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
                        None
                    }
                }
            };
            let Some(d) = datagram else { continue };
            // §4.8 — `SendDatagramError` is FOUR outcomes, and treating all of them as
            // "connection gone" was wrong in a way hi-res makes reachable. A single oversized
            // frame used to kill audio for the whole remaining session, silently: no log, no
            // counter, and the video stream carrying on around it.
            match conn.send_datagram(d.into()) {
                Ok(()) => {
                    seq = seq.wrapping_add(1);
                    // Score the departure against its slot and against the previous one. `now` is
                    // from the top of this iteration — microseconds earlier and one clock read
                    // cheaper, 200 times a second.
                    send_stats.observe_departure(
                        late,
                        last_departure.map(|t| now.duration_since(t)),
                        infilled,
                    );
                    last_departure = Some(now);
                    // From here there is a continuity worth protecting, and `clock` has a real
                    // anchor to continue from — both preconditions for synthesizing anything.
                    sent_any = true;
                }
                // The only outcome that really is "the session is over".
                Err(quinn::SendDatagramError::ConnectionLost(_)) => break 'session,
                // The frame exceeds what this path can carry right now — quinn refuses it
                // outright rather than fragmenting. One frame, not the plane: drop it, advance
                // `seq` so the client SEES a gap and conceals it rather than silently
                // mis-attributing the next frame, and keep going. Warn on powers of two (the
                // idiom the encode-error arm above uses) so a persistently oversized format
                // says so once, then twice, then four times, instead of 400 times a second.
                //
                // Persistent rather than transient is the case worth reading for: it means the
                // negotiated `audio_frame_us` was sized against a datagram budget this path
                // turned out not to have — see the MTU note in `handshake::negotiate`.
                Err(quinn::SendDatagramError::TooLarge) => {
                    oversized_drops += 1;
                    if oversized_drops.is_power_of_two() {
                        tracing::warn!(
                            count = oversized_drops,
                            frame_us,
                            rate_hz,
                            bits,
                            max_datagram = conn.max_datagram_size(),
                            "audio datagram rejected as too large — dropping the frame and \
                             continuing (the session's negotiated audio frame does not fit this \
                             path's datagram size)"
                        );
                    }
                    seq = seq.wrapping_add(1);
                    prev_frame.clear();
                }
                // Datagrams are gone for the rest of the connection — the peer never supported
                // them, or the transport disabled them. Nothing this loop can do will make the
                // next frame land, so end the audio plane cleanly (the capturer is parked below,
                // the session keeps streaming video) instead of burning a core paced against a
                // wire that cannot take it. Logged once, by construction: this arm breaks.
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "the QUIC datagram path is unavailable — ending the audio plane for this \
                         session (video continues)"
                    );
                    break 'session;
                }
            }
        }
        if last_send_stats.elapsed() >= crate::audio::capture_policy::STATS_EVERY {
            // Deliberately the same window as the capture line, so the two can be read as a pair:
            // holes at the tap with clean departures means the host delivered everything it had,
            // and the search belongs upstream of us.
            tracing::info!(
                sent = send_stats.sent,
                infilled = send_stats.infilled,
                late = send_stats.late,
                max_late_ms = send_stats.max_late_ms(),
                max_spacing_ms = send_stats.max_spacing_ms(),
                reanchors = send_stats.reanchors,
                // §4.8 — frames the wire refused as oversized, cumulative for the session
                // (unlike the rest of this line, which resets per window: a total is what
                // answers "did this ever happen at all", and it is the question a field log
                // gets asked). Zero on every healthy session, which is the point — a counter
                // that reads zero for a REASON rather than by luck.
                oversized_drops,
                "audio egress"
            );
            // The frame rides into the fresh window too — `Default` would have reset it to zero,
            // which is why `SendStats` no longer has one: a zero frame makes every departure
            // "late by a whole frame", and this is the line that would have said so, every 30 s,
            // for the rest of the session.
            send_stats = crate::audio::capture_policy::SendStats::new(frame_us);
            last_send_stats = std::time::Instant::now();
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
    _plane: super::handshake::AudioPlane,
) {
    tracing::warn!("punktfunk/1 audio requires Linux or Windows — session continues without it");
}

#[cfg(all(test, any(target_os = "linux", target_os = "windows")))]
mod tests {
    use super::*;

    /// The number this whole clock exists for, pinned exactly rather than approximately.
    ///
    /// One second of "5 ms" frames at 44 100 Hz stereo: 200 frames of 440 interleaved samples =
    /// 88 000 samples, which is 997 732 426 ns — the second is 0.23 % SHORT of the 200 × 5 ms the
    /// rung is labelled with, because 44 100 divides none of the seven ladder rungs and
    /// `samples_per_frame` floors 220.5 to 220 per channel.
    ///
    /// The old stamp added `frame_us * 1000` per frame and would land on exactly 1 000 000 000 —
    /// 2 267 574 ns of time this host never captured, every second, forever. Planting that error
    /// (`base += frame_us * 1000` in place of `advance`) makes the equality below fail by
    /// 2 267 574 ns and the ppm assertion report 2 268.
    #[test]
    fn the_clock_counts_samples_and_not_nominal_frames() {
        let (rate, us, ch) = (44_100u32, 5_000u32, 2u8);
        let n = pcm::samples_per_frame(rate, us, ch);
        assert_eq!(n, 440, "220 samples per channel, not 220.5");

        let mut c = PtsClock::new(rate, ch);
        let frames = 1_000_000 / us as u64; // 200
        for _ in 0..frames {
            c.advance(n);
        }
        let real_ns = c.pts_ns();
        assert_eq!(real_ns, 997_732_426, "88 000 samples at 44 100 Hz stereo");

        // What the nominal advance would have claimed, and the gap between the two.
        let nominal_ns = frames * us as u64 * 1_000;
        assert_eq!(nominal_ns - real_ns, 2_267_574, "invented ns per second");
        let fast_ppm = (nominal_ns - real_ns) * 1_000_000 / real_ns;
        assert_eq!(fast_ppm, 2_272, "the nominal clock runs this many ppm fast");

        // ⚠ And the drift is CUMULATIVE, which is what makes it a defect rather than an offset:
        // an hour of session is 8.2 seconds of invented time. The re-anchor cannot take any of it
        // back (it only moves forward), so the client's A/V sync loop chases it to the end.
        let hour = 3_600 * (nominal_ns - real_ns) / 1_000_000;
        assert_eq!(hour, 8_163, "ms of drift over an hour");
    }

    /// Summing floored per-frame durations — the alternative core's doc offers — is *also* fine,
    /// and this pins the size of the difference so the choice is on the record rather than
    /// re-litigated. Under 1 ns per frame against the running total, four orders of magnitude
    /// below what the nominal advance invents.
    #[test]
    fn a_running_total_beats_summing_floored_frames_by_a_hair() {
        let (rate, us, ch) = (44_100u32, 5_000u32, 2u8);
        let n = pcm::samples_per_frame(rate, us, ch);
        let frames = 200u64;
        let mut c = PtsClock::new(rate, ch);
        for _ in 0..frames {
            c.advance(n);
        }
        let summed = frames * pcm::frame_duration_ns(n, rate, ch);
        let total = c.pts_ns();
        assert!(
            total >= summed && total - summed < frames,
            "{total} vs {summed}"
        );
    }

    /// On the 48 kHz family a rung IS a duration, so the new clock and the old nominal advance are
    /// the same clock. This is why the defect went unnoticed while the ladder was 48/96 only — and
    /// it is the compatibility claim that matters most: an Opus session's timestamps must not move
    /// by a nanosecond.
    #[test]
    fn the_48k_family_is_bit_identical_to_the_nominal_advance() {
        for (rate, ch) in [(48_000u32, 2u8), (48_000, 6), (48_000, 8), (96_000, 2)] {
            for us in pcm::FRAME_US_LADDER {
                let n = pcm::samples_per_frame(rate, us, ch);
                let mut c = PtsClock::new(rate, ch);
                for i in 1..=400u64 {
                    c.advance(n);
                    assert_eq!(
                        c.pts_ns(),
                        i * us as u64 * 1_000,
                        "{rate} Hz/{ch}ch at {us} µs, frame {i}"
                    );
                }
            }
        }
    }

    /// The fold that keeps the running total bounded must be invisible: it moves whole seconds
    /// from the sample count into the base, and `rate × channels` samples are exactly 1e9 ns at
    /// every rate the plane carries. Run long enough to fold many times, on a rate that divides
    /// nothing.
    #[test]
    fn folding_whole_seconds_does_not_move_the_clock() {
        for (rate, ch) in [(44_100u32, 2u8), (176_400, 8), (88_200, 6), (96_000, 2)] {
            let n = pcm::samples_per_frame(rate, 1_000, ch);
            let mut c = PtsClock::new(rate, ch);
            let mut unfolded: u128 = 0;
            for _ in 0..5_000 {
                c.advance(n);
                unfolded += n as u128;
                // The clock must always equal the duration of every sample ever charged to it,
                // computed in one shot from zero — the property the fold could silently break.
                assert!(
                    c.samples < c.samples_per_sec,
                    "{rate}/{ch}ch: the total was not folded"
                );
                assert_eq!(
                    c.pts_ns(),
                    (unfolded * 1_000_000_000 / (rate as u128 * ch as u128)) as u64,
                    "{rate} Hz/{ch}ch after {unfolded} samples"
                );
            }
        }
    }

    /// The re-anchor is forward-only, and it restarts the running total rather than adding to it.
    /// Both halves matter: moving back would re-issue a pts the client has already played, and
    /// keeping the count across a re-anchor would charge the same span twice.
    #[test]
    fn the_reanchor_only_ever_moves_the_clock_forward() {
        let (rate, ch) = (44_100u32, 2u8);
        let n = pcm::samples_per_frame(rate, 5_000, ch);
        let mut c = PtsClock::new(rate, ch);
        c.reanchor(1_000_000_000);
        assert_eq!(c.pts_ns(), 1_000_000_000);
        c.advance(n);
        let after = c.pts_ns();
        assert_eq!(after, 1_000_000_000 + 4_988_662);
        // An anchor behind the wire clock — capture returning after infilled frames advanced it —
        // is ignored outright.
        c.reanchor(1_000_000_000);
        assert_eq!(c.pts_ns(), after, "a late anchor must not rewind the wire");
        c.reanchor(after);
        assert_eq!(c.pts_ns(), after, "an equal anchor is not a step either");
        // Forward is taken, and the total restarts from it rather than compounding.
        c.reanchor(after + 1_000_000);
        assert_eq!(c.pts_ns(), after + 1_000_000);
        c.advance(n);
        assert_eq!(c.pts_ns(), after + 1_000_000 + 4_988_662);
    }
}
