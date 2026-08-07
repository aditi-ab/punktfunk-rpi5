//! Fault injection proves detection — M4's exit criterion, on the CPU.
//!
//! The native-decode program exists because a field corruption was
//! ARCHITECTURALLY undetectable: FFmpeg's Vulkan decoder creates no status queries,
//! never sets `AV_FRAME_FLAG_CORRUPT`, and reports trouble only as log lines. The
//! native lane now has the signals. This harness fires them on purpose, so
//! "detection works" is a test result rather than a belief.
//!
//! It drives the REAL detection path minus the GPU: [`pf_vkdecode::AuFault`] — the
//! very injector the client's `PUNKTFUNK_AU_FAULT` knob arms — damages a real
//! 250-AU host-shaped stream, the damaged AUs go through the planner exactly as
//! `VkH264Decoder::decode` / `VkH265Decoder::decode` feed it, and the verdict is
//! read with [`pf_vkdecode::is_integrity_warning`] /
//! [`pf_vkdecode::is_integrity_warning_h265`] — the same predicates the client
//! conceals on. Nothing is mocked; what is absent is only the Vulkan submission
//! below the planner, which cannot change a plan's warnings.
//!
//! **Both codecs**, because they detect a lost AU through genuinely different
//! predicates and a harness covering one proves nothing about the other. H.264 has
//! a `frame_num` gap — an explicit, cheap counter break that fires whether or not
//! the missing picture is ever referenced. HEVC has no analogue at all (POC is
//! coded per picture and simply jumps), so its entire drop detection rests on the
//! RPS resolving a reference the DPB does not hold: `MissingReference` or nothing.
//! HEVC ships in this milestone, so it is tested here on the same terms.
//!
//! What it therefore proves, and — just as deliberately — what it proves is
//! IMPOSSIBLE here:
//!
//! * **Proved**: a dropped AU is DETECTED as *integrity* damage within a bounded
//!   number of AUs, on both codecs — the class that makes the client release the
//!   picture unshown and ask for a re-anchor. That is M4's exit criterion for the
//!   parser-visible half.
//! * **Proved**: the clean stream produces NO integrity warning anywhere, so the
//!   detector cannot be passing by firing on everything (the failure mode that
//!   would cost a keyframe round trip per second on healthy links).
//! * **Proved impossible**: truncation and payload flips are INVISIBLE to the
//!   parser. Annex-B carries no NALU length, so a cut slice is just a shorter
//!   slice, and a flipped payload byte is syntactically perfect. Both decode to a
//!   wrong picture with nothing in the bitstream to object to. The only detector
//!   left is the driver's per-op `RESULT_STATUS` verdict, which needs real hardware
//!   — the GPU smoke/parity tests' ground. These assertions are the negative space
//!   that justifies the status ring existing at all, and the reason a session on a
//!   driver without `queryResultStatusSupport` must be reported as unmeasured
//!   rather than clean.
//! * **Proved impossible, and correctly so**: a dropped SUB-LAYER NON-REFERENCE
//!   picture is invisible to the planner on either codec, because nothing ever
//!   references it. That is not a hole in detection — no later picture is damaged
//!   — it is a missing OUTPUT frame, which is the wire's frame-index gap detector's
//!   job (`punktfunk_core::reanchor::index_gap`), not the bitstream's. The HEVC
//!   vector below contains both kinds and the test asserts both verdicts, so the
//!   distinction cannot quietly become "HEVC misses drops".

use pf_bitstream::h264::H264Planner;
use pf_bitstream::h265::H265Planner;
use pf_vkdecode::{
    is_integrity_warning, is_integrity_warning_h265, AuFault, FaultAction, FaultMode,
};
use std::io::Cursor;

/// The same vendored vectors the WP-A conversion tests and the GPU tests use: 250
/// AUs of real encoder output each, an IDR then P-frames — the punktfunk
/// envelope's shape, in both codecs.
const TEST_25FPS_H264: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
);
const TEST_25FPS_H265: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
);

/// Test-only H.264 AU splitter, mirroring pf-bitstream's (`#[cfg(test)]`-private
/// there) and the GPU tests'.
fn split_h264(stream: &[u8]) -> Vec<&[u8]> {
    use cros_codecs::codec::h264::parser::{Nalu, NaluType};
    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let nalu_offset = cursor.position() as usize;
        let start = nalu_offset - nalu.offset;
        let is_slice = matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr);
        let first_mb_zero = is_slice && stream.get(nalu_offset + 1).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_mb_zero) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// The H.265 twin, mirroring `pic_h265`'s test splitter: a new AU starts at a
