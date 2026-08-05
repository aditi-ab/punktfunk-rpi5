// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file (vendor/cros-codecs/LICENSE).
//
// Adapted from cros-codecs `decoder/stateless/h264.rs` (see
// vendor/cros-codecs/PROVENANCE.md for the snapshot pin). The spec machinery — POC
// computation (8.2.1), frame_num-gap handling (8.2.5.2), reference list initialization
// and modification (8.2.4), sliding-window and adaptive MMCO marking (8.2.5), DPB
// bumping/output (C.4.5.3) — is ported faithfully and keeps upstream's structure and
// spec-section comments so future upstream diffs stay legible. Stripped: the
// StatelessDecoder/backend trait plumbing, fd/event machinery, pooled-buffer handling,
// and the interlaced field-splitting paths (the envelope gate below rejects interlaced
// streams outright).

//! Per-AU H.264 planning: [`H264Planner::plan_au`] turns one access unit exactly as the
//! pump hands it to a decoder (Annex-B, parameter sets + the slices of one picture) into
//! an [`AuPlan`] — everything a stateless hardware decoder needs before submission and
//! nothing it has to re-derive: parsed headers, POC, per-slice reference lists (with
//! long-term/MMCO state, which host RFI recovery leans on) and the DPB delta.
//!
//! Concealment posture: a `frame_num` gap or a reference that is not in the DPB is a
//! [`PlanWarning`], never an error — the session layer sees the warning and requests
//! recovery while planning continues. [`PlanError`] is reserved for AUs that cannot be
//! planned at all.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::mem;
use std::ops::Range;
use std::rc::Rc;

use cros_codecs::codec::h264::dpb::Dpb;
use cros_codecs::codec::h264::dpb::DpbEntry;
use cros_codecs::codec::h264::dpb::DpbPicRefList;
use cros_codecs::codec::h264::dpb::MmcoError;
use cros_codecs::codec::h264::dpb::ReferencePicLists;
use cros_codecs::codec::h264::parser::MaxLongTermFrameIdx;
use cros_codecs::codec::h264::parser::Nalu;
use cros_codecs::codec::h264::parser::NaluType;
use cros_codecs::codec::h264::parser::Parser;
use cros_codecs::codec::h264::parser::Pps;
use cros_codecs::codec::h264::parser::RefPicListModification;
use cros_codecs::codec::h264::parser::Slice;
use cros_codecs::codec::h264::parser::SliceType;
use cros_codecs::codec::h264::parser::Sps;
use cros_codecs::codec::h264::picture::Field;
use cros_codecs::codec::h264::picture::FieldRank;
use cros_codecs::codec::h264::picture::IsIdr;
use cros_codecs::codec::h264::picture::PictureData;
use cros_codecs::codec::h264::picture::RcPictureData;
use cros_codecs::codec::h264::picture::Reference;
use cros_codecs::Resolution;
use tracing::trace;

pub use cros_codecs::codec::h264::parser::Level;
pub use cros_codecs::codec::h264::parser::SliceHeader;

use crate::sei;
pub use crate::sei::RecoveryPoint;

/// Stable identity of a stored picture, monotonically increasing per stored picture.
///
/// This is what backends map to hardware DPB slots. Indices into the live DPB `Vec`
/// shift on bumping and must never be exposed; the `Dpb<PicId>` handle parameter carries
/// this id instead.
pub type PicId = u64;

/// Everything a backend needs to submit one access unit.
#[derive(Debug, Clone)]
pub struct AuPlan {
    pub picture: PicturePlan,
    pub slices: Vec<SlicePlan>,
    pub dpb: DpbUpdate,
    pub warnings: Vec<PlanWarning>,
    /// The SPS the planner activated for this AU — the one [`Self::picture`]'s
    /// parameters derive from (the FIRST slice's PPS's SPS; a later slice may
    /// legally reference another PPS, and that drift deliberately does not reach
    /// here). Cloned out of the parser's table so backends build their parameter
    /// objects from exactly what was activated, never by re-parsing the AU.
    pub sps: Rc<Sps>,
    /// The PPS the picture was begun with (the first slice's), same contract as
    /// [`Self::sps`]. Its `sps` field is the same `Rc` as [`Self::sps`].
    pub pps: Rc<Pps>,
}

/// Per-picture parameters, captured after 8.2.1 POC derivation and before end-of-picture
/// marking (the values a hardware picture-parameters struct wants).
#[derive(Debug, Clone)]
pub struct PicturePlan {
    pub is_idr: bool,
    pub nal_ref_idc: u8,
    pub is_reference: bool,
    pub frame_num: u16,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    /// The final PicOrderCnt of the picture (min of top/bottom for a frame).
    pub pic_order_cnt: i32,
    pub coded_width: u32,
    pub coded_height: u32,
    /// Conformance-window crop (7.4.2.1.1), in luma samples of the coded picture.
    pub display_crop: DisplayCrop,
    /// Colour signalling from the ACTIVE SPS's VUI (E.2.1 inference where absent).
    /// Per picture, like [`Self::display_crop`], never latched at session start:
    /// the Windows host switches an HDR desktop to PQ/BT.2020 IN-BAND with a new
    /// SPS mid-stream, so a backend that captured the first AU's colour would
    /// paint HDR frames washed out.
    pub colour: ColourDescription,
    pub profile_idc: u8,
    pub level_idc: Level,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub chroma_format_idc: u8,
    /// DPB size in frames per A.3.1 — backends size their slot pool from this.
    pub max_dpb_frames: usize,
    pub recovery_point: Option<RecoveryPoint>,
}

/// The region of the coded picture that is actually displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One picture's colour signalling: raw H.273 code points off the active SPS's
/// VUI. When the VUI (or its `video_signal_type`/`colour_description` blocks) is
/// absent these hold E.2.1's INFERRED values — 2/2/2 ("unspecified") with limited
/// range — never a raw struct-zero (0 is a reserved code point no real stream
/// means). That matches the CICP libavcodec reports for such streams, so backends
/// forward these untouched and the consumer's CSC resolves "unspecified" to its
/// SDR default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourDescription {
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    /// `video_full_range_flag` (E.2.1 infers limited range when absent).
    pub video_full_range: bool,
}

/// One slice NALU of the picture, with its reference lists fully derived.
#[derive(Debug, Clone)]
pub struct SlicePlan {
    /// Byte range of the slice NALU in the input AU, start code included — hardware
    /// decoders take the raw bitstream, so the plan points instead of copying.
    pub data: Range<usize>,
    /// The parsed slice header, as the vendored parser produced it.
    pub header: SliceHeader,
    pub ref_list0: Vec<RefPic>,
    pub ref_list1: Vec<RefPic>,
}

/// A reference list entry: the minimum every backend picparams format needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub id: PicId,
    /// The stored picture's 8.2.1 field order counts. Equal for a progressive frame
    /// UNLESS the PPS set `bottom_field_pic_order_in_frame_present_flag` and the
    /// slice carried a nonzero `delta_pic_order_cnt_bottom` — backend picparams
    /// formats want the pair, and collapsing to one value would fabricate the bottom
    /// count. After an MMCO 5 these are the picture's REBASED values (8.2.5.4.5),
    /// which is what later AUs reference it by — see [`PlanWarning::Mmco5Rebase`].
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub is_long_term: bool,
    /// `frame_num` for short-term references, `LongTermFrameIdx` for long-term ones —
    /// the pair DXVA and Vulkan both key reference pictures by.
    pub frame_num_or_lt_idx: u16,
}

/// The DPB delta of one planned AU: what to allocate, what is display-ready, what can
/// be freed.
#[derive(Debug, Clone, Default)]
pub struct DpbUpdate {
    /// The id assigned to this AU's picture — allocate a surface for it.
    pub stored: Option<PicId>,
    /// Display-ready pictures, in output (bumping) order.
    pub outputs: Vec<PicId>,
    /// Pictures the planner will never reference again; free once displayed.
    pub removed: Vec<PicId>,
}

/// Concealment signals: planning continues, the session layer requests recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWarning {
    /// `frame_num` skipped a value — at least one reference AU was lost upstream.
    FrameNumGap { expected: u16, got: u16 },
    /// A slice or MMCO named a reference picture the DPB does not hold.
    MissingReference {
        context: &'static str,
        detail: String,
    },
    /// The AU's NALU walk stopped early — a malformed NALU with real data behind it,
    /// or a slice belonging to another picture (mis-split AU). The plan covers only
    /// the slices before the cut; `offset` is the byte position of the cut in the AU.
    TruncatedAu { offset: usize },
    /// The AU carried an MMCO 5 (8.2.5.4.5): the DPB was drained and the CURRENT
    /// picture's stored frame_num/POC were rebased to zero AFTER its plan was
    /// captured. Spec-legal and fully planned — the [`PicturePlan`] holds the
    /// pre-rebase 8.2.1 values a decoder submits with, while later AUs reference the
    /// picture by its rebased values ([`RefPic`] carries the stored pair). punktfunk
    /// hosts never emit MMCO 5, so this warning is the field signal if that
    /// assumption ever breaks.
    Mmco5Rebase,
}

