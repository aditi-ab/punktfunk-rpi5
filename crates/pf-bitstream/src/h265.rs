// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file (vendor/cros-codecs/LICENSE).
//
// Adapted from cros-codecs `decoder/stateless/h265.rs` (see
// vendor/cros-codecs/PROVENANCE.md for the snapshot pin). The spec machinery — POC
// computation (8.3.1, in the vendored `PictureData`), reference picture set derivation
// and marking (8.3.2), reference list construction (8.3.3/8.3.4), DPB update and
// bumping (C.5.2) — is ported faithfully and keeps upstream's structure and
// spec-section comments so future upstream diffs stay legible. Stripped: the
// StatelessDecoder/backend trait plumbing, fd/event machinery, pooled-buffer handling,
// and the SCC current-picture-reference paths (the envelope gate below rejects SCC
// self-referencing outright, so a reference list can never contain the current
// picture).

//! Per-AU H.265 planning: [`H265Planner::plan_au`] turns one access unit exactly as
//! the pump hands it to a decoder (Annex-B, parameter sets + the slice segments of one
//! picture) into an [`AuPlan`] — everything a stateless hardware decoder needs before
//! submission and nothing it has to re-derive: parsed headers, POC, the derived
//! reference picture sets (including the long-term entries host RFI recovery leans
//! on — HEVC's analogue of H.264 LTR/MMCO), per-slice reference lists and the DPB
//! delta.
//!
//! Concealment posture, same as the H.264 layer: an RPS entry that names a picture the
//! DPB does not hold is a [`PlanWarning`], never an error — the reference is
//! substituted in place, the session layer sees the warning and requests recovery
//! while planning continues. [`PlanError`] is reserved for AUs that cannot (or, for
//! RASL pictures behind a skipped CRA, must not) be planned at all.
//!
//! WP-2 contract note (the pf-vkdecode/client wiring): the existing H.264 session
//! layer maps every planner `Err` to release-the-frame-unshown + request-reanchor.
//! [`PlanError::RaslSkipped`] must NOT inherit that mapping in the future H.265
//! backend — it is the spec's own skip (8.1.3 NOTE: decode nothing, show nothing,
//! the stream is healthy), so the backend treats it as an Ok-skip of the AU, never
//! as a recovery trigger. Dead in the field today (punktfunk hosts emit IDR-only
//! re-entry points), recorded here so WP-2 does not copy the H.264 error path
//! blindly.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::mem;
use std::ops::Range;
use std::rc::Rc;

use cros_codecs::codec::h265::dpb::Dpb;
use cros_codecs::codec::h265::dpb::DpbEntry;
use cros_codecs::codec::h265::parser::Nalu;
use cros_codecs::codec::h265::parser::Parser;
use cros_codecs::codec::h265::parser::Pps;
use cros_codecs::codec::h265::parser::ShortTermRefPicSet;
use cros_codecs::codec::h265::parser::Slice;
use cros_codecs::codec::h265::parser::Sps;
use cros_codecs::codec::h265::picture::PictureData;
use cros_codecs::codec::h265::picture::Reference;
use cros_codecs::Resolution;
use tracing::trace;

pub use cros_codecs::codec::h265::parser::Level;
pub use cros_codecs::codec::h265::parser::NaluType;
pub use cros_codecs::codec::h265::parser::SliceHeader;

// Codec-neutral plan vocabulary, shared with (and canonically owned by) the H.264
// layer: pf-vkdecode already imports these from `pf_bitstream::h264`, so they stay
// defined there and are re-exported here rather than lifted to a third module.
pub use crate::h264::ColourDescription;
pub use crate::h264::DisplayCrop;
pub use crate::h264::DpbUpdate;
pub use crate::h264::PicId;

use crate::sei;
pub use crate::sei::RecoveryPointHevc;

/// Everything a backend needs to submit one access unit.
#[derive(Debug, Clone)]
pub struct AuPlan {
    pub picture: PicturePlan,
    /// The picture's 8.3.2 reference picture sets, resolved to stored pictures —
    /// the DPB snapshot DXVA picparams and Vulkan `StdVideoDecodeH265PictureInfo`
    /// both key their reference arrays by.
    pub rps: RpsPlan,
    pub slices: Vec<SlicePlan>,
    pub dpb: DpbUpdate,
    /// Every picture the DPB holds marked "used for reference" at the moment this AU
    /// decodes — the MARKED DPB, which is a SUPERSET of [`Self::rps`]'s three current
    /// sets.
    ///
    /// The difference is 8.3.2's *Foll* sets: `RefPicSetStFoll` and `RefPicSetLtFoll`
    /// are pictures this AU keeps marked for FUTURE pictures to reference while
    /// naming none of them itself. They belong in `DXVA_PicParams_HEVC::RefPicList`
    /// all the same — that array is spec-defined as the pictures currently marked
    /// used for reference, and libavcodec's DXVA HEVC path fills it by walking its
    /// whole DPB for `HEVC_FRAME_FLAG_{LONG,SHORT}_REF`, then points the
    /// `RefPicSet*Curr` INDEX arrays into it. A driver keeping per-reference state is
    /// entitled to read a picture's absence from `RefPicList` as "no longer a
    /// reference" and discard it; a long-term anchor held across an RFI recovery
    /// window is exactly the picture that disappears and comes back.
    ///
    /// Vulkan asks the opposite question — `pReferenceSlots` is spec-defined as the
    /// slots THIS decode operation uses — so the native Vulkan rung binds
    /// [`Self::rps`] and is right to.
    ///
    /// Captured at BEGIN-picture time: after 8.3.2 has derived and MARKED this
    /// picture's reference picture sets and after C.5.2.2's pre-decode DPB update,
    /// before the current picture itself is stored. It therefore never contains the
    /// current picture, and — like libavcodec's walk — never contains a picture the
    /// RPS just unmarked.
    ///
    /// Order is the DPB's own (oldest stored first), which is also libavcodec's.
    /// Entries are unique: one picture, one surface, one marking.
    pub dpb_refs: Vec<RefPic>,
    pub warnings: Vec<PlanWarning>,
    /// The SPS the planner activated for this AU — the one [`Self::picture`]'s
    /// parameters derive from (the FIRST slice's PPS's SPS; a later slice segment may
    /// legally reference another PPS, and that drift deliberately does not reach
    /// here). Cloned out of the parser's table so backends build their parameter
    /// objects from exactly what was activated, never by re-parsing the AU.
    pub sps: Rc<Sps>,
    /// The PPS the picture was BEGUN with (the first slice's), same contract as
    /// [`Self::sps`]. Its `sps` field is the same `Rc` as [`Self::sps`]; the VPS, if
    /// the stream carried one, hangs off `sps.vps`.
    pub pps: Rc<Pps>,
}

/// Per-picture parameters, captured after 8.3.1 POC derivation and 8.3.2 RPS marking
/// (the values a hardware picture-parameters struct wants).
#[derive(Debug, Clone)]
pub struct PicturePlan {
    /// The picture's NALU type — HEVC encodes the picture taxonomy (IDR/BLA/CRA,
    /// RADL/RASL, sub-layer non-reference) here rather than in header flags.
    pub nalu_type: NaluType,
    pub is_idr: bool,
    pub is_irap: bool,
    /// `NoRaslOutputFlag` (8.1.3): set on every IDR/BLA and on a CRA that opens the
    /// bitstream or follows an EOS — the signal that RASL pictures leading this IRAP
    /// are undecodable.
    pub no_rasl_output_flag: bool,
    /// Whether later pictures may reference this one. Every planned picture is stored
    /// in the DPB (C.3.4 marks it "used for short-term reference" wholesale); this is
    /// false only for sub-layer non-reference NALU types, which nothing at the same
    /// temporal layer may reference.
    pub is_reference: bool,
    /// `PicOrderCntVal` per 8.3.1.
    pub pic_order_cnt: i32,
    pub coded_width: u32,
    pub coded_height: u32,
    /// Conformance-window crop (7.4.3.2.1: `conf_win_*` offsets scale by
    /// SubWidthC/SubHeightC), in luma samples of the coded picture.
    pub display_crop: DisplayCrop,
    /// Colour signalling from the ACTIVE SPS's VUI (E.3.1 inference where absent —
    /// the vendored parser defaults the colour code points to 2/"unspecified" with
    /// limited range, so reading unconditionally IS the inference). Per picture,
    /// never latched at session start: the Windows host switches an HDR desktop to
    /// PQ/BT.2020 IN-BAND with a new SPS mid-stream.
    pub colour: ColourDescription,
    pub general_profile_idc: u8,
    pub level_idc: Level,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub chroma_format_idc: u8,
    /// DPB size in frames: the stream's own `sps_max_dec_pic_buffering_minus1 + 1`,
    /// capped at 16 — backends size their slot pool from this. See [`dpb_limit`] for
    /// why this is NOT equation A-2's level ceiling.
    pub max_dpb_frames: usize,
    /// Bits of the `st_ref_pic_set()` the FIRST slice carried inline (0 when the RPS
    /// came from the SPS by index) — Vulkan's `NumBitsForSTRefPicSetInSlice`.
    pub short_term_ref_pic_set_size_bits: u32,
    pub recovery_point: Option<RecoveryPointHevc>,
    /// Every picture this AU predicts from was itself decoded from a fully-available
    /// reference chain — so a host claim that this AU is a clean re-anchor
    /// (`USER_FLAG_RECOVERY_ANCHOR`) can be corroborated rather than taken on trust.
    /// `true` for an IRAP (nothing to predict from) and for any picture whose whole
    /// reference chain is clean; `false` from the moment this AU — or anything it
    /// descends from — needed concealment.
    ///
    /// Purely additive observation: nothing in the plan, the warnings or the DPB
    /// changes because of it, and on a stream that never loses a reference it is
    /// `true` on every picture forever. See [`crate::clean`] for why it propagates and
    /// why every rule errs toward `false`.
    pub references_clean: bool,
}

/// A reference list / RPS entry: the minimum every backend picparams format needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub id: PicId,
    /// The stored picture's `PicOrderCntVal` — the key HEVC hardware formats identify
    /// references by (there is no `frame_num` in this codec).
    pub pic_order_cnt: i32,
    pub is_long_term: bool,
}

/// The three "current" reference picture sets of 8.3.2, resolved to stored pictures.
///
/// Entries the DPB could not resolve are ABSENT here (each one was flagged via
/// [`PlanWarning::MissingReference`] when the RPS was derived); the per-slice
/// reference lists — where positional stability matters because `ref_idx` indexes
/// them — conceal by substitution instead.
#[derive(Debug, Clone, Default)]
pub struct RpsPlan {
    /// `RefPicSetStCurrBefore`: short-term references with POC below the current
    /// picture's, nearest first.
    pub st_curr_before: Vec<RefPic>,
    /// `RefPicSetStCurrAfter`: short-term references with POC above the current
    /// picture's, nearest first.
    pub st_curr_after: Vec<RefPic>,
    /// `RefPicSetLtCurr`: the long-term references — the entries punktfunk hosts'
    /// RFI recovery rides.
    pub lt_curr: Vec<RefPic>,
}

/// One slice segment NALU of the picture, with its reference lists fully derived.
#[derive(Debug, Clone)]
pub struct SlicePlan {
    /// Byte range of the slice NALU in the input AU, start code included — hardware
    /// decoders take the raw bitstream, so the plan points instead of copying.
    pub data: Range<usize>,
    /// The parsed slice segment header. For a dependent slice segment this is the
    /// COMPLETED header — the inherited fields already copied from the picture's last
    /// independent slice segment (7.4.7.1), so backends never see a partial header.
    pub header: SliceHeader,
    pub ref_list0: Vec<RefPic>,
    pub ref_list1: Vec<RefPic>,
}

/// Concealment signals: planning continues, the session layer requests recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWarning {
    /// An RPS entry (or a reference list built from one) named a picture the DPB does
    /// not hold — at least one reference AU was lost upstream. The corresponding
    /// reference-list positions carry an in-place substitute.
    MissingReference {
        context: &'static str,
        detail: String,
    },
    /// The AU's NALU walk stopped early — a truncated NALU with real data behind it,
    /// or a slice belonging to another picture (mis-split AU). The plan covers only
    /// the slices before the cut; `offset` is the byte position of the cut in the AU.
    TruncatedAu { offset: usize },
    /// The activated SPS signals output reordering (`sps_max_num_reorder_pics > 0`).
    /// Spec-legal and fully planned — the C.5.2 bumping honours it — but punktfunk
    /// hosts emit zero-reorder low-delay streams only, so this warning is the field
    /// signal if that assumption ever breaks (the H.264 layer's `Mmco5Rebase` idiom).
    NonZeroReorder { max_num_reorder_pics: u8 },
}

impl PlanWarning {
    /// Does this warning mean the PICTURE is damaged? The H.265 twin of
    /// [`crate::h264::PlanWarning::is_integrity`] — the same one-list argument applies,
    /// and `pf_vkdecode::is_integrity_warning_h265` delegates here.
    ///
    /// `NonZeroReorder` is NOT damage, and excluding it matters more here than the
    /// H.264 exclusions do: it fires on the AU that ACTIVATES an SPS — the opening
    /// IRAP, and the fresh IRAP at every ABR resolution change — so treating it as
    /// concealment would cost a released-unshown frame plus a keyframe round trip at
    /// every renegotiation, on a stream the planner says it planned correctly. It
    /// would also poison the [`crate::clean::CleanLedger`] at exactly those IRAPs,
    /// marking the one picture that is clean by construction as broken.
    ///
    /// Exhaustive with no wildcard, for the reason the H.264 twin spells out.
    pub fn is_integrity(&self) -> bool {
        match self {
            PlanWarning::MissingReference { .. } | PlanWarning::TruncatedAu { .. } => true,
            PlanWarning::NonZeroReorder { .. } => false,
        }
    }
}