/// non-VCL NALU following slices, or at a slice segment whose
/// `first_slice_segment_in_pic_flag` is set (the first bit of the byte after the
/// 2-byte NAL header) when the current AU already has slices.
fn split_h265(stream: &[u8]) -> Vec<&[u8]> {
    use cros_codecs::codec::h265::parser::Nalu;
    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let header_start = cursor.position() as usize;
        let start = header_start - nalu.offset;
        let is_slice = (nalu.header.type_ as u32) < 32;
        let first_slice_flag =
            is_slice && stream.get(header_start + 2).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_slice_flag) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// What the client would do with each AU, condensed to the one bit that matters:
/// did THIS AU make the client conceal and ask for recovery?
///
/// Mirrors `video_vk_native::NativeVulkanDecoder::decode`'s CPU half exactly — a
/// plan that fails outright is trouble, and a plan whose warnings include an
/// integrity warning is damage. (The third source, a driver `Failed` verdict on a
/// prior frame, has no CPU analogue and is the GPU tests' business.)
fn damaged_h264(planner: &mut H264Planner, au: &[u8]) -> bool {
    match planner.plan_au(au) {
        Ok(plan) => plan.warnings.iter().any(is_integrity_warning),
        // A refusal is the loudest possible detection: the client turns it into an
        // Err, releases nothing, and asks. Counting it as detected is what the
        // client does.
        Err(_) => true,
    }
}

/// The H.265 twin — with the one arm that is NOT damage.
///
/// `RaslSkipped` is the spec's own answer (8.1.3 NOTE) for a leading picture whose
/// references precede an open-GOP join: `VkH265Decoder::decode` turns it into
/// `Ok(None)` and clears the warning ledger, so the client neither drops a frame
/// nor asks for anything. Counting it here would let this harness "detect" a fault
/// through a path production treats as a non-event.
fn damaged_h265(planner: &mut H265Planner, au: &[u8]) -> bool {
    use pf_bitstream::h265::PlanError;
    match planner.plan_au(au) {
        Ok(plan) => plan.warnings.iter().any(is_integrity_warning_h265),
        Err(PlanError::RaslSkipped { .. }) => false,
        Err(_) => true,
    }
}

/// Replay a vector with `fault` armed, returning for each AU index whether the
/// client would have flagged damage — plus how many AUs were actually faulted, so
/// a test can prove the injector fired at all rather than passing vacuously.
///
/// Generic over the codec through the split + verdict pair, so both legs replay
/// through ONE driver: the injector, the AU cadence and the "a dropped AU has no
/// verdict of its own" rule are codec-independent, and forking them per codec is
/// how the two legs would quietly stop testing the same thing.
fn replay<'a>(
    stream: &'a [u8],
    split: fn(&'a [u8]) -> Vec<&'a [u8]>,
    mut verdict: impl FnMut(&[u8]) -> bool,
    fault: Option<AuFault>,
) -> (Vec<bool>, usize) {
    let aus = split(stream);
    let mut fault = fault;
    let mut flags = Vec::with_capacity(aus.len());
    let mut faulted = 0usize;
    for au in aus {
        let action = match &mut fault {
            Some(f) => f.apply(au),
            None => FaultAction::Pass,
        };
        match action {
            FaultAction::Pass => flags.push(verdict(au)),
            FaultAction::Drop => {
                faulted += 1;
                // The AU never reaches the decoder — exactly what the client does
                // for a dropped AU, and exactly what the network does for a lost
                // one. No verdict for this index.
                flags.push(false);
            }
            FaultAction::Corrupt(bytes) => {
                faulted += 1;
                flags.push(verdict(&bytes));
            }
        }
    }
    (flags, faulted)
}

fn replay_h264(fault: Option<AuFault>) -> (Vec<bool>, usize) {
    let mut planner = H264Planner::new();
    replay(
        TEST_25FPS_H264,
        split_h264,
        move |au| damaged_h264(&mut planner, au),
        fault,
    )
}

fn replay_h265(fault: Option<AuFault>) -> (Vec<bool>, usize) {
    let mut planner = H265Planner::new();
    replay(
        TEST_25FPS_H265,
        split_h265,
        move |au| damaged_h265(&mut planner, au),
        fault,
    )
}

/// How many AUs after the fault detection is allowed to take on H.264. ONE: a
/// dropped reference is visible to the planner on the very next AU that references
/// it. The bound is stated as a number rather than "eventually" because
/// "eventually" is what the 500 ms freeze backstop already provides — the whole
/// point of local detection is that it is immediate.
const DETECT_WITHIN: usize = 1;