/// The AU cannot be planned at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Parse(String),
    /// Legal H.264, but outside what punktfunk hosts emit (clients only decode
    /// punktfunk hosts, so this is a stream-integrity failure, not a feature gap).
    OutsideEnvelope(&'static str),
    NoActiveParamSet {
        pps_id: u8,
    },
    /// [`H264Planner::flush`] discarded the decoding state; planning resumes only at
    /// an IDR (the port of upstream's `Reset` gating).
    AwaitingIdr,
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
                write!(f, "flushed: waiting for an IDR to resume planning")
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Keeps track of the last values seen for negotiation purposes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NegotiationInfo {
    coded_resolution: Resolution,
    profile_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    chroma_format_idc: u8,
    max_dpb_frames: usize,
    interlaced: bool,
}

impl From<&Sps> for NegotiationInfo {
    fn from(sps: &Sps) -> Self {
        NegotiationInfo {
            coded_resolution: Resolution::from((sps.width(), sps.height())),
            profile_idc: sps.profile_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: sps.max_dpb_frames(),
            interlaced: !sps.frame_mbs_only_flag,
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum RefPicList {
    RefPicList0,
    RefPicList1,
}

/// Cached variables from the previous reference picture (8.2.1).
struct PrevReferencePicInfo {
    frame_num: u32,
    has_mmco_5: bool,
    top_field_order_cnt: i32,
    pic_order_cnt_msb: i32,
    pic_order_cnt_lsb: i32,
    field: Field,
}

impl Default for PrevReferencePicInfo {
    fn default() -> Self {
        Self {
            frame_num: Default::default(),
            has_mmco_5: Default::default(),
            top_field_order_cnt: Default::default(),
            pic_order_cnt_msb: Default::default(),
            pic_order_cnt_lsb: Default::default(),
            field: Field::Frame,
        }
    }
}

impl PrevReferencePicInfo {
    fn fill(&mut self, pic: &PictureData) {
        self.has_mmco_5 = pic.has_mmco_5;
        self.top_field_order_cnt = pic.top_field_order_cnt;
        self.pic_order_cnt_msb = pic.pic_order_cnt_msb;
        self.pic_order_cnt_lsb = pic.pic_order_cnt_lsb;
        self.field = pic.field;
        self.frame_num = pic.frame_num;
    }
}

/// Cached variables from the previous picture (8.2.1).
#[derive(Default)]
struct PrevPicInfo {
    frame_num: u32,
    frame_num_offset: u32,
    has_mmco_5: bool,
}

impl PrevPicInfo {
    fn fill(&mut self, pic: &PictureData) {
        self.frame_num = pic.frame_num;
        self.has_mmco_5 = pic.has_mmco_5;
        self.frame_num_offset = pic.frame_num_offset;
    }
}

/// Used to track that `first_mb_in_slice` increases monotonically (7.4.3).
///
/// Upstream tracks this too, but with an inverted comparison that fires on every
/// well-formed slice; corrected here (strictly increasing across a picture's slices),
/// and `None`/vacant marks "no slice seen yet" so the first slice never trips it.
enum CurrentMacroblockTracking {
    SeparateColorPlane(BTreeMap<u8, u32>),
    NonSeparateColorPlane(Option<u32>),
}

/// State of the picture being planned, spanning the slices of one AU.
struct CurrentPicState {
    /// Data for the current picture as extracted from the stream.
    pic: PictureData,
    /// PPS at the time of the current picture. Follows the slices — a later slice may
    /// reference another PPS — and feeds end-of-picture marking, as upstream does.
    pps: Rc<Pps>,
    /// The PPS the picture was BEGUN with. [`H264Planner::picture_plan`] reads this
    /// snapshot, like upstream's `start_picture`, so per-picture parameters cannot
    /// drift to a later slice's PPS.
    first_slice_pps: Rc<Pps>,
    /// The id backends will know this picture by (upstream: the backend picture).
    id: PicId,
    /// Reference picture lists, derived once per picture, indexed per slice.
    ref_pic_lists: ReferencePicLists,
    current_macroblock: CurrentMacroblockTracking,
}

/// Plans H.264 access units for stateless hardware decoders.
///
/// Owns the vendored parser and DPB plus the POC/marking state that upstream keeps in
/// `H264DecoderState`. One instance per elementary stream; feed AUs in decode order.
#[derive(Default)]
pub struct H264Planner {
    parser: Parser,
    negotiation_info: NegotiationInfo,
    dpb: Dpb<PicId>,
    prev_ref_pic_info: PrevReferencePicInfo,
    prev_pic_info: PrevPicInfo,
    max_long_term_frame_idx: MaxLongTermFrameIdx,
    /// Next [`PicId`] to hand out (upstream: the backend allocates here).
    next_pic_id: PicId,
    /// Display-ready pictures accumulated while planning (upstream: the decoder's
    /// ready queue). Not cleared on a failed AU — the next emitted [`DpbUpdate`]
    /// carries them, so an error can never swallow a frame.
    pending_outputs: Vec<PicId>,
    /// Ids the last emitted [`DpbUpdate`] left alive: the baseline for `removed`.
    /// Kept across failed AUs so interim evictions are reported, never dropped.
    reported_live: BTreeSet<PicId>,
    /// Set by [`Self::flush`]: planning resumes only at an IDR (upstream: `Reset`).
    awaiting_idr: bool,
}

impl H264Planner {
    pub fn new() -> Self {
        Default::default()
    }

    /// Plan one access unit: Annex-B bytes containing SPS/PPS/SEI/AUD NALUs plus the
    /// 1..N slice NALUs of exactly one picture.
    ///
    /// After a [`PlanError`] the planner state is best-effort; the session should
    /// request an IDR before feeding more AUs. Outputs and removals queued by a failed
    /// AU are retained and emitted with the next successful plan (or [`Self::flush`]) —
    /// never discarded.
    pub fn plan_au(&mut self, au: &[u8]) -> Result<AuPlan, PlanError> {
        let mut warnings = Vec::new();
        let mut slices = Vec::new();
        let mut recovery_point = None;
        let mut current: Option<CurrentPicState> = None;
        let mut saw_nalu = false;

        let mut cursor = Cursor::new(au);
        loop {
            let nalu = match Nalu::next(&mut cursor) {
                Ok(nalu) => nalu,
                Err(_) => {
                    // End of the AU — or a NALU whose header failed to parse (reserved
                    // type, truncated byte). A start code past the cursor means real
                    // data was cut off: degrade to a concealment signal covering the
                    // slices already planned. Without one this is benign trailing
                    // padding — a NALU's own payload is emulation-prevented and cannot
                    // contain a start code.
                    let pos = (cursor.position() as usize).min(au.len());
                    if au[pos..].windows(3).any(|w| w == [0x00, 0x00, 0x01]) {
                        warnings.push(PlanWarning::TruncatedAu { offset: pos });
                    }
                    break;
                }
            };
            saw_nalu = true;
            // After `Nalu::next` the cursor sits on the NAL header byte; `offset` is the
            // start-code length and `size` the NALU payload length, which pins the
            // NALU's absolute byte range in the AU without copying.
            let nalu_offset = cursor.position() as usize;
            let range = (nalu_offset - nalu.offset)..(nalu_offset + nalu.size);
            debug_assert_eq!(&au[range.clone()], nalu.data.as_ref());

            match nalu.header.type_ {
                NaluType::Sps => {
                    let sps = self.parser.parse_sps(&nalu).map_err(PlanError::Parse)?;
                    Self::check_envelope(sps)?;
                }
                NaluType::Pps => {
                    self.parser.parse_pps(&nalu).map_err(PlanError::Parse)?;
                }
                NaluType::Sei => {
                    match sei::parse_recovery_point(nalu.as_ref().get(1..).unwrap_or(&[])) {
                        Ok(Some(rp)) => recovery_point = Some(rp),
                        Ok(None) => {}
                        // A broken SEI must not cost the picture it decorates.
                        Err(err) => trace!("ignoring unparseable SEI NALU: {err}"),
                    }
                }
                NaluType::Slice | NaluType::SliceIdr => {
                    // Upstream's `Reset` gating: after a flush, only an IDR restarts
                    // the decoding process.
                    if current.is_none() && self.awaiting_idr {
                        if !nalu.header.idr_pic_flag {
                            return Err(PlanError::AwaitingIdr);
                        }
                        self.awaiting_idr = false;
                    }
                    let slice = match self.parser.parse_slice_header(nalu) {
                        Ok(slice) => slice,
                        Err(err) => return Err(Self::slice_parse_error(err)),
                    };
                    match &current {
                        None => current = Some(self.begin_picture(&slice, &mut warnings)?),
                        // Upstream would finish the picture and begin another; our
                        // contract is one picture per AU, so a second first-slice means
                        // the pump upstream of us is broken.
                        Some(_) if slice.header.first_mb_in_slice == 0 => {
                            return Err(PlanError::OutsideEnvelope(
                                "more than one coded picture in one access unit",
                            ));
                        }
                        Some(cur) => {
                            // Mis-split-AU guard: a continuation slice must belong to
                            // the picture the first slice began (7.4.3: same frame_num,
                            // same IDR-ness). A foreign slice and everything after it
                            // are dropped behind a concealment signal.
                            if u32::from(slice.header.frame_num) != cur.pic.frame_num
                                || slice.nalu.header.idr_pic_flag
                                    != matches!(cur.pic.is_idr, IsIdr::Yes { .. })
                            {
                                warnings.push(PlanWarning::TruncatedAu {
                                    offset: range.start,
                                });
                                break;
                            }
                        }
                    }
                    let cur = current.as_mut().expect("a picture was begun above");
                    slices.push(self.plan_slice(cur, slice, range, &mut warnings)?);
                }
                NaluType::SliceDpa | NaluType::SliceDpb | NaluType::SliceDpc => {
                    return Err(PlanError::OutsideEnvelope("data-partitioned slices"));
                }
                other => trace!("skipping NAL unit type {other:?}"),
            }
        }

        if !saw_nalu {
            return Err(PlanError::Parse("no NAL units in access unit".into()));
        }
        let cur = current
            .ok_or_else(|| PlanError::Parse("access unit contains no coded picture".into()))?;

        // Captured before finish_picture: MMCO5 rewrites the stored POC afterwards, but
        // backends submit the picture with its 8.2.1 values.
        let picture = Self::picture_plan(&cur, recovery_point);
        // The activated parameter sets ride out with the plan (AuPlan field docs);
        // cloned before finish_picture consumes `cur`.
        let pps = Rc::clone(&cur.first_slice_pps);
        let sps = Rc::clone(&pps.sps);
        let stored = self.finish_picture(cur, &mut warnings)?;

        // `removed` is the delta against what the backend last SAW alive, not against
        // this call's start — a failed AU in between may have evicted pictures, and
        // those removals must still be reported here.
        let live_after = self.live_ids();
        let mut previously_live = mem::take(&mut self.reported_live);
        previously_live.insert(stored);
        let removed = previously_live.difference(&live_after).copied().collect();
        self.reported_live = live_after;

        Ok(AuPlan {
            picture,
            slices,
            dpb: DpbUpdate {
                stored: Some(stored),
                outputs: mem::take(&mut self.pending_outputs),
                removed,
            },
            warnings,
            sps,
            pps,
        })
    }

    /// Drain the DPB: every still-buffered picture becomes display-ready and every id is
    /// released. The session calls this at teardown or a stream discontinuity.
    ///
    /// The 8.2.1/8.2.5 decoding state is discarded with the pictures; planning resumes
    /// only at an IDR ([`PlanError::AwaitingIdr`] until then). Parameter sets survive —
    /// per 7.4.1.2 they persist until replaced.
    pub fn flush(&mut self) -> DpbUpdate {
        let mut removed = mem::take(&mut self.reported_live);
        removed.extend(self.live_ids());
        self.drain_dpb();

        self.prev_ref_pic_info = Default::default();
        self.prev_pic_info = Default::default();
        self.max_long_term_frame_idx = Default::default();
        self.negotiation_info = Default::default();
        self.awaiting_idr = true;

        DpbUpdate {
            stored: None,
            outputs: mem::take(&mut self.pending_outputs),
            removed: removed.into_iter().collect(),
        }
    }

    /// The envelope gate: punktfunk clients only decode punktfunk hosts, and no host
    /// ever emits interlaced video or separate-colour-plane coding.
    fn check_envelope(sps: &Sps) -> Result<(), PlanError> {
        if !sps.frame_mbs_only_flag {
            return Err(PlanError::OutsideEnvelope(
                "interlaced stream (frame_mbs_only_flag == 0)",
            ));
        }
        if sps.separate_colour_plane_flag {
            return Err(PlanError::OutsideEnvelope(
                "separate colour plane coding (separate_colour_plane_flag == 1)",
            ));
        }
        // A.3.1 caps the DPB at 16 frames; the only route past the cap is the VUI's
        // max_dec_frame_buffering, an unbounded ue(v) the vendored parser reads
        // uncapped. No hardware decoder implements a deeper DPB — a larger value is a
        // corrupt (or hostile) VUI, not a feature request — and backends size real
        // slot pools from this number, so it is gated here, at SPS activation.
        if sps.max_dpb_frames() > 16 {
            return Err(PlanError::OutsideEnvelope(
                "DPB deeper than 16 frames (max_dec_frame_buffering)",
            ));
        }
        Ok(())
    }

    /// Map a vendored slice-header parse failure, sniffing the missing-PPS message so it
    /// surfaces as [`PlanError::NoActiveParamSet`]. The prefix match is best-effort: if
    /// an upstream re-sync rewords it, the error degrades to `Parse`, not silence.
    fn slice_parse_error(err: String) -> PlanError {
        match err.strip_prefix("Could not get PPS for pic_parameter_set_id ") {
            Some(id) => PlanError::NoActiveParamSet {
                pps_id: id.trim().parse().unwrap_or(0),
            },
            None => PlanError::Parse(err),
        }
    }

    /// Ids of every picture the DPB currently holds (non-existing gap placeholders carry
    /// no id and are invisible to backends by design).
    fn live_ids(&self) -> BTreeSet<PicId> {
        self.dpb
            .entries()
            .iter()
            .filter_map(|entry| entry.reference)
            .collect()
    }

    fn compute_pic_order_count(
        &mut self,
        pic: &mut PictureData,
        sps: &Sps,
    ) -> Result<(), PlanError> {
        match pic.pic_order_cnt_type {
            // Spec 8.2.1.1
            0 => {
                let prev_pic_order_cnt_msb;
                let prev_pic_order_cnt_lsb;

                if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    prev_pic_order_cnt_lsb = 0;
                    prev_pic_order_cnt_msb = 0;
                } else if self.prev_ref_pic_info.has_mmco_5 {
                    if !matches!(self.prev_ref_pic_info.field, Field::Bottom) {
                        prev_pic_order_cnt_msb = 0;
                        prev_pic_order_cnt_lsb = self.prev_ref_pic_info.top_field_order_cnt;
                    } else {
                        prev_pic_order_cnt_msb = 0;
                        prev_pic_order_cnt_lsb = 0;
                    }
                } else {
                    prev_pic_order_cnt_msb = self.prev_ref_pic_info.pic_order_cnt_msb;
                    prev_pic_order_cnt_lsb = self.prev_ref_pic_info.pic_order_cnt_lsb;
                }

                let max_pic_order_cnt_lsb = 1 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);

                // 8.2.1.1 compares against prevPicOrderCntLsb — the DERIVED value,
                // which is 0 or the previous TopFieldOrderCnt after an MMCO5 — in BOTH
                // wrap branches. Upstream reads the raw stored lsb in the first branch;
                // deliberate divergence from upstream here, in favour of spec
                // conformance.
                pic.pic_order_cnt_msb = if (pic.pic_order_cnt_lsb < prev_pic_order_cnt_lsb)
                    && (prev_pic_order_cnt_lsb - pic.pic_order_cnt_lsb >= max_pic_order_cnt_lsb / 2)
                {
                    prev_pic_order_cnt_msb + max_pic_order_cnt_lsb
                } else if (pic.pic_order_cnt_lsb > prev_pic_order_cnt_lsb)
                    && (pic.pic_order_cnt_lsb - prev_pic_order_cnt_lsb > max_pic_order_cnt_lsb / 2)
                {
                    prev_pic_order_cnt_msb - max_pic_order_cnt_lsb
                } else {
                    prev_pic_order_cnt_msb
                };

                if !matches!(pic.field, Field::Bottom) {
                    pic.top_field_order_cnt = pic.pic_order_cnt_msb + pic.pic_order_cnt_lsb;
                }

                if !matches!(pic.field, Field::Top) {
                    if matches!(pic.field, Field::Frame) {
                        pic.bottom_field_order_cnt =
                            pic.top_field_order_cnt + pic.delta_pic_order_cnt_bottom;
                    } else {
                        pic.bottom_field_order_cnt = pic.pic_order_cnt_msb + pic.pic_order_cnt_lsb;
                    }
                }
            }

            // Spec 8.2.1.2
            1 => {
                if self.prev_pic_info.has_mmco_5 {
                    self.prev_pic_info.frame_num_offset = 0;
                }

                if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    pic.frame_num_offset = 0;
                } else if self.prev_pic_info.frame_num > pic.frame_num {
                    pic.frame_num_offset =
                        self.prev_pic_info.frame_num_offset + sps.max_frame_num();
                } else {
                    pic.frame_num_offset = self.prev_pic_info.frame_num_offset;
                }

                let mut abs_frame_num = if sps.num_ref_frames_in_pic_order_cnt_cycle != 0 {
                    pic.frame_num_offset + pic.frame_num
                } else {
                    0
                };

                if pic.nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }

                let mut expected_pic_order_cnt = 0;

                if abs_frame_num > 0 {
                    if sps.num_ref_frames_in_pic_order_cnt_cycle == 0 {
                        return Err(PlanError::Parse(
                            "invalid num_ref_frames_in_pic_order_cnt_cycle".into(),
                        ));
                    }

                    let pic_order_cnt_cycle_cnt =
                        (abs_frame_num - 1) / sps.num_ref_frames_in_pic_order_cnt_cycle as u32;
                    let frame_num_in_pic_order_cnt_cycle =
                        (abs_frame_num - 1) % sps.num_ref_frames_in_pic_order_cnt_cycle as u32;
                    expected_pic_order_cnt =
                        pic_order_cnt_cycle_cnt as i32 * sps.expected_delta_per_pic_order_cnt_cycle;

                    assert!(frame_num_in_pic_order_cnt_cycle < 255);

                    // NOTE: upstream sums the full cycle here where 8.2.1.2 sums
                    // frame_num_in_pic_order_cnt_cycle + 1 entries; ported as-is —
                    // punktfunk hosts emit pic_order_cnt_type 0 only.
                    let cycle = usize::from(sps.num_ref_frames_in_pic_order_cnt_cycle);
                    for offset in &sps.offset_for_ref_frame[..cycle] {
                        expected_pic_order_cnt += offset;
                    }
                }

                if pic.nal_ref_idc == 0 {
                    expected_pic_order_cnt += sps.offset_for_non_ref_pic;
                }

                if matches!(pic.field, Field::Frame) {
                    pic.top_field_order_cnt = expected_pic_order_cnt + pic.delta_pic_order_cnt0;

                    pic.bottom_field_order_cnt = pic.top_field_order_cnt
                        + sps.offset_for_top_to_bottom_field
                        + pic.delta_pic_order_cnt1;
                } else if !matches!(pic.field, Field::Bottom) {
                    pic.top_field_order_cnt = expected_pic_order_cnt + pic.delta_pic_order_cnt0;
                } else {
                    pic.bottom_field_order_cnt = expected_pic_order_cnt
                        + sps.offset_for_top_to_bottom_field
                        + pic.delta_pic_order_cnt0;
                }
            }

            // Spec 8.2.1.3
            2 => {
                if self.prev_pic_info.has_mmco_5 {
                    self.prev_pic_info.frame_num_offset = 0;
                }

                if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    pic.frame_num_offset = 0;
                } else if self.prev_pic_info.frame_num > pic.frame_num {
                    pic.frame_num_offset =
                        self.prev_pic_info.frame_num_offset + sps.max_frame_num();
                } else {
                    pic.frame_num_offset = self.prev_pic_info.frame_num_offset;
                }

                let pic_order_cnt = if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    0
                } else if pic.nal_ref_idc == 0 {
                    2 * (pic.frame_num_offset + pic.frame_num) as i32 - 1
                } else {
                    2 * (pic.frame_num_offset + pic.frame_num) as i32
                };

                if matches!(pic.field, Field::Frame | Field::Top) {
                    pic.top_field_order_cnt = pic_order_cnt;
                }
                if matches!(pic.field, Field::Frame | Field::Bottom) {
                    pic.bottom_field_order_cnt = pic_order_cnt;
                }
            }

            _ => {
                return Err(PlanError::Parse(format!(
                    "invalid pic_order_cnt_type: {}",
                    sps.pic_order_cnt_type
                )))
            }
        }

        match pic.field {
            Field::Frame => {
                pic.pic_order_cnt =
                    std::cmp::min(pic.top_field_order_cnt, pic.bottom_field_order_cnt);
            }
            Field::Top => {
                pic.pic_order_cnt = pic.top_field_order_cnt;
            }
            Field::Bottom => {
                pic.pic_order_cnt = pic.bottom_field_order_cnt;
            }
        }

        Ok(())
    }

    /// Queue the frames that the C.4.5.3 bumping process declares ready for output.
    fn bump_as_needed(&mut self, current_pic: &PictureData) {
        let bumped = self.dpb.bump_as_needed(current_pic);
        self.pending_outputs.extend(bumped.into_iter().flatten());
    }

    /// Queue all frames still present in the DPB for output.
    fn drain_dpb(&mut self) {
        let pics = self.dpb.drain();
        self.pending_outputs.extend(pics.into_iter().flatten());
    }

    /// Find the first field for the picture started by `hdr`, if any. Always `None`
    /// under the envelope gate (the DPB never enters interlaced mode); kept as ported so
    /// the upstream diff stays small.
    fn find_first_field(
        &self,
        hdr: &SliceHeader,
    ) -> Result<Option<(RcPictureData, PicId)>, String> {
        let mut prev_field = None;

        if self.dpb.interlaced() {
            if let Some(last_dpb_entry) = self.dpb.entries().last() {
                // Use the last entry in the DPB
                let last_pic = last_dpb_entry.pic.borrow();

                // If the picture is interlaced but doesn't have its other field set yet,
                // then it must be the first field.
                if !matches!(last_pic.field, Field::Frame)
                    && matches!(last_pic.field_rank(), FieldRank::Single)
                {
                    if let Some(id) = &last_dpb_entry.reference {
                        // Still waiting for the second field
                        prev_field = Some((last_dpb_entry.pic.clone(), *id));
                    }
                }
            }
        }

        let prev_field = match prev_field {
            None => return Ok(None),
            Some(prev_field) => prev_field,
        };

        let prev_field_pic = prev_field.0.borrow();

        if prev_field_pic.frame_num != u32::from(hdr.frame_num) {
            return Err(format!(
                "the previous field's frame_num value {} differs from the current one's {}",
                prev_field_pic.frame_num, hdr.frame_num
            ));
        }

        let cur_field = if hdr.bottom_field_flag {
            Field::Bottom
        } else {
            Field::Top
        };

        if !hdr.field_pic_flag || cur_field == prev_field_pic.field {
            let field = prev_field_pic.field;
            return Err(format!(
                "expected complementary field {:?}, got {:?}",
                field.opposite(),
                field
            ));
        }

        drop(prev_field_pic);
        Ok(Some(prev_field))
    }

    // 8.2.4.3.1 Modification process of reference picture lists for short-term
    // reference pictures
    #[allow(clippy::too_many_arguments)]
    fn short_term_pic_list_modification<'a>(
        cur_pic: &PictureData,
        dpb: &'a Dpb<PicId>,
        ref_pic_list_x: &mut DpbPicRefList<'a, PicId>,
        num_ref_idx_lx_active_minus1: u8,
        max_pic_num: i32,
        rplm: &RefPicListModification,
        pic_num_lx_pred: &mut i32,
        ref_idx_lx: &mut usize,
    ) -> Result<(), String> {
        let pic_num_lx_no_wrap;
        let abs_diff_pic_num = rplm.abs_diff_pic_num_minus1 as i32 + 1;
        let modification_of_pic_nums_idc = rplm.modification_of_pic_nums_idc;

        if modification_of_pic_nums_idc == 0 {
            if *pic_num_lx_pred - abs_diff_pic_num < 0 {
                pic_num_lx_no_wrap = *pic_num_lx_pred - abs_diff_pic_num + max_pic_num;
            } else {
                pic_num_lx_no_wrap = *pic_num_lx_pred - abs_diff_pic_num;
            }
        } else if modification_of_pic_nums_idc == 1 {
            if *pic_num_lx_pred + abs_diff_pic_num >= max_pic_num {
                pic_num_lx_no_wrap = *pic_num_lx_pred + abs_diff_pic_num - max_pic_num;
            } else {
                pic_num_lx_no_wrap = *pic_num_lx_pred + abs_diff_pic_num;
            }
        } else {
            return Err(format!(
                "unexpected value for modification_of_pic_nums_idc {modification_of_pic_nums_idc:?}"
            ));
        }

        *pic_num_lx_pred = pic_num_lx_no_wrap;

        let pic_num_lx = if pic_num_lx_no_wrap > cur_pic.pic_num {
            pic_num_lx_no_wrap - max_pic_num
        } else {
            pic_num_lx_no_wrap
        };

        let handle = dpb
            .find_short_term_with_pic_num(pic_num_lx)
            .ok_or_else(|| format!("no ShortTerm reference found with pic_num {pic_num_lx}"))?;

        if *ref_idx_lx >= ref_pic_list_x.len() {
            return Err("invalid ref_idx_lx index".into());
        }
        ref_pic_list_x.insert(*ref_idx_lx, handle);
        *ref_idx_lx += 1;

        let mut nidx = *ref_idx_lx;

        for cidx in *ref_idx_lx..=usize::from(num_ref_idx_lx_active_minus1) + 1 {
            if cidx == ref_pic_list_x.len() {
                break;
            }

            let target = &ref_pic_list_x[cidx].pic;

            if target.borrow().pic_num_f(max_pic_num) != pic_num_lx {
                ref_pic_list_x[nidx] = ref_pic_list_x[cidx];
                nidx += 1;
            }
        }

        while ref_pic_list_x.len() > (usize::from(num_ref_idx_lx_active_minus1) + 1) {
            ref_pic_list_x.pop();
        }

        Ok(())
    }

    // 8.2.4.3.2 Modification process of reference picture lists for long-term
    // reference pictures
    fn long_term_pic_list_modification<'a>(
        dpb: &'a Dpb<PicId>,
        ref_pic_list_x: &mut DpbPicRefList<'a, PicId>,
        num_ref_idx_lx_active_minus1: u8,
        max_long_term_frame_idx: MaxLongTermFrameIdx,
        rplm: &RefPicListModification,
        ref_idx_lx: &mut usize,
    ) -> Result<(), String> {
        let long_term_pic_num = rplm.long_term_pic_num;

        let handle = dpb
            .find_long_term_with_long_term_pic_num(long_term_pic_num)
            .ok_or_else(|| {
                format!("no LongTerm reference found with long_term_pic_num {long_term_pic_num}")
            })?;

        if *ref_idx_lx >= ref_pic_list_x.len() {
            return Err("invalid ref_idx_lx index".into());
        }
        ref_pic_list_x.insert(*ref_idx_lx, handle);
        *ref_idx_lx += 1;

        let mut nidx = *ref_idx_lx;

        for cidx in *ref_idx_lx..=usize::from(num_ref_idx_lx_active_minus1) + 1 {
            if cidx == ref_pic_list_x.len() {
                break;
            }

            let target = &ref_pic_list_x[cidx].pic;
            if target.borrow().long_term_pic_num_f(max_long_term_frame_idx) != long_term_pic_num {
                ref_pic_list_x[nidx] = ref_pic_list_x[cidx];
                nidx += 1;
            }
        }

        while ref_pic_list_x.len() > (usize::from(num_ref_idx_lx_active_minus1) + 1) {
            ref_pic_list_x.pop();
        }

        Ok(())
    }

    fn modify_ref_pic_list(
        &self,
        cur_pic: &PictureData,
        hdr: &SliceHeader,
        ref_pic_list_type: RefPicList,
        ref_pic_list_indices: &[usize],
    ) -> Result<DpbPicRefList<'_, PicId>, String> {
        let (ref_pic_list_modification_flag_lx, num_ref_idx_lx_active_minus1, rplm) =
            match ref_pic_list_type {
                RefPicList::RefPicList0 => (
                    hdr.ref_pic_list_modification_flag_l0,
                    hdr.num_ref_idx_l0_active_minus1,
                    &hdr.ref_pic_list_modification_l0,
                ),
                RefPicList::RefPicList1 => (
                    hdr.ref_pic_list_modification_flag_l1,
                    hdr.num_ref_idx_l1_active_minus1,
                    &hdr.ref_pic_list_modification_l1,
                ),
            };

        let mut ref_pic_list: Vec<_> = ref_pic_list_indices
            .iter()
            .map(|&i| &self.dpb.entries()[i])
            .take(usize::from(num_ref_idx_lx_active_minus1) + 1)
            .collect();

        if !ref_pic_list_modification_flag_lx {
            return Ok(ref_pic_list);
        }

        let mut pic_num_lx_pred = cur_pic.pic_num;
        let mut ref_idx_lx = 0;

        for modification in rplm {
            let idc = modification.modification_of_pic_nums_idc;

            match idc {
                0 | 1 => {
                    Self::short_term_pic_list_modification(
                        cur_pic,
                        &self.dpb,
                        &mut ref_pic_list,
                        num_ref_idx_lx_active_minus1,
                        hdr.max_pic_num as i32,
                        modification,
                        &mut pic_num_lx_pred,
                        &mut ref_idx_lx,
                    )?;
                }
                2 => Self::long_term_pic_list_modification(
                    &self.dpb,
                    &mut ref_pic_list,
                    num_ref_idx_lx_active_minus1,
                    self.max_long_term_frame_idx,
                    modification,
                    &mut ref_idx_lx,
                )?,
                3 => break,
                _ => return Err(format!("unexpected modification_of_pic_nums_idc {idc:?}")),
            }
        }

        Ok(ref_pic_list)
    }

    /// [`Self::modify_ref_pic_list`], degrading a failed modification to a
    /// [`PlanWarning::MissingReference`] plus the unmodified 8.2.4.2 initial list —
    /// upstream aborts the decode here, but a modification naming a lost picture is
    /// punktfunk's cue to conceal and request recovery, not to kill the session.
    fn modified_or_initial_list(
        &self,
        cur_pic: &PictureData,
        hdr: &SliceHeader,
        ref_pic_list_type: RefPicList,
        ref_pic_list_indices: &[usize],
        warnings: &mut Vec<PlanWarning>,
    ) -> DpbPicRefList<'_, PicId> {
        match self.modify_ref_pic_list(cur_pic, hdr, ref_pic_list_type, ref_pic_list_indices) {
            Ok(list) => list,
            Err(detail) => {
                warnings.push(PlanWarning::MissingReference {
                    context: "ref_pic_list_modification",
                    detail,
                });
                let num_ref_idx_lx_active_minus1 = match ref_pic_list_type {
                    RefPicList::RefPicList0 => hdr.num_ref_idx_l0_active_minus1,
                    RefPicList::RefPicList1 => hdr.num_ref_idx_l1_active_minus1,
                };
                ref_pic_list_indices
                    .iter()
                    .map(|&i| &self.dpb.entries()[i])
                    .take(usize::from(num_ref_idx_lx_active_minus1) + 1)
                    .collect()
            }
        }
    }

    /// Generate RefPicList0 and RefPicList1 for one slice (8.2.4), already converted to
    /// backend-facing [`RefPic`]s.
    fn create_ref_pic_lists(
        &self,
        cur_pic: &PictureData,
        hdr: &SliceHeader,
        ref_pic_lists: &ReferencePicLists,
        warnings: &mut Vec<PlanWarning>,
    ) -> (Vec<RefPic>, Vec<RefPic>) {
        let ref_pic_list0 = match hdr.slice_type {
            SliceType::P | SliceType::Sp => self.modified_or_initial_list(
                cur_pic,
                hdr,
                RefPicList::RefPicList0,
                &ref_pic_lists.ref_pic_list_p0,
                warnings,
            ),
            SliceType::B => self.modified_or_initial_list(
                cur_pic,
                hdr,
                RefPicList::RefPicList0,
                &ref_pic_lists.ref_pic_list_b0,
                warnings,
            ),
            _ => Vec::new(),
        };

        let ref_pic_list1 = match hdr.slice_type {
            SliceType::B => self.modified_or_initial_list(
                cur_pic,
                hdr,
                RefPicList::RefPicList1,
                &ref_pic_lists.ref_pic_list_b1,
                warnings,
            ),
            _ => Vec::new(),
        };

        (
            Self::to_ref_pics(&ref_pic_list0, warnings),
            Self::to_ref_pics(&ref_pic_list1, warnings),
        )
    }

    /// Convert one reference list to backend-facing [`RefPic`]s, preserving list
    /// positions: every ref_idx in the slice syntax indexes the returned Vec 1:1.
    ///
    /// A non-existing picture (8.2.5.2 gap placeholder) has no id a backend could
    /// resolve, so it is substituted IN PLACE by the nearest existing reference in
    /// list order (the previous existing entry, else the first existing one) —
    /// stable-but-wrong concealment, flagged via [`PlanWarning::MissingReference`].
    /// Compacting instead would shift every subsequent ref_idx and make the decoder
    /// predict from the wrong pictures. Only a list with no existing reference at all
    /// collapses to empty (the caller warns on that separately).
    fn to_ref_pics(list: &[&DpbEntry<PicId>], warnings: &mut Vec<PlanWarning>) -> Vec<RefPic> {
        // Each slot: the resolvable entry plus its picture's frame_num (a long-term
        // substitute is re-labelled short-term, so its frame_num is needed).
        let mut slots: Vec<Option<(RefPic, u16)>> = Vec::with_capacity(list.len());
        for entry in list {
            let pic = entry.pic.borrow();
            match entry.reference {
                Some(id) => {
                    let is_long_term = matches!(pic.reference(), Reference::LongTerm);
                    let frame_num_or_lt_idx = if is_long_term {
                        // long_term_frame_idx is ue(v)-coded; the spec bounds it (<= 15
                        // via max_long_term_frame_idx) but the parser does not, so
                        // saturate rather than truncate — unreachable-in-practice
                        // hardening.
                        u16::try_from(pic.long_term_frame_idx).unwrap_or(u16::MAX)
                    } else {
                        pic.frame_num as u16
                    };
                    slots.push(Some((
                        RefPic {
                            id,
                            top_field_order_cnt: pic.top_field_order_cnt,
                            bottom_field_order_cnt: pic.bottom_field_order_cnt,
                            is_long_term,
                            frame_num_or_lt_idx,
                        },
                        pic.frame_num as u16,
                    )));
                }
                None => {
                    warnings.push(PlanWarning::MissingReference {
                        context: "non-existing picture (frame_num gap placeholder) in list",
                        detail: format!("frame_num {}", pic.frame_num),
                    });
                    slots.push(None);
                }
            }
        }

        let first_existing = slots.iter().flatten().next().copied();
        let mut out = Vec::with_capacity(slots.len());
        let mut prev_existing: Option<(RefPic, u16)> = None;
        for slot in &slots {
            match slot {
                Some(real) => {
                    prev_existing = Some(*real);
                    out.push(real.0);
                }
                None => {
                    // Index mapping preserved; the substituted entry is stable-but-
                    // wrong concealment. Placeholders are short-term (8.2.5.2), so the
                    // substitute is labelled short-term with its own frame_num.
                    if let Some((substitute, frame_num)) = prev_existing.or(first_existing) {
                        out.push(RefPic {
                            id: substitute.id,
                            top_field_order_cnt: substitute.top_field_order_cnt,
                            bottom_field_order_cnt: substitute.bottom_field_order_cnt,
                            is_long_term: false,
                            frame_num_or_lt_idx: frame_num,
                        });
                    }
                }
            }
        }
        out
    }

    fn handle_memory_management_ops(&mut self, pic: &mut PictureData) -> Result<(), MmcoError> {
        let markings = pic.ref_pic_marking.clone();

        for marking in &markings.inner {
            match marking.memory_management_control_operation {
                0 => break,
                1 => self.dpb.mmco_op_1(pic, marking)?,
                2 => self.dpb.mmco_op_2(pic, marking)?,
                3 => self.dpb.mmco_op_3(pic, marking)?,
                4 => self.max_long_term_frame_idx = self.dpb.mmco_op_4(marking),
                5 => self.max_long_term_frame_idx = self.dpb.mmco_op_5(pic),
                6 => self.dpb.mmco_op_6(pic, marking),
                other => return Err(MmcoError::UnknownMmco(other)),
            }
        }

        Ok(())
    }

    fn reference_pic_marking(&mut self, pic: &mut PictureData, sps: &Sps) -> Result<(), MmcoError> {
        /* 8.2.5.1 */
        if matches!(pic.is_idr, IsIdr::Yes { .. }) {
            self.dpb.mark_all_as_unused_for_ref();

            if pic.ref_pic_marking.long_term_reference_flag {
                pic.set_reference(Reference::LongTerm, false);
                pic.long_term_frame_idx = 0;
                self.max_long_term_frame_idx = MaxLongTermFrameIdx::Idx(0);
            } else {
                pic.set_reference(Reference::ShortTerm, false);
                self.max_long_term_frame_idx = MaxLongTermFrameIdx::NoLongTermFrameIndices;
            }

            return Ok(());
        }

        if pic.ref_pic_marking.adaptive_ref_pic_marking_mode_flag {
            self.handle_memory_management_ops(pic)?;
        } else {
            self.dpb.sliding_window_marking(pic, sps);
        }

        Ok(())
    }

    // Apply the parameters of `sps` to the planning state.
    fn apply_sps(&mut self, sps: &Sps) {
        self.negotiation_info = NegotiationInfo::from(sps);

        let max_dpb_frames = sps.max_dpb_frames();
        let interlaced = !sps.frame_mbs_only_flag;
        let max_num_order_frames = sps.max_num_order_frames() as usize;
        let max_num_reorder_frames = if max_num_order_frames > max_dpb_frames {
            0
        } else {
            max_num_order_frames
        };

        self.dpb.set_limits(max_dpb_frames, max_num_reorder_frames);
        self.dpb.set_interlaced(interlaced);
    }

    fn negotiation_possible(sps: &Sps, old_negotiation_info: &NegotiationInfo) -> bool {
        let negotiation_info = NegotiationInfo::from(sps);
        *old_negotiation_info != negotiation_info
    }

    fn renegotiate_if_needed(&mut self, sps: &Sps) -> Result<(), PlanError> {
        if Self::negotiation_possible(sps, &self.negotiation_info) {
            Self::check_envelope(sps)?;
            // Make sure all the frames planned so far are display-ready before the
            // stream parameters change under them.
            self.drain_dpb();
            self.apply_sps(sps);
        }

        Ok(())
    }

    fn handle_frame_num_gap(
        &mut self,
        sps: &Sps,
        frame_num: u32,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<(), PlanError> {
        if self.dpb.is_empty() {
            return Ok(());
        }

        trace!("frame_num gap detected");

        // Upstream refuses the gap when gaps_in_frame_num_value_allowed_flag is unset.
        // Here the caller has already emitted PlanWarning::FrameNumGap and the 8.2.5.2
        // process runs regardless: losing a reference AU on the wire must degrade to
        // concealment + recovery, and inserting the non-existing pictures keeps the
        // frame_num/pic_num bookkeeping of everything that follows spec-true.
        let mut unused_short_term_frame_num =
            (self.prev_ref_pic_info.frame_num + 1) % sps.max_frame_num();
        while unused_short_term_frame_num != frame_num {
            let max_frame_num = sps.max_frame_num();

            let mut pic = PictureData::new_non_existing(unused_short_term_frame_num, 0);
            self.compute_pic_order_count(&mut pic, sps)?;

            self.dpb
                .update_pic_nums(unused_short_term_frame_num, max_frame_num, &pic);

            self.dpb.sliding_window_marking(&mut pic, sps);

            self.bump_as_needed(&pic);

            // Interlaced field-splitting dropped: the envelope gate keeps the DPB in
            // progressive mode.
            if let Err(err) = self.dpb.store_picture(pic.into_rc(), None) {
                // A full DPB must not error the AU (warnings-not-errors contract):
                // stop inserting placeholders. The pic_num bookkeeping degrades from
                // here, which the recovery the warning triggers will heal.
                warnings.push(PlanWarning::MissingReference {
                    context: "frame_num gap placeholder dropped (DPB full)",
                    detail: err.to_string(),
                });
                break;
            }

            unused_short_term_frame_num += 1;
            unused_short_term_frame_num %= max_frame_num;
        }

        Ok(())
    }

    /// Init the current picture being planned.
    fn init_current_pic(
        &mut self,
        slice: &Slice,
        sps: &Sps,
        first_field: Option<&RcPictureData>,
    ) -> Result<PictureData, PlanError> {
        let mut pic = PictureData::new_from_slice(slice, sps, 0, first_field);
        self.compute_pic_order_count(&mut pic, sps)?;

        if matches!(pic.is_idr, IsIdr::Yes { .. }) {
            // C.4.5.3 "Bumping process"
            // The bumping process is invoked in the following cases:
            // Clause 2:
            // The current picture is an IDR picture and
            // no_output_of_prior_pics_flag is not equal to 1 and is not
            // inferred to be equal to 1, as specified in clause C.4.4.
            if !pic.ref_pic_marking.no_output_of_prior_pics_flag {
                self.drain_dpb();
            } else {
                // C.4.4 When no_output_of_prior_pics_flag is equal to 1 or is
                // inferred to be equal to 1, all frame buffers in the DPB are
                // emptied without output of the pictures they contain, and DPB
                // fullness is set to 0.
                self.dpb.clear();
            }
        }

        self.dpb
            .update_pic_nums(u32::from(slice.header.frame_num), sps.max_frame_num(), &pic);

        Ok(pic)
    }

    /// Called once per picture, on its first slice.
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

        // A picture's SPS may require renegotiation.
        self.renegotiate_if_needed(&pps.sps)?;

        let first_field = self.find_first_field(hdr).map_err(PlanError::Parse)?;

        // Upstream secures the backend picture here; the plan's equivalent is the id
        // backends will allocate against.
        let id = self.next_pic_id;
        self.next_pic_id += 1;

        if slice.nalu.header.idr_pic_flag {
            self.prev_ref_pic_info.frame_num = 0;
        }

        let frame_num = u32::from(hdr.frame_num);

        let current_macroblock = match pps.sps.separate_colour_plane_flag {
            true => CurrentMacroblockTracking::SeparateColorPlane(Default::default()),
            false => CurrentMacroblockTracking::NonSeparateColorPlane(None),
        };

        if frame_num != self.prev_ref_pic_info.frame_num
            && frame_num != (self.prev_ref_pic_info.frame_num + 1) % pps.sps.max_frame_num()
        {
            if !self.dpb.is_empty() {
                warnings.push(PlanWarning::FrameNumGap {
                    expected: ((self.prev_ref_pic_info.frame_num + 1) % pps.sps.max_frame_num())
                        as u16,
                    got: hdr.frame_num,
                });
            }
            self.handle_frame_num_gap(&pps.sps, frame_num, warnings)?;
        }

        let pic = self.init_current_pic(slice, &pps.sps, first_field.as_ref().map(|f| &f.0))?;
        let ref_pic_lists = self.dpb.build_ref_pic_lists(&pic);

        Ok(CurrentPicState {
            pic,
            first_slice_pps: Rc::clone(&pps),
            pps,
            id,
            ref_pic_lists,
            current_macroblock,
        })
    }

    // Check whether first_mb_in_slice increases monotonically for the current
    // picture as required by 7.4.3.
    fn check_first_mb_in_slice(current_macroblock: &mut CurrentMacroblockTracking, slice: &Slice) {
        let first_mb_in_slice = slice.header.first_mb_in_slice;
        match current_macroblock {
            CurrentMacroblockTracking::SeparateColorPlane(current_macroblock) => {
                match current_macroblock.entry(slice.header.colour_plane_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(first_mb_in_slice);
                    }
                    Entry::Occupied(mut entry) => {
                        let current_macroblock = entry.get_mut();
                        if first_mb_in_slice <= *current_macroblock {
                            trace!(
                                "first_mb_in_slice does not increase monotonically, expect \
                                 corrupted output"
                            );
                        }
                        *current_macroblock = first_mb_in_slice;
                    }
                }
            }
            CurrentMacroblockTracking::NonSeparateColorPlane(current_macroblock) => {
                if current_macroblock.is_some_and(|current| first_mb_in_slice <= current) {
                    trace!(
                        "first_mb_in_slice does not increase monotonically, expect corrupted \
                         output"
                    );
                }
                *current_macroblock = Some(first_mb_in_slice);
            }
        }
    }

    /// Handle one slice of the current picture (upstream: `handle_slice`).
    fn plan_slice(
        &self,
        cur: &mut CurrentPicState,
        slice: Slice,
        data: Range<usize>,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<SlicePlan, PlanError> {
        Self::check_first_mb_in_slice(&mut cur.current_macroblock, &slice);

        // A slice can technically refer to another PPS.
        let pps = self
            .parser
            .get_pps(slice.header.pic_parameter_set_id)
            .ok_or(PlanError::NoActiveParamSet {
                pps_id: slice.header.pic_parameter_set_id,
            })?;
        cur.pps = Rc::clone(pps);

        // Make sure that no negotiation is possible mid-picture. How could it?
        // We'd lose the context of the previous slices.
        if Self::negotiation_possible(&cur.pps.sps, &self.negotiation_info) {
            return Err(PlanError::Parse(
                "invalid stream: inter-picture renegotiation requested".into(),
            ));
        }

        let (ref_list0, ref_list1) =
            self.create_ref_pic_lists(&cur.pic, &slice.header, &cur.ref_pic_lists, warnings);

        // 8.2.4.2.1: an inter slice shall have at least one usable reference. Ending up
        // empty (every candidate lost or a gap placeholder) is undecodable-as-intended,
        // and it can happen without any per-entry warning when the DPB holds only
        // non-existing pictures — flag it so the session requests recovery.
        let slice_type = slice.header.slice_type;
        if matches!(slice_type, SliceType::P | SliceType::Sp | SliceType::B) && ref_list0.is_empty()
        {
            warnings.push(PlanWarning::MissingReference {
                context: "inter slice with no usable RefPicList0",
                detail: format!("slice_type {slice_type:?}"),
            });
        }
        if matches!(slice_type, SliceType::B) && ref_list1.is_empty() {
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

    /// Adds the picture to the output queue when it could not be added to the DPB.
    fn add_to_ready_queue(&mut self, pic: PictureData, id: PicId) {
        if matches!(pic.field, Field::Frame) {
            self.pending_outputs.push(id);
        } else if let FieldRank::Second(..) = pic.field_rank() {
            self.pending_outputs.push(id);
        }
    }

    fn finish_picture(
        &mut self,
        cur: CurrentPicState,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<PicId, PlanError> {
        let CurrentPicState {
            mut pic, pps, id, ..
        } = cur;

        if matches!(pic.reference(), Reference::ShortTerm | Reference::LongTerm) {
            // Upstream aborts on a failed MMCO; an op naming a picture the DPB lost is
            // a concealment signal here, and the remaining marking state stays usable.
            if let Err(err) = self.reference_pic_marking(&mut pic, &pps.sps) {
                warnings.push(PlanWarning::MissingReference {
                    context: "reference picture marking (MMCO)",
                    detail: err.to_string(),
                });
            }
            self.prev_ref_pic_info.fill(&pic);
        }

        self.prev_pic_info.fill(&pic);

        if pic.has_mmco_5 {
            warnings.push(PlanWarning::Mmco5Rebase);
            // C.4.5.3 "Bumping process"
            // The bumping process is invoked in the following cases:
            // Clause 3:
            // The current picture has memory_management_control_operation equal
            // to 5, as specified in clause C.4.4.
            self.drain_dpb();
        }

        // Bump the DPB as per C.4.5.3 to cover clauses 1, 4, 5 and 6.
        self.bump_as_needed(&pic);

        // C.4.5.1, C.4.5.2
        // If the current decoded picture is the second field of a complementary
        // reference field pair, add to DPB.
        // C.4.5.1
        // For a reference decoded picture, the "bumping" process is invoked
        // repeatedly until there is an empty frame buffer, by which point it is
        // added to the DPB. Notice that Dpb::needs_bumping already accounts for
        // this.
        // C.4.5.2
        // For a non-reference decoded picture, if there is empty frame buffer
        // after bumping the smaller POC, add to DPB. Otherwise, add it to the
        // output queue.
        if pic.is_second_field_of_complementary_ref_pair()
            || pic.is_ref()
            || self.dpb.has_empty_frame_buffer()
        {
            // Upstream splits frames into complementary field pairs when the DPB is in
            // interlaced mode; the envelope gate keeps that path unreachable.
            self.dpb
                .store_picture(pic.into_rc(), Some(id))
                .map_err(|err| PlanError::Parse(err.to_string()))?;
        } else {
            self.add_to_ready_queue(pic, id);
        }

        Ok(id)
    }

    fn picture_plan(cur: &CurrentPicState, recovery_point: Option<RecoveryPoint>) -> PicturePlan {
        let pic = &cur.pic;
        // The first slice's PPS defines the picture's parameters (upstream's
        // start_picture semantics); `cur.pps` may have drifted to a later slice's.
        let sps = &cur.first_slice_pps.sps;
        let rect = sps.visible_rectangle();

        PicturePlan {
            is_idr: matches!(pic.is_idr, IsIdr::Yes { .. }),
            nal_ref_idc: pic.nal_ref_idc,
            is_reference: pic.is_ref(),
            frame_num: pic.frame_num as u16,
            top_field_order_cnt: pic.top_field_order_cnt,
            bottom_field_order_cnt: pic.bottom_field_order_cnt,
            pic_order_cnt: pic.pic_order_cnt,
            coded_width: sps.width(),
            coded_height: sps.height(),
            // The vendored `visible_rectangle()` returns the crop OFFSET in `min` and
            // the visible SIZE in `max` (not an edge coordinate): subtracting would
            // double-count the left/top crop and underflow on large offsets.
            display_crop: DisplayCrop {
                x: rect.min.x,
                y: rect.min.y,
                width: rect.max.x,
                height: rect.max.y,
            },
            // Read unconditionally: the vendored parser builds every SPS from
            // `Default`, whose `VuiParams` already holds E.2.1's inferred values
            // (2/2/2, limited range), and parsing only overwrites them under the
            // present flags — so this IS the spec inference whether or not the
            // stream carried a VUI.
            colour: ColourDescription {
                colour_primaries: sps.vui_parameters.colour_primaries,
                transfer_characteristics: sps.vui_parameters.transfer_characteristics,
                matrix_coefficients: sps.vui_parameters.matrix_coefficients,
                video_full_range: sps.vui_parameters.video_full_range_flag,
            },
            profile_idc: sps.profile_idc,
            level_idc: sps.level_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: sps.max_dpb_frames(),
            recovery_point,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::rc::Rc;

    use cros_codecs::codec::h264::nalu_writer::NaluWriter;
    use cros_codecs::codec::h264::parser::Nalu;
    use cros_codecs::codec::h264::parser::NaluType;
    use cros_codecs::codec::h264::parser::PpsBuilder;
    use cros_codecs::codec::h264::parser::Profile;
    use cros_codecs::codec::h264::parser::SpsBuilder;
    use cros_codecs::codec::h264::parser::VuiParams;
    use cros_codecs::codec::h264::synthesizer::Synthesizer;

    use super::*;

    const TEST_25FPS: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264");
    // The plain 64x64-I-P-B-P.h264 is a constrained-baseline encode: x264 silently
    // dropped the requested B frame (its slices parse as I, P, P). The -high variant of
    // the same sequence carries the real B slice.
    const TEST_64X64_I_P_B_P_HIGH: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/h264/test_data/64x64-I-P-B-P-high.h264");

    /// Test-only AU splitter: the vendored vectors are raw Annex-B streams, while
    /// `plan_au` takes the pre-split AUs punktfunk's pump produces. A new AU starts at a
    /// non-slice NALU following slices, or at a slice with first_mb_in_slice == 0
    /// (whose ue(v) encoding makes the first RBSP bit 1) when the current AU already
    /// has slices.
    fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
        let mut aus = Vec::new();
        let mut cursor = Cursor::new(stream);
        let mut au_start = 0usize;
        let mut au_has_slice = false;

        while let Ok(nalu) = Nalu::next(&mut cursor) {
            let nalu_offset = cursor.position() as usize;
            let start = nalu_offset - nalu.offset;
            let is_slice = matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr);
            let first_mb_zero =
                is_slice && stream.get(nalu_offset + 1).is_some_and(|b| b & 0x80 != 0);

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

    #[test]
    fn the_full_25fps_vector_plans_every_picture_and_every_pic_id_reaches_output() {
        let aus = split_into_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(
                planner
                    .plan_au(au)
                    .expect("the clean vector must plan without errors"),
            );
        }

        assert_eq!(plans.len(), 250);
        assert_eq!(plans.iter().map(|p| p.slices.len()).sum::<usize>(), 500);
        assert!(plans[0].picture.is_idr);
        assert!(plans.iter().all(|p| p.warnings.is_empty()));
        assert_eq!(
            (plans[0].picture.coded_width, plans[0].picture.coded_height),
            (320, 240),
            "coded size comes from the vector's SPS"
        );

        for plan in &plans {
            if plan.picture.is_idr {
                assert_eq!(plan.picture.pic_order_cnt, 0, "POC must reset at an IDR");
            }
            for slice in &plan.slices {
                if slice.header.slice_type.is_p() || slice.header.slice_type.is_b() {
                    assert!(!slice.ref_list0.is_empty());
                }
            }
        }

        let stored: BTreeSet<PicId> = plans.iter().filter_map(|p| p.dpb.stored).collect();
        assert_eq!(stored.len(), 250);
        let mut emitted: Vec<PicId> = plans
            .iter()
            .flat_map(|p| p.dpb.outputs.iter().copied())
            .collect();
        emitted.extend(planner.flush().outputs);
        let output: BTreeSet<PicId> = emitted.iter().copied().collect();
        assert_eq!(
            output, stored,
            "bumping plus the final flush must output every picture"
        );

        // Output ORDER, not just coverage: within each IDR period, ids must emerge in
        // ascending POC — the invariant the C.4.5.3 bumping process exists to provide.
        // (POC was recorded at plan time; an IDR resets it, hence the period key.)
        let mut period = 0usize;
        let mut order_key: BTreeMap<PicId, (usize, i32)> = BTreeMap::new();
        for plan in &plans {
            if plan.picture.is_idr {
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
                    "outputs must emerge in ascending POC order per IDR period: \
                     {key:?} emitted after {last:?}"
                );
            }
            last = Some(key);
        }
    }

    #[test]
    fn b_slices_get_a_poc_ordered_list1_distinct_from_list0() {
        let aus = split_into_aus(TEST_64X64_I_P_B_P_HIGH);
        let mut planner = H264Planner::new();
        let mut b_slices_seen = 0usize;

        for au in &aus {
            let plan = planner
                .plan_au(au)
                .expect("the clean vector must plan without errors");
            for slice in &plan.slices {
                if !slice.header.slice_type.is_b() {
                    continue;
                }
                b_slices_seen += 1;
                assert!(!slice.ref_list0.is_empty());
                assert!(!slice.ref_list1.is_empty());

                let ids0: Vec<PicId> = slice.ref_list0.iter().map(|r| r.id).collect();
                let ids1: Vec<PicId> = slice.ref_list1.iter().map(|r| r.id).collect();
                assert_ne!(ids0, ids1, "list1 must not be list0's ordering");

                // 8.2.4.2.3: list0 leads with the past, list1 with the future
                // (a frame's PicOrderCnt is the min of its field order counts).
                let poc = |r: &RefPic| r.top_field_order_cnt.min(r.bottom_field_order_cnt);
                assert!(poc(&slice.ref_list0[0]) < plan.picture.pic_order_cnt);
                assert!(poc(&slice.ref_list1[0]) > plan.picture.pic_order_cnt);
            }
        }

        assert!(b_slices_seen > 0, "the vector must contain B slices");
    }

    /// Byte-level authoring for the MMCO/LTR test: parameter sets via the vendored
    /// builders + synthesizer, slice headers written by hand with the vendored
    /// `NaluWriter` (upstream has no slice-header synthesizer — its encoder packs
    /// headers in hardware). The planner only reads headers, so no slice data follows
    /// the rbsp stop bit.
    fn base_sps() -> SpsBuilder {
        SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
    }

    fn authored_sps_pps() -> (Rc<Sps>, Rc<Pps>) {
        let sps = base_sps().resolution(64, 64).build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        (sps, pps)
    }

    fn param_set_au(sps: &Sps, pps: &Pps) -> Vec<u8> {
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, sps, &mut au, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, pps, &mut au, true).unwrap();
        au
    }

    fn write_idr_slice() -> Vec<u8> {
        write_idr_slice_at(0, 0)
    }

    fn write_idr_slice_at(first_mb: u32, pps_id: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            w.write_header(3, NaluType::SliceIdr as u8).unwrap();
            w.write_ue(first_mb).unwrap(); // first_mb_in_slice
            w.write_ue(2u32).unwrap(); // slice_type: I
            w.write_ue(pps_id).unwrap(); // pic_parameter_set_id
            w.write_f(4, 0u32).unwrap(); // frame_num, u(4): log2_max_frame_num_minus4 = 0
            w.write_ue(0u32).unwrap(); // idr_pic_id
            w.write_f(4, 0u32).unwrap(); // pic_order_cnt_lsb, u(4)
            w.write_f(1, 0u32).unwrap(); // no_output_of_prior_pics_flag
            w.write_f(1, 0u32).unwrap(); // long_term_reference_flag
            w.write_se(0i32).unwrap(); // slice_qp_delta
            w.write_f(1, 1u32).unwrap(); // rbsp stop bit
            while !w.aligned() {
                w.write_f(1, 0u32).unwrap();
            }
        }
        buf
    }

    /// One P slice NALU. `mmco_ops` = `None` for sliding-window marking, `Some(ops)` for
    /// adaptive marking with `(operation, single-argument)` pairs (ops 2/4/6 all take
    /// exactly one) — the writer appends the terminating op 0.
    fn write_p_slice(
        frame_num: u32,
        poc_lsb: u32,
        ref_idc: u8,
        num_ref_idx_l0_active: u32,
        mmco_ops: Option<&[(u32, u32)]>,
    ) -> Vec<u8> {
        write_p_slice_at(
            0,
            0,
            frame_num,
            poc_lsb,
            ref_idc,
            num_ref_idx_l0_active,
            mmco_ops,
        )
    }

    fn write_p_slice_at(
        first_mb: u32,
        pps_id: u32,
        frame_num: u32,
        poc_lsb: u32,
        ref_idc: u8,
        num_ref_idx_l0_active: u32,
        mmco_ops: Option<&[(u32, u32)]>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            w.write_header(ref_idc, NaluType::Slice as u8).unwrap();
            w.write_ue(first_mb).unwrap(); // first_mb_in_slice
            w.write_ue(0u32).unwrap(); // slice_type: P
            w.write_ue(pps_id).unwrap(); // pic_parameter_set_id
            w.write_f(4, frame_num).unwrap(); // frame_num, u(4)
            w.write_f(4, poc_lsb).unwrap(); // pic_order_cnt_lsb, u(4)
            w.write_f(1, 1u32).unwrap(); // num_ref_idx_active_override_flag
            w.write_ue(num_ref_idx_l0_active - 1).unwrap();
            w.write_f(1, 0u32).unwrap(); // ref_pic_list_modification_flag_l0
            if ref_idc != 0 {
                match mmco_ops {
                    None => w.write_f(1, 0u32).map(|_| ()).unwrap(),
                    Some(ops) => {
                        w.write_f(1, 1u32).unwrap(); // adaptive_ref_pic_marking_mode_flag
                        for (op, arg) in ops {
                            w.write_ue(*op).unwrap();
                            w.write_ue(*arg).unwrap();
                        }
                        w.write_ue(0u32).unwrap(); // memory_management_control_operation end
                    }
                }
            }
            w.write_se(0i32).unwrap(); // slice_qp_delta
            w.write_f(1, 1u32).unwrap(); // rbsp stop bit
            while !w.aligned() {
                w.write_f(1, 0u32).unwrap();
            }
        }
        buf
    }

    #[test]
    fn mmco_marks_a_picture_long_term_later_lists_carry_it_and_mmco2_evicts_it() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());

        // AU1 marks itself long-term: MMCO 4 admits long-term index 0, MMCO 6 assigns
        // it to the current picture.
        let au1 = write_p_slice(1, 2, 1, 1, Some(&[(4, 1), (6, 0)]));
        let au2 = write_p_slice(2, 4, 1, 2, None);
        // AU3 evicts it again: MMCO 2 unmarks long_term_pic_num 0. Its own list is
        // 3 deep so the long-term picture's presence (or wrongful absence) is visible.
        let au3 = write_p_slice(3, 6, 1, 3, Some(&[(2, 0)]));
        let au4 = write_p_slice(4, 8, 0, 3, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();
        let p2 = planner.plan_au(&au2).unwrap();
        let p3 = planner.plan_au(&au3).unwrap();
        let p4 = planner.plan_au(&au4).unwrap();
        for plan in [&p0, &p1, &p2, &p3, &p4] {
            assert!(
                plan.warnings.is_empty(),
                "authored stream must plan clean: {plan:?}"
            );
        }
        assert!(p0.picture.is_idr);
        assert_eq!(p2.picture.pic_order_cnt, 4);

        let idr_id = p0.dpb.stored.unwrap();
        let lt_id = p1.dpb.stored.unwrap();

        // After AU1's marking, AU2's list must be [short-term IDR, long-term AU1] —
        // 8.2.4.2.1 puts long-term references after the short-term ones.
        let list0 = &p2.slices[0].ref_list0;
        assert_eq!(list0.len(), 2);
        assert!(!list0[0].is_long_term);
        assert_eq!(list0[0].id, idr_id);
        assert!(list0[1].is_long_term);
        assert_eq!(list0[1].id, lt_id);
        assert_eq!(
            list0[1].frame_num_or_lt_idx, 0,
            "LongTermFrameIdx, not frame_num"
        );

        // AU3 carries the MMCO 2, but marking is an end-of-picture process (8.2.5):
        // its OWN list is built before the op applies and must still hold the
        // long-term picture, after the short-terms in descending-PicNum order. An
        // applied-marking-before-list-build ordering bug surfaces exactly here.
        let p2_id = p2.dpb.stored.unwrap();
        let p3_id = p3.dpb.stored.unwrap();
        assert_eq!(
            p3.slices[0]
                .ref_list0
                .iter()
                .map(|r| (r.id, r.is_long_term))
                .collect::<Vec<_>>(),
            vec![(p2_id, false), (idr_id, false), (lt_id, true)],
            "AU3 still sees the long-term ref; its own MMCO 2 applies only at finish"
        );

        // AU4 must no longer see it: exactly the three short-terms, in descending
        // PicNum order (8.2.4.2.1).
        assert_eq!(
            p4.slices[0]
                .ref_list0
                .iter()
                .map(|r| (r.id, r.is_long_term))
                .collect::<Vec<_>>(),
            vec![(p3_id, false), (p2_id, false), (idr_id, false)],
            "the unmarked picture must have left, short-terms sorted by PicNum"
        );

        // Unmarked and displayed, the picture leaves the DPB for good.
        let flush = planner.flush();
        assert!(flush.outputs.contains(&lt_id));
        assert!(flush.removed.contains(&lt_id));
    }

    #[test]
    fn a_dropped_reference_au_degrades_to_gap_warnings_and_planning_continues() {
        let aus = split_into_aus(TEST_25FPS);

        // Pass 1: find a droppable AU — a non-IDR reference picture not followed by an
        // IDR (an IDR right after would reset the state and hide the gap).
        let mut planner = H264Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(planner.plan_au(au).unwrap());
        }
        let dropped = plans
            .iter()
            .enumerate()
            .position(|(i, p)| {
                p.picture.is_reference
                    && !p.picture.is_idr
                    && plans.get(i + 1).is_some_and(|next| !next.picture.is_idr)
            })
            .expect("the vector contains a droppable reference picture");

        // Pass 2: the same stream minus that AU must warn, not error — and every ref
        // list entry it emits must still resolve to a picture the backend was told to
        // store (substitution never leaks a placeholder).
        let mut planner = H264Planner::new();
        let mut gap_seen = false;
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
            gap_seen |= plan
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::FrameNumGap { .. }));
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
        }

        assert_eq!(planned, 249);
        assert!(
            gap_seen,
            "the AU after the drop must report the frame_num gap"
        );
        // The 8.2.5.2 placeholder is un-resolvable for backends, so planning around
        // it must also have flagged it.
        assert!(missing_seen);
    }

    #[test]
    fn a_gap_placeholder_inside_a_ref_list_is_substituted_in_place_not_compacted() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        let au1 = write_p_slice(1, 2, 1, 1, None);
        // The reference picture with frame_num 2 is never fed (lost on the wire); the
        // next AU's 3-deep list then holds the 8.2.5.2 placeholder at its HEAD.
        let au3 = write_p_slice(3, 6, 1, 3, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();
        let p3 = planner.plan_au(&au3).unwrap();

        assert!(p3
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::FrameNumGap { .. })));
        assert!(p3
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::MissingReference { .. })));

        // Initial list by descending PicNum: [placeholder(2), P1(1), IDR(0)]. The
        // placeholder heads the list, so its substitute is the first existing entry
        // (P1) — and crucially the two real entries keep their ref_idx positions.
        let id0 = p0.dpb.stored.unwrap();
        let id1 = p1.dpb.stored.unwrap();
        let list0 = &p3.slices[0].ref_list0;
        assert_eq!(
            list0.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![id1, id1, id0],
            "substitution must preserve list length and positions"
        );
        assert!(list0.iter().all(|r| !r.is_long_term));
    }

    #[test]
    fn a_recovery_point_sei_in_the_au_lands_on_the_picture_plan() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        // SEI NALU: recovery point, recovery_frame_cnt = 5, exact_match = 1.
        au0.extend([0x00, 0x00, 0x00, 0x01, 0x06, 0x06, 0x02, 0x34, 0x40, 0x80]);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let plan = planner.plan_au(&au0).unwrap();
        assert_eq!(
            plan.picture.recovery_point,
            Some(RecoveryPoint {
                recovery_frame_cnt: 5,
                exact_match: true,
                broken_link: false
            })
        );

        // The following AU carries no SEI: the field must not stick.
        let au1 = write_p_slice(1, 2, 1, 1, None);
        let plan = planner.plan_au(&au1).unwrap();
        assert_eq!(plan.picture.recovery_point, None);
    }

    #[test]
    fn an_interlaced_sps_is_rejected_as_outside_the_envelope() {
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .resolution(64, 64)
            .frame_mbs_only_flag(false)
            .mb_adaptive_frame_field_flag(false)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
            .build();
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au, true).unwrap();

        let mut planner = H264Planner::new();
        assert!(matches!(
            planner.plan_au(&au),
            Err(PlanError::OutsideEnvelope(_))
        ));
    }

    #[test]
    fn a_dpb_deeper_than_16_frames_is_rejected_as_outside_the_envelope() {
        // The one route past the A.3.1 16-frame cap: the VUI bitstream restriction's
        // max_dec_frame_buffering, an unbounded ue(v) that overrides the level-derived
        // size in `Sps::max_dpb_frames`. The builder has no VUI-restriction setter, so
        // the Sps is constructed directly (its fields are public).
        let sps = Sps {
            profile_idc: Profile::Main as u8,
            level_idc: Level::L4,
            frame_mbs_only_flag: true,
            direct_8x8_inference_flag: true,
            max_num_ref_frames: 4,
            vui_parameters_present_flag: true,
            vui_parameters: VuiParams {
                bitstream_restriction_flag: true,
                max_dec_frame_buffering: 17,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au, true).unwrap();

        let err = H264Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("DPB")),
            "{err:?}"
        );
    }

    /// MMCO 5 writer: op 5 takes NO argument (Table 7-9), so the generic
    /// [`write_p_slice`] — whose supported ops all take exactly one — cannot author
    /// it.
    fn write_p_slice_mmco5(frame_num: u32, poc_lsb: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            w.write_header(1, NaluType::Slice as u8).unwrap();
            w.write_ue(0u32).unwrap(); // first_mb_in_slice
            w.write_ue(0u32).unwrap(); // slice_type: P
            w.write_ue(0u32).unwrap(); // pic_parameter_set_id
            w.write_f(4, frame_num).unwrap(); // frame_num, u(4)
            w.write_f(4, poc_lsb).unwrap(); // pic_order_cnt_lsb, u(4)
            w.write_f(1, 1u32).unwrap(); // num_ref_idx_active_override_flag
            w.write_ue(0u32).unwrap(); // num_ref_idx_l0_active_minus1
            w.write_f(1, 0u32).unwrap(); // ref_pic_list_modification_flag_l0
            w.write_f(1, 1u32).unwrap(); // adaptive_ref_pic_marking_mode_flag
            w.write_ue(5u32).unwrap(); // memory_management_control_operation 5
            w.write_ue(0u32).unwrap(); // memory_management_control_operation end
            w.write_se(0i32).unwrap(); // slice_qp_delta
            w.write_f(1, 1u32).unwrap(); // rbsp stop bit
            while !w.aligned() {
                w.write_f(1, 0u32).unwrap();
            }
        }
        buf
    }

    #[test]
    fn an_mmco_5_is_planned_with_a_rebase_warning_not_rejected() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());
        let au1 = write_p_slice_mmco5(1, 2);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();

        assert!(p1.warnings.contains(&PlanWarning::Mmco5Rebase));
        // The plan carries the pre-rebase 8.2.1 values a decoder submits with; the
        // zeroed frame_num/POC exist only in the STORED picture later AUs reference.
        assert_eq!(p1.picture.frame_num, 1);
        assert_eq!(p1.picture.pic_order_cnt, 2);
        // And the op's C.4.5.3 clause-3 drain ran: the IDR is display-ready.
        assert!(p1.dpb.outputs.contains(&p0.dpb.stored.unwrap()));
    }

    #[test]
    fn a_separate_colour_plane_sps_is_rejected_as_outside_the_envelope() {
        // SpsBuilder has no separate_colour_plane setter; construct the Sps directly
        // (its fields are public) — the synthesizer writes the flag for High profile
        // with chroma_format_idc 3.
        let sps = Sps {
            profile_idc: Profile::High as u8,
            chroma_format_idc: 3,
            separate_colour_plane_flag: true,
            frame_mbs_only_flag: true,
            ..Default::default()
        };
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au, true).unwrap();

        let err = H264Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("separate")),
            "{err:?}"
        );
    }

    #[test]
    fn display_crop_reports_the_conformance_window_offset_and_size() {
        // chroma_format_idc 1 (inferred for Main) puts CropUnitX/Y at 2 with
        // frame_mbs_only: offsets top 2 / bottom 2 / left 4 / right 2 are 4/4/8/4 in
        // luma samples.
        let sps = base_sps()
            .resolution(64, 64)
            .frame_crop_offsets(2, 2, 4, 2)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 8,
                y: 4,
                width: 52,
                height: 56
            }
        );

        // The review's underflow shape: crop_left 100 (200 luma samples) on a
        // 320-wide picture passes SPS validation; a max-minus-min derivation
        // underflows on it.
        let sps = base_sps()
            .resolution(320, 240)
            .frame_crop_offsets(0, 0, 100, 0)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 200,
                y: 0,
                width: 120,
                height: 240
            }
        );
    }

    /// A 64x64 SPS with the VUI colour fields set as given. SpsBuilder has no
    /// colour setters, so the built Sps is unwrapped and mutated directly (the
    /// separate_colour_plane test's idiom); the synthesizer writes the whole
    /// `video_signal_type` block from the struct.
    fn sps_with_vui_colour(
        signal_type: bool,
        full_range: bool,
        description: Option<(u8, u8, u8)>,
    ) -> Rc<Sps> {
        let mut sps = Rc::try_unwrap(base_sps().resolution(64, 64).build()).expect("freshly built");
        sps.vui_parameters_present_flag = true;
        sps.vui_parameters.video_signal_type_present_flag = signal_type;
        sps.vui_parameters.video_full_range_flag = full_range;
        if let Some((primaries, transfer, matrix)) = description {
            sps.vui_parameters.colour_description_present_flag = true;
            sps.vui_parameters.colour_primaries = primaries;
            sps.vui_parameters.transfer_characteristics = transfer;
            sps.vui_parameters.matrix_coefficients = matrix;
        }
        Rc::new(sps)
    }

    fn plan_one_idr(sps: &Rc<Sps>) -> AuPlan {
        let pps = PpsBuilder::new(Rc::clone(sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au = param_set_au(sps, &pps);
        au.extend(write_idr_slice());
        H264Planner::new().plan_au(&au).unwrap()
    }

    #[test]
    fn an_sps_without_vui_plans_the_e211_unspecified_colour() {
        let (sps, pps) = authored_sps_pps();
        assert!(
            !sps.vui_parameters_present_flag,
            "the base SPS carries no VUI"
        );
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: false,
            },
            "E.2.1 inference: 'unspecified' code points + limited range, never a raw 0"
        );
    }

    #[test]
    fn an_explicit_colour_description_rides_the_plan_and_follows_a_new_sps() {
        // BT.2020/PQ HDR signalling — the in-band switch the Windows host emits.
        let hdr = sps_with_vui_colour(true, false, Some((9, 16, 9)));
        let plan = plan_one_idr(&hdr);
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                video_full_range: false,
            }
        );

        // The colour must track the SPS active for EACH picture, not the
        // session's first: an SDR stream renegotiated to HDR mid-stream (same
        // SPS id, new content, SPS+PPS in-band at the IDR — the parser's Pps
        // snapshots its SPS at PPS-parse time, and hosts re-send both exactly
        // so the new content activates) flips at the very next planned picture.
        let (sdr_sps, sdr_pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sdr_sps, &sdr_pps);
        au0.extend(write_idr_slice());
        let mut planner = H264Planner::new();
        let plan0 = planner.plan_au(&au0).unwrap();
        assert_eq!(plan0.picture.colour.matrix_coefficients, 2);

        let hdr_pps = PpsBuilder::new(Rc::clone(&hdr))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au1 = param_set_au(&hdr, &hdr_pps);
        au1.extend(write_idr_slice());
        let plan1 = planner.plan_au(&au1).unwrap();
        assert_eq!(
            plan1.picture.colour.matrix_coefficients, 9,
            "the replacing SPS's colour lands on its own picture, not latched"
        );
    }

    #[test]
    fn a_vui_without_colour_description_keeps_unspecified_but_honours_the_range_flag() {
        // video_signal_type present, full-range set, but NO colour description:
        // the code points stay E.2.1's "unspecified" while the range flag rides.
        let plan = plan_one_idr(&sps_with_vui_colour(true, true, None));
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
    fn a_malformed_nalu_mid_au_truncates_with_a_warning_keeping_prior_slices() {
        let (sps, pps) = authored_sps_pps();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        // Reserved NAL type 24: the vendored header parser rejects it.
        au.extend([0x00, 0x00, 0x00, 0x01, 0x18, 0xAA, 0xBB]);
        au.extend(write_idr_slice()); // real data behind the cut, never reached

        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.slices.len(),
            1,
            "only the slice before the cut is planned"
        );
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    #[test]
    fn a_foreign_slice_in_the_au_is_dropped_with_a_truncated_au_warning() {
        let (sps, pps) = authored_sps_pps();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        // A mis-split AU: continuation slices belonging to ANOTHER picture (non-IDR,
        // frame_num 1). They and everything after them must be ignored.
        au.extend(write_p_slice_at(8, 0, 1, 2, 1, 1, None));
        au.extend(write_p_slice_at(9, 0, 1, 2, 1, 1, None));

        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert!(plan.picture.is_idr);
        assert_eq!(plan.slices.len(), 1, "the foreign slices are not planned");
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    #[test]
    fn outputs_queued_during_a_failed_au_surface_in_the_next_successful_plan() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let id0 = planner.plan_au(&au0).unwrap().dpb.stored.unwrap();

        // This AU errors AFTER its IDR begin drained the DPB (queueing id0 for
        // output): the continuation slice references PPS 1, which was never sent.
        let mut bad_au = write_idr_slice();
        bad_au.extend(write_p_slice_at(8, 1, 0, 0, 1, 1, None));
        assert!(matches!(
            planner.plan_au(&bad_au),
            Err(PlanError::NoActiveParamSet { pps_id: 1 })
        ));

        // The queued output and the eviction must surface here, not vanish.
        let plan = planner.plan_au(&write_idr_slice()).unwrap();
        assert!(plan.dpb.outputs.contains(&id0));
        assert!(plan.dpb.removed.contains(&id0));
    }

    #[test]
    fn flush_resets_decoding_state_and_refuses_non_idr_until_an_idr_arrives() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let id0 = planner.plan_au(&au0).unwrap().dpb.stored.unwrap();
        let id1 = planner
            .plan_au(&write_p_slice(1, 2, 1, 1, None))
            .unwrap()
            .dpb
            .stored
            .unwrap();

        let flushed = planner.flush();
        assert!(flushed.outputs.contains(&id0) && flushed.outputs.contains(&id1));
        assert_eq!(flushed.removed, vec![id0, id1]);

        // A non-IDR AU is refused until the next IDR.
        assert!(matches!(
            planner.plan_au(&write_p_slice(2, 4, 1, 1, None)),
            Err(PlanError::AwaitingIdr)
        ));

        // The IDR restarts planning; parameter sets survived the flush (7.4.1.2).
        let plan = planner.plan_au(&write_idr_slice()).unwrap();
        assert!(plan.picture.is_idr);
        assert_eq!(plan.picture.pic_order_cnt, 0);
        assert!(plan.warnings.is_empty());

        // And the stream continues cleanly on the reset state.
        let plan = planner.plan_au(&write_p_slice(1, 2, 1, 1, None)).unwrap();
        assert!(plan.warnings.is_empty());
        assert_eq!(plan.slices[0].ref_list0.len(), 1);
    }

    #[test]
    fn picture_plan_parameters_come_from_the_first_slices_pps() {
        // Two SPSes with identical negotiation parameters but different conformance
        // windows; PPS 1 references the cropped one.
        let sps0 = base_sps().resolution(64, 64).build();
        let sps1 = base_sps()
            .seq_parameter_set_id(1)
            .resolution(64, 64)
            .frame_crop_offsets(2, 2, 4, 2)
            .build();
        let pps0 = PpsBuilder::new(Rc::clone(&sps0))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let pps1 = PpsBuilder::new(Rc::clone(&sps1))
            .pic_parameter_set_id(1)
            .pic_init_qp(26)
            .build();

        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps0, &mut au, true).unwrap();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps1, &mut au, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps0, &mut au, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps1, &mut au, true).unwrap();
        au.extend(write_idr_slice_at(0, 0));
        // Legal but perverse: a continuation slice may reference another PPS.
        au.extend(write_idr_slice_at(8, 1));

        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert!(plan.warnings.is_empty());
        assert_eq!(plan.slices.len(), 2);
        // The picture parameters come from the FIRST slice's PPS (the uncropped
        // SPS 0); they must not drift to the last slice's.
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 0,
                y: 0,
                width: 64,
                height: 64
            }
        );
        // The accessor pair follows the same first-slice rule: backends build
        // their parameter objects from these, so drifting to PPS 1 here would
        // desynchronize them from `picture`.
        assert_eq!(plan.pps.pic_parameter_set_id, 0);
        assert_eq!(plan.sps.seq_parameter_set_id, 0);
        assert!(
            Rc::ptr_eq(&plan.sps, &plan.pps.sps),
            "the SPS accessor is the PPS's own SPS, not a second copy"
        );
        assert!(
            !plan.sps.frame_cropping_flag,
            "SPS 0, not the cropped SPS 1"
        );
    }

    #[test]
    fn the_plans_parameter_set_accessors_carry_the_activated_content() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let plan = H264Planner::new().plan_au(&au0).unwrap();
        // The parser re-parses the in-band parameter sets, so pointer identity
        // with the authored `sps`/`pps` is not expected (and whole-struct
        // equality would compare parser-side normalizations like the flat
        // scaling-list fill); the contract is that the ACTIVATED content rides
        // out. Spot-check the fields backends build parameter objects from.
        assert_eq!(plan.sps.seq_parameter_set_id, sps.seq_parameter_set_id);
        assert_eq!(plan.sps.profile_idc, sps.profile_idc);
        assert_eq!(plan.sps.level_idc, sps.level_idc);
        assert_eq!(plan.sps.max_num_ref_frames, sps.max_num_ref_frames);
        assert_eq!(plan.sps.width(), sps.width());
        assert_eq!(plan.sps.height(), sps.height());
        assert_eq!(plan.pps.pic_parameter_set_id, pps.pic_parameter_set_id);
        assert_eq!(plan.pps.seq_parameter_set_id, pps.seq_parameter_set_id);
        assert_eq!(plan.pps.pic_init_qp_minus26, pps.pic_init_qp_minus26);
        assert!(Rc::ptr_eq(&plan.sps, &plan.pps.sps));
    }
}
