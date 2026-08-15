//! Lossless PCM for the `0xD3` audio plane.
//!
//! The `0xC9` plane carries Opus, which is transparent but lossy and — by construction — 48 kHz
//! only (RFC 6716; `opus_encoder_create` rejects 96 000). This module is the second plane's
//! payload: interleaved little-endian integer samples, no codec, no container.
//!
//! **Why no codec.** The obvious alternative was FLAC, and it was measured against this on the
//! four axes that decide it (`design/hi-res-audio.md` §5):
//!
//! - A datagram that exceeds the path MTU is not sent *at all*, and this plane is never
//!   fragmented — so [`frame_us_for`] must size frames from the **worst case**. FLAC's worst
//!   case is a VERBATIM subframe: raw samples plus a frame header. So FLAC and PCM get the same
//!   negotiated frame duration, the same packet rate, and the same send-buffer sizing. The codec
//!   buys *average* bytes on the wire and nothing structural.
//! - The plane rides outside the ABR loop, so it is provisioned for peak, not average — which is
//!   the number a codec's typical-case saving does not move.
//! - The host scales f32 capture to 24-bit with no dither, so the low bits of a float game mix
//!   are close to incompressible. At 24 bits — the depth that is the entire point of the feature
//!   — a lossless coder saves the least.
//! - PCM adds no dependency to the NDK / xcframework / flatpak / MSIX / Arch packaging targets,
//!   and no spike gate.
//!
//! Both formats deliver the identical product claim: bit-exact playback with no lossy stage.
//!
//! **On "lossless".** Neither depth is bit-exact against the f32 engine mix it came from — the
//! host quantises once, deliberately and without dither ([`from_f32`]). What this plane
//! guarantees is that nothing is lost *after* that quantisation: [`from_f32`] → [`to_f32`] →
//! [`from_f32`] is the identity, proven by test, so the samples the client renders are the
//! samples the host captured.

/// The `0xD3` datagram's fixed header: tag + `u32` seq + `u64` pts_ns, the same shape as `0xC9`
/// so the gap tracker and the A/V-sync plumbing work unchanged.
/// `quic::datagram` asserts this against its own encoder.
pub const PCM_HEADER_LEN: usize = 1 + 4 + 8;

/// Frame durations the plane may negotiate, longest first.
///
/// Every rung divides both 48 000 and 96 000 into a whole number of samples, so the host pacer
/// and the client ring never carry a fractional frame:
///
/// | µs | samples @48 kHz | samples @96 kHz |
/// |---|---|---|
/// | 5000 | 240 | 480 |
/// | 4000 | 192 | 384 |
/// | 3000 | 144 | 288 |
/// | 2500 | 120 | 240 |
/// | 2000 | 96 | 192 |
/// | 1500 | 72 | 144 |
/// | 1000 | 48 | 96 |
pub const FRAME_US_LADDER: [u32; 7] = [5000, 4000, 3000, 2500, 2000, 1500, 1000];

/// Bit depths the plane carries. 32-bit float is deliberately absent: no source produces detail
/// 24 bits does not capture, and it would cost 33 % more for nothing.
pub const BITS_16: u8 = 16;
/// See [`BITS_16`].
pub const BITS_24: u8 = 24;

/// Full-scale magnitude at a given depth. Deliberately **symmetric** — the most-negative code
/// (`-2^(n-1)`) is not used, so [`from_f32`]/[`to_f32`] round-trip exactly in both directions
/// rather than folding one code onto its neighbour. One code out of 16.7 million is not audible;
/// a round trip that is not the identity would make the bit-exactness gate untestable.
const fn full_scale(bits: u8) -> i32 {
    match bits {
        BITS_16 => 32_767,
        _ => 8_388_607,
    }
}

/// Bytes each sample occupies on the wire at `bits`.
pub const fn bytes_per_sample(bits: u8) -> usize {
    match bits {
        BITS_16 => 2,
        _ => 3,
    }
}