/// The AU cannot be planned at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Parse(String),
    /// Legal H.265, but outside what punktfunk hosts emit (clients only decode
    /// punktfunk hosts, so this is a stream-integrity failure, not a feature gap).
    OutsideEnvelope(&'static str),
    NoActiveParamSet {
        pps_id: u8,
    },
    /// [`H265Planner::flush`] discarded the decoding state; planning resumes only at
    /// an IRAP (the port of upstream's `Reset` gating — HEVC's CRA/BLA are full
    /// re-entry points here because a flush marks the next picture "first after EOS",
    /// which gives any IRAP `NoRaslOutputFlag = 1`).
    AwaitingIdr,
    /// A RASL picture whose associated CRA/BLA had `NoRaslOutputFlag = 1` (an
    /// open-GOP join): the spec says it may reference pictures from before the join
    /// and must not be decoded or output (8.1.3 NOTE). The AU is deliberately not
    /// planned — planner state is untouched and the next AU plans normally, mirroring
    /// how the H.264 layer refuses pre-anchor pictures without wedging.
    RaslSkipped {
        poc: i32,
    },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Parse(msg) => write!(f, "parse error: {msg}"),
            PlanError::OutsideEnvelope(what) => {
                write!(f, "outside the punktfunk decode envelope: {what}")
            }
            PlanError::NoActiveParamSet { pps_id } => {
                write!(f, "slice references PPS {pps_id}, which has not been seen")
            }
            PlanError::AwaitingIdr => {
                write!(f, "flushed: waiting for an IRAP to resume planning")
            }
            PlanError::RaslSkipped { poc } => {
                write!(
                    f,
                    "RASL picture (poc {poc}) after a CRA join is not decodable"
                )
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Keeps track of the last values seen for negotiation purposes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NegotiationInfo {
    coded_resolution: Resolution,
    general_profile_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    chroma_format_idc: u8,
    max_dpb_frames: usize,
    /// Output latency is a negotiation fact too: an SPS that raises the reorder
    /// depth mid-stream (same geometry) changes what the session must expect from
    /// [`DpbUpdate::outputs`], and it is what [`PlanWarning::NonZeroReorder`] keys
    /// on — omitting it would let a 0→N reorder switch stall outputs silently.
    max_num_reorder_pics: u8,
}

impl From<&Sps> for NegotiationInfo {
    fn from(sps: &Sps) -> Self {
        NegotiationInfo {
            coded_resolution: Resolution::from((u32::from(sps.width()), u32::from(sps.height()))),
            general_profile_idc: sps.profile_tier_level.general_profile_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: dpb_limit(sps),
            max_num_reorder_pics: sps.max_num_reorder_pics[usize::from(sps.max_sub_layers_minus1)],
        }
    }
}

/// The DPB size the planner enforces: the stream's own
/// `sps_max_dec_pic_buffering_minus1[HighestTid] + 1`, capped at 16.
///
/// This deliberately does NOT consult equation A-2 (the vendored
/// `Sps::max_dpb_size`). A-2 is a CEILING on what an SPS may signal — 7.4.3.2.1
/// constrains `sps_max_dec_pic_buffering_minus1[i]` to `0..=MaxDpbSize - 1` — not a
/// statement of what the stream needs. The size the stream actually needs is the
/// signalled buffering, which is precisely what C.5.2.2's fullness clause bumps
/// against, and A.4.1 bounds the total RPS entries by the same number: `buffering`
/// pictures hold `buffering - 1` references plus the current one, exactly.
///
/// Reading A-2 as a requirement cost us HEVC outright, measured on `.21`
/// (RTX 5070 Ti, 2026-08-07). The host's SPS is minimal and honest at every
/// resolution — `general_level_idc = 153` (L5.1 High, which NVENC autoselects
/// because 130 Mbps needs it), `sps_max_dec_pic_buffering_minus1 = 5`, i.e. six
/// pictures: `RFI_DPB` references plus the current one. But A-2 branches on picture
/// size against the LEVEL's `MaxLumaPs`, and at 1080p the coded 1920x1088 =
/// 2 088 960 samples fall under `MaxLumaPs(L5.1) >> 2` = 2 228 224, taking the first
/// branch: `min(4 * MaxDpbPicBuf, 16)` = 16. The old `max(A-2, buffering)` therefore
/// reported 16 where the stream asked for 6, backends added the current picture and
/// demanded 17 hardware DPB slots, and NVIDIA's Vulkan Video caps `maxDpbSlots` at
/// 16 — so every access unit was refused, the ladder ran out of rungs, and the
/// session reconnected without HEVC (fatal on a build with no software HEVC
/// decoder).
///
/// A resolution sweep on the same box put the blast radius at the two commonest
/// streaming resolutions and nowhere else, which is A-2's branch table exactly:
/// 720p (1280x720 = 921 600) and 1080p died on branch 1 at 16 frames; 1440p
/// (2560x1440 = 3 686 400, under `MaxLumaPs >> 1`) survived on branch 2 at 12; 4K
/// (3840x2176 = 8 355 840, past `3 * MaxLumaPs >> 2`) survived on the `else` branch
/// at 6 and decoded at 1.9 ms. One host, one level, one six-picture requirement —
/// only which branch the picture size landed in decided whether HEVC worked at all.
///
/// The `max()` that produced the 16 bought nothing even for the malformed streams
/// it was written for: `Dpb::needs_bumping` (C.5.2.2) already keys on the signalled
/// buffering, not on `max_num_pics`, so a stream referencing more pictures than it
/// declared was ALREADY being bumped below its own declared depth before every
/// store. All the widened limit ever did was over-allocate hardware surfaces.
///
/// The cap stays: `check_envelope` rejects `buffering > 16` at activation, but this
/// also runs from `NegotiationInfo::from`, which sees SPSes that have not reached
/// the gate yet, and backends size real slot pools from the result.
fn dpb_limit(sps: &Sps) -> usize {
    let buffering =
        usize::from(sps.max_dec_pic_buffering_minus1[usize::from(sps.max_sub_layers_minus1)]) + 1;
    buffering.min(16)
}

/// The RefPicSet data (8.3.2), derived once per picture.
///
/// Upstream keeps fixed `[_; 16]` arrays plus counts; `Vec`s here carry the same
/// derivation without the out-of-bounds panics a hostile header could otherwise
/// reach (a slice may signal more RPS entries than any conforming DPB holds).
#[derive(Default)]
struct RefPicSet {
    /// `PocStCurrBefore` / `PocStCurrAfter` / `PocStFoll` (equation 8-5).
    poc_st_curr_before: Vec<i32>,
    poc_st_curr_after: Vec<i32>,
    poc_st_foll: Vec<i32>,
    /// `PocLtCurr` / `PocLtFoll` with their `delta_poc_msb_present_flag` (8-5).
    poc_lt_curr: Vec<(i32, bool)>,
    poc_lt_foll: Vec<(i32, bool)>,

    /// The resolved sets (equations 8-6/8-7). `None` = the DPB does not hold the
    /// picture (lost upstream); kept positional so the reference-list construction
    /// can substitute in place.
    ref_pic_set_st_curr_before: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_st_curr_after: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_lt_curr: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_st_foll: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_lt_foll: Vec<Option<DpbEntry<PicId>>>,
}

impl RefPicSet {
    /// Whether the current picture has any usable reference at all.
    fn curr_is_empty(&self) -> bool {
        self.poc_st_curr_before.is_empty()
            && self.poc_st_curr_after.is_empty()
            && self.poc_lt_curr.is_empty()
    }
}

/// State of the picture being planned, spanning the slice segments of one AU.
struct CurrentPicState {
    /// Data for the current picture as extracted from the stream.
    pic: PictureData,
    /// PPS at the time of the current slice segment. Follows the slices — a later
    /// segment may reference another PPS — and feeds end-of-picture bumping, as
    /// upstream does.
    pps: Rc<Pps>,
    /// The PPS the picture was BEGUN with. [`H265Planner::picture_plan`] reads this
    /// snapshot so per-picture parameters cannot drift to a later segment's PPS.
    first_slice_pps: Rc<Pps>,
    /// The id backends will know this picture by (upstream: the backend picture).
    id: PicId,
    /// The picture's RPS, resolved at begin-picture time and captured for the plan.
    rps_plan: RpsPlan,
    /// The marked DPB as it stands for THIS picture's decode, captured beside the
    /// RPS that marked it — see [`AuPlan::dpb_refs`].
    dpb_refs: Vec<RefPic>,
    /// 7.4.7.1: `slice_segment_address` must increase across a picture's segments.
    prev_segment_address: Option<u32>,
}

/// Plans H.265 access units for stateless hardware decoders.
///
/// Owns the vendored parser and DPB plus the POC/RPS state that upstream keeps in
/// `H265DecoderState`. One instance per elementary stream; feed AUs in decode order.
pub struct H265Planner {
    parser: Parser,
    negotiation_info: NegotiationInfo,
    dpb: Dpb<PicId>,
    rps: RefPicSet,
    /// Same as `PrevTid0Pic` in the specification (8.3.1).
    prev_tid0_pic: Option<PictureData>,
    /// `MaxPicOrderCntLsb` of the ACTIVE SPS. Upstream latches this at SPS parse
    /// time — the last SPS PARSED, not the one the picture activates; taken from the
    /// activating PPS's SPS here instead (deliberate divergence, in favour of 8.3.1
    /// which reads the active SPS).
    max_pic_order_cnt_lsb: i32,
    /// The value of `NoRaslOutputFlag` for the last IRAP picture.
    irap_no_rasl_output_flag: bool,
    /// Whether the next picture is the first in the bitstream / follows an EOS NALU
    /// (both feed `NoRaslOutputFlag`, 8.1.3).
    first_picture_in_bitstream: bool,
    first_picture_after_eos: bool,
    /// The last independent slice segment header, copied into dependent segments
    /// (7.4.7.1).
    last_independent_header: Option<SliceHeader>,
    /// Next [`PicId`] to hand out (upstream: the backend allocates here).
    next_pic_id: PicId,
    /// Display-ready pictures accumulated while planning (upstream: the decoder's
    /// ready queue). Not cleared on a failed AU — the next emitted [`DpbUpdate`]
    /// carries them, so an error can never swallow a frame.
    pending_outputs: Vec<PicId>,
    /// Ids the last emitted [`DpbUpdate`] left alive: the baseline for `removed`.
    /// Kept across failed AUs so interim evictions are reported, never dropped.
    reported_live: BTreeSet<PicId>,
    /// Set by [`Self::flush`]: planning resumes only at an IRAP (upstream: `Reset`).
    awaiting_idr: bool,
    /// Which resident pictures came off a BROKEN reference chain — the fact behind
    /// [`PicturePlan::references_clean`]. Empty on a healthy stream; see
    /// [`crate::clean::CleanLedger`] for the propagation rules.
    clean: crate::clean::CleanLedger,
}

impl Default for H265Planner {
    fn default() -> Self {
        Self {
            parser: Default::default(),
            negotiation_info: Default::default(),
            dpb: Default::default(),
            rps: Default::default(),
            prev_tid0_pic: None,
            max_pic_order_cnt_lsb: 0,
            irap_no_rasl_output_flag: false,
            first_picture_in_bitstream: true,
            first_picture_after_eos: true,
            last_independent_header: None,
            next_pic_id: 0,
            pending_outputs: Vec::new(),
            reported_live: BTreeSet::new(),
            awaiting_idr: false,
            clean: Default::default(),
        }
    }
}

impl H265Planner {
    pub fn new() -> Self {
        Default::default()
    }

    /// Plan one access unit: Annex-B bytes containing VPS/SPS/PPS/SEI/AUD NALUs plus
    /// the 1..N slice segment NALUs of exactly one picture.
    ///
    /// After a [`PlanError`] the planner state is best-effort ([`PlanError::RaslSkipped`]
    /// excepted — that one leaves the state fully intact); the session should request
    /// an IDR before feeding more AUs. Outputs and removals queued by a failed AU are
    /// retained and emitted with the next successful plan (or [`Self::flush`]) — never
    /// discarded.
    pub fn plan_au(&mut self, au: &[u8]) -> Result<AuPlan, PlanError> {
        let mut warnings = Vec::new();
        let mut slices = Vec::new();
        let mut recovery_point = None;
        let mut current: Option<CurrentPicState> = None;
        let mut saw_nalu = false;

        // Byte position just past the last fully consumed NALU: the truncation
        // detector's anchor. (The cursor is useless for this — after a successful
        // `Nalu::next` it sits at the CURRENT NALU's header, and a failed one leaves
        // it mid-scan, so a cursor-based "start code behind the cursor" test can
        // never fire.)
        let mut consumed_end = 0usize;
        let mut cursor = Cursor::new(au);
        loop {
            let nalu = match Nalu::next(&mut cursor) {
                Ok(nalu) => nalu,
                Err(_) => {
                    // End of the AU — or a NALU cut so short its two header bytes are
                    // missing (unlike H.264, every 6-bit HEVC type code is a valid
                    // header, so this is the only header-level failure). Anything
                    // after the last consumed NALU other than zero bytes
                    // (trailing_zero_8bits padding, B.2.2) is cut-off data: degrade
                    // to a concealment signal covering the slices already planned.
                    let tail = &au[consumed_end.min(au.len())..];
                    if tail.iter().any(|&b| b != 0) {
                        warnings.push(PlanWarning::TruncatedAu {
                            offset: consumed_end,
                        });
                    }
                    break;
                }
            };
            saw_nalu = true;
            // After `Nalu::next` the cursor sits on the NAL header bytes; `offset` is
            // the start-code length and `size` the NALU payload length, which pins the
            // NALU's absolute byte range in the AU without copying.
            let nalu_offset = cursor.position() as usize;
            let range = (nalu_offset - nalu.offset)..(nalu_offset + nalu.size);
            debug_assert_eq!(&au[range.clone()], nalu.data.as_ref());
            consumed_end = range.end;

            // Multilayer/scalable streams put enhancement layers at nuh_layer_id > 0;
            // punktfunk hosts emit single-layer only, and half-decoding the base layer
            // of a stream we do not understand is exactly the kind of silent
            // degradation the envelope gates exist to prevent.
            if nalu.header.nuh_layer_id != 0 {
                return Err(PlanError::OutsideEnvelope(
                    "multilayer stream (nuh_layer_id != 0)",
                ));
            }

            match nalu.header.type_ {
                NaluType::VpsNut => {
                    self.parser.parse_vps(&nalu).map_err(PlanError::Parse)?;
                }
                NaluType::SpsNut => {
                    let sps = self.parser.parse_sps(&nalu).map_err(PlanError::Parse)?;
                    Self::check_envelope(sps)?;
                }
                NaluType::PpsNut => {
                    self.parser.parse_pps(&nalu).map_err(PlanError::Parse)?;
                }
                NaluType::PrefixSeiNut => {
                    // The HEVC NAL header is two bytes; the recovery point is a
                    // prefix-only payload (D.2.1), so suffix SEI NALUs never carry it
                    // and fall through to the skip arm below.
                    match sei::parse_recovery_point_hevc(nalu.as_ref().get(2..).unwrap_or(&[])) {
                        Ok(Some(rp)) => recovery_point = Some(rp),
                        Ok(None) => {}
                        // A broken SEI must not cost the picture it decorates.
                        Err(err) => trace!("ignoring unparseable SEI NALU: {err}"),
                    }
                }
                NaluType::EosNut => {
                    // 8.1.3: the first picture after an end-of-sequence NALU gets
                    // NoRaslOutputFlag = 1.
                    self.first_picture_after_eos = true;
                }
                NaluType::EobNut => {
                    self.first_picture_in_bitstream = true;
                }
                NaluType::TrailN
                | NaluType::TrailR
                | NaluType::TsaN
                | NaluType::TsaR
                | NaluType::StsaN
                | NaluType::StsaR
                | NaluType::RadlN
                | NaluType::RadlR
                | NaluType::RaslN
                | NaluType::RaslR
                | NaluType::BlaWLp
                | NaluType::BlaWRadl
                | NaluType::BlaNLp
                | NaluType::IdrWRadl
                | NaluType::IdrNLp
                | NaluType::CraNut => {
                    // An AU that OPENS with a continuation segment (the first payload
                    // bit — first_slice_segment_in_pic_flag — is 0) is the tail of a
                    // previous picture, mis-split onto this AU. Beginning a picture
                    // from it would fabricate a duplicate (a dependent segment would
                    // even inherit a PREVIOUS AU's independent header wholesale), so
                    // it is skipped behind a concealment signal instead.
                    let first_segment = nalu.as_ref().get(2).is_some_and(|byte| byte & 0x80 != 0);
                    if current.is_none() && !first_segment {
                        warnings.push(PlanWarning::TruncatedAu {
                            offset: range.start,
                        });
                        continue;
                    }
                    // Upstream's `Reset` gating: after a flush, only an IRAP restarts
                    // the decoding process (any IRAP works here, not just IDR: the
                    // flush set `first_picture_after_eos`, which hands a CRA/BLA
                    // NoRaslOutputFlag = 1 and with it full re-entry semantics).
                    // The gate is CLEARED only once the IRAP's picture actually
                    // begins — an IRAP AU that fails before that must not unlatch it.
                    if current.is_none() && self.awaiting_idr && !nalu.header.type_.is_irap() {
                        return Err(PlanError::AwaitingIdr);
                    }
                    let mut slice = match self.parser.parse_slice_header(nalu) {
                        Ok(slice) => slice,
                        Err(err) => {
                            // A continuation segment that fails to parse is a cut mid-
                            // AU: keep the slices already planned behind a concealment
                            // signal. (The H.264 layer's equivalent cut fires one
                            // level up, at the NALU header — HEVC's 2-byte header
                            // accepts every type code, so the failure surfaces here.)
                            if current.is_some() {
                                warnings.push(PlanWarning::TruncatedAu {
                                    offset: range.start,
                                });
                                break;
                            }
                            return Err(Self::slice_parse_error(err));
                        }
                    };

                    // 7.4.7.1: a dependent slice segment inherits everything but its
                    // address from the preceding independent one. Completing the
                    // header HERE means every SlicePlan carries a full header and the
                    // picture-continuity checks below see real values.
                    if slice.header.dependent_slice_segment_flag {
                        let independent =
                            self.last_independent_header.clone().ok_or_else(|| {
                                PlanError::Parse(
                                    "dependent slice segment without a preceding \
                                     independent slice segment header"
                                        .into(),
                                )
                            })?;
                        slice
                            .replace_header(independent)
                            .map_err(PlanError::Parse)?;
                    }

                    match &current {
                        None => {
                            current = Some(self.begin_picture(&slice, &mut warnings)?);
                            self.awaiting_idr = false;
                        }
                        // Upstream would finish the picture and begin another; our
                        // contract is one picture per AU, so a second first-segment
                        // means the pump upstream of us is broken.
                        Some(_) if slice.header.first_slice_segment_in_pic_flag => {
                            return Err(PlanError::OutsideEnvelope(
                                "more than one coded picture in one access unit",
                            ));
                        }
                        Some(cur) => {
                            // Mis-split-AU guard: a continuation segment must belong
                            // to the picture the first segment began (7.4.2.4.4: same
                            // NALU type; 7.4.7.1: same POC lsb). A foreign slice and
                            // everything after it are dropped behind a concealment
                            // signal.
                            if slice.nalu.header.type_ != cur.pic.nalu_type
                                || i32::from(slice.header.pic_order_cnt_lsb)
                                    != cur.pic.slice_pic_order_cnt_lsb
                            {
                                warnings.push(PlanWarning::TruncatedAu {
                                    offset: range.start,
                                });
                                break;
                            }
                        }
                    }
                    if !slice.header.dependent_slice_segment_flag {
                        self.last_independent_header = Some(slice.header.clone());
                    }
                    let cur = current.as_mut().expect("a picture was begun above");
                    slices.push(self.plan_slice(cur, slice, range, &mut warnings)?);
                }
                other => trace!("skipping NAL unit type {other:?}"),
            }
        }

        if !saw_nalu {
            return Err(PlanError::Parse("no NAL units in access unit".into()));
        }
        let cur = current
            .ok_or_else(|| PlanError::Parse("access unit contains no coded picture".into()))?;

        // Was every picture this AU predicts from decoded off an intact chain? Asked
        // over the SLICE reference lists rather than the RPS or the DPB snapshot,
        // because those are what this picture actually predicts from: 8.3.2 RETAINS
        // pictures in the RPS that the current picture does not use
        // (`used_by_curr_pic` clear), and a damaged one among those says nothing about
        // this picture. An IRAP's lists are empty, so this is vacuously true for it
        // (`CleanLedger::references_clean`).
        let references_clean = self.clean.references_clean(
            slices
                .iter()
                .flat_map(|s: &SlicePlan| s.ref_list0.iter().chain(&s.ref_list1))
                .map(|r| r.id),
        );
        let picture = Self::picture_plan(&cur, recovery_point, references_clean);
        let rps = cur.rps_plan.clone();
        let dpb_refs = cur.dpb_refs.clone();
        // The activated parameter sets ride out with the plan (AuPlan field docs);
        // cloned before finish_picture consumes `cur`.
        let pps = Rc::clone(&cur.first_slice_pps);
        let sps = Rc::clone(&pps.sps);
        let stored = self.finish_picture(cur)?;

        // `removed` is the delta against what the backend last SAW alive, not against
        // this call's start — a failed AU in between may have evicted pictures, and
        // those removals must still be reported here.
        let live_after = self.live_ids();
        let mut previously_live = mem::take(&mut self.reported_live);
        previously_live.insert(stored);
        let removed = previously_live.difference(&live_after).copied().collect();
        self.reported_live = live_after;

        // Fold this picture's verdict, then bound the ledger to DPB residency. Both
        // AFTER `finish_picture`, so `stored` is the id the picture really got and
        // `live_after` reflects the C.3.4/8.3.2 marking this AU performed — a mark
        // written against a pre-marking view could survive an eviction it should have
        // died with. `concealed` mirrors what a consumer conceals on, via the ONE
        // classification (`PlanWarning::is_integrity`), so the ledger and the consumer
        // can never disagree about whether this AU was damaged.
        self.clean.note_stored(
            stored,
            references_clean,
            warnings.iter().any(PlanWarning::is_integrity),
        );
        self.clean.retain_live(self.reported_live.iter().copied());

        Ok(AuPlan {
            picture,
            rps,
            slices,
            dpb: DpbUpdate {
                stored: Some(stored),
                outputs: mem::take(&mut self.pending_outputs),
                removed,
            },
            dpb_refs,
            warnings,
            sps,
            pps,
        })
    }

    /// Drain the DPB: every still-buffered picture becomes display-ready and every id
    /// is released. The session calls this at teardown or a stream discontinuity.
    ///
    /// The 8.3 decoding state is discarded with the pictures; planning resumes only at
    /// an IRAP ([`PlanError::AwaitingIdr`] until then). Parameter sets survive — per
    /// 7.4.2.4 they persist until replaced.
    pub fn flush(&mut self) -> DpbUpdate {
        let mut removed = mem::take(&mut self.reported_live);
        removed.extend(self.live_ids());
        self.drain_dpb();

        self.rps = Default::default();
        self.prev_tid0_pic = None;
        self.negotiation_info = Default::default();
        self.last_independent_header = None;
        self.irap_no_rasl_output_flag = false;
        // The resuming picture behaves as the first after an EOS: an IRAP of any
        // flavour gets NoRaslOutputFlag = 1 (8.1.3), which is what makes non-IDR
        // re-entry sound.
        self.first_picture_after_eos = true;
        self.awaiting_idr = true;
        // The DPB is drained, so no mark describes a resident picture any more — and
        // planning resumes at an IRAP, which is clean by construction.
        self.clean.clear();

        DpbUpdate {
            stored: None,
            outputs: mem::take(&mut self.pending_outputs),
            removed: removed.into_iter().collect(),
        }
    }

    /// The envelope gate: punktfunk clients only decode punktfunk hosts, and no host
    /// emits interlaced video, separate-colour-plane coding, SCC self-referencing or
    /// an oversized DPB.
    fn check_envelope(sps: &Sps) -> Result<(), PlanError> {
        if sps.separate_colour_plane_flag {
            return Err(PlanError::OutsideEnvelope(
                "separate colour plane coding (separate_colour_plane_flag == 1)",
            ));
        }
        // HEVC has no frame_mbs_only_flag; field coding is signalled through the
        // VUI's field_seq_flag (and pic_struct SEI). The vendored parser defaults the
        // flag to 0 when the VUI is absent, so the read is safe unconditionally.
        if sps.vui_parameters.field_seq_flag {
            return Err(PlanError::OutsideEnvelope(
                "field-coded stream (vui field_seq_flag == 1)",
            ));
        }
        let ptl = &sps.profile_tier_level;
        if ptl.general_interlaced_source_flag && !ptl.general_progressive_source_flag {
            return Err(PlanError::OutsideEnvelope(
                "interlaced source (general_interlaced_source_flag)",
            ));
        }
        // A.4 caps the DPB at 16 frames. sps_max_dec_pic_buffering_minus1 is what
        // C.5.2.2's fullness clause trusts, the vendored parser reads it up to 16
        // (17 frames), and no hardware decoder implements a deeper DPB — a larger
        // value is a corrupt (or hostile) SPS, not a feature request. Backends size
        // real slot pools from this, so it is gated here, at SPS activation.
        let buffering =
            usize::from(sps.max_dec_pic_buffering_minus1[usize::from(sps.max_sub_layers_minus1)])
                + 1;
        if buffering > 16 {
            return Err(PlanError::OutsideEnvelope(
                "DPB deeper than 16 frames (sps_max_dec_pic_buffering_minus1)",
            ));
        }
        if sps.scc_extension.curr_pic_ref_enabled_flag {
            return Err(PlanError::OutsideEnvelope(
                "SCC current-picture referencing (sps_curr_pic_ref_enabled_flag)",
            ));
        }
        // 7.4.3.2.1 bounds the conformance window inside the coded size. The vendored
        // `visible_rectangle()` subtracts in u32 and would panic on an offset sum
        // beyond the picture; validated here (in u64 — the offsets are unbounded
        // ue(v)) so a hostile SPS is an error, not a crash.
        const SUB_WIDTH_C: [u64; 4] = [1, 2, 2, 1];
        const SUB_HEIGHT_C: [u64; 4] = [1, 2, 1, 1];
        if sps.conformance_window_flag {
            let idx = usize::from(sps.chroma_array_type.min(3));
            let horizontal = SUB_WIDTH_C[idx]
                * (u64::from(sps.conf_win_left_offset) + u64::from(sps.conf_win_right_offset));
            let vertical = SUB_HEIGHT_C[idx]
                * (u64::from(sps.conf_win_top_offset) + u64::from(sps.conf_win_bottom_offset));
            if horizontal >= u64::from(sps.width()) || vertical >= u64::from(sps.height()) {
                return Err(PlanError::Parse(
                    "conformance window exceeds the coded picture".into(),
                ));
            }
        }
        Ok(())
    }

    /// Map a vendored slice-header parse failure, sniffing the missing-PPS message so
    /// it surfaces as [`PlanError::NoActiveParamSet`]. The prefix match is
    /// best-effort: if an upstream re-sync rewords it, the error degrades to `Parse`,
    /// not silence.
    fn slice_parse_error(err: String) -> PlanError {
        match err.strip_prefix("Could not get PPS for pic_parameter_set_id ") {
            Some(id) => PlanError::NoActiveParamSet {
                pps_id: id.trim().parse().unwrap_or(0),
            },
            None => PlanError::Parse(err),
        }
    }

    /// Ids of every picture the DPB currently holds.
    fn live_ids(&self) -> BTreeSet<PicId> {
        self.dpb.entries().iter().map(|entry| entry.1).collect()
    }

    /// Queue the pictures the C.5.2 bumping process declares ready for output.
    /// `additional` selects C.5.2.3 (after decoding the picture) over C.5.2.2
    /// (before), exactly upstream's `BumpingType`.
    fn bump_as_needed(&mut self, sps: &Sps, additional: bool) {
        loop {
            let needs = if additional {
                self.dpb.needs_additional_bumping(sps)
            } else {
                self.dpb.needs_bumping(sps)
            };
            if !needs {
                break;
            }
            match self.dpb.bump(false) {
                Some(entry) => self.pending_outputs.push(entry.1),
                None => break,
            }
        }
    }

    /// Queue all frames still pending output and empty the DPB.
    fn drain_dpb(&mut self) {
        let pics = self.dpb.drain();
        self.pending_outputs.extend(pics.into_iter().map(|e| e.1));
        self.dpb.clear();
    }

    // See 8.3.2, Note 2.
    fn st_ref_pic_set<'a>(
        hdr: &'a SliceHeader,
        sps: &'a Sps,
    ) -> Result<&'a ShortTermRefPicSet, PlanError> {
        if hdr.curr_rps_idx == sps.num_short_term_ref_pic_sets {
            Ok(&hdr.short_term_ref_pic_set)
        } else {
            sps.short_term_ref_pic_set
                .get(usize::from(hdr.curr_rps_idx))
                .ok_or_else(|| PlanError::Parse("invalid short_term_ref_pic_set_idx".into()))
        }
    }

    // See 8.3.2: derivation of the five POC lists.
    fn decode_rps(
        &mut self,
        slice: &Slice,
        sps: &Sps,
        cur_pic: &PictureData,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<(), PlanError> {
        let hdr = &slice.header;

        if cur_pic.nalu_type.is_irap() && cur_pic.no_rasl_output_flag {
            self.dpb.mark_all_as_unused_for_ref();
        }

        self.rps = RefPicSet::default();

        if !slice.nalu.header.type_.is_idr() {
            let curr_st_rps = Self::st_ref_pic_set(hdr, sps)?;
            // Equation 8-5, short-term half. Saturating adds: the deltas are
            // stream-controlled and a POC outside i32 is a corrupt header, which must
            // degrade to a missing-reference warning downstream, not overflow here
            // (upstream adds unchecked).
            for i in 0..usize::from(curr_st_rps.num_negative_pics) {
                let poc = cur_pic
                    .pic_order_cnt_val
                    .saturating_add(curr_st_rps.delta_poc_s0[i]);
                if curr_st_rps.used_by_curr_pic_s0[i] {
                    self.rps.poc_st_curr_before.push(poc);
                } else {
                    self.rps.poc_st_foll.push(poc);
                }
            }
            for i in 0..usize::from(curr_st_rps.num_positive_pics) {
                let poc = cur_pic
                    .pic_order_cnt_val
                    .saturating_add(curr_st_rps.delta_poc_s1[i]);
                if curr_st_rps.used_by_curr_pic_s1[i] {
                    self.rps.poc_st_curr_after.push(poc);
                } else {
                    self.rps.poc_st_foll.push(poc);
                }
            }

            // Equation 8-5, long-term half: PocLtCurr/PocLtFoll from PocLsbLt plus the
            // optional MSB cycle. This is the path punktfunk RFI recovery rides — a
            // host pins a picture long-term and the recovery slice names it here.
            let num_lt = usize::from(hdr.num_long_term_sps) + usize::from(hdr.num_long_term_pics);
            for i in 0..num_lt.min(hdr.poc_lsb_lt.len()) {
                let mut poc_lt = i64::from(hdr.poc_lsb_lt[i]);
                if hdr.delta_poc_msb_present_flag[i] {
                    poc_lt += i64::from(cur_pic.pic_order_cnt_val);
                    poc_lt -= i64::from(hdr.delta_poc_msb_cycle_lt[i])
                        * i64::from(self.max_pic_order_cnt_lsb);
                    poc_lt -=
                        i64::from(cur_pic.pic_order_cnt_val & (self.max_pic_order_cnt_lsb - 1));
                }
                let poc_lt = poc_lt.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                if hdr.used_by_curr_pic_lt[i] {
                    self.rps
                        .poc_lt_curr
                        .push((poc_lt, hdr.delta_poc_msb_present_flag[i]));
                } else {
                    self.rps
                        .poc_lt_foll
                        .push((poc_lt, hdr.delta_poc_msb_present_flag[i]));
                }
            }
        }

        self.derive_and_mark_rps(warnings);
        Ok(())
    }

    // See the derivation process in the second half of 8.3.2 (equations 8-6/8-7 plus
    // the marking step). Upstream logs unresolvable entries and stores `None`; the
    // `None` is kept (positional stability for the list construction) and each miss
    // is a [`PlanWarning::MissingReference`] when the current picture needs it.
    fn derive_and_mark_rps(&mut self, warnings: &mut Vec<PlanWarning>) {
        let mask = self.max_pic_order_cnt_lsb.wrapping_sub(1);

        // Equation 8-6.
        for &(poc, msb_present) in &self.rps.poc_lt_curr {
            let reference = if msb_present {
                self.dpb.find_ref_by_poc(poc)
            } else {
                // The lsb-masked lookup: 7.4.7.1 requires at most ONE reference per
                // poc_lsb when delta_poc_msb is absent — this is the RFI anchor path,
                // and silently taking the oldest of several matches (as the vendored
                // find does) could hand the decoder the wrong anchor. Still picked
                // (concealment posture), but flagged.
                let candidates = self
                    .dpb
                    .pictures()
                    .filter(|p| p.is_ref() && (p.pic_order_cnt_val & mask) == poc)
                    .count();
                if candidates > 1 {
                    warnings.push(PlanWarning::MissingReference {
                        context: "ambiguous long-term poc_lsb (7.4.7.1 requires a unique match)",
                        detail: format!("poc lsb {poc}: {candidates} candidates"),
                    });
                }
                self.dpb.find_ref_by_poc_masked(poc, mask)
            };
            if reference.is_none() {
                warnings.push(PlanWarning::MissingReference {
                    context: "long-term RPS entry (RefPicSetLtCurr)",
                    detail: format!("poc {poc}"),
                });
            }
            self.rps.ref_pic_set_lt_curr.push(reference);
        }
        for &(poc, msb_present) in &self.rps.poc_lt_foll {
            let reference = if msb_present {
                self.dpb.find_ref_by_poc(poc)
            } else {
                self.dpb.find_ref_by_poc_masked(poc, mask)
            };
            if reference.is_none() {
                // Not referenced by THIS picture — a future one names it, and its own
                // RPS will warn then. A trace keeps join scenarios from spamming.
                trace!("RefPicSetLtFoll entry poc {poc} not in the DPB");
            }
            self.rps.ref_pic_set_lt_foll.push(reference);
        }

        for pic in self.rps.ref_pic_set_lt_curr.iter().flatten() {
            pic.0.borrow_mut().set_reference(Reference::LongTerm);
        }
        for pic in self.rps.ref_pic_set_lt_foll.iter().flatten() {
            pic.0.borrow_mut().set_reference(Reference::LongTerm);
        }

        // Equation 8-7.
        for &poc in &self.rps.poc_st_curr_before {
            let reference = self.dpb.find_short_term_ref_by_poc(poc);
            if reference.is_none() {
                warnings.push(PlanWarning::MissingReference {
                    context: "short-term RPS entry (RefPicSetStCurrBefore)",
                    detail: format!("poc {poc}"),
                });
            }
            self.rps.ref_pic_set_st_curr_before.push(reference);
        }
        for &poc in &self.rps.poc_st_curr_after {
            let reference = self.dpb.find_short_term_ref_by_poc(poc);
            if reference.is_none() {
                warnings.push(PlanWarning::MissingReference {
                    context: "short-term RPS entry (RefPicSetStCurrAfter)",
                    detail: format!("poc {poc}"),
                });
            }
            self.rps.ref_pic_set_st_curr_after.push(reference);
        }
        for &poc in &self.rps.poc_st_foll {
            let reference = self.dpb.find_short_term_ref_by_poc(poc);
            if reference.is_none() {
                trace!("RefPicSetStFoll entry poc {poc} not in the DPB");
            }
            self.rps.ref_pic_set_st_foll.push(reference);
        }

        // 8.3.2 step 4: every DPB picture in none of the five sets is marked "unused
        // for reference" (identity by Rc, not POC — POC collisions across a corrupt
        // stream must not keep the wrong picture alive).
        let in_any_set = |pic: &Rc<RefCell<PictureData>>| {
            self.rps
                .ref_pic_set_lt_curr
                .iter()
                .chain(&self.rps.ref_pic_set_lt_foll)
                .chain(&self.rps.ref_pic_set_st_curr_before)
                .chain(&self.rps.ref_pic_set_st_curr_after)
                .chain(&self.rps.ref_pic_set_st_foll)
                .flatten()
                .any(|entry| Rc::ptr_eq(&entry.0, pic))
        };
        for entry in self.dpb.entries() {
            if !in_any_set(&entry.0) {
                entry.0.borrow_mut().set_reference(Reference::None);
            }
        }
    }

    // See C.5.2.2: the DPB update that runs before decoding the current picture. The
    // exemption is for "picture 0" — the first picture of the BITSTREAM, whose DPB is
    // empty by definition — and for nothing else: an IRAP that merely follows an
    // in-band EOS still drains (or, under no_output_of_prior_pics_flag, discards) the
    // previous sequence's pictures, so two sequences' outputs never interleave.
    // `was_first_in_bitstream` is the flag value AT this picture (upstream clears its
    // planner field before reading it, which makes its check vacuous; observable
    // behavior only differs when the DPB is already empty).
    fn update_dpb_before_decoding(
        &mut self,
        cur_pic: &PictureData,
        was_first_in_bitstream: bool,
        sps: &Sps,
    ) {
        if cur_pic.nalu_type.is_irap() && cur_pic.no_rasl_output_flag && !was_first_in_bitstream {
            if cur_pic.no_output_of_prior_pics_flag {
                // C.3.2: prior pictures are discarded without output.
                self.dpb.clear();
            } else {
                self.drain_dpb();
            }
        } else {
            self.dpb.remove_unused();
            self.bump_as_needed(sps, false);
        }
    }

    /// Called once per picture, on its first slice segment.
    fn begin_picture(
        &mut self,
        slice: &Slice,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<CurrentPicState, PlanError> {
        let hdr = &slice.header;
        let pps = Rc::clone(self.parser.get_pps(hdr.pic_parameter_set_id).ok_or(
            PlanError::NoActiveParamSet {
                pps_id: hdr.pic_parameter_set_id,
            },
        )?);
        // The SPS-level SCC gate cannot catch a PPS that enables self-referencing on
        // its own; a reference list containing the current picture is a backend
        // contract violation, so it fails closed here, at activation.
        if pps.scc_extension.curr_pic_ref_enabled_flag {
            return Err(PlanError::OutsideEnvelope(
                "SCC current-picture referencing (pps_curr_pic_ref_enabled_flag)",
            ));
        }

        // The envelope gate runs at EVERY activation, not only when NegotiationInfo
        // changes: the parser's table keeps an SPS whose parse-time gate rejected its
        // AU, and NegotiationInfo deliberately omits envelope-only facts (conformance
        // window, field_seq_flag, SCC flags) — a PPS-only AU rebinding to such an SPS
        // must not smuggle it past the gate (the conformance-window leg would reach
        // `visible_rectangle()`'s unchecked u32 subtraction).
        Self::check_envelope(&pps.sps)?;

        // 8.3.1 reads MaxPicOrderCntLsb off the ACTIVE SPS (see the field doc for the
        // upstream divergence). Kept local until the picture is accepted below.
        let max_pic_order_cnt_lsb = 1i32 << (pps.sps.log2_max_pic_order_cnt_lsb_minus4 + 4);

        // The vendored PictureData runs the 8.3.1 POC process and the 8.1.3 output
        // flags in its constructor — a pure computation, no planner state touched.
        let pic = PictureData::new_from_slice(
            slice,
            self.first_picture_in_bitstream,
            self.first_picture_after_eos,
            self.prev_tid0_pic.as_ref(),
            max_pic_order_cnt_lsb,
        );

        if pic.nalu_type.is_rasl() && self.irap_no_rasl_output_flag {
            // 8.1.3 NOTE: RASL pictures of an IRAP with NoRaslOutputFlag = 1 may
            // reference pictures from before the join and are neither decoded nor
            // output. Refused BEFORE any state change — including renegotiation: a
            // RASL AU carrying a renegotiating SPS must not drain the DPB on its way
            // out (upstream also drops here, but after consuming its firstness
            // flags).
            return Err(PlanError::RaslSkipped {
                poc: pic.pic_order_cnt_val,
            });
        }

        // A picture's SPS may require renegotiation. From here on the picture is
        // accepted and state changes begin.
        self.renegotiate_if_needed(&pps.sps, warnings);
        self.max_pic_order_cnt_lsb = max_pic_order_cnt_lsb;

        if pic.nalu_type.is_irap() {
            self.irap_no_rasl_output_flag = pic.no_rasl_output_flag;
        }

        let was_first_in_bitstream = self.first_picture_in_bitstream;
        self.first_picture_after_eos = false;
        self.first_picture_in_bitstream = false;

        // Upstream secures the backend picture here; the plan's equivalent is the id
        // backends will allocate against.
        let id = self.next_pic_id;
        self.next_pic_id += 1;

        self.decode_rps(slice, &pps.sps, &pic, warnings)?;
        self.update_dpb_before_decoding(&pic, was_first_in_bitstream, &pps.sps);

        let rps_plan = self.rps_plan();
        // Taken here, beside the RPS that marked it, and never later: `finish_picture`
        // stores the current picture, which belongs to the NEXT AU's snapshot.
        let dpb_refs = self.dpb_snapshot();

        Ok(CurrentPicState {
            pic,
            first_slice_pps: Rc::clone(&pps),
            pps,
            id,
            rps_plan,
            dpb_refs,
            prev_segment_address: None,
        })
    }

    /// The marked DPB as [`AuPlan::dpb_refs`] reports it — every picture 8.3.2 left
    /// marked "used for short-term reference" or "used for long-term reference",
    /// whether or not THIS picture names it.
    fn dpb_snapshot(&self) -> Vec<RefPic> {
        self.dpb
            .get_all_references()
            .iter()
            .map(Self::to_ref_pic)
            .collect()
    }

    /// Infallible by design: the caller ([`Self::begin_picture`]) has already run the
    /// envelope gate on this SPS, and everything here is bookkeeping.
    fn renegotiate_if_needed(&mut self, sps: &Sps, warnings: &mut Vec<PlanWarning>) {
        if NegotiationInfo::from(sps) == self.negotiation_info {
            return;
        }
        // Make sure all the frames planned so far are display-ready before the
        // stream parameters change under them.
        self.drain_dpb();
        self.negotiation_info = NegotiationInfo::from(sps);
        self.dpb.set_max_num_pics(dpb_limit(sps));

        let reorder = sps.max_num_reorder_pics[usize::from(sps.max_sub_layers_minus1)];
        if reorder > 0 {
            warnings.push(PlanWarning::NonZeroReorder {
                max_num_reorder_pics: reorder,
            });
        }
    }

    /// Handle one slice segment of the current picture (upstream: `handle_slice`).
    fn plan_slice(
        &self,
        cur: &mut CurrentPicState,
        slice: Slice,
        data: Range<usize>,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<SlicePlan, PlanError> {
        // 7.4.7.1: slice_segment_address increases across a picture's segments.
        if let Some(prev) = cur.prev_segment_address {
            if slice.header.segment_address <= prev && !slice.header.first_slice_segment_in_pic_flag
            {
                trace!("slice_segment_address does not increase monotonically, expect corrupted output");
            }
        }
        cur.prev_segment_address = Some(slice.header.segment_address);

        // A slice segment can technically refer to another PPS.
        let pps = self
            .parser
            .get_pps(slice.header.pic_parameter_set_id)
            .ok_or(PlanError::NoActiveParamSet {
                pps_id: slice.header.pic_parameter_set_id,
            })?;
        cur.pps = Rc::clone(pps);

        // Make sure that no negotiation is possible mid-picture. How could it?
        // We'd lose the context of the previous slices.
        if NegotiationInfo::from(&*cur.pps.sps) != self.negotiation_info {
            return Err(PlanError::Parse(
                "invalid stream: mid-picture renegotiation requested".into(),
            ));
        }

        let (ref_list0, ref_list1) = self.build_ref_pic_lists(&slice.header, warnings);

        // An inter slice shall have at least one usable reference (8.3.4 requires
        // num_ref_idx_l0_active entries). Ending up empty — every candidate lost —
        // is undecodable-as-intended: flag it so the session requests recovery.
        let slice_type = slice.header.type_;
        if (slice_type.is_p() || slice_type.is_b()) && ref_list0.is_empty() {
            warnings.push(PlanWarning::MissingReference {
                context: "inter slice with no usable RefPicList0",
                detail: format!("slice_type {slice_type:?}"),
            });
        }
        if slice_type.is_b() && ref_list1.is_empty() {
            warnings.push(PlanWarning::MissingReference {
                context: "B slice with no usable RefPicList1",
                detail: format!("slice_type {slice_type:?}"),
            });
        }

        Ok(SlicePlan {
            data,
            header: slice.header,
            ref_list0,
            ref_list1,
        })
    }

    // See 8.3.4: reference picture list construction for P and B slices, already
    // converted to backend-facing [`RefPic`]s with in-place concealment.
    fn build_ref_pic_lists(
        &self,
        hdr: &SliceHeader,
        warnings: &mut Vec<PlanWarning>,
    ) -> (Vec<RefPic>, Vec<RefPic>) {
        // I slices do not use inter prediction.
        if !hdr.type_.is_p() && !hdr.type_.is_b() {
            return (Vec::new(), Vec::new());
        }

        // The 8-8/8-10 temporal-list loops cycle the three current sets until the
        // list is full; with all three empty they would never terminate. Upstream
        // only guards this behind its SCC flags — a bare broken P slice loops
        // forever there. Bail out with empty lists; plan_slice flags them.
        if self.rps.curr_is_empty() {
            return (Vec::new(), Vec::new());
        }

        let list0 = self.build_one_list(
            hdr,
            usize::from(hdr.num_ref_idx_l0_active_minus1) + 1,
            hdr.ref_pic_list_modification
                .ref_pic_list_modification_flag_l0,
            &hdr.ref_pic_list_modification.list_entry_l0,
            // Equation 8-8: list 0 leads with the past (StCurrBefore first).
            [
                &self.rps.ref_pic_set_st_curr_before,
                &self.rps.ref_pic_set_st_curr_after,
                &self.rps.ref_pic_set_lt_curr,
            ],
            warnings,
        );

        let list1 = if hdr.type_.is_b() {
            self.build_one_list(
                hdr,
                usize::from(hdr.num_ref_idx_l1_active_minus1) + 1,
                hdr.ref_pic_list_modification
                    .ref_pic_list_modification_flag_l1,
                &hdr.ref_pic_list_modification.list_entry_l1,
                // Equation 8-10: list 1 leads with the future (StCurrAfter first).
                [
                    &self.rps.ref_pic_set_st_curr_after,
                    &self.rps.ref_pic_set_st_curr_before,
                    &self.rps.ref_pic_set_lt_curr,
                ],
                warnings,
            )
        } else {
            Vec::new()
        };

        (list0, list1)
    }

    #[allow(clippy::type_complexity)]
    fn build_one_list(
        &self,
        hdr: &SliceHeader,
        num_active: usize,
        modification_flag: bool,
        list_entries: &[u32],
        set_order: [&Vec<Option<DpbEntry<PicId>>>; 3],
        warnings: &mut Vec<PlanWarning>,
    ) -> Vec<RefPic> {
        // Equations 8-8/8-10: RefPicListTempX cycles the current sets until
        // NumRpsCurrTempListX entries exist.
        let temp_len = num_active.max(hdr.num_pic_total_curr as usize);
        let mut temp: Vec<Option<RefPic>> = Vec::with_capacity(temp_len);
        'fill: while temp.len() < temp_len {
            for set in set_order {
                for entry in set {
                    if temp.len() == temp_len {
                        break 'fill;
                    }
                    temp.push(entry.as_ref().map(Self::to_ref_pic));
                }
            }
        }

        // Equations 8-9/8-11: the final list is the temporal list, reordered through
        // list_entry_lX when the modification flag is set.
        let mut list: Vec<Option<RefPic>> = Vec::with_capacity(num_active);
        for r_idx in 0..num_active {
            let entry = if modification_flag {
                match list_entries
                    .get(r_idx)
                    .and_then(|&idx| temp.get(idx as usize))
                {
                    Some(entry) => *entry,
                    None => {
                        // The parser bounds list_entry below NumPicTotalCurr, so this
                        // is unreachable on its output — belt over suspenders for a
                        // future parser re-sync.
                        warnings.push(PlanWarning::MissingReference {
                            context: "ref_pic_list_modification entry out of range",
                            detail: format!("ref_idx {r_idx}"),
                        });
                        None
                    }
                }
            } else {
                temp.get(r_idx).copied().flatten()
            };
            list.push(entry);
        }

        Self::substitute_in_place(list)
    }

    /// Fill the holes a lost reference leaves, preserving list positions: every
    /// `ref_idx` in the slice syntax indexes the returned Vec 1:1. A hole is
    /// substituted by the nearest existing reference in list order (the previous
    /// existing entry, else the first existing one) — stable-but-wrong concealment,
    /// already flagged via [`PlanWarning::MissingReference`] when the RPS was derived.
    /// Compacting instead would shift every subsequent ref_idx and make the decoder
    /// predict from the wrong pictures. Only a list with no existing reference at all
    /// collapses to empty (the caller warns on that separately).
    fn substitute_in_place(list: Vec<Option<RefPic>>) -> Vec<RefPic> {
        let first_existing = list.iter().flatten().next().copied();
        let mut out = Vec::with_capacity(list.len());
        let mut prev_existing: Option<RefPic> = None;
        for slot in &list {
            match slot {
                Some(real) => {
                    prev_existing = Some(*real);
                    out.push(*real);
                }
                None => {
                    if let Some(substitute) = prev_existing.or(first_existing) {
                        out.push(substitute);
                    }
                }
            }
        }
        out
    }

    fn to_ref_pic(entry: &DpbEntry<PicId>) -> RefPic {
        let pic = entry.0.borrow();
        RefPic {
            id: entry.1,
            pic_order_cnt: pic.pic_order_cnt_val,
            is_long_term: matches!(pic.reference(), Reference::LongTerm),
        }
    }

    /// Snapshot the resolved current sets for the plan (backends rebuild their
    /// reference arrays from these).
    fn rps_plan(&self) -> RpsPlan {
        let convert = |set: &Vec<Option<DpbEntry<PicId>>>| -> Vec<RefPic> {
            set.iter().flatten().map(Self::to_ref_pic).collect()
        };
        RpsPlan {
            st_curr_before: convert(&self.rps.ref_pic_set_st_curr_before),
            st_curr_after: convert(&self.rps.ref_pic_set_st_curr_after),
            lt_curr: convert(&self.rps.ref_pic_set_lt_curr),
        }
    }

    fn finish_picture(&mut self, cur: CurrentPicState) -> Result<PicId, PlanError> {
        let CurrentPicState { pic, pps, id, .. } = cur;

        // 8.3.1: this picture becomes PrevTid0Pic for the next one if eligible.
        if pic.valid_for_prev_tid0_pic {
            self.prev_tid0_pic = Some(pic.clone());
        }

        let sps = Rc::clone(&pps.sps);

        // First store the current picture in the DPB, only then decide whether to
        // bump (C.3.4 marks it short-term inside store_picture).
        self.dpb
            .store_picture(Rc::new(RefCell::new(pic)), id)
            .map_err(PlanError::Parse)?;
        self.bump_as_needed(&sps, true);

        Ok(id)
    }

    fn picture_plan(
        cur: &CurrentPicState,
        recovery_point: Option<RecoveryPointHevc>,
        references_clean: bool,
    ) -> PicturePlan {
        let pic = &cur.pic;
        // The first slice's PPS defines the picture's parameters; `cur.pps` may have
        // drifted to a later segment's.
        let sps = &cur.first_slice_pps.sps;
        let rect = sps.visible_rectangle();

        PicturePlan {
            nalu_type: pic.nalu_type,
            is_idr: pic.nalu_type.is_idr(),
            is_irap: pic.nalu_type.is_irap(),
            no_rasl_output_flag: pic.no_rasl_output_flag,
            is_reference: !pic.nalu_type.is_slnr(),
            pic_order_cnt: pic.pic_order_cnt_val,
            coded_width: u32::from(sps.width()),
            coded_height: u32::from(sps.height()),
            // The vendored `visible_rectangle()` returns the crop OFFSET in `min` and
            // the visible SIZE in `max` (not an edge coordinate), with the
            // SubWidthC/SubHeightC scaling already applied — same convention the
            // H.264 layer verified.
            display_crop: DisplayCrop {
                x: rect.min.x,
                y: rect.min.y,
                width: rect.max.x,
                height: rect.max.y,
            },
            // Read unconditionally: the vendored parser builds every SPS from
            // `Default`, whose `VuiParams` already holds E.3.1's inferred values
            // (2/2/2, limited range), and parsing only overwrites them under the
            // present flags — so this IS the spec inference whether or not the
            // stream carried a VUI. (The vendored fields are u32; E.2 reads them as
            // u(8), so the casts cannot truncate.)
            colour: ColourDescription {
                colour_primaries: sps.vui_parameters.colour_primaries as u8,
                transfer_characteristics: sps.vui_parameters.transfer_characteristics as u8,
                matrix_coefficients: sps.vui_parameters.matrix_coeffs as u8,
                video_full_range: sps.vui_parameters.video_full_range_flag,
            },
            general_profile_idc: sps.profile_tier_level.general_profile_idc,
            level_idc: sps.profile_tier_level.general_level_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: dpb_limit(sps),
            short_term_ref_pic_set_size_bits: pic.short_term_ref_pic_set_size_bits,
            recovery_point,
            references_clean,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    const TEST_25FPS: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265");
    const TEST_BEAR: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/bear.h265");
    const TEST_BBB: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/bbb.h265");
    const TEST_64X64_I_P_B_P: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/64x64-I-P-B-P.h265");

    /// Test-only AU splitter: the vendored vectors are raw Annex-B streams, while
    /// `plan_au` takes the pre-split AUs punktfunk's pump produces. A new AU starts
    /// at a non-VCL NALU following slices, or at a slice segment with
    /// `first_slice_segment_in_pic_flag == 1` (the first bit of the payload, i.e. of
    /// the byte after the 2-byte NAL header) when the current AU already has slices.
    fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
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

    /// The concealment/envelope warnings a clean stream must not produce.
    /// `NonZeroReorder` is excluded: the vendored conformance clips are general
    /// (reordering) encodes, and the planner deliberately plans them while flagging
    /// the envelope fact.
    ///
    /// Delegates rather than restating the list. This harness exists to prove the
    /// planner conceals exactly where production conceals, so a second copy here
    /// could drift and quietly prove the wrong thing — and a `matches!` in
    /// particular reads any FUTURE variant as clean, which is the one answer a
    /// damage predicate must never default to. [`PlanWarning::is_integrity`] is an
    /// exhaustive match, so a new variant stops the compiler there instead.
    fn is_integrity_warning(w: &PlanWarning) -> bool {
        w.is_integrity()
    }

    /// Plan a whole vendored clip and assert the global invariants: every AU plans,
    /// no integrity warnings, every stored id reaches output exactly once, and
    /// outputs emerge in ascending POC order within each IRAP period.
    fn plan_whole_clip(stream: &[u8]) -> (H265Planner, Vec<AuPlan>) {
        let aus = split_into_aus(stream);
        let mut planner = H265Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(
                planner
                    .plan_au(au)
                    .expect("the clean vector must plan without errors"),
            );
        }

        for plan in &plans {
            assert!(
                !plan.warnings.iter().any(is_integrity_warning),
                "clean vector produced an integrity warning: {:?}",
                plan.warnings
            );
        }

        let stored: BTreeSet<PicId> = plans.iter().filter_map(|p| p.dpb.stored).collect();
        assert_eq!(stored.len(), plans.len());
        let mut emitted: Vec<PicId> = plans
            .iter()
            .flat_map(|p| p.dpb.outputs.iter().copied())
            .collect();
        emitted.extend(planner.flush().outputs);
        let output: BTreeSet<PicId> = emitted.iter().copied().collect();
        assert_eq!(
            output.len(),
            emitted.len(),
            "no picture may be output twice"
        );
        assert_eq!(
            output, stored,
            "bumping plus the final flush must output every picture"
        );

        // Output ORDER, not just coverage: within each IRAP period, ids must emerge
        // in ascending POC — the invariant the C.5.2 bumping process exists to
        // provide. (An IRAP with NoRaslOutputFlag resets POC continuity, hence the
        // period key.)
        let mut period = 0usize;
        let mut order_key: BTreeMap<PicId, (usize, i32)> = BTreeMap::new();
        for plan in &plans {
            if plan.picture.is_irap && plan.picture.no_rasl_output_flag {
                period += 1;
            }
            order_key.insert(
                plan.dpb.stored.unwrap(),
                (period, plan.picture.pic_order_cnt),
            );
        }
        let mut last: Option<(usize, i32)> = None;
        for id in &emitted {
            let key = order_key[id];
            if let Some(last) = last {
                assert!(
                    key > last,
                    "outputs must emerge in ascending POC order per IRAP period: \
                     {key:?} emitted after {last:?}"
                );
            }
            last = Some(key);
        }

        (planner, plans)
    }

    #[test]
    fn the_full_25fps_vector_plans_every_picture_and_every_pic_id_reaches_output() {
        let aus = split_into_aus(TEST_25FPS);
        assert_eq!(aus.len(), 250, "the vendored golden: 250 pictures");
        let (_, plans) = plan_whole_clip(TEST_25FPS);
        assert_eq!(plans.len(), 250);
        assert_eq!(plans.iter().map(|p| p.slices.len()).sum::<usize>(), 250);
        assert!(plans[0].picture.is_idr);
        assert_eq!(
            plans[0].picture.pic_order_cnt, 0,
            "POC 0 at the opening IRAP"
        );
        for plan in &plans {
            for slice in &plan.slices {
                if slice.header.type_.is_p() || slice.header.type_.is_b() {
                    assert!(!slice.ref_list0.is_empty());
                }
            }
        }
    }

    #[test]
    fn the_bear_and_bbb_vectors_plan_clean_end_to_end() {
        let (_, bear) = plan_whole_clip(TEST_BEAR);
        assert!(!bear.is_empty());
        let (_, bbb) = plan_whole_clip(TEST_BBB);
        assert!(!bbb.is_empty());
    }

    /// The false-positive guard for [`PicturePlan::references_clean`], on REAL
    /// bitstreams rather than authored ones: two conformance clips that lose nothing
    /// must report every single picture clean. A regression that let the ledger mark a
    /// healthy stream would refuse every host recovery anchor and force an IDR on
    /// every loss — the cheap re-anchor path gone, silently.
    ///
    /// These clips carry B-slices and real reordering, so they also exercise the
    /// "reference lists, not the RPS" reading: 8.3.2 retains pictures the current
    /// picture does not use, and folding those in would condemn pictures at random.
    #[test]
    fn a_lossless_conformance_clip_reports_clean_references_on_every_picture() {
        for (name, clip) in [("bear", TEST_BEAR), ("bbb", TEST_BBB)] {
            let (_, plans) = plan_whole_clip(clip);
            assert!(!plans.is_empty(), "{name} produced no plans");
            for (i, plan) in plans.iter().enumerate() {
                assert!(
                    plan.picture.references_clean,
                    "{name} picture {i} (poc {}) must read clean on a lossless clip",
                    plan.picture.pic_order_cnt
                );
            }
        }
    }

    #[test]
    fn b_slices_get_a_future_led_list1_distinct_from_list0() {
        let aus = split_into_aus(TEST_64X64_I_P_B_P);
        let mut planner = H265Planner::new();
        let mut b_slices_seen = 0usize;

        for au in &aus {
            let plan = planner
                .plan_au(au)
                .expect("the clean vector must plan without errors");
            for slice in &plan.slices {
                if !slice.header.type_.is_b() {
                    continue;
                }
                b_slices_seen += 1;
                assert!(!slice.ref_list0.is_empty());
                assert!(!slice.ref_list1.is_empty());
                // 8.3.4: list0 leads with the past (StCurrBefore), list1 with the
                // future (StCurrAfter).
                assert!(slice.ref_list0[0].pic_order_cnt < plan.picture.pic_order_cnt);
                assert!(slice.ref_list1[0].pic_order_cnt > plan.picture.pic_order_cnt);
                assert!(!plan.rps.st_curr_before.is_empty());
                assert!(!plan.rps.st_curr_after.is_empty());
            }
        }

        assert!(b_slices_seen > 0, "the vector must contain B slices");
    }

    /// Byte-level authoring: the vendored crate has no H.265 builders or synthesizer
    /// (its encoder is H.264-only), so the tests carry a minimal bit writer plus
    /// SPS/PPS/slice-segment writers for exactly the syntax the planner reads —
    /// the h264 tests' `write_idr_slice` idiom, one codec over. The planner only
    /// reads headers, so no slice data follows the alignment bit.
    struct BitSink {
        bytes: Vec<u8>,
        acc: u8,
        nbits: u8,
    }

    impl BitSink {
        fn new() -> Self {
            BitSink {
                bytes: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }

        fn bit(&mut self, b: u32) {
            self.acc = (self.acc << 1) | (b as u8 & 1);
            self.nbits += 1;
            if self.nbits == 8 {
                self.bytes.push(self.acc);
                self.acc = 0;
                self.nbits = 0;
            }
        }

        fn bits(&mut self, count: usize, value: u32) {
            for i in (0..count).rev() {
                self.bit((value >> i) & 1);
            }
        }

        fn ue(&mut self, value: u32) {
            let x = value + 1;
            let len = 32 - x.leading_zeros() as usize;
            self.bits(len - 1, 0);
            self.bits(len, x);
        }

        fn se(&mut self, value: i32) {
            let k = if value > 0 {
                2 * value as u32 - 1
            } else {
                (-2 * (value as i64)) as u32
            };
            self.ue(k);
        }

        /// rbsp_trailing_bits(): the stop bit plus zero padding to a byte boundary.
        fn finish(mut self) -> Vec<u8> {
            self.bit(1);
            while self.nbits != 0 {
                self.bit(0);
            }
            self.bytes
        }
    }

    /// Wrap an RBSP in start code + 2-byte HEVC NAL header + emulation prevention.
    fn h265_nalu_with_layer(nalu_type: u8, layer_id: u8, rbsp: &[u8]) -> Vec<u8> {
        let mut out = vec![
            0x00,
            0x00,
            0x00,
            0x01,
            (nalu_type << 1) | (layer_id >> 5),
            ((layer_id & 0x1f) << 3) | 0x01, // nuh_temporal_id_plus1 = 1
        ];
        let mut zeros = 0usize;
        for &byte in rbsp {
            if zeros >= 2 && byte <= 0x03 {
                out.push(0x03);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    fn h265_nalu(nalu_type: u8, rbsp: &[u8]) -> Vec<u8> {
        h265_nalu_with_layer(nalu_type, 0, rbsp)
    }

    #[derive(Clone)]
    enum VuiOpt {
        Absent,
        SignalType {
            full_range: bool,
            colour: Option<(u8, u8, u8)>,
        },
        FieldSeq,
    }

    #[derive(Clone)]
    struct SpsOpts {
        profile_idc: u8,
        chroma_format_idc: u32,
        width: u32,
        height: u32,
        bit_depth_minus8: u32,
        /// `general_level_idc` (30 x the level number: 120 = L4, 153 = L5.1). Only the
        /// DPB-sizing tests care, and they care because equation A-2 keys on it — the
        /// planner deliberately does not.
        level_idc: u32,
        /// (left, right, top, bottom) conf_win offsets, in chroma units.
        conf_win: Option<(u32, u32, u32, u32)>,
        max_dec_pic_buffering_minus1: u32,
        max_num_reorder_pics: u32,
        long_term: bool,
        vui: VuiOpt,
    }

    impl Default for SpsOpts {
        fn default() -> Self {
            SpsOpts {
                profile_idc: 1, // Main
                chroma_format_idc: 1,
                width: 64,
                height: 64,
                bit_depth_minus8: 0,
                level_idc: 120, // L4
                conf_win: None,
                max_dec_pic_buffering_minus1: 4,
                max_num_reorder_pics: 0,
                long_term: false,
                vui: VuiOpt::Absent,
            }
        }
    }

    /// profile_tier_level()'s general block: general_profile_space u(2), tier u(1),
    /// profile_idc u(5), 32 compatibility flags, progressive/interlaced/non-packed/
    /// frame-only, 43 constraint/reserved bits (all zero for every profile branch the
    /// parser takes), inbld/reserved bit, level u(8). The per-sub-layer tail follows
    /// only when max_sub_layers_minus1 > 0.
    fn ptl_general(s: &mut BitSink, profile_idc: u8, level_idc: u32) {
        s.bits(2, 0);
        s.bit(0);
        s.bits(5, u32::from(profile_idc));
        s.bits(32, 0);
        s.bit(1); // general_progressive_source_flag
        s.bit(0); // general_interlaced_source_flag
        s.bit(0); // general_non_packed_constraint_flag
        s.bit(1); // general_frame_only_constraint_flag
        s.bits(31, 0);
        s.bits(12, 0); // 43 zero bits total
        s.bit(0); // general_inbld_flag / reserved
        s.bits(8, level_idc); // general_level_idc
    }

    fn synth_sps(o: &SpsOpts) -> Vec<u8> {
        let mut s = BitSink::new();
        s.bits(4, 0); // sps_video_parameter_set_id
        s.bits(3, 0); // sps_max_sub_layers_minus1
        s.bit(1); // sps_temporal_id_nesting_flag
        ptl_general(&mut s, o.profile_idc, o.level_idc);

        s.ue(0); // sps_seq_parameter_set_id
        s.ue(o.chroma_format_idc);
        if o.chroma_format_idc == 3 {
            s.bit(0); // separate_colour_plane_flag
        }
        s.ue(o.width);
        s.ue(o.height);
        match o.conf_win {
            Some((left, right, top, bottom)) => {
                s.bit(1);
                s.ue(left);
                s.ue(right);
                s.ue(top);
                s.ue(bottom);
            }
            None => s.bit(0),
        }
        s.ue(o.bit_depth_minus8); // bit_depth_luma_minus8
        s.ue(o.bit_depth_minus8); // bit_depth_chroma_minus8
        s.ue(0); // log2_max_pic_order_cnt_lsb_minus4: 4-bit POC lsb
        s.bit(1); // sps_sub_layer_ordering_info_present_flag
        s.ue(o.max_dec_pic_buffering_minus1);
        s.ue(o.max_num_reorder_pics);
        s.ue(0); // sps_max_latency_increase_plus1
        s.ue(0); // log2_min_luma_coding_block_size_minus3: 8
        s.ue(3); // log2_diff_max_min_luma_coding_block_size: CTB 64
        s.ue(0); // log2_min_luma_transform_block_size_minus2: 4
        s.ue(3); // log2_diff_max_min_luma_transform_block_size: 32
        s.ue(0); // max_transform_hierarchy_depth_inter
        s.ue(0); // max_transform_hierarchy_depth_intra
        s.bit(0); // scaling_list_enabled_flag
        s.bit(0); // amp_enabled_flag
        s.bit(0); // sample_adaptive_offset_enabled_flag
        s.bit(0); // pcm_enabled_flag
        s.ue(0); // num_short_term_ref_pic_sets
        if o.long_term {
            s.bit(1); // long_term_ref_pics_present_flag
            s.ue(0); // num_long_term_ref_pics_sps
        } else {
            s.bit(0);
        }
        s.bit(0); // sps_temporal_mvp_enabled_flag
        s.bit(0); // strong_intra_smoothing_enabled_flag
        match &o.vui {
            VuiOpt::Absent => s.bit(0),
            vui => {
                s.bit(1); // vui_parameters_present_flag
                s.bit(0); // aspect_ratio_info_present_flag
                s.bit(0); // overscan_info_present_flag
                match vui {
                    VuiOpt::SignalType { full_range, colour } => {
                        s.bit(1); // video_signal_type_present_flag
                        s.bits(3, 5); // video_format: unspecified
                        s.bit(u32::from(*full_range));
                        match colour {
                            Some((primaries, transfer, matrix)) => {
                                s.bit(1); // colour_description_present_flag
                                s.bits(8, u32::from(*primaries));
                                s.bits(8, u32::from(*transfer));
                                s.bits(8, u32::from(*matrix));
                            }
                            None => s.bit(0),
                        }
                    }
                    _ => s.bit(0), // video_signal_type_present_flag
                }
                s.bit(0); // chroma_loc_info_present_flag
                s.bit(0); // neutral_chroma_indication_flag
                s.bit(u32::from(matches!(vui, VuiOpt::FieldSeq))); // field_seq_flag
                s.bit(0); // frame_field_info_present_flag
                s.bit(0); // default_display_window_flag
                s.bit(0); // vui_timing_info_present_flag
                s.bit(0); // bitstream_restriction_flag
            }
        }
        s.bit(0); // sps_extension_present_flag
        h265_nalu(33, &s.finish())
    }

    fn synth_pps(dependent_slice_segments: bool) -> Vec<u8> {
        let mut s = BitSink::new();
        s.ue(0); // pps_pic_parameter_set_id
        s.ue(0); // pps_seq_parameter_set_id
        s.bit(u32::from(dependent_slice_segments));
        s.bit(0); // output_flag_present_flag
        s.bits(3, 0); // num_extra_slice_header_bits
        s.bit(0); // sign_data_hiding_enabled_flag
        s.bit(0); // cabac_init_present_flag
        s.ue(0); // num_ref_idx_l0_default_active_minus1
        s.ue(0); // num_ref_idx_l1_default_active_minus1
        s.se(0); // init_qp_minus26
        s.bit(0); // constrained_intra_pred_flag
        s.bit(0); // transform_skip_enabled_flag
        s.bit(0); // cu_qp_delta_enabled_flag
        s.se(0); // pps_cb_qp_offset
        s.se(0); // pps_cr_qp_offset
        s.bit(0); // pps_slice_chroma_qp_offsets_present_flag
        s.bit(0); // weighted_pred_flag
        s.bit(0); // weighted_bipred_flag
        s.bit(0); // transquant_bypass_enabled_flag
        s.bit(0); // tiles_enabled_flag
        s.bit(0); // entropy_coding_sync_enabled_flag
        s.bit(0); // pps_loop_filter_across_slices_enabled_flag
        s.bit(0); // deblocking_filter_control_present_flag
        s.bit(0); // pps_scaling_list_data_present_flag
        s.bit(0); // lists_modification_present_flag
        s.ue(0); // log2_parallel_merge_level_minus2
        s.bit(0); // slice_segment_header_extension_present_flag
        s.bit(0); // pps_extension_present_flag
        h265_nalu(34, &s.finish())
    }

    const IDR_W_RADL: u8 = 19;
    const TRAIL_R: u8 = 1;
    const CRA_NUT: u8 = 21;
    const RASL_N: u8 = 8;

    #[derive(Clone)]
    struct SliceOpts {
        nalu_type: u8,
        layer_id: u8,
        /// Continuation segments: `Some((address, address_bits, dependent))`.
        segment: Option<(u32, usize, bool)>,
        /// PPS 0 has dependent_slice_segments_enabled: the flag bit is only written
        /// when the PPS enables it, so the writer must know.
        pps_dependent_enabled: bool,
        slice_type: u32, // 2 = I, 1 = P, 0 = B
        poc_lsb: u32,
        /// Short-term RPS: (delta_poc_sX_minus1, used_by_curr_pic) pairs.
        neg: Vec<(u32, bool)>,
        pos: Vec<(u32, bool)>,
        /// Present only when the SPS set long_term_ref_pics_present_flag:
        /// (poc_lsb_lt, used_by_curr_pic_lt, delta_poc_msb_cycle_lt).
        lt: Vec<(u32, bool, Option<u32>)>,
        sps_long_term: bool,
        num_ref_idx_l0: u32,
        num_ref_idx_l1: u32,
        no_output_of_prior_pics: bool,
    }

    impl Default for SliceOpts {
        fn default() -> Self {
            SliceOpts {
                nalu_type: TRAIL_R,
                layer_id: 0,
                segment: None,
                pps_dependent_enabled: false,
                slice_type: 1,
                poc_lsb: 0,
                neg: Vec::new(),
                pos: Vec::new(),
                lt: Vec::new(),
                sps_long_term: false,
                num_ref_idx_l0: 1,
                num_ref_idx_l1: 1,
                no_output_of_prior_pics: false,
            }
        }
    }

    fn synth_slice(o: &SliceOpts) -> Vec<u8> {
        let is_irap = (16..=23).contains(&o.nalu_type);
        let is_idr = o.nalu_type == IDR_W_RADL || o.nalu_type == 20;
        let mut s = BitSink::new();
        s.bit(u32::from(o.segment.is_none())); // first_slice_segment_in_pic_flag
        if is_irap {
            s.bit(u32::from(o.no_output_of_prior_pics));
        }
        s.ue(0); // slice_pic_parameter_set_id
        let mut dependent = false;
        if let Some((address, bits, dep)) = o.segment {
            if o.pps_dependent_enabled {
                s.bit(u32::from(dep));
            }
            s.bits(bits, address);
            dependent = dep;
        }
        if !dependent {
            s.ue(o.slice_type);
            if !is_idr {
                s.bits(4, o.poc_lsb);
                s.bit(0); // short_term_ref_pic_set_sps_flag
                          // st_ref_pic_set(stRpsIdx = 0): no inter-RPS prediction flag.
                s.ue(o.neg.len() as u32);
                s.ue(o.pos.len() as u32);
                for &(delta_minus1, used) in &o.neg {
                    s.ue(delta_minus1);
                    s.bit(u32::from(used));
                }
                for &(delta_minus1, used) in &o.pos {
                    s.ue(delta_minus1);
                    s.bit(u32::from(used));
                }
                if o.sps_long_term {
                    s.ue(o.lt.len() as u32); // num_long_term_pics
                    for &(poc_lsb_lt, used, msb) in &o.lt {
                        s.bits(4, poc_lsb_lt);
                        s.bit(u32::from(used));
                        match msb {
                            Some(cycle) => {
                                s.bit(1);
                                s.ue(cycle);
                            }
                            None => s.bit(0),
                        }
                    }
                }
            }
            if o.slice_type != 2 {
                s.bit(1); // num_ref_idx_active_override_flag
                s.ue(o.num_ref_idx_l0 - 1);
                if o.slice_type == 0 {
                    s.ue(o.num_ref_idx_l1 - 1);
                    s.bit(0); // mvd_l1_zero_flag
                }
                s.ue(0); // five_minus_max_num_merge_cand
            }
            s.se(0); // slice_qp_delta
        }
        h265_nalu_with_layer(o.nalu_type, o.layer_id, &s.finish())
    }

    fn idr_slice() -> Vec<u8> {
        synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            ..Default::default()
        })
    }

    fn trail_p(poc_lsb: u32, neg: &[(u32, bool)], num_ref_idx_l0: u32) -> Vec<u8> {
        synth_slice(&SliceOpts {
            poc_lsb,
            neg: neg.to_vec(),
            num_ref_idx_l0,
            ..Default::default()
        })
    }

    fn param_sets(sps: &SpsOpts) -> Vec<u8> {
        let mut au = synth_sps(sps);
        au.extend(synth_pps(false));
        au
    }

    fn opening_idr_au(sps: &SpsOpts) -> Vec<u8> {
        let mut au = param_sets(sps);
        au.extend(idr_slice());
        au
    }

    #[test]
    fn an_idr_opens_planning_with_poc_zero_full_crop_and_inferred_colour() {
        let mut planner = H265Planner::new();
        let plan = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();

        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert!(plan.picture.is_idr && plan.picture.is_irap);
        assert!(plan.picture.no_rasl_output_flag);
        assert!(plan.picture.is_reference);
        assert_eq!(plan.picture.pic_order_cnt, 0);
        assert_eq!(plan.picture.nalu_type, NaluType::IdrWRadl);
        assert_eq!(
            (plan.picture.coded_width, plan.picture.coded_height),
            (64, 64)
        );
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 0,
                y: 0,
                width: 64,
                height: 64
            }
        );
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: false,
            },
            "E.3.1 inference: 'unspecified' code points + limited range, never a raw 0"
        );
        assert_eq!(plan.picture.general_profile_idc, 1);
        assert_eq!(plan.picture.level_idc, Level::L4);
        assert_eq!(plan.picture.chroma_format_idc, 1);
        assert_eq!(
            plan.picture.max_dpb_frames, 5,
            "the DPB is the stream's sps_max_dec_pic_buffering_minus1 + 1, NOT A-2's \
             level ceiling (which would say 16 for a 64x64 L4 stream)"
        );
        // Zero-reorder low-delay: the picture is display-ready in its own plan.
        assert_eq!(plan.dpb.outputs, vec![plan.dpb.stored.unwrap()]);
        // An IDR carries no RPS.
        assert!(plan.rps.st_curr_before.is_empty());
        assert!(plan.rps.lt_curr.is_empty());
        assert_eq!(plan.picture.short_term_ref_pic_set_size_bits, 0);
    }

    #[test]
    fn a_p_slice_references_the_idr_short_term_and_outputs_immediately() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let idr_id = p0.dpb.stored.unwrap();

        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);
        assert_eq!(p1.picture.pic_order_cnt, 1);
        assert_eq!(
            p1.slices[0].ref_list0,
            vec![RefPic {
                id: idr_id,
                pic_order_cnt: 0,
                is_long_term: false
            }]
        );
        assert_eq!(p1.rps.st_curr_before.len(), 1);
        assert_eq!(p1.rps.st_curr_before[0].id, idr_id);
        assert!(p1.rps.st_curr_after.is_empty());
        // The slice carried its RPS inline, so the bit count must be nonzero
        // (Vulkan's NumBitsForSTRefPicSetInSlice).
        assert!(p1.picture.short_term_ref_pic_set_size_bits > 0);
        // Zero-reorder: p1 is display-ready immediately.
        assert!(p1.dpb.outputs.contains(&p1.dpb.stored.unwrap()));
    }

    #[test]
    fn long_term_rps_entries_carry_the_rfi_reference_shape() {
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        let p0 = planner.plan_au(&au0).unwrap();
        let idr_id = p0.dpb.stored.unwrap();

        let p1 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 1,
                neg: vec![(0, true)],
                sps_long_term: true,
                num_ref_idx_l0: 1,
                ..Default::default()
            }))
            .unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);
        let p1_id = p1.dpb.stored.unwrap();

        // The RFI shape: the recovery slice keeps the previous picture short-term
        // AND pins the anchor (the IDR, poc 0) through the long-term RPS.
        let p2 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 2,
                neg: vec![(0, true)],
                lt: vec![(0, true, None)],
                sps_long_term: true,
                num_ref_idx_l0: 2,
                ..Default::default()
            }))
            .unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);

        // 8.3.4: short-term current entries lead the list, long-term follow.
        assert_eq!(
            p2.slices[0].ref_list0,
            vec![
                RefPic {
                    id: p1_id,
                    pic_order_cnt: 1,
                    is_long_term: false
                },
                RefPic {
                    id: idr_id,
                    pic_order_cnt: 0,
                    is_long_term: true
                },
            ]
        );
        assert_eq!(p2.rps.lt_curr.len(), 1);
        assert_eq!(p2.rps.lt_curr[0].id, idr_id);
        assert!(p2.rps.lt_curr[0].is_long_term);

        // And with delta_poc_msb_present the same anchor resolves by FULL POC
        // (8.3.2's other lookup path).
        let p3 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 3,
                neg: vec![(0, true)],
                lt: vec![(0, true, Some(0))],
                sps_long_term: true,
                num_ref_idx_l0: 2,
                ..Default::default()
            }))
            .unwrap();
        assert!(p3.warnings.is_empty(), "{:?}", p3.warnings);
        assert_eq!(p3.rps.lt_curr.len(), 1);
        assert_eq!(p3.rps.lt_curr[0].id, idr_id);
    }

    #[test]
    fn the_dpb_snapshot_holds_a_foll_long_term_anchor_the_current_rps_never_names() {
        // 8.3.2's *Foll* sets are the whole point of the snapshot: an anchor pinned
        // long-term for LATER pictures, marked in the DPB, in none of this picture's
        // three current sets. `RefPicSetLtCurr` is empty here — a DXVA `RefPicList`
        // built from the current sets alone drops the anchor for exactly the
        // pictures between the pin and its use.
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        let p0 = planner.plan_au(&au0).unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        assert!(
            p0.dpb_refs.is_empty(),
            "an opening IDR has no DPB behind it"
        );

        let p1 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 1,
                neg: vec![(0, true)],
                sps_long_term: true,
                num_ref_idx_l0: 1,
                ..Default::default()
            }))
            .unwrap();
        let p1_id = p1.dpb.stored.unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);

        // used_by_curr_pic_lt_flag = 0: the IDR lands in RefPicSetLtFoll — kept
        // marked long-term, referenced by nothing in this picture.
        let p2 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 2,
                neg: vec![(0, true)],
                lt: vec![(0, false, None)],
                sps_long_term: true,
                num_ref_idx_l0: 1,
                ..Default::default()
            }))
            .unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);
        assert!(
            p2.rps.lt_curr.is_empty(),
            "the anchor is Foll, not Curr, for this picture"
        );
        assert_eq!(
            p2.slices[0]
                .ref_list0
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![p1_id]
        );
        // …and it is still in the DPB, still marked long-term.
        assert_eq!(
            p2.dpb_refs
                .iter()
                .map(|r| (r.id, r.is_long_term))
                .collect::<Vec<_>>(),
            vec![(idr_id, true), (p1_id, false)]
        );
        assert!(
            !p2.dpb.removed.contains(&idr_id),
            "a Foll entry must not be retired"
        );
    }

    #[test]
    fn the_dpb_snapshot_is_a_superset_of_the_current_rps_across_the_whole_vector() {
        let (_, plans) = plan_whole_clip(TEST_25FPS);
        assert_eq!(plans.len(), 250);
        for (i, plan) in plans.iter().enumerate() {
            let mut ids: Vec<PicId> = plan.dpb_refs.iter().map(|r| r.id).collect();
            let count = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), count, "AU {i}: the snapshot repeats a picture");
            // Every current-set entry is a marked DPB picture, with the SAME marking
            // — the snapshot is the authority a backend keys its arrays by.
            for rp in plan
                .rps
                .st_curr_before
                .iter()
                .chain(&plan.rps.st_curr_after)
                .chain(&plan.rps.lt_curr)
            {
                let found = plan
                    .dpb_refs
                    .iter()
                    .find(|d| d.id == rp.id)
                    .unwrap_or_else(|| panic!("AU {i}: RPS entry {} is not marked", rp.id));
                assert_eq!(found.is_long_term, rp.is_long_term);
                assert_eq!(found.pic_order_cnt, rp.pic_order_cnt);
            }
            // The current picture is stored AFTER the snapshot is taken.
            assert!(!ids.contains(&plan.dpb.stored.unwrap()));
        }
    }

    #[test]
    fn an_rps_that_drops_a_reference_retires_it_from_the_dpb() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let p1_id = p1.dpb.stored.unwrap();

        // p2's RPS names only poc 1: the IDR leaves every set, is marked unused and
        // — already output — must be reported removed.
        let p2 = planner.plan_au(&trail_p(2, &[(0, true)], 1)).unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);
        assert_eq!(p2.slices[0].ref_list0[0].id, p1_id);
        assert!(p2.dpb.removed.contains(&idr_id));
        assert!(!p2.dpb.removed.contains(&p1_id));
    }

    #[test]
    fn a_missing_reference_is_substituted_in_place_not_compacted() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let p1_id = p1.dpb.stored.unwrap();

        // The picture at poc 2 was lost on the wire. p3's RPS still names poc 2, 1
        // and 0; the list0 head (poc 2) is unresolvable and must be substituted in
        // place — the two real entries keep their ref_idx positions.
        let p3 = planner
            .plan_au(&trail_p(3, &[(0, true), (0, true), (0, true)], 3))
            .unwrap();
        assert!(
            p3.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::MissingReference { .. })),
            "{:?}",
            p3.warnings
        );
        let ids: Vec<PicId> = p3.slices[0].ref_list0.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![p1_id, p1_id, idr_id],
            "substitution must preserve list length and positions"
        );
        // The plan's RPS omits the unresolvable entry rather than fabricating one.
        assert_eq!(p3.rps.st_curr_before.len(), 2);
    }

    #[test]
    fn a_rasl_behind_a_join_cra_is_skipped_and_the_trailing_picture_plans() {
        // A CRA opening the stream (an open-GOP join): NoRaslOutputFlag = 1.
        let mut au0 = param_sets(&SpsOpts::default());
        au0.extend(synth_slice(&SliceOpts {
            nalu_type: CRA_NUT,
            slice_type: 2,
            poc_lsb: 0,
            ..Default::default()
        }));
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        assert!(p0.picture.is_irap && !p0.picture.is_idr);
        assert!(p0.picture.no_rasl_output_flag);
        assert_eq!(p0.picture.pic_order_cnt, 0);
        let cra_id = p0.dpb.stored.unwrap();

        // Its RASL leading picture (poc -1, referencing a pre-join picture) must be
        // refused without wedging the planner.
        let rasl = synth_slice(&SliceOpts {
            nalu_type: RASL_N,
            poc_lsb: 15,
            neg: vec![(0, true)],
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&rasl),
            Err(PlanError::RaslSkipped { poc: -1 })
        ));

        // The trailing picture referencing the CRA plans clean.
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);
        assert_eq!(p1.slices[0].ref_list0[0].id, cra_id);
    }

    #[test]
    fn a_rasl_behind_a_mid_stream_cra_plans_normally() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let _idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let p1_id = p1.dpb.stored.unwrap();

        // A CRA reached by continuous decoding keeps NoRaslOutputFlag = 0; its RPS
        // may keep pre-CRA pictures around for its RASLs (used = false → StFoll).
        let mut cra = synth_slice(&SliceOpts {
            nalu_type: CRA_NUT,
            slice_type: 2,
            poc_lsb: 4,
            neg: vec![(2, false)],
            ..Default::default()
        });
        // In-band parameter re-send at the IRAP, as hosts do.
        let mut au = param_sets(&SpsOpts::default());
        au.append(&mut cra);
        let p2 = planner.plan_au(&au).unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);
        assert!(!p2.picture.no_rasl_output_flag);
        let cra_id = p2.dpb.stored.unwrap();

        // The RASL at poc 2 references both sides of the CRA — decodable here.
        let p3 = planner
            .plan_au(&synth_slice(&SliceOpts {
                nalu_type: RASL_N,
                poc_lsb: 2,
                neg: vec![(0, true)],
                pos: vec![(1, true)],
                num_ref_idx_l0: 2,
                ..Default::default()
            }))
            .unwrap();
        assert!(p3.warnings.is_empty(), "{:?}", p3.warnings);
        assert_eq!(p3.picture.pic_order_cnt, 2);
        assert_eq!(
            p3.slices[0]
                .ref_list0
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![p1_id, cra_id],
            "list0: the past (StCurrBefore) then the future (StCurrAfter)"
        );
        assert!(
            !p3.picture.is_reference,
            "RASL_N is a sub-layer non-reference type"
        );
    }

    #[test]
    fn a_recovery_point_sei_lands_on_the_picture_plan_and_does_not_stick() {
        let mut au0 = param_sets(&SpsOpts::default());
        // Prefix SEI (type 39): recovery point, recovery_poc_cnt = 0, exact = 0,
        // broken = 0 (payload bits: se(0) '1', two flag zeros, alignment).
        au0.extend(h265_nalu(39, &[0x06, 0x01, 0x90, 0x80]));
        au0.extend(idr_slice());

        let mut planner = H265Planner::new();
        let plan = planner.plan_au(&au0).unwrap();
        assert_eq!(
            plan.picture.recovery_point,
            Some(RecoveryPointHevc {
                recovery_poc_cnt: 0,
                exact_match: false,
                broken_link: false
            })
        );

        // The following AU carries no SEI: the field must not stick.
        let plan = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert_eq!(plan.picture.recovery_point, None);
    }

    #[test]
    fn the_conformance_window_scales_by_the_chroma_format() {
        // 4:2:0: SubWidthC = SubHeightC = 2.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                conf_win: Some((2, 1, 1, 2)),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 4,
                y: 2,
                width: 58,
                height: 58
            }
        );

        // 4:4:4 (chroma_format_idc 3, RExt profile): offsets are luma samples.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                profile_idc: 4,
                chroma_format_idc: 3,
                conf_win: Some((2, 1, 1, 2)),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(plan.picture.chroma_format_idc, 3);
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 2,
                y: 1,
                width: 61,
                height: 61
            }
        );

        // 4:2:2: SubWidthC = 2, SubHeightC = 1.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                profile_idc: 4,
                chroma_format_idc: 2,
                conf_win: Some((1, 1, 1, 1)),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 2,
                y: 1,
                width: 60,
                height: 62
            }
        );
    }

    #[test]
    fn main10_depths_ride_the_plan() {
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                profile_idc: 2,
                bit_depth_minus8: 2,
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(plan.picture.general_profile_idc, 2);
        assert_eq!(plan.picture.bit_depth_luma_minus8, 2);
        assert_eq!(plan.picture.bit_depth_chroma_minus8, 2);
        assert_eq!(plan.picture.chroma_format_idc, 1);
    }

    #[test]
    fn explicit_vui_colour_rides_the_plan_and_the_range_flag_stands_alone() {
        // BT.2020/PQ HDR signalling — the in-band switch the Windows host emits.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                vui: VuiOpt::SignalType {
                    full_range: false,
                    colour: Some((9, 16, 9)),
                },
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                video_full_range: false,
            }
        );

        // video_signal_type present, full-range set, but NO colour description: the
        // code points stay E.3.1's "unspecified" while the range flag rides.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                vui: VuiOpt::SignalType {
                    full_range: true,
                    colour: None,
                },
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: true,
            }
        );
    }

    #[test]
    fn a_field_coded_stream_is_rejected_as_outside_the_envelope() {
        let err = H265Planner::new()
            .plan_au(&synth_sps(&SpsOpts {
                vui: VuiOpt::FieldSeq,
                ..Default::default()
            }))
            .unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("field")),
            "{err:?}"
        );
    }

    #[test]
    fn a_dpb_deeper_than_16_frames_is_rejected_as_outside_the_envelope() {
        // The vendored parser reads sps_max_dec_pic_buffering_minus1 up to 16 — a
        // 17-frame DPB no hardware implements. Gated at SPS activation.
        let err = H265Planner::new()
            .plan_au(&synth_sps(&SpsOpts {
                max_dec_pic_buffering_minus1: 16,
                ..Default::default()
            }))
            .unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("DPB")),
            "{err:?}"
        );
    }

    #[test]
    fn a_multilayer_nalu_is_rejected_as_outside_the_envelope() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            layer_id: 1,
            ..Default::default()
        }));
        let err = H265Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("nuh_layer_id")),
            "{err:?}"
        );
    }

    #[test]
    fn flush_resets_decoding_state_and_refuses_non_irap_until_one_arrives() {
        let mut planner = H265Planner::new();
        let id0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap()
            .dpb
            .stored
            .unwrap();
        let id1 = planner
            .plan_au(&trail_p(1, &[(0, true)], 1))
            .unwrap()
            .dpb
            .stored
            .unwrap();

        let flushed = planner.flush();
        assert!(flushed.outputs.is_empty(), "both pictures already output");
        assert_eq!(flushed.removed, vec![id0, id1]);

        // A non-IRAP AU is refused until the next IRAP.
        assert!(matches!(
            planner.plan_au(&trail_p(2, &[(0, true)], 1)),
            Err(PlanError::AwaitingIdr)
        ));

        // A CRA restarts planning — the flush gave it NoRaslOutputFlag = 1, making
        // it as good a re-entry point as an IDR. Parameter sets survived (7.4.2.4).
        let plan = planner
            .plan_au(&synth_slice(&SliceOpts {
                nalu_type: CRA_NUT,
                slice_type: 2,
                poc_lsb: 0,
                ..Default::default()
            }))
            .unwrap();
        assert!(plan.picture.is_irap);
        assert!(plan.picture.no_rasl_output_flag);
        assert_eq!(plan.picture.pic_order_cnt, 0);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        // And the stream continues cleanly on the reset state.
        let plan = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.slices[0].ref_list0.len(), 1);
    }

    #[test]
    fn a_foreign_slice_in_the_au_is_dropped_with_a_truncated_au_warning() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(idr_slice());
        // A mis-split AU: a continuation segment belonging to ANOTHER picture (a
        // TRAIL slice after an IDR — 7.4.2.4.4 requires one NALU type per picture).
        au.extend(synth_slice(&SliceOpts {
            segment: Some((0, 0, false)),
            poc_lsb: 1,
            neg: vec![(0, true)],
            ..Default::default()
        }));

        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(plan.picture.is_idr);
        assert_eq!(plan.slices.len(), 1, "the foreign slice is not planned");
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    #[test]
    fn a_truncated_continuation_slice_degrades_to_a_warning() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(idr_slice());
        // A second IDR segment, cut mid-header: its parse fails, the planned slice
        // before the cut survives.
        let mut cont = synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            segment: Some((0, 0, false)),
            ..Default::default()
        });
        cont.truncate(cont.len() - 1);
        // Drop the alignment content so the header read runs out of bits.
        let cut_at = au.len();
        au.extend(&cont[..7.min(cont.len())]);

        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert_eq!(plan.slices.len(), 1);
        assert!(
            plan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::TruncatedAu { offset } if *offset == cut_at)),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn two_pictures_in_one_au_is_outside_the_envelope() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(idr_slice());
        au.extend(idr_slice());
        let err = H265Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("one access unit")),
            "{err:?}"
        );
    }

    #[test]
    fn dependent_slice_segments_plan_with_the_completed_header() {
        // 128x64 with a 64-sample CTB: two CTBs, so segment addresses exist (1 bit).
        let sps = SpsOpts {
            width: 128,
            ..Default::default()
        };
        let mut au = synth_sps(&sps);
        au.extend(synth_pps(true));
        au.extend(idr_slice());
        au.extend(synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            segment: Some((1, 1, true)),
            pps_dependent_enabled: true,
            ..Default::default()
        }));

        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.slices.len(), 2);
        let dependent = &plan.slices[1].header;
        assert!(dependent.dependent_slice_segment_flag);
        assert_eq!(dependent.segment_address, 1);
        assert!(
            dependent.type_.is_i(),
            "the dependent header inherited the independent slice's type"
        );
    }

    #[test]
    fn reordering_streams_plan_with_the_envelope_fact_flagged() {
        let sps = SpsOpts {
            max_num_reorder_pics: 2,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&opening_idr_au(&sps)).unwrap();
        assert!(
            p0.warnings.contains(&PlanWarning::NonZeroReorder {
                max_num_reorder_pics: 2
            }),
            "{:?}",
            p0.warnings
        );
        // With reorder depth 2 nothing is display-ready yet.
        assert!(p0.dpb.outputs.is_empty());
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(p1.dpb.outputs.is_empty());

        // The flush releases everything, in POC order.
        let flushed = planner.flush();
        assert_eq!(
            flushed.outputs,
            vec![p0.dpb.stored.unwrap(), p1.dpb.stored.unwrap()]
        );
    }

    #[test]
    fn poc_msb_wraps_across_the_lsb_boundary() {
        // 4-bit POC lsb (MaxPicOrderCntLsb 16): 0 → 7 → 14 → 2, the last step
        // crossing the lsb boundary, must decode as POC 18 (8.3.1 msb increment).
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let p1 = planner.plan_au(&trail_p(7, &[(6, true)], 1)).unwrap();
        assert_eq!(p1.picture.pic_order_cnt, 7);
        let p2 = planner.plan_au(&trail_p(14, &[(6, true)], 1)).unwrap();
        assert_eq!(p2.picture.pic_order_cnt, 14);
        let p3 = planner.plan_au(&trail_p(2, &[(3, true)], 1)).unwrap();
        assert!(p3.warnings.is_empty(), "{:?}", p3.warnings);
        assert_eq!(p3.picture.pic_order_cnt, 18);
        assert_eq!(p3.slices[0].ref_list0[0].pic_order_cnt, 14);
    }

    #[test]
    fn the_plans_parameter_set_accessors_carry_the_activated_content() {
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        assert_eq!(plan.sps.seq_parameter_set_id, 0);
        assert_eq!(plan.sps.width(), 64);
        assert_eq!(plan.sps.height(), 64);
        assert_eq!(plan.pps.pic_parameter_set_id, 0);
        assert_eq!(plan.pps.seq_parameter_set_id, 0);
        assert!(
            Rc::ptr_eq(&plan.sps, &plan.pps.sps),
            "the SPS accessor is the PPS's own SPS, not a second copy"
        );
    }

    #[test]
    fn outputs_queued_during_a_failed_au_surface_in_the_next_successful_plan() {
        let sps = SpsOpts {
            max_num_reorder_pics: 1,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&opening_idr_au(&sps)).unwrap();
        let id0 = p0.dpb.stored.unwrap();
        assert!(p0.dpb.outputs.is_empty(), "held back by the reorder depth");
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let id1 = p1.dpb.stored.unwrap();
        assert_eq!(p1.dpb.outputs, vec![id0]);

        // This AU errors AFTER its mid-stream IDR begin drained the DPB (queueing
        // p1 for output): a second first-segment makes it a two-picture AU.
        let mut bad_au = idr_slice();
        bad_au.extend(idr_slice());
        assert!(matches!(
            planner.plan_au(&bad_au),
            Err(PlanError::OutsideEnvelope(_))
        ));

        // The queued output and the eviction must surface here, not vanish.
        let plan = planner.plan_au(&idr_slice()).unwrap();
        assert!(plan.dpb.outputs.contains(&id1));
        assert!(plan.dpb.removed.contains(&id1));
    }

    #[test]
    fn a_dropped_reference_au_degrades_to_warnings_and_planning_continues() {
        let aus = split_into_aus(TEST_25FPS);

        // Pass 1: find a droppable AU — a non-IRAP reference picture not followed
        // by an IRAP (an IRAP right after would reset the state and hide the loss).
        let mut planner = H265Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(planner.plan_au(au).unwrap());
        }
        let dropped = plans
            .iter()
            .enumerate()
            .position(|(i, p)| {
                p.picture.is_reference
                    && !p.picture.is_irap
                    && plans.get(i + 1).is_some_and(|next| !next.picture.is_irap)
            })
            .expect("the vector contains a droppable reference picture");

        // Pass 2: the same stream minus that AU must warn, not error — and every
        // ref list entry it emits must still resolve to a picture the backend was
        // told to store (substitution never leaks a hole).
        let mut planner = H265Planner::new();
        let mut missing_seen = false;
        let mut planned = 0usize;
        let mut stored_so_far: BTreeSet<PicId> = BTreeSet::new();
        for (i, au) in aus.iter().enumerate() {
            if i == dropped {
                continue;
            }
            let plan = planner
                .plan_au(au)
                .expect("a lost reference AU must degrade to warnings, not errors");
            planned += 1;
            missing_seen |= plan
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::MissingReference { .. }));
            stored_so_far.insert(plan.dpb.stored.unwrap());
            for slice in &plan.slices {
                for entry in slice.ref_list0.iter().chain(&slice.ref_list1) {
                    assert!(
                        stored_so_far.contains(&entry.id),
                        "every emitted reference must be a real stored PicId"
                    );
                }
            }
            for entry in plan
                .rps
                .st_curr_before
                .iter()
                .chain(&plan.rps.st_curr_after)
                .chain(&plan.rps.lt_curr)
            {
                assert!(stored_so_far.contains(&entry.id));
            }
        }

        assert_eq!(planned, aus.len() - 1);
        assert!(
            missing_seen,
            "an AU after the drop must report the missing reference"
        );
    }

    #[test]
    fn a_first_slice_naming_an_unseen_pps_is_a_no_active_param_set_error() {
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        // A first slice segment referencing PPS 1, which was never sent: the AU has
        // no picture to conceal around, so this is an error, not a warning.
        let mut s = BitSink::new();
        s.bit(1); // first_slice_segment_in_pic_flag
        s.ue(1); // slice_pic_parameter_set_id — never seen
        let au = h265_nalu(TRAIL_R, &s.finish());
        assert!(matches!(
            planner.plan_au(&au),
            Err(PlanError::NoActiveParamSet { pps_id: 1 })
        ));
    }

    // ------- review-round regressions (findings 1-9) -------

    /// Finding 1: the vendored parser indexed its 16-deep long-term arrays with a
    /// count it read up to 32 — a hostile header was a production panic. Now a parse
    /// error (vendor deviation 7).
    #[test]
    fn a_hostile_long_term_count_is_a_parse_error_not_a_panic() {
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        planner.plan_au(&au0).unwrap();

        let hostile = synth_slice(&SliceOpts {
            poc_lsb: 1,
            neg: vec![(0, true)],
            lt: vec![(0, true, None); 17], // num_long_term_pics = 17 > the 16 slots
            sps_long_term: true,
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&hostile),
            Err(PlanError::Parse(_))
        ));
        // And the planner survives to plan the next clean AU.
        let plan = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    fn eos_nalu() -> Vec<u8> {
        h265_nalu(36, &[])
    }

    /// Finding 2: C.5.2.2 exempts only picture 0 of the BITSTREAM — an IRAP behind an
    /// in-band EOS must drain the previous sequence's outputs before the new one
    /// starts, and must honour no_output_of_prior_pics_flag there.
    #[test]
    fn an_eos_then_idr_drains_the_previous_sequence_before_the_new_one() {
        let sps = SpsOpts {
            max_num_reorder_pics: 2,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&opening_idr_au(&sps)).unwrap();
        let id0 = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let id1 = p1.dpb.stored.unwrap();
        assert!(p1.dpb.outputs.is_empty(), "held back by the reorder depth");
        let p2 = planner
            .plan_au(&trail_p(2, &[(0, true), (0, true)], 1))
            .unwrap();
        let id2 = p2.dpb.stored.unwrap();
        assert_eq!(p2.dpb.outputs, vec![id0], "depth 2 releases poc 0 here");

        // EOS + IDR in one AU: pocs 1 and 2 must ALL come out here, in POC order,
        // before the new sequence emits anything — never interleaved with it.
        let mut au = eos_nalu();
        au.extend(idr_slice());
        let p3 = planner.plan_au(&au).unwrap();
        assert!(p3.picture.no_rasl_output_flag, "EOS gave the IDR the flag");
        assert_eq!(p3.dpb.outputs, vec![id1, id2]);
        for id in [id0, id1, id2] {
            assert!(p3.dpb.removed.contains(&id));
        }

        // Same join with no_output_of_prior_pics_flag = 1: the leftovers are
        // DISCARDED — removed without ever being output (C.3.2).
        let mut planner = H265Planner::new();
        planner.plan_au(&opening_idr_au(&sps)).unwrap();
        let q1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let q1_id = q1.dpb.stored.unwrap();
        let mut au = eos_nalu();
        au.extend(synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            no_output_of_prior_pics: true,
            ..Default::default()
        }));
        let q2 = planner.plan_au(&au).unwrap();
        assert!(
            !q2.dpb.outputs.contains(&q1_id),
            "no_output_of_prior_pics discards without output"
        );
        assert!(q2.dpb.removed.contains(&q1_id));
    }

    /// Finding 3: parse_sps stores an SPS even when its AU is rejected, and
    /// NegotiationInfo deliberately omits envelope-only facts — so a later PPS-only
    /// rebind must re-run the envelope gate at activation. Both bypass legs.
    #[test]
    fn a_rejected_sps_cannot_be_activated_through_a_pps_only_rebind() {
        // Leg A: hostile conformance window on otherwise-identical geometry. Without
        // the activation-time gate this reaches visible_rectangle()'s unchecked u32
        // subtraction (panic in debug, silent wrap in release).
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let hostile = synth_sps(&SpsOpts {
            conf_win: Some((100, 0, 0, 0)), // 200 luma samples of a 64-wide picture
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&hostile),
            Err(PlanError::Parse(_))
        ));
        let mut rebind = synth_pps(false);
        rebind.extend(idr_slice());
        match planner.plan_au(&rebind) {
            Err(PlanError::Parse(msg)) => assert!(msg.contains("conformance window"), "{msg}"),
            other => panic!("the rebind must not activate the rejected SPS: {other:?}"),
        }

        // Leg B: a 17-frame DPB, same geometry. The envelope gate is what must catch
        // this on BOTH the direct AU and the PPS-only rebind: renegotiation runs only
        // after `check_envelope` has passed, so a NegotiationInfo difference (5 vs the
        // capped 16) is never reached and cannot stand in for the gate.
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let hostile = synth_sps(&SpsOpts {
            max_dec_pic_buffering_minus1: 16,
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&hostile),
            Err(PlanError::OutsideEnvelope(_))
        ));
        let mut rebind = synth_pps(false);
        rebind.extend(idr_slice());
        assert!(matches!(
            planner.plan_au(&rebind),
            Err(PlanError::OutsideEnvelope(what)) if what.contains("DPB")
        ));
    }

    /// The DPB the planner reports is the stream's own declared depth, and it must fit
    /// a mainstream Vulkan Video decoder — pinned at the exact stream that broke it.
    ///
    /// Field defect, `.21` (RTX 5070 Ti), 2026-08-07: a punktfunk 1080p HEVC session
    /// refused EVERY access unit with "stream needs 17 DPB slots, device caps at 16",
    /// exhausted the decode ladder and reconnected without HEVC — fatal, because there
    /// is no permissively licensed software HEVC decoder to fall back to. The host was
    /// blameless: its SPS asked for six pictures at both resolutions. The planner was
    /// reporting equation A-2's LEVEL ceiling instead, and A-2 branches on picture size
    /// against `MaxLumaPs`, so the identical stream reported 16 at 1080p and 6 at 4K.
    ///
    /// The three assertions below are the whole defect: the two resolutions must agree,
    /// they must agree on the number the STREAM signalled, and `+ 1` for the current
    /// picture must clear the 16-slot floor that NVIDIA's `maxDpbSlots` sets. The host
    /// side of the same arithmetic — that `RFI_DPB` never grows past what this leaves
    /// room for — is pinned in `pf-encode`'s `rfi_dpb_fits_a_mainstream_vulkan_decoder`.
    #[test]
    fn the_reported_dpb_is_the_streams_own_depth_and_fits_a_16_slot_decoder() {
        /// `VkVideoCapabilitiesKHR::maxDpbSlots` on NVIDIA — the lowest cap among the
        /// decoders punktfunk targets (RADV reports 17). A stream needing more than
        /// this cannot be decoded natively at all.
        const VULKAN_MAX_DPB_SLOTS: usize = 16;

        // The field stream, byte for byte on the fields that matter: L5.1 High (which
        // NVENC autoselects at 130 Mbps), coded 1920x1088, six pictures declared.
        let field = SpsOpts {
            width: 1920,
            height: 1088,
            level_idc: 153,
            max_dec_pic_buffering_minus1: 5,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let at_1080p = planner.plan_au(&opening_idr_au(&field)).unwrap();
        assert_eq!(
            at_1080p.picture.max_dpb_frames, 6,
            "sps_max_dec_pic_buffering_minus1 = 5 means six pictures — five references \
             plus the current one. A-2 would have said 16 here, because 1920x1088 = \
             2088960 luma samples fall under MaxLumaPs(L5.1) >> 2 = 2228224."
        );
        // What the backends ask the driver for: one slot per DPB picture, plus one for
        // the picture in flight (`pf_vkdecode::slots` pins the same convention).
        let slots_for = |plan: &AuPlan| plan.picture.max_dpb_frames + 1;
        assert!(
            slots_for(&at_1080p) <= VULKAN_MAX_DPB_SLOTS,
            "{} DPB frames need {} slots, over the {VULKAN_MAX_DPB_SLOTS} a mainstream \
             Vulkan Video decoder offers — this is the refusal that cost HEVC",
            at_1080p.picture.max_dpb_frames,
            slots_for(&at_1080p)
        );

        // The same stream at the other three resolutions punktfunk streams at. 720p and
        // 1080p died in the field and 1440p and 4K survived, purely because A-2 branches
        // on picture size: 1280x720 and 1920x1088 fall under MaxLumaPs >> 2 (16 frames),
        // 2560x1440 under MaxLumaPs >> 1 (12), and 3840x2176 past 3 * MaxLumaPs >> 2 (6).
        // All four must now be indistinguishable, because the stream is.
        for (w, h) in [(1280, 720), (2560, 1440), (3840, 2176)] {
            let mut planner = H265Planner::new();
            let plan = planner
                .plan_au(&opening_idr_au(&SpsOpts {
                    width: w,
                    height: h,
                    ..field.clone()
                }))
                .unwrap();
            assert_eq!(
                plan.picture.max_dpb_frames, at_1080p.picture.max_dpb_frames,
                "{w}x{h}: the DPB requirement is a property of the stream, not of which \
                 A-2 branch the picture size lands in"
            );
        }

        // The invariant behind all of them: the reported depth is the declared one, and
        // every depth that leaves a slot for the picture in flight fits a 16-slot device.
        for minus1 in 0..=14u32 {
            let mut planner = H265Planner::new();
            let plan = planner
                .plan_au(&opening_idr_au(&SpsOpts {
                    max_dec_pic_buffering_minus1: minus1,
                    ..field.clone()
                }))
                .unwrap();
            assert_eq!(plan.picture.max_dpb_frames, minus1 as usize + 1);
            assert!(slots_for(&plan) <= VULKAN_MAX_DPB_SLOTS);
        }

        // The one honest residue: A.4 lets a conforming stream declare a full 16-picture
        // DPB, and 16 pictures plus the one in flight is 17 slots, which NVIDIA does not
        // have. Refusing that is right — it genuinely does not fit, and decoding it with
        // fewer slots would silently corrupt references. No punktfunk host comes near it
        // (`RFI_DPB` puts us at 6), and this is pinned so the distinction stays visible:
        // what was fixed is streams that never needed the slots, not this one.
        let mut planner = H265Planner::new();
        let deepest = planner
            .plan_au(&opening_idr_au(&SpsOpts {
                max_dec_pic_buffering_minus1: 15,
                ..field.clone()
            }))
            .unwrap();
        assert_eq!(deepest.picture.max_dpb_frames, 16);
        assert_eq!(slots_for(&deepest), VULKAN_MAX_DPB_SLOTS + 1);
        let mut planner = H265Planner::new();
        assert!(
            matches!(
                planner.plan_au(&opening_idr_au(&SpsOpts {
                    max_dec_pic_buffering_minus1: 16,
                    ..field.clone()
                })),
                Err(PlanError::OutsideEnvelope(what)) if what.contains("DPB")
            ),
            "a 17-picture DPB is past A.4's cap and must be refused at activation, not \
             clamped silently into a slot pool that cannot hold it"
        );
    }

    /// Finding 4: the RASL refusal must run BEFORE renegotiation — a RASL AU carrying
    /// a renegotiating SPS must not drain the DPB on its way out.
    #[test]
    fn a_skipped_rasl_carrying_a_renegotiating_sps_leaves_the_dpb_intact() {
        // A CRA join: RASLs behind it are skipped.
        let mut au0 = param_sets(&SpsOpts::default());
        au0.extend(synth_slice(&SliceOpts {
            nalu_type: CRA_NUT,
            slice_type: 2,
            ..Default::default()
        }));
        let mut planner = H265Planner::new();
        let cra_id = planner.plan_au(&au0).unwrap().dpb.stored.unwrap();

        // The RASL AU re-sends parameter sets with NEW geometry (128x64) — a
        // renegotiation trigger — and must still be refused state-free.
        let mut rasl_au = param_sets(&SpsOpts {
            width: 128,
            ..Default::default()
        });
        rasl_au.extend(synth_slice(&SliceOpts {
            nalu_type: RASL_N,
            poc_lsb: 15,
            neg: vec![(0, true)],
            ..Default::default()
        }));
        assert!(matches!(
            planner.plan_au(&rasl_au),
            Err(PlanError::RaslSkipped { .. })
        ));

        // With the original parameter sets re-sent, the trailing picture still finds
        // the CRA in the DPB: nothing was drained or renegotiated by the skip.
        let mut au = param_sets(&SpsOpts::default());
        au.extend(trail_p(1, &[(0, true)], 1));
        let plan = planner.plan_au(&au).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.slices[0].ref_list0[0].id, cra_id);
    }

    /// Finding 5: an AU that OPENS with a continuation segment is the mis-split tail
    /// of a previous picture; beginning a picture from it would fabricate a duplicate
    /// (a dependent one would even wear a previous AU's independent header).
    #[test]
    fn a_leading_continuation_segment_is_skipped_not_fabricated_into_a_picture() {
        let sps = SpsOpts {
            width: 128, // two CTBs, so continuation addresses exist
            ..Default::default()
        };
        let mut au0 = synth_sps(&sps);
        au0.extend(synth_pps(true));
        au0.extend(idr_slice());
        let mut planner = H265Planner::new();
        planner.plan_au(&au0).unwrap();

        // A dependent segment leads the AU; the real picture follows.
        let mut au = synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            segment: Some((1, 1, true)),
            pps_dependent_enabled: true,
            ..Default::default()
        });
        au.extend(trail_p(1, &[(0, true)], 1));
        let plan = planner.plan_au(&au).unwrap();
        assert_eq!(plan.slices.len(), 1, "only the real picture is planned");
        assert_eq!(plan.picture.pic_order_cnt, 1);
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));

        // Same for a leading INDEPENDENT continuation segment.
        let mut au = synth_slice(&SliceOpts {
            segment: Some((1, 1, false)),
            pps_dependent_enabled: true,
            poc_lsb: 1,
            neg: vec![(0, true)],
            ..Default::default()
        });
        au.extend(trail_p(2, &[(0, true)], 1));
        let plan = planner.plan_au(&au).unwrap();
        assert_eq!(plan.slices.len(), 1);
        assert_eq!(plan.picture.pic_order_cnt, 2);
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    /// Finding 6: the old truncation detector keyed on the cursor, which never has a
    /// start code behind it — cut-off data at the AU tail went unreported.
    #[test]
    fn cut_off_data_at_the_au_tail_warns_while_zero_padding_does_not() {
        let mut au = opening_idr_au(&SpsOpts::default());
        let cut_at = au.len();
        au.extend([0x00, 0x00, 0x01, 0x02]); // a start code + half a NAL header
        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::TruncatedAu { offset } if *offset == cut_at)),
            "{:?}",
            plan.warnings
        );

        // trailing_zero_8bits padding (B.2.2) is legal and stays silent.
        let mut au = opening_idr_au(&SpsOpts::default());
        au.extend([0x00, 0x00, 0x00, 0x00]);
        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    /// Finding 7: a mid-stream SPS that raises the reorder depth on unchanged
    /// geometry must still renegotiate and flag the envelope fact.
    #[test]
    fn a_mid_stream_reorder_increase_flags_the_envelope_fact() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        assert!(p0.warnings.is_empty(), "{:?}", p0.warnings);
        planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();

        let mut au = param_sets(&SpsOpts {
            max_num_reorder_pics: 3,
            ..Default::default()
        });
        au.extend(idr_slice());
        let plan = planner.plan_au(&au).unwrap();
        assert!(
            plan.warnings.contains(&PlanWarning::NonZeroReorder {
                max_num_reorder_pics: 3
            }),
            "{:?}",
            plan.warnings
        );
    }

    /// Finding 8: two references sharing a poc_lsb make the MSB-less long-term
    /// lookup ambiguous (a 7.4.7.1 violation) — the RFI anchor path must say so
    /// rather than silently picking one.
    #[test]
    fn an_ambiguous_long_term_poc_lsb_warns_instead_of_silently_picking() {
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        planner.plan_au(&au0).unwrap(); // poc 0

        let lt_slice = |poc_lsb: u32, neg: Vec<(u32, bool)>, lt| {
            synth_slice(&SliceOpts {
                poc_lsb,
                neg,
                lt,
                sps_long_term: true,
                ..Default::default()
            })
        };
        // Build to POC 16 while keeping POC 0 referenced: 0, 7, 14, 16 — the msb
        // wrap makes 0 and 16 share poc_lsb 0.
        planner
            .plan_au(&lt_slice(7, vec![(6, true)], vec![]))
            .unwrap();
        planner
            .plan_au(&lt_slice(14, vec![(6, true), (6, true)], vec![]))
            .unwrap();
        let p3 = planner
            .plan_au(&lt_slice(0, vec![(1, true), (13, true)], vec![]))
            .unwrap();
        assert_eq!(p3.picture.pic_order_cnt, 16, "msb wrap");

        // POC 0 and POC 16 are both referenced and share poc_lsb 0: an MSB-less
        // long-term entry naming lsb 0 is ambiguous.
        let p4 = planner
            .plan_au(&lt_slice(1, vec![(0, true)], vec![(0, true, None)]))
            .unwrap();
        assert!(
            p4.warnings.iter().any(|w| matches!(
                w,
                PlanWarning::MissingReference { context, .. } if context.contains("ambiguous")
            )),
            "{:?}",
            p4.warnings
        );
        assert_eq!(p4.rps.lt_curr.len(), 1, "still resolved (concealment)");
    }

    /// Finding 9: an IRAP AU that fails before its picture begins must not unlatch
    /// the AwaitingIdr gate.
    #[test]
    fn a_failed_resume_irap_keeps_the_awaiting_gate_latched() {
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        planner.flush();

        // A CRA naming an unseen PPS: the resume attempt fails before begin_picture.
        let mut s = BitSink::new();
        s.bit(1); // first_slice_segment_in_pic_flag
        s.bit(0); // no_output_of_prior_pics_flag
        s.ue(1); // slice_pic_parameter_set_id — never seen
        let bad_cra = h265_nalu(CRA_NUT, &s.finish());
        assert!(matches!(
            planner.plan_au(&bad_cra),
            Err(PlanError::NoActiveParamSet { pps_id: 1 })
        ));

        // The gate must still hold against non-IRAP pictures...
        assert!(matches!(
            planner.plan_au(&trail_p(1, &[(0, true)], 1)),
            Err(PlanError::AwaitingIdr)
        ));

        // ...and a valid IRAP still resumes (parameter sets survived the flush).
        let plan = planner.plan_au(&idr_slice()).unwrap();
        assert!(plan.picture.is_idr);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    // ------- vendored-parser bounds regressions (deviations 9-11) -------

    const VPS_NUT: u8 = 32;
    const SPS_NUT: u8 = 33;
    const PPS_NUT: u8 = 34;

    /// Trailing bits, so the parser keeps reading past the guarded field instead of
    /// stopping short — a truncated NALU would be an error for the wrong reason.
    fn padded(mut s: BitSink) -> Vec<u8> {
        for _ in 0..64 {
            s.bits(8, 0xff);
        }
        s.finish()
    }

    /// Deviation 9: `{vps,sps}_max_sub_layers_minus1` is u(3), so 7 is representable,
    /// but 7.4.3.1/7.4.3.2 stop at 6 — and the sub-layer arrays the parser then walks
    /// are six and seven deep. Both param sets are now a parse error, not a panic.
    /// This is also what keeps the planner's own
    /// `max_num_reorder_pics[max_sub_layers_minus1]` reads in bounds.
    #[test]
    fn a_param_set_claiming_eight_sub_layers_is_a_parse_error_not_a_panic() {
        let mut s = BitSink::new();
        s.bits(4, 0); // vps_video_parameter_set_id
        s.bit(1); // vps_base_layer_internal_flag
        s.bit(1); // vps_base_layer_available_flag
        s.bits(6, 0); // vps_max_layers_minus1
        s.bits(3, 7); // vps_max_sub_layers_minus1 — one past the spec's 6
        s.bit(1); // vps_temporal_id_nesting_flag
        s.bits(16, 0xffff); // vps_reserved_0xffff_16bits
        ptl_general(&mut s, 1, 120);
        let vps = h265_nalu(VPS_NUT, &padded(s));
        assert!(matches!(
            H265Planner::new().plan_au(&vps),
            Err(PlanError::Parse(_))
        ));

        let mut s = BitSink::new();
        s.bits(4, 0); // sps_video_parameter_set_id
        s.bits(3, 7); // sps_max_sub_layers_minus1
        s.bit(1); // sps_temporal_id_nesting_flag
        ptl_general(&mut s, 1, 120);
        let sps = h265_nalu(SPS_NUT, &padded(s));
        assert!(matches!(
            H265Planner::new().plan_au(&sps),
            Err(PlanError::Parse(_))
        ));
    }

    /// The PPS fields ahead of the two this section attacks, all zero.
    fn pps_prefix() -> BitSink {
        let mut s = BitSink::new();
        s.ue(0); // pps_pic_parameter_set_id
        s.ue(0); // pps_seq_parameter_set_id
        s.bits(2, 0); // dependent_slice_segments_enabled / output_flag_present
        s.bits(3, 0); // num_extra_slice_header_bits
        s.bits(2, 0); // sign_data_hiding_enabled / cabac_init_present
        s.ue(0); // num_ref_idx_l0_default_active_minus1
        s.ue(0); // num_ref_idx_l1_default_active_minus1
        s.se(0); // init_qp_minus26
        s.bits(3, 0); // constrained_intra_pred / transform_skip / cu_qp_delta_enabled
        s.se(0); // pps_cb_qp_offset
        s.se(0); // pps_cr_qp_offset
        s.bits(4, 0); // chroma_qp_offsets / weighted_pred / weighted_bipred / bypass
        s
    }

    /// The PPS fields after the tile block, all zero, plus rbsp_trailing_bits().
    fn pps_tail(mut s: BitSink) -> Vec<u8> {
        s.bits(2, 0); // loop_filter_across_slices / deblocking_filter_control_present
        s.bits(2, 0); // pps_scaling_list_data_present / lists_modification_present
        s.ue(0); // log2_parallel_merge_level_minus2
        s.bits(2, 0); // slice_segment_header_extension / pps_extension_present
        s.finish()
    }

    /// Deviation 10: equation 7-42 subtracts `scaling_list_pred_matrix_id_delta` from
    /// matrixId in u32. Unbounded, it underflows into an out-of-bounds read of the
    /// six-entry scaling lists; 7.4.5 caps it at matrixId / (sizeId == 3 ? 3 : 1).
    #[test]
    fn a_scaling_list_predicting_from_a_negative_matrix_is_a_parse_error_not_a_panic() {
        let mut s = pps_prefix();
        s.bits(2, 0); // tiles_enabled / entropy_coding_sync_enabled
        s.bits(2, 0); // loop_filter_across_slices / deblocking_filter_control_present
        s.bit(1); // pps_scaling_list_data_present_flag
        s.bit(0); // scaling_list_pred_mode_flag[0][0]
        s.ue(1); // scaling_list_pred_matrix_id_delta[0][0] — refMatrixId = 0 - 1
        let mut au = synth_sps(&SpsOpts::default());
        au.extend(h265_nalu(PPS_NUT, &padded(s)));
        assert!(matches!(
            H265Planner::new().plan_au(&au),
            Err(PlanError::Parse(_))
        ));
    }

    /// Deviation 11: the tile counts were bounded by the picture's CTB size only, so a
    /// wide-enough SPS let them run past the width and height arrays. Table A.8 caps
    /// them at 20 columns and 22 rows for every level.
    #[test]
    fn a_pps_with_more_tiles_than_any_level_allows_is_a_parse_error_not_a_panic() {
        // 2048x2048 luma with 64x64 CTBs: 32 CTBs each way, so the picture bound
        // alone would admit 31 tile columns and rows.
        let sps = SpsOpts {
            width: 2048,
            height: 2048,
            ..Default::default()
        };
        for (columns, rows) in [(25, 0), (0, 25)] {
            let mut s = pps_prefix();
            s.bit(1); // tiles_enabled_flag
            s.bit(0); // entropy_coding_sync_enabled_flag
            s.ue(columns); // num_tile_columns_minus1
            s.ue(rows); // num_tile_rows_minus1
            s.bit(1); // uniform_spacing_flag
            let mut au = synth_sps(&sps);
            au.extend(h265_nalu(PPS_NUT, &padded(s)));
            assert!(
                matches!(H265Planner::new().plan_au(&au), Err(PlanError::Parse(_))),
                "{columns} columns / {rows} rows must be refused"
            );
        }
    }

    /// The tile ceiling's two downstream sites, both reached from one slice header:
    /// the entry-point maximum multiplied the two tile counts in u8 (20 x 22 overflows
    /// it), and `entry_point_offset_minus1` is 32 deep however large that maximum is.
    #[test]
    fn a_slice_claiming_more_entry_points_than_the_header_holds_is_a_parse_error_not_a_panic() {
        let sps = SpsOpts {
            width: 2048,
            height: 2048,
            ..Default::default()
        };
        let mut s = pps_prefix();
        s.bit(1); // tiles_enabled_flag
        s.bit(0); // entropy_coding_sync_enabled_flag
        s.ue(19); // num_tile_columns_minus1 — Table A.8's ceiling, and legal here
        s.ue(21); // num_tile_rows_minus1
        s.bit(1); // uniform_spacing_flag
        s.bit(0); // loop_filter_across_tiles_enabled_flag
        let mut au = synth_sps(&sps);
        au.extend(h265_nalu(PPS_NUT, &pps_tail(s)));

        let mut s = BitSink::new();
        s.bit(1); // first_slice_segment_in_pic_flag
        s.bit(0); // no_output_of_prior_pics_flag
        s.ue(0); // slice_pic_parameter_set_id
        s.ue(2); // slice_type: I
        s.se(0); // slice_qp_delta
        s.ue(35); // num_entry_point_offsets — 440 tiles would allow it, 32 slots do not
        au.extend(h265_nalu(IDR_W_RADL, &padded(s)));

        assert!(matches!(
            H265Planner::new().plan_au(&au),
            Err(PlanError::Parse(_))
        ));
    }
}
