//! The wire budget: what an encoder rate costs, and what parity takes out of it.
//!
//! A session's bitrate is its *wire* budget — every byte the media plane puts
//! on the link, headers, seals and FEC parity included. The encoder gets what
//! is left after the audio reservation and the parity share, so adaptive FEC
//! reallocates inside the budget instead of widening it. The host derives the
//! encoder rate from it, the controller judges delivery against it, and the
//! simulator spends it: one arithmetic, one file.

/// 40-byte header + 24-byte crypto seal inside each UDP payload (~4.5 % at 1408).
pub const SHARD_WIRE_OVERHEAD: u64 =
    (crate::packet::HEADER_LEN + crate::packet::CRYPTO_OVERHEAD) as u64;

/// Floor on an encoder rate. Below this the picture is not worth the packets.
pub const MIN_BITRATE_KBPS: u32 = 500;

/// Adaptive-FEC band. A clean link decays to [`FEC_MIN`]; loss ramps toward
/// [`FEC_MAX`]. 5 % is 4 parity shards on a ~110 KB frame (2 on a 30 KB one) —
/// the burst a clean link still drops. A 1 % floor left one, so the cleanest
/// link lost a frame to two packets. A session opens at
/// [`FEC_ADAPTIVE_START`], before any loss report has sized it.
pub const FEC_MIN: u8 = 5;
pub const FEC_MAX: u8 = 50;
pub const FEC_ADAPTIVE_START: u8 = 10;
/// Points over the measured level while frames die that parity might have caught.
pub const FEC_STEP: u8 = 3;
/// Report windows (~750 ms each) the step gets to prove itself.
pub const FEC_STEP_WINDOWS: u32 = 4;

/// Wire budget → encoder rate.
///
/// ```text
/// wire  = video × (payload+64)/payload × (100+fec)/100 + audio
/// video = (wire − audio) × payload/(payload+64) × 100/(100+fec)
/// ```
///
/// More parity means a lower encoder rate, never a fatter wire. Floored at
/// [`MIN_BITRATE_KBPS`]. PyroWave bypasses this (bpp pin, Automatic off).
pub fn encoder_kbps_for_budget(
    budget_kbps: u32,
    audio_kbps: u32,
    fec_percent: u8,
    shard_payload: u16,
) -> u32 {
    let payload = shard_payload.max(1) as u64;
    let video_wire = budget_kbps.saturating_sub(audio_kbps) as u64;
    let video =
        video_wire * payload * 100 / ((payload + SHARD_WIRE_OVERHEAD) * (100 + fec_percent as u64));
    u32::try_from(video)
        .unwrap_or(u32::MAX)
        .max(MIN_BITRATE_KBPS)
}

/// Inverse: the wire spend of an encoder rate. A short apply reports this so
/// the client's climb base tracks wire truth. Rounds up where the derivation
/// rounds down, so a roundtrip never inflates the budget the client believes.
pub fn budget_kbps_for_encoder(
    encoder_kbps: u32,
    audio_kbps: u32,
    fec_percent: u8,
    shard_payload: u16,
) -> u32 {
    let payload = shard_payload.max(1) as u64;
    let wire = encoder_kbps as u64 * (payload + SHARD_WIRE_OVERHEAD) * (100 + fec_percent as u64)
        / (payload * 100);
    u32::try_from(wire.saturating_add(audio_kbps as u64)).unwrap_or(u32::MAX)
}

/// Loss ppm ([`crate::quic::LossReport`]) → recovery %. FEC must exceed the
/// loss it covers, so the target is `loss × 1.4 + 1`, clamped to the band.
/// Clean (≈0 ppm) lands on [`FEC_MIN`].
///
/// Integer: `ceil(ppm/10_000 × 1.4) + 1` is `(ppm × 14).div_ceil(100_000) + 1`.
pub fn adapt_fec(loss_ppm: u32) -> u8 {
    let target = (loss_ppm as u64 * 14).div_ceil(100_000) as u32 + 1;
    target.clamp(FEC_MIN as u32, FEC_MAX as u32) as u8
}

