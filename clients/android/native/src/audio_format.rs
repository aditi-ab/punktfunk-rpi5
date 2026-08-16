//! The audio format a session RESOLVED, and the millisecond ⇄ interleaved-sample arithmetic every
//! figure the playback plane reports is expressed in.
//!
//! **Why this is its own module, and why it is NOT `#[cfg(target_os = "android")]` like the
//! [`crate::audio`] that owns it.** The conversions below are the part of the plane that was
//! *wrong* — see [`ms_to_samples`] — and the whole class of defect is one that measures cleanly
//! while being off by a fixed percentage. A bug like that is only ever caught by arithmetic tests,
//! and an arithmetic test that can only run on a phone is a test that runs when someone remembers.
//! Nothing here touches AAudio, so nothing here needs a device: it compiles and is tested on the
//! ordinary `cargo test -p punktfunk-client-android --lib` leg, and `:kit:cargoNdkClippy` lints it
//! at both Android widths on top.

use punktfunk_core::audio::pcm;
// Only [`SessionAudio::of`] touches the connector, and only on device — see the `cfg` there.
#[cfg(target_os = "android")]
use punktfunk_core::client::NativeClient;

/// The `0xC9` plane's frame duration: fixed by the protocol at 5 ms (the host's `audio_thread`),
/// not negotiated. Only `0xD3` carries `audio_frame_us`.
pub(crate) const OPUS_FRAME_US: u32 = 5_000;

// ---- ms ⇄ interleaved samples: multiply FIRST, divide LAST ------------------------------------
//
// This mirrors `punktfunk_core::audio`'s own pair, and it mirrors it because the defect it fixes
// was copied from there. Both used to precompute `per_ms = rate_hz / 1000 * channels` and express
// every ms-denominated figure as `ms * per_ms`. **That division happens first**, so 44 100 Hz
// became 44 samples per millisecond and every depth, hard cap and reported `buffer_ms` was 2.3 %
// out — quietly, permanently, and only on the rates the old ladder happened not to offer. 48 000
// and 96 000 were exact by luck: they divide.
//
// Keeping the rate and the channel count as the two numbers they are, and dividing last, is exact
// at every rate on `pcm::rate_is_supported`'s ladder for one integer division per conversion, and
// 48/96 kHz stay bit-identical by construction (`per_sec == 1000 × per_ms` exactly there, so both
// conversions reduce to the expression they replace).

/// Interleaved samples per second at a negotiated layout — the denominator both conversions share.
///
/// `max(1)` on both factors: a degenerate layout must not divide by zero on a realtime thread.
/// [`SessionAudio::of`] already clamps, so this is the belt to that pair of braces.
fn interleaved_per_sec(rate_hz: u32, channels: usize) -> u64 {
    let hz = if rate_hz == 0 { 1 } else { rate_hz } as u64;
    let ch = if channels == 0 { 1 } else { channels } as u64;
    hz * ch
}

/// `ms` milliseconds of audio, in interleaved samples.
///
/// u64 intermediates because the product is large where a `usize` may be 32 bits — and on this
/// client that is not hypothetical: **armeabi-v7a is a shipping ABI** (every 32-bit Google TV /
/// Android TV box), so the same expression runs at both widths. `JitterTuning::AAUDIO`'s hard cap
/// against 176 400 Hz × 8 ch would be fine, but the type is what makes that a fact rather than an
/// audit. Saturating rather than wrapping, because a wrapped window is a *tiny* one — a buffer cap
/// that is instantly exceeded instead of one that is never reached.
fn ms_to_samples(rate_hz: u32, channels: usize, ms: u32) -> usize {
    let n = ms as u64 * interleaved_per_sec(rate_hz, channels) / 1000;
    if n > u32::MAX as u64 {
        u32::MAX as usize
    } else {
        n as usize
    }
}

/// Interleaved samples back to whole milliseconds — the exact inverse of [`ms_to_samples`].
///
/// u128 because `samples` arrives from a ring depth and nothing bounds it: `usize::MAX * 1000`
/// overflows a u64 on the 64-bit ABI.
fn samples_to_ms(rate_hz: u32, channels: usize, samples: usize) -> u32 {
    let ms = samples as u128 * 1000 / interleaved_per_sec(rate_hz, channels) as u128;
    if ms > u32::MAX as u128 {
        u32::MAX
    } else {
        ms as u32
    }
}