/// Whether `bits` is a depth this plane can carry.
pub const fn depth_is_supported(bits: u8) -> bool {
    matches!(bits, BITS_16 | BITS_24)
}

/// Interleaved samples in one `frame_us` frame — **per channel × channels**.
pub const fn samples_per_frame(rate_hz: u32, frame_us: u32, channels: u8) -> usize {
    (rate_hz as usize * frame_us as usize / 1_000_000) * channels as usize
}

/// Wire bytes one `frame_us` frame occupies, excluding [`PCM_HEADER_LEN`].
pub const fn frame_payload_bytes(rate_hz: u32, bits: u8, channels: u8, frame_us: u32) -> usize {
    samples_per_frame(rate_hz, frame_us, channels) * bytes_per_sample(bits)
}

/// What the plane costs, in kbps — payload only, the number [`crate::audio::plan_audio_budget`]
/// must be told about rather than left to infer.
pub const fn bitrate_kbps(rate_hz: u32, bits: u8, channels: u8) -> u32 {
    (rate_hz as u64 * bits as u64 * channels as u64 / 1000) as u32
}

/// The longest [`FRAME_US_LADDER`] rung whose frame fits one datagram of `max_datagram` bytes,
/// or `None` if even the shortest does not.
///
/// **Sized from the raw frame, never from a coded estimate.** A datagram larger than the path
/// MTU is not sent at all and this plane is never fragmented, so the only safe input to this
/// decision is the size the payload is *guaranteed* not to exceed. For PCM that is exactly the
/// raw size; for any lossless codec added later it is the raw size plus a small header (a FLAC
/// VERBATIM frame), so this bound holds for both and the two would negotiate the same duration.
///
/// The caller must not ask before QUIC MTU discovery has settled, or it will size against the
/// conservative initial value and spend the rest of the session on shorter frames than the path
/// can carry (`design/hi-res-audio.md` §4.2).
pub fn frame_us_for(rate_hz: u32, bits: u8, channels: u8, max_datagram: usize) -> Option<u32> {
    let budget = max_datagram.checked_sub(PCM_HEADER_LEN)?;
    FRAME_US_LADDER
        .iter()
        .copied()
        .find(|&us| frame_payload_bytes(rate_hz, bits, channels, us) <= budget)
}

/// Quantise one interleaved f32 frame onto the wire, appending to `out`.
///
/// Scale-and-clamp with **no dither**: the source is a game mix that was already quantised
/// upstream, and dithering it would add noise while destroying the bit-exactness this plane
/// exists to provide.
pub fn from_f32(samples: &[f32], bits: u8, out: &mut Vec<u8>) {
    let fs = full_scale(bits);
    let scale = fs as f32;
    out.reserve(samples.len() * bytes_per_sample(bits));
    if bits == BITS_16 {
        for &s in samples {
            let v = (s * scale).round().clamp(-scale, scale) as i32;
            out.extend_from_slice(&(v as i16).to_le_bytes());
        }
    } else {
        for &s in samples {
            let v = (s * scale).round().clamp(-scale, scale) as i32;
            let b = v.to_le_bytes();
            out.extend_from_slice(&b[..3]);
        }
    }
}

/// Reverse of [`from_f32`]: decode `bytes` into `out`, returning the interleaved sample count.
/// `None` if `bytes` is not a whole number of samples at `bits`.
pub fn to_f32(bytes: &[u8], bits: u8, out: &mut Vec<f32>) -> Option<usize> {
    let step = bytes_per_sample(bits);
    if bytes.len() % step != 0 {
        return None;
    }
    let inv = 1.0 / full_scale(bits) as f32;
    out.clear();
    out.reserve(bytes.len() / step);
    if bits == BITS_16 {
        for c in bytes.chunks_exact(2) {
            out.push(i16::from_le_bytes([c[0], c[1]]) as f32 * inv);
        }
    } else {
        for c in bytes.chunks_exact(3) {
            // Sign-extend 24 bits into an i32 by placing the sample in the TOP three bytes and
            // arithmetic-shifting back down.
            let v = i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8;
            out.push(v as f32 * inv);
        }
    }
    Some(out.len())
}