/// One window's FEC target. `unrecovered_run` is the consecutive report
/// windows a keyframe ask landed in — frames parity could not repair.
///
/// The first [`FEC_STEP_WINDOWS`] of a run add [`FEC_STEP`] over the measured
/// level; a run past that is loss no per-frame parity bridges, so the step
/// comes off and only the measured loss holds. Decays one point per window so
/// a burst every few seconds does not fall to the floor between hits.
pub fn fec_target(loss_ppm: u32, prev: u8, unrecovered_run: u32) -> u8 {
    let step = if (1..=FEC_STEP_WINDOWS).contains(&unrecovered_run) {
        FEC_STEP
    } else {
        0
    };
    adapt_fec(loss_ppm)
        .saturating_add(step)
        .min(FEC_MAX)
        .max(prev.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapt_fec_maps_loss_to_recovery_band() {
        // Clean window (0 loss) is FEC_MIN; loss under ~2.8 % clamps to it.
        assert_eq!(adapt_fec(0), FEC_MIN);
        assert_eq!(adapt_fec(1), FEC_MIN);
        // FEC exceeds the loss it covers (×1.4 + 1 pt).
        assert_eq!(adapt_fec(30_000), 6); // 3% → ceil(4.2)+1 = 6
        assert_eq!(adapt_fec(50_000), 8); // 5% → ceil(7)+1 = 8
        assert_eq!(adapt_fec(100_000), 15); // 10% → ceil(14)+1 = 15
        assert_eq!(adapt_fec(1_000_000), FEC_MAX); // 100% → clamped
        assert!(adapt_fec(u32::MAX) <= FEC_MAX);
    }

    /// The integer form is the `f64` one the host shipped, for every loss a
    /// report can carry. Floating point decided a parity percent for three
    /// releases; this pins that nothing moved when it stopped.
    #[test]
    fn the_integer_band_is_the_float_one_it_replaced() {
        let float_form = |loss_ppm: u32| -> u8 {
            let loss_pct = loss_ppm as f64 / 10_000.0;
            let target = (loss_pct * 1.4).ceil() as u32 + 1;
            target.clamp(FEC_MIN as u32, FEC_MAX as u32) as u8
        };
        for ppm in 0..=1_000_000u32 {
            assert_eq!(adapt_fec(ppm), float_form(ppm), "loss_ppm {ppm}");
        }
    }

    /// A frame dying every window under low measured loss is a bounded step over the
    /// measured level, never a 5 % reading: on for a few windows, off once it has not
    /// stopped the asks, and a clean window ends the run.
    #[test]
    fn fec_step_is_bounded_and_gives_up_when_frames_keep_dying() {
        // Clean: measured level, decaying one point per window.
        assert_eq!(fec_target(0, FEC_MIN, 0), FEC_MIN);
        assert_eq!(fec_target(0, 12, 0), 11);
        // A dropped frame at 0.3 % loss: +3 over the floor, held while the run is young.
        let mut fec = FEC_MIN;
        let mut seen = Vec::new();
        for run in 1..=FEC_STEP_WINDOWS + 2 {
            fec = fec_target(3_000, fec, run);
            seen.push(fec);
        }
        assert_eq!(
            seen,
            [8, 8, 8, 8, 7, 6],
            "step, then the decay back to measured"
        );
        // Measured loss still carries its own level once the step is off.
        assert_eq!(fec_target(100_000, FEC_MIN, FEC_STEP_WINDOWS + 1), 15);
        // Never past the band.
        assert_eq!(fec_target(1_000_000, FEC_MAX, 1), FEC_MAX);
    }

    #[test]
    fn wire_budget_derivation_never_overshoots() {
        // 20 Mbps budget, 300 kbps audio, 10 % FEC, 1408-byte shards → 17 130 kbps video.
        assert_eq!(encoder_kbps_for_budget(20_000, 300, 10, 1408), 17_130);
        // Wire spend rounds back under the budget, never over.
        assert_eq!(budget_kbps_for_encoder(17_130, 300, 10, 1408), 19_999);

        // Non-floored roundtrip spends within the budget.
        for budget in [2_000u32, 5_000, 20_000, 100_000, 1_000_000] {
            for fec in [1u8, 5, 10, 25, 50] {
                for audio in [0u32, 256, 512, 8_500] {
                    for payload in [1388u16, 1408, 8896] {
                        let e = encoder_kbps_for_budget(budget, audio, fec, payload);
                        if e > MIN_BITRATE_KBPS {
                            let back = budget_kbps_for_encoder(e, audio, fec, payload);
                            assert!(
                                back <= budget,
                                "budget {budget} fec {fec} audio {audio} payload {payload}: \
                                 derived {e} spends {back}"
                            );
                        }
                    }
                }
            }
        }

        // Budget too small for its audio: floor at MIN and overshoot honestly.
        assert_eq!(
            encoder_kbps_for_budget(500, 8_500, 50, 1408),
            MIN_BITRATE_KBPS
        );

        // More parity ⇒ lower video rate, same budget.
        let calm = encoder_kbps_for_budget(20_000, 300, 1, 1408);
        let burned = encoder_kbps_for_budget(20_000, 300, 5, 1408);
        let stormy = encoder_kbps_for_budget(20_000, 300, 50, 1408);
        assert!(calm > burned && burned > stormy);
    }
}