/// The fault period the tests replay at: five faults over the 250-AU vector, far
/// enough apart that each one's detection is unambiguously about ITS fault.
const PERIOD: u32 = 50;

/// The 0-based indices [`AuFault`] faults at `PERIOD`, over `total` AUs. The
/// injector counts the AUs it is OFFERED, 1-based, so the first fault lands on
/// index `PERIOD - 1` — which is also why any period above 1 leaves a session's
/// opening parameter sets and IDR untouched.
fn fault_indices(total: usize) -> Vec<usize> {
    (PERIOD as usize - 1..total)
        .step_by(PERIOD as usize)
        .collect()
}

/// Every faulted AU is followed within [`DETECT_WITHIN`] AUs by a damage verdict.
/// Returns how many faults actually had a successor window to check, so the caller
/// can refuse a vacuous pass.
fn assert_detected_after_each_fault(flags: &[bool], what: &str) -> usize {
    let mut checked = 0usize;
    for dropped in fault_indices(flags.len()) {
        // The last AU of the vector has no successor to detect on — the stream
        // simply ends there. Skipping it is honest; asserting on it would be a
        // statement about the fixture's length, not about detection.
        let Some(window) = flags.get(dropped + 1..(dropped + 1 + DETECT_WITHIN).min(flags.len()))
        else {
            continue;
        };
        if window.is_empty() {
            continue;
        }
        checked += 1;
        assert!(
            window.iter().any(|&d| d),
            "{what}: the AU(s) after dropped AU {dropped} must read as damaged — \
             the reference it needs was never decoded"
        );
    }
    checked
}

/// The indices that read as damaged — the message a failing assertion needs.
fn flagged(flags: &[bool]) -> Vec<usize> {
    flags
        .iter()
        .enumerate()
        .filter(|(_, &d)| d)
        .map(|(i, _)| i)
        .collect()
}

/// The control: a healthy stream must produce NO damage verdict anywhere, on
/// either codec. Without this the fault tests below prove nothing — a detector
/// that fires on every AU would pass them and cost a keyframe round trip per
/// second in the field.
#[test]
fn a_clean_stream_never_reads_as_damaged() {
    for (codec, (flags, faulted)) in [("h264", replay_h264(None)), ("h265", replay_h265(None))] {
        assert_eq!(faulted, 0, "{codec}: no fault armed");
        assert_eq!(flags.len(), 250, "{codec}: the whole vector replayed");
        assert!(
            flagged(&flags).is_empty(),
            "{codec}: the clean vector must plan without a single integrity \
             warning — flagged AUs: {:?}",
            flagged(&flags)
        );
    }
}

/// A DROPPED access unit — the everyday network-loss shape — is detected on the
/// next AU, because that AU references a picture the DPB never received. This is
/// the exit criterion's first half: deliberately corrupted input, detection within
/// a bounded number of frames.
#[test]
fn a_dropped_access_unit_is_detected_on_the_very_next_one() {
    let (flags, faulted) = replay_h264(Some(AuFault::new(FaultMode::Drop, PERIOD)));
    assert!(faulted >= 4, "the injector fired ({faulted} AUs dropped)");
    let checked = assert_detected_after_each_fault(&flags, "h264");
    assert!(
        checked >= 4,
        "{checked} drops actually had a successor to check"
    );
}