/// The audio format this session RESOLVED, read once from the connector and threaded through the
/// whole plane.
///
/// Gathered into one value rather than passed as five parameters because the fields are only
/// meaningful together: a rate without the codec cannot tell a 48 kHz lossless session from a
/// 48 kHz Opus one, and those two agree on every other resolved value.
///
/// ⚠ Everything here is what the HOST resolved, never what this client asked for. A client that
/// requests 96 kHz, is answered 48 kHz and opens at 96 kHz anyway is `design/hi-res-audio.md`
/// §4.3's failure one end further along — a session that audits clean at both ends and plays the
/// wrong content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionAudio {
    /// [`punktfunk_core::quic::AUDIO_CODEC_OPUS`] (`0xC9`) or
    /// [`punktfunk_core::quic::AUDIO_CODEC_PCM`] (`0xD3`) — what SELECTS the decoder, and the only
    /// field that can.
    pub(crate) codec: u8,
    /// The resolved sample rate: 48 000 on every Opus session, and any rung of
    /// [`pcm::rate_is_supported`] on `0xD3` — 44 100 / 48 000 / 88 200 / 96 000 / 176 400. Both
    /// families are exact in every conversion this module performs; the 44.1 one was deferred
    /// only for as long as the arithmetic above divided before it multiplied.
    pub(crate) rate_hz: u32,
    /// The resolved sample depth (16 or 24) — the stride `0xD3` payloads are unpacked at.
    /// Meaningless on the Opus plane, which decodes to f32 regardless.
    pub(crate) bits: u8,
    /// The resolved, normalized channel count (2 / 6 / 8).
    ///
    /// ⚠ Not "2" any more on the lossless plane. Surround was excluded from `0xD3` because a 5.1
    /// frame did not fit a datagram at the default MTU, but the ladder is channel-aware and the
    /// restriction was one host-side condition, not a wire limitation: at the conservative
    /// datagram size a 48 kHz/16-bit 5.1 session negotiates 2 ms frames and a 24-bit one 1 ms
    /// (a thousand datagrams a second), while 96/24 5.1 still fits nothing and is declined. Every
    /// per-frame size below is taken from THIS count for that reason.
    pub(crate) channels: usize,
    /// How much audio one datagram carries. Negotiated from the path MTU on `0xD3` (at 96 kHz /
    /// 24-bit stereo the default MTU ceiling only leaves room for 2 ms, and for 24-bit surround
    /// 1 ms), and the Opus plane's fixed 5 ms otherwise — folded to one field here so nothing
    /// downstream has to branch to size a buffer.
    ///
    /// ⚠ A **label**, not a duration. It is a whole number of samples per channel only when the
    /// rate divides the rung, which the 44.1 kHz family never does: a nominal 5 ms frame at
    /// 44 100 Hz carries 220 samples per channel = 4 988 662 ns. Size from [`Self::frame_samples`];
    /// time from [`pcm::frame_duration_ns`].
    pub(crate) frame_us: u32,
}

impl SessionAudio {
    /// Read the whole resolved format off the connector, once, at the top of the plane.
    ///
    /// Android-only, because a [`NativeClient`] only exists once a session has been negotiated on
    /// a device — the pure half is [`resolved`](Self::resolved), which is where the clamping lives
    /// and what the tests exercise. Left ungated it would be dead code on the host build, and
    /// `-D warnings` is a hard gate there.
    #[cfg(target_os = "android")]
    pub(crate) fn of(client: &NativeClient) -> SessionAudio {
        SessionAudio::resolved(
            client.audio_codec,
            client.audio_sample_rate_hz,
            client.audio_bits,
            client.audio_channels,
            u32::from(client.audio_frame_us),
        )
    }

    /// The `Welcome`'s five audio fields, clamped into something every buffer below can be sized
    /// from. **Every value here arrives off the wire**, so each clamp is defending a realtime
    /// thread against a host that is old, wrong, or hostile — none of them bite a conforming one.
    pub(crate) fn resolved(
        codec: u8,
        rate_hz: u32,
        bits: u8,
        channels: u8,
        frame_us: u32,
    ) -> SessionAudio {
        let is_pcm = codec == punktfunk_core::quic::AUDIO_CODEC_PCM;
        // A zero rate is inexpressible off the wire (`Welcome::decode` folds both absence and a
        // literal 0 to the legacy 48 kHz), but it is the denominator of every conversion above,
        // so a 0 that ever DID reach here would be a division by zero on the decode thread. One
        // clamp, at the one place the value enters this module.
        let rate_hz = if rate_hz == 0 {
            punktfunk_core::audio::SAMPLE_RATE_HZ
        } else {
            rate_hz
        };
        SessionAudio {
            codec,
            rate_hz,
            bits,
            channels: punktfunk_core::audio::normalize_channels(channels) as usize,
            // Same reasoning as the rate: an old host sends no `audio_frame_us` at all and a
            // hostile one could send 0, and this number divides nothing but sizes everything.
            //
            // Capped at the longest rung of `FRAME_US_LADDER` (which is also the Opus plane's
            // 5 ms) because the decode scratch is sized from that rung and clamps its copies to
            // it: an unclamped `frame_us` would let a `Welcome` claim frames the scratch cannot
            // hold, and the ring would then be reserved for a size the loop can never deliver.
            // A conforming host only ever names a rung, so this bites nobody real.
            frame_us: match (is_pcm, frame_us) {
                (true, us) if us > 0 => us.min(pcm::FRAME_US_LADDER[0]),
                // The Opus plane's frames are the protocol's fixed 5 ms (host `audio_thread`).
                _ => OPUS_FRAME_US,
            },
        }
    }