/// Packet-loss concealment for a plane that has none.
///
/// [`crate::audio::AudioGapTracker`] feeds libopus PLC on the `0xC9` plane, so a lost datagram
/// interpolates instead of clicking. **A lossless format cannot do that** — there is nothing in
/// a raw frame from which to synthesise its successor. This is the replacement, and it is the
/// least-proven part of the plane (`design/hi-res-audio.md` §4.5): its tuning wants a
/// loss-injection listen, not just the unit tests below.
///
/// - **One frame lost** → repeat the previous frame with a raised-cosine fade. A frame is short
///   enough that repetition reads as continuity rather than as the pitch artefact a longer
///   repeat would produce.
/// - **Two or more** → fade to silence across the gap and back in on recovery. A clean dropout
///   beats a warble.
#[derive(Debug, Default, Clone)]
pub struct PcmConceal {
    /// The last good frame, interleaved f32 — the material every concealed frame is built from.
    prev: Vec<f32>,
    /// Consecutive frames concealed since the last real one.
    run: u32,
}

impl PcmConceal {
    pub fn new() -> PcmConceal {
        PcmConceal::default()
    }

    /// Remember a frame that really arrived, and end any concealment run.
    pub fn accept(&mut self, frame: &[f32]) {
        self.prev.clear();
        self.prev.extend_from_slice(frame);
        self.run = 0;
    }

    /// Frames concealed since the last real one — for stats, and for the caller's own cap.
    pub fn run(&self) -> u32 {
        self.run
    }

    /// Produce one concealed frame into `out`, or `false` when there is nothing to build from
    /// (no frame has arrived yet) — in which case the caller should emit silence and let the
    /// ring re-prime.
    pub fn conceal(&mut self, out: &mut Vec<f32>) -> bool {
        if self.prev.is_empty() {
            return false;
        }
        self.run = self.run.saturating_add(1);
        out.clear();
        out.extend_from_slice(&self.prev);
        let n = out.len();
        match self.run {
            // First loss: hand back the previous frame, faded out across its tail so a repeated
            // waveform does not step at the splice.
            1 => raised_cosine_tail(out, n),
            // Sustained loss: decay toward silence rather than looping a fragment.
            r => {
                let g = 0.5f32.powi(r.min(8) as i32 - 1);
                for s in out.iter_mut() {
                    *s *= g;
                }
                raised_cosine_tail(out, n);
            }
        }
        // The faded frame becomes the source for the next one, so a run decays monotonically
        // instead of restarting from the last loud frame every time.
        self.prev.clear();
        self.prev.extend_from_slice(out);
        true
    }
}