/// The H.265 leg of the same criterion, and the reason it is a separate test
/// rather than a loop over both codecs: HEVC detects a dropped AU through a
/// DIFFERENT predicate, and it has a class of AU whose loss is legitimately
/// invisible.
///
/// There is no `frame_num` gap to notice — POC is coded per picture and a jump in
/// it is legal — so everything rests on the RPS asking for a picture the DPB does
/// not hold (`MissingReference`). If that ever stopped firing, HEVC would lose
/// drop detection entirely while the H.264 leg above stayed green.
///
/// And the RPS can only speak for pictures something REFERENCES. A sub-layer
/// non-reference picture (`TRAIL_N` and friends — 3 of the 5 faults this vector
/// takes) is referenced by nothing, so its loss damages no later picture and the
/// planner is right to stay silent: it is a missing output frame, caught by the
/// wire's frame-index gap, not by the bitstream. Asserting BOTH verdicts is what
/// stops that correct silence from being mistaken for a detection hole — or a
/// detection hole from hiding behind it.
#[test]
fn a_dropped_hevc_reference_picture_is_detected_through_the_rps() {
    // Which AUs carry a picture something can reference? Read off a CLEAN replay,
    // so the classification is the stream's own and not this test's guess.
    let mut planner = H265Planner::new();
    let referenced: Vec<bool> = split_h265(TEST_25FPS_H265)
        .into_iter()
        .map(|au| {
            planner
                .plan_au(au)
                .map(|plan| !plan.picture.nalu_type.is_slnr())
                .unwrap_or(false)
        })
        .collect();

    let (flags, faulted) = replay_h265(Some(AuFault::new(FaultMode::Drop, PERIOD)));
    assert!(faulted >= 4, "the injector fired ({faulted} AUs dropped)");

    let (mut checked_refs, mut checked_slnr) = (0usize, 0usize);
    for dropped in fault_indices(flags.len()) {
        let Some(window) = flags.get(dropped + 1..(dropped + 1 + DETECT_WITHIN).min(flags.len()))
        else {
            continue;
        };
        if window.is_empty() {
            continue;
        }
        if referenced[dropped] {
            checked_refs += 1;
            assert!(
                window.iter().any(|&d| d),
                "h265: the AU after dropped REFERENCE picture {dropped} must read \
                 as damaged — its RPS names a picture the DPB never received"
            );
        } else {
            checked_slnr += 1;
            assert!(
                !window.iter().any(|&d| d),
                "h265: dropping sub-layer non-reference picture {dropped} damages \
                 nothing — if the planner starts complaining here it is reporting \
                 damage that did not happen, and every such report costs a frame \
                 and a keyframe round trip"
            );
        }
    }
    assert!(
        checked_refs >= 2 && checked_slnr >= 2,
        "the vector must exercise BOTH classes ({checked_refs} reference drops, \
         {checked_slnr} non-reference drops) or this test proves only half of what \
         it claims"
    );
}

/// A TRUNCATED access unit is NOT parser-visible, and the assertion is that it
/// stays that way — on both codecs.
///
/// Annex-B has no NALU length field: a slice cut at a byte boundary is
/// indistinguishable from a shorter slice. Its header parses, the picture plans,
/// it enters the DPB, and every later AU resolves its reference against an entry
/// that exists — so nothing in the syntax is ever wrong. (pf-bitstream's
/// `TruncatedAu` warning is a narrower thing entirely: a NALU whose HEADER is
/// malformed with real data still behind it.) The hardware, meanwhile, is handed a
/// slice whose bitstream ends mid-picture, which is a decode error it can report —
/// so this mode is how a lab run fires the driver's `RESULT_STATUS` detector
/// deterministically, and it is useless without one.
#[test]
fn a_truncated_access_unit_is_invisible_to_the_parser_and_needs_the_driver_verdict() {
    for (codec, (flags, faulted)) in [
        (
            "h264",
            replay_h264(Some(AuFault::new(FaultMode::Truncate, PERIOD))),
        ),
        (
            "h265",
            replay_h265(Some(AuFault::new(FaultMode::Truncate, PERIOD))),
        ),
    ] {
        assert!(
            faulted >= 4,
            "{codec}: the injector fired ({faulted} AUs truncated)"
        );
        assert!(
            flagged(&flags).is_empty(),
            "{codec}: a mid-slice cut carries no syntax error — if this starts \
             firing, the cut is landing on a NALU header and the mode has stopped \
             exercising the driver-only path it exists for (flagged AUs: {:?})",
            flagged(&flags)
        );
    }
}

/// The mode that shows what the DRIVER's status query is for: a byte flipped deep
/// in a slice payload leaves a bitstream that parses perfectly, resolves every
/// reference, and decodes to a wrong picture. The planner is silent — as it should
/// be, since nothing about the syntax is wrong — and on the FFmpeg rungs that
/// silence is the end of the story (`nb_queries = 0`, no `AV_FRAME_FLAG_CORRUPT`).
/// This is the Xbox Ally X class exactly, on both codecs.
#[test]
fn a_payload_bit_flip_is_invisible_to_the_parser_which_is_why_the_status_query_exists() {
    for (codec, (flags, faulted)) in [
        (
            "h264",
            replay_h264(Some(AuFault::new(FaultMode::Flip, PERIOD))),
        ),
        (
            "h265",
            replay_h265(Some(AuFault::new(FaultMode::Flip, PERIOD))),
        ),
    ] {
        assert!(
            faulted >= 4,
            "{codec}: the injector fired ({faulted} AUs flipped)"
        );
        assert!(
            flagged(&flags).is_empty(),
            "{codec}: a payload flip must not be parser-visible — if this ever \
             starts firing, the flip is landing in syntax rather than payload and \
             the mode has stopped testing what it claims (flagged AUs: {:?})",
            flagged(&flags)
        );
    }
}