    /// True when this session runs the lossless `0xD3` plane rather than Opus on `0xC9`.
    pub(crate) fn is_pcm(&self) -> bool {
        self.codec == punktfunk_core::quic::AUDIO_CODEC_PCM
    }

    /// `ms` of audio in interleaved samples at this session's layout — see [`ms_to_samples`].
    pub(crate) fn ms_samples(&self, ms: u32) -> usize {
        ms_to_samples(self.rate_hz, self.channels, ms)
    }

    /// The inverse, for the depths this plane reports to the HUD — see [`samples_to_ms`].
    pub(crate) fn samples_ms(&self, samples: usize) -> u32 {
        samples_to_ms(self.rate_hz, self.channels, samples)
    }

    /// Interleaved samples in ONE frame of this plane — what the ring reserves per queued chunk
    /// and what the decode-scratch assertion is written against.
    ///
    /// Taken from [`pcm::samples_per_frame`] rather than re-derived here, because that function is
    /// the single source of truth for how long a frame is and the host fills its buffers from it.
    /// The two are only interchangeable when the rate divides the rung: **5 ms of audio at
    /// 44 100 Hz stereo is 441 interleaved samples, but a 5 ms FRAME carries 440** — 220.5 samples
    /// per channel do not exist, so the wire floors. Both the ring reserve and the debug assertion
    /// that guards it mean "exactly one packet", and a self-derived answer would describe a packet
    /// no host ever sends.
    pub(crate) fn frame_samples(&self) -> usize {
        pcm::samples_per_frame(self.rate_hz, self.frame_us, self.channels as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rate the lossless plane admits, so a conversion that is only exact on one family can
    /// never be pinned by accident.
    const RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 176_400];

    fn fmt(rate_hz: u32, channels: usize, frame_us: u32) -> SessionAudio {
        SessionAudio {
            codec: punktfunk_core::quic::AUDIO_CODEC_PCM,
            rate_hz,
            bits: pcm::BITS_24,
            channels,
            frame_us,
        }
    }

    /// **The defect, stated as numbers.** `per_ms = rate_hz / 1000 * channels` truncates 44 100 Hz
    /// stereo to 88 samples per millisecond where it is really 88.2, so everything the plane sizes
    /// or reports in milliseconds came out 2.3 % wrong: the ring's hard cap 2.3 % SHALLOW, and the
    /// `buffer_ms` on the HUD 2.3 % DEEP — a plane that measures itself cleanly while being off in
    /// both directions at once.
    ///
    /// Paired with [`the_48_khz_family_is_bit_identical_to_the_arithmetic_it_replaced`], which is
    /// the other half of the claim and passes under BOTH expressions. Restore
    /// `ms × (rate_hz / 1000 × channels)` and exactly one of the two fails; that asymmetry is the
    /// whole point, and it is why they are two tests rather than one.
    #[test]
    fn the_ms_conversions_are_exact_on_the_rates_that_do_not_divide() {
        // 44 100 × 2 = 88 200 interleaved samples a second; one second of them is 88 200, not the
        // 88 000 an integer samples-per-millisecond would have claimed.
        let f = fmt(44_100, 2, 5_000);
        assert_eq!(f.ms_samples(1_000), 88_200);
        assert_eq!(f.samples_ms(88_200), 1_000);
        // …and the truncated pair, spelled out, so the size of the error is on the record rather
        // than in a commit message: 88 000 samples and 1 002 ms are what the old code produced.
        assert_ne!(f.ms_samples(1_000), 1_000 * 88);
        assert_ne!(f.samples_ms(88_200), 88_200 / 88);

        // 5.1 at 88 200 Hz — the two axes that used to be folded into one constant, both moving.
        let s = fmt(88_200, 6, 2_000);
        assert_eq!(s.ms_samples(100), 52_920);
        assert_eq!(s.samples_ms(52_920), 100);

        // And the top of the ladder, where the truncation is smallest in relative terms and still
        // wrong: 176 400 Hz × 8 ch is 1 411 200 samples a second, not 1 408 000.
        let top = fmt(176_400, 8, 1_000);
        assert_eq!(top.ms_samples(1_000), 1_411_200);
    }

    /// The other half of the same claim: on 48 000 and 96 000 Hz the new conversions are
    /// **bit-identical** to the `ms × (rate_hz / 1000 × channels)` they replaced, at every layout
    /// and every figure the tuning names.
    ///
    /// Load-bearing, not decorative. Every session anyone has ever run is on this family, and the
    /// value of "we fixed the arithmetic" depends entirely on nobody's ring having moved by a
    /// sample while we did it. It holds by construction — `rate × ch` is exactly `1000 × per_ms`
    /// where the rate divides — and this is that construction asserted rather than argued.
    ///
    /// It also passes under the OLD expression, which is what makes its partner above a real test:
    /// plant `per_ms` back and this one still goes green.
    #[test]
    fn the_48_khz_family_is_bit_identical_to_the_arithmetic_it_replaced() {
        for rate in [48_000u32, 96_000] {
            for ch in [2usize, 6, 8] {
                let f = fmt(rate, ch, 5_000);
                let per_ms = (rate as usize / 1000) * ch;
                for ms in [1u32, 2, 12, 25, 47, 120, 1_000] {
                    assert_eq!(
                        f.ms_samples(ms),
                        ms as usize * per_ms,
                        "{rate} Hz/{ch}ch must be unchanged at {ms} ms"
                    );
                    assert_eq!(
                        f.samples_ms(ms as usize * per_ms),
                        ms,
                        "{rate} Hz/{ch}ch must read back unchanged at {ms} ms"
                    );
                }
                // A depth that is NOT a whole number of milliseconds truncates the same way it
                // always did — the reported `buffer_ms` never rounds up into a figure the ring
                // does not hold.
                assert_eq!(f.samples_ms(per_ms * 12 + per_ms / 2), 12);
            }
        }
    }

    /// The round trip `design/hi-res-audio.md` §4.1 names as the tell that this rework is
    /// incomplete: a depth expressed in samples and read back as milliseconds must be the
    /// milliseconds it was built from, at every rate on the ladder and every layout the plane can
    /// resolve. Core asserts the same property for [`punktfunk_core::audio::JitterPolicy`]; this
    /// is the half of it this client owns, since the ring's cap and the HUD's `buffer_ms` are
    /// converted here rather than there.
    #[test]
    fn the_shipping_ladder_round_trips_ms_to_samples_at_every_rate() {
        let t = punktfunk_core::audio::JitterTuning::AAUDIO;
        for rate in RATES {
            for ch in [2usize, 6, 8] {
                let f = fmt(rate, ch, 2_000);
                // Every ms figure this preset names — each is a threshold something compares a
                // sample count against, and a rate that skewed 2.3 % skewed all of them together,
                // which is exactly what kept the defect invisible.
                for ms in [
                    t.base_target_ms,
                    t.max_target_ms,
                    t.headroom_ms,
                    t.hard_cap_ms,
                    t.deprime_ms,
                    t.plc_max_ms(),
                ] {
                    assert_eq!(
                        f.samples_ms(f.ms_samples(ms)),
                        ms,
                        "{rate} Hz/{ch}ch lost {ms} ms on the round trip"
                    );
                }
                // The conversion itself, against the arithmetic done the honest way rather than
                // against itself: multiply by the rate and the channels, and only THEN divide.
                for ms in [1u32, 2, 12, 47, 1_000, 480_000] {
                    assert_eq!(
                        f.ms_samples(ms) as u64,
                        ms as u64 * rate as u64 * ch as u64 / 1000,
                        "{ms} ms at {rate} Hz/{ch}ch"
                    );
                }
            }
        }

        // ⚠ Exact is not the same as lossless in both directions, and the difference is worth
        // stating rather than discovering. A millisecond is 88.2 samples at 44 100 Hz stereo, so
        // an ms figure that is not a multiple of 5 has no whole-sample answer at all: 1 ms floors
        // to 88 samples, which reads back as 0. That is a floor of at most ONE SAMPLE — against
        // the 2.3 % the old arithmetic was out by on EVERY figure, in the same direction,
        // permanently. Every threshold `JitterTuning::AAUDIO` names is a multiple of 5, which is
        // why the loop above is exact and this note is a note.
        let f = fmt(44_100, 2, 5_000);
        assert_eq!(f.ms_samples(1), 88); // the true 88.2, floored
        assert_eq!(f.samples_ms(88), 0);
        assert_eq!(f.ms_samples(15), 1_323, "15 ms of 44.1 kHz stereo");
        assert_eq!(15 * (44_100 / 1000) * 2, 1_320, "what it used to compute");
    }

    /// **A frame is not the milliseconds it is labelled with.** At 44 100 Hz a nominal 5 ms frame
    /// carries 220 samples per channel — 440 interleaved — while 5 ms of *audio* is 441, because
    /// 220.5 samples per channel do not exist and the wire floors.
    ///
    /// The ring reserve, the decode-scratch assertion and the policy's shed all mean "exactly one
    /// packet", so this must come from [`pcm::samples_per_frame`] — the same function the host
    /// fills its buffers from — and never from a millisecond count. One sample of disagreement on
    /// an interleaved stream walks the channels around each other.
    #[test]
    fn a_frame_is_the_wires_sample_count_not_the_labels_milliseconds() {
        let f = fmt(44_100, 2, 5_000);
        assert_eq!(f.frame_samples(), 440, "220 samples per channel, floored");
        assert_eq!(f.ms_samples(5), 441, "5 ms of AUDIO is 441 interleaved");
        assert_ne!(f.frame_samples(), f.ms_samples(5));
        // The real duration of that frame, which is what a `pts_ns` must advance by — 0.23 % short
        // of the label it negotiated.
        assert_eq!(pcm::frame_duration_ns(440, 44_100, 2), 4_988_662);

        // Where the rate divides the rung the two agree, which is why nothing noticed for as long
        // as the ladder was 48/96 kHz only.
        for rate in [48_000u32, 96_000] {
            for ch in [2usize, 6, 8] {
                let f = fmt(rate, ch, 5_000);
                assert_eq!(f.frame_samples(), f.ms_samples(5), "{rate} Hz/{ch}ch");
            }
        }

        // Surround sizes from the RESOLVED channel count, not from a stereo assumption: a 5.1
        // frame is three times a stereo one and the ring is reserved from it.
        let stereo = fmt(48_000, 2, 2_000);
        let five_one = fmt(48_000, 6, 2_000);
        assert_eq!(stereo.frame_samples(), 192);
        assert_eq!(five_one.frame_samples(), 576);
    }

    /// A `Welcome` this client cannot trust must not become a division fault or a buffer sized
    /// from garbage on the decode thread. Absence, a literal zero and an over-long frame all have
    /// defined answers, and they are the safe ones.
    #[test]
    fn a_degenerate_welcome_clamps_instead_of_dividing_by_zero() {
        const OPUS: u8 = punktfunk_core::quic::AUDIO_CODEC_OPUS;
        const PCM: u8 = punktfunk_core::quic::AUDIO_CODEC_PCM;

        // The ordinary session: a pre-lossless host sends none of these fields, and every absent
        // one has to land on exactly what the plane has always been.
        let legacy = SessionAudio::resolved(OPUS, 0, 0, 0, 0);
        assert!(!legacy.is_pcm());
        assert_eq!(legacy.rate_hz, punktfunk_core::audio::SAMPLE_RATE_HZ);
        assert_eq!(legacy.channels, 2);
        assert_eq!(legacy.frame_us, OPUS_FRAME_US);

        // `audio_frame_us` is a `0xD3` field and must not be honoured on the Opus plane, whose
        // frames the protocol fixes at 5 ms — a host that sent one anyway would otherwise resize
        // this client's ring for frames it never sends.
        assert_eq!(
            SessionAudio::resolved(OPUS, 48_000, 16, 2, 2_000).frame_us,
            OPUS_FRAME_US
        );
        // …and on `0xD3` a frame longer than the ladder's top rung is capped there, because the
        // decode scratch is sized from that rung and clamps its copies to it.
        let overlong = SessionAudio::resolved(PCM, 96_000, 24, 6, 60_000);
        assert!(overlong.is_pcm());
        assert_eq!(overlong.frame_us, pcm::FRAME_US_LADDER[0]);
        assert_eq!(overlong.channels, 6);
        // A layout off the wire is normalized rather than trusted: 3 channels is not a layout the
        // decoder or AAudio can be opened with.
        assert_eq!(
            SessionAudio::resolved(PCM, 44_100, 24, 3, 5_000).channels,
            2
        );

        // The conversions still have to survive a 0 that reached them some other way, because they
        // run on a realtime-adjacent thread that may not panic.
        let broken = fmt(0, 0, 0);
        assert_eq!(broken.ms_samples(10), 0);
        assert_eq!(broken.samples_ms(480), 480_000);
        assert_eq!(broken.frame_samples(), 0);
    }
}