/// Apply a raised-cosine fade to the final `n` samples in place, so a spliced or repeated frame
/// meets what follows it without a step.
fn raised_cosine_tail(buf: &mut [f32], n: usize) {
    let n = n.min(buf.len());
    if n == 0 {
        return;
    }
    let start = buf.len() - n;
    for (i, s) in buf[start..].iter_mut().enumerate() {
        let t = (i as f32 + 0.5) / n as f32;
        *s *= 0.5 * (1.0 + (std::f32::consts::PI * t).cos());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim the whole plane exists to make. Every representable code at both depths must
    /// survive wire → f32 → wire unchanged; anything less and "lossless" is marketing.
    #[test]
    fn every_code_round_trips_bit_exactly() {
        for bits in [BITS_16, BITS_24] {
            let fs = full_scale(bits);
            // The endpoints, zero, and a deterministic sweep across the range.
            let mut codes: Vec<i32> = vec![0, 1, -1, fs, -fs, fs - 1, -fs + 1];
            let stride = (fs / 4096).max(1);
            codes.extend((-fs..=fs).step_by(stride as usize));

            let mut wire = Vec::new();
            for &c in &codes {
                if bits == BITS_16 {
                    wire.extend_from_slice(&(c as i16).to_le_bytes());
                } else {
                    wire.extend_from_slice(&c.to_le_bytes()[..3]);
                }
            }

            let mut floats = Vec::new();
            let n = to_f32(&wire, bits, &mut floats).expect("whole samples");
            assert_eq!(n, codes.len());

            let mut back = Vec::new();
            from_f32(&floats, bits, &mut back);
            assert_eq!(
                back, wire,
                "{bits}-bit PCM must survive wire → f32 → wire unchanged"
            );
        }
    }

    /// Sign extension is the one place a 24-bit unpack goes quietly wrong — a missing shift
    /// turns every negative sample into a large positive one, which sounds like loud noise
    /// rather than like a bug.
    #[test]
    fn twenty_four_bit_negatives_sign_extend() {
        let mut wire = Vec::new();
        from_f32(&[-0.5, 0.5, -1.0, 1.0], BITS_24, &mut wire);
        let mut out = Vec::new();
        to_f32(&wire, BITS_24, &mut out).expect("whole samples");
        assert!(out[0] < -0.4 && out[0] > -0.6, "got {}", out[0]);
        assert!(out[1] > 0.4 && out[1] < 0.6, "got {}", out[1]);
        assert!((out[2] + 1.0).abs() < 1e-6, "got {}", out[2]);
        assert!((out[3] - 1.0).abs() < 1e-6, "got {}", out[3]);
    }

    /// Out-of-range input must clamp, not wrap. A wrapped sample is full-scale noise of the
    /// opposite sign — the loudest possible artefact from the quietest possible mistake.
    #[test]
    fn out_of_range_input_clamps_instead_of_wrapping() {
        for bits in [BITS_16, BITS_24] {
            let mut wire = Vec::new();
            from_f32(
                &[9.0, -9.0, f32::INFINITY, f32::NEG_INFINITY],
                bits,
                &mut wire,
            );
            let mut out = Vec::new();
            to_f32(&wire, bits, &mut out).expect("whole samples");
            for v in &out {
                assert!((-1.0..=1.0).contains(v), "{bits}-bit clamp failed: {v}");
            }
            assert!(out[0] > 0.99 && out[1] < -0.99);
            assert!(out[2] > 0.99 && out[3] < -0.99);
        }
    }

    /// The ladder must never hand back a frame that does not fit, and must take the longest one
    /// that does. This is the check that keeps the plane off the fragmentation path.
    #[test]
    fn the_frame_ladder_never_exceeds_the_datagram() {
        for rate in [48_000u32, 96_000] {
            for bits in [BITS_16, BITS_24] {
                for budget in [600usize, 900, 1200, 1387, 1400, 1440, 9000] {
                    let us = frame_us_for(rate, bits, 2, budget).expect("some rung fits");
                    let bytes = frame_payload_bytes(rate, bits, 2, us);
                    assert!(
                        bytes + PCM_HEADER_LEN <= budget,
                        "{rate}/{bits} at {budget} B chose {us} µs = {bytes} B + header"
                    );
                    // …and nothing longer would have fitted.
                    for &longer in FRAME_US_LADDER.iter().take_while(|&&x| x > us) {
                        assert!(
                            frame_payload_bytes(rate, bits, 2, longer) + PCM_HEADER_LEN > budget,
                            "{rate}/{bits} at {budget} B should have chosen {longer} µs"
                        );
                    }
                }
            }
        }
    }

    /// The concrete ladder the default 1472-byte MTU ceiling produces. Pinned because these are
    /// the numbers the design argues from, and a silent change to any of them changes the
    /// plane's packet rate.
    #[test]
    fn the_default_mtu_yields_the_documented_ladder() {
        // Conservative usable datagram at the 1472-byte discovery ceiling.
        let d = 1400;
        assert_eq!(frame_us_for(48_000, BITS_16, 2, d), Some(5000));
        assert_eq!(frame_us_for(48_000, BITS_24, 2, d), Some(4000));
        assert_eq!(frame_us_for(96_000, BITS_16, 2, d), Some(3000));
        assert_eq!(frame_us_for(96_000, BITS_24, 2, d), Some(2000));
        // The doc's original 2.5 ms at 96/24 does NOT fit: 240 × 2 × 3 = 1440 B of payload
        // against a ~1387 B budget. Sizing from a coded estimate is what hid that.
        assert!(frame_payload_bytes(96_000, BITS_24, 2, 2500) + PCM_HEADER_LEN > d);
    }

    /// Every rung must divide both shipping rates into whole samples, or the pacer carries a
    /// fractional frame and the ring and the host disagree about what a frame is.
    #[test]
    fn every_ladder_rung_is_whole_samples_at_both_rates() {
        for &us in &FRAME_US_LADDER {
            for rate in [48_000u64, 96_000] {
                assert_eq!(
                    rate * us as u64 % 1_000_000,
                    0,
                    "{us} µs is not a whole number of samples at {rate} Hz"
                );
            }
        }
    }

    /// The costs the §8.4 gate and `plan_audio_budget` reason about.
    #[test]
    fn the_plane_costs_what_the_design_says() {
        assert_eq!(bitrate_kbps(48_000, BITS_16, 2), 1_536);
        assert_eq!(bitrate_kbps(48_000, BITS_24, 2), 2_304);
        assert_eq!(bitrate_kbps(96_000, BITS_16, 2), 3_072);
        assert_eq!(bitrate_kbps(96_000, BITS_24, 2), 4_608);
    }

    /// A truncated datagram must be rejected outright rather than decoded as a shifted frame —
    /// half a sample at the end would desync every sample after it.
    #[test]
    fn a_partial_sample_is_rejected() {
        let mut out = Vec::new();
        assert!(to_f32(&[0, 0, 0], BITS_16, &mut out).is_none());
        assert!(to_f32(&[0, 0], BITS_24, &mut out).is_none());
        assert_eq!(to_f32(&[], BITS_24, &mut out), Some(0));
    }

    /// Concealment with nothing to conceal from must say so, so the caller emits silence and
    /// lets the ring re-prime instead of playing an uninitialised buffer.
    #[test]
    fn concealment_needs_a_frame_to_build_from() {
        let mut c = PcmConceal::new();
        let mut out = Vec::new();
        assert!(!c.conceal(&mut out), "nothing to repeat yet");
        c.accept(&[0.5; 240]);
        assert!(c.conceal(&mut out));
        assert_eq!(out.len(), 240);
    }

    /// A sustained gap must decay toward silence rather than loop a fragment, and must never
    /// grow louder than the audio it is standing in for.
    #[test]
    fn a_sustained_gap_decays_to_silence() {
        let mut c = PcmConceal::new();
        c.accept(&[1.0; 128]);
        let mut out = Vec::new();
        let mut peaks = Vec::new();
        for _ in 0..8 {
            assert!(c.conceal(&mut out));
            peaks.push(out.iter().fold(0f32, |m, s| m.max(s.abs())));
        }
        for w in peaks.windows(2) {
            assert!(w[1] <= w[0] + 1e-6, "concealment grew louder: {peaks:?}");
        }
        assert!(peaks[0] <= 1.0, "never louder than the source: {peaks:?}");
        assert!(
            *peaks.last().unwrap() < 0.05,
            "should have faded out: {peaks:?}"
        );
        assert_eq!(c.run(), 8);
        // A real frame ends the run.
        c.accept(&[0.25; 128]);
        assert_eq!(c.run(), 0);
    }

    /// The fade must actually reach (near) zero at the splice point, which is the whole reason
    /// it exists — a repeat that ends mid-waveform steps audibly into whatever follows.
    #[test]
    fn the_fade_lands_on_silence() {
        let mut c = PcmConceal::new();
        c.accept(&[1.0; 64]);
        let mut out = Vec::new();
        c.conceal(&mut out);
        assert!(out[0] > 0.9, "starts at full level: {}", out[0]);
        assert!(
            *out.last().unwrap() < 0.01,
            "ends at silence: {}",
            out.last().unwrap()
        );
    }
}
