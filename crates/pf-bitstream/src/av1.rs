//! AV1 access-unit planning — M7's foundation, and the third planner in this crate.
//!
//! Same contract as [`crate::h264`] and [`crate::h265`]: one access unit in, one
//! [`AuPlan`] out, carrying everything a hardware backend needs to submit the frame
//! and everything the client needs to manage surfaces. The vendored cros-codecs
//! parser does the bitstream reading; this module owns the reference ledger, the
//! output bookkeeping and the concealment posture.
//!
//! # AV1's reference model is simpler than H.264's, and explicit
//!
//! There is no sliding window, no MMCO, no POC derivation and no bumping process.
//! There are **eight numbered reference slots**, and each frame says outright what it
//! does with them:
//!
//! * `ref_frame_idx[0..7]` names the slots this frame READS (seven references, which
//!   may repeat a slot) — and its POSITION is the AV1 reference name, which is why
//!   [`AuPlan::refs`] is name-indexed and a lost reference leaves a hole;
//! * `refresh_frame_flags` is an eight-bit mask naming the slots this frame WRITES
//!   once decoded;
//! * `show_frame` says whether the frame displays now, and `show_existing_frame`
//!   displays a slot's existing contents with no decode at all.
//!
//! That means this planner's job is bookkeeping rather than derivation, and the whole
//! of it is checkable against the stream: a frame that names a slot holding nothing is
//! a lost reference, full stop, with no spec process that might legitimately have
//! emptied it.
//!
//! # What is deliberately NOT here
//!
//! The per-backend conversions. Vulkan's `StdVideoDecodeAV1PictureInfo`, DXVA's
//! `DXVA_PicParams_AV1` and libva's `VAPictureParameterBufferAV1` are three more
//! spellings of the same plan, and they belong in `pf-vkdecode` / `pf-dxvadec` /
//! `pf-vaadec` beside their H.264 and H.265 siblings — for the reason the HEVC
//! reference-set disaster taught: the three APIs disagree about what a "reference
//! list" even indexes, and each conversion is where its own convention is written
//! down and tested.

use std::ops::Range;
use std::rc::Rc;

use cros_codecs::codec::av1::parser::FrameHeaderObu;
use cros_codecs::codec::av1::parser::ObuAction;
use cros_codecs::codec::av1::parser::ParsedObu;
use cros_codecs::codec::av1::parser::Parser;
use cros_codecs::codec::av1::parser::SequenceHeaderObu;

use crate::h264::ColourDescription;

/// The parsed types a backend conversion names, re-exported so each names them
/// through this module rather than reaching into the vendored crate — the same
/// courtesy [`crate::h264`] does with its `Sps`/`Pps`.
pub use cros_codecs::codec::av1::parser::FrameHeaderObu as ParsedFrameHeader;
pub use cros_codecs::codec::av1::parser::FrameType;
pub use cros_codecs::codec::av1::parser::SequenceHeaderObu as ParsedSequenceHeader;

/// A stable identity for a decoded picture, the same currency the other two planners
/// deal in: the backends key their surface tables by it and never by slot index.
pub type PicId = u64;

/// AV1's reference slot count (`NUM_REF_FRAMES`).
pub const NUM_REF_SLOTS: usize = 8;

/// References a single inter frame may name (`REFS_PER_FRAME`).
pub const REFS_PER_FRAME: usize = 7;

/// One reference: which picture, which slot holds it, and what that picture's OWN
/// frame header said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub id: PicId,
    /// The slot index, 0..8. Backends that address references by slot (Vulkan) want
    /// this; backends that address them by surface resolve `id` through their own
    /// table.
    pub slot: u8,
    /// The reference's own header state — see [`RefState`], and note it is the
    /// REFERENCE's, never the frame being decoded.
    pub state: RefState,
}

/// What one picture's own frame header said, kept for as long as that picture can
/// serve as a reference.
///
/// Every backend has a per-REFERENCE structure — Vulkan's
/// `StdVideoDecodeAV1ReferenceInfo`, DXVA's `DXVA_PicEntry_AV1`, libva's
/// `VAReferenceFrameAV1` — and each of them asks questions about the reference
/// picture, not about the frame being decoded. Answering them from the CURRENT
/// header is the shape of a whole bug class: it compiles, it looks like the fields
/// are filled, and the hardware predicts from a picture it has been told the wrong
/// things about. So the answers are recorded once, where they are unambiguous —
/// when the picture is STORED into its slots — and travel on the slot.
///
/// [`Av1Planner::refresh_slots`] is the only writer, and [`RefState::of`] the only
/// way to build one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefState {
    /// The picture's `OrderHint`.
    pub order_hint: u32,
    /// The picture's own frame type — a reference is routinely a different type
    /// from the frame reading it.
    pub frame_type: FrameType,
    /// `RefFrameSignBias` packed the way Vulkan wants it: bit `i` set where
    /// `RefFrameSignBias[i]` is 1, `i` being an AV1 reference frame index
    /// (`INTRA_FRAME` = 0, `LAST_FRAME` = 1 … `ALTREF_FRAME` = 7).
    ///
    /// This is what tells a decoder that a reference lies in the FUTURE, so it
    /// drives compound prediction and motion-field projection. All-zero means
    /// "every reference is in the past", which for any stream with hidden ALTREFs
    /// — the ordinary case — is wrong rather than merely conservative.
    pub ref_frame_sign_bias: u8,
    /// The picture's own `OrderHints[]`, which become `SavedOrderHints` once it is
    /// a reference (7.20). Indexed by AV1 reference frame index, as above.
    pub saved_order_hints: [u32; NUM_REF_SLOTS],
    pub disable_frame_end_update_cdf: bool,
    pub segmentation_enabled: bool,
}

impl RefState {
    /// Read one frame header's reference-relevant state.
    ///
    /// Called by the planner when a picture is stored, and by a backend for the
    /// picture it is about to decode (which activates a slot, so it needs the same
    /// answers). One function so the two can never drift.
    pub fn of(header: &FrameHeaderObu) -> RefState {
        // ⚠⚠ INDEX SHIFT, and it is the vendored parser's, not ours.
        //
        // AV1 7.8 writes `RefFrameSignBias[ refFrame ]` with `refFrame =
        // LAST_FRAME + i`, and libavcodec's `av1dec.c` (`order_hint_info`) does
        // exactly that — so `RefFrameSignBias` bit 1 is LAST_FRAME. The vendored
        // cros-codecs parser writes `fh.ref_frame_sign_bias[i]` in the SAME loop
        // body where it writes `fh.order_hints[ref_frame]`, so its array is
        // shifted one down: index 0 holds LAST_FRAME's bias and index 7 is never
        // written. (Its own VP9 parser gets this right, which is how the AV1 one
        // reads as a slip rather than a convention.)
        //
        // Corrected here rather than in the vendored tree so the pin stays clean,
        // and pinned by `the_sign_bias_mask_is_spec_indexed_not_parser_indexed`,
        // which recomputes the bias from `order_hints` through the parser's own
        // `get_relative_dist`.
        let mut ref_frame_sign_bias = 0u8;
        for (i, biased) in header
            .ref_frame_sign_bias
            .iter()
            .take(REFS_PER_FRAME)
            .enumerate()
        {
            if *biased {
                ref_frame_sign_bias |= 1 << (i + 1);
            }
        }
        RefState {
            order_hint: header.order_hint,
            frame_type: header.frame_type,
            ref_frame_sign_bias,
            saved_order_hints: header.order_hints,
            disable_frame_end_update_cdf: header.disable_frame_end_update_cdf,
            segmentation_enabled: header.segmentation_params.segmentation_enabled,
        }
    }
}

/// One CDEF secondary strength as every hardware API wants it: the **coded
/// two-bit syntax element**, `0..=3`.
///
/// ⚠⚠ The parser does not hold that value. AV1 5.9.19 reads `cdef_y_sec_strength[i]`
/// as `f(2)` and then mutates the variable of the same name in place —
/// `if (cdef_y_sec_strength[i] == 3) cdef_y_sec_strength[i] += 1` — so the spec's
/// own `cdef_y_sec_strength` afterwards holds `0, 1, 2` or **`4`**, and cros-codecs
/// follows the spec literally (`parser.rs`, `parse_cdef_params`). Every decode API
/// wants the value BEFORE that fixup, and applies the expansion itself:
///
/// * **Vulkan** — libavcodec's `vulkan_av1.c` sends `frame_header->cdef_y_sec_strength[i]`
///   straight out of CBS, and `cbs_av1_syntax_template.c` reads it as a bare
///   `fbs(2, …)` with no fixup. Vulkan's `StdVideoAV1CDEF` therefore carries the
///   coded value, because libavcodec is what every driver was validated against;
/// * **VA-API** — `vaapi_av1.c` packs `(pri << 2) + sec`, two bits for `sec`;
/// * **NVDEC** — `nvdec_av1.c` packs `(pri & 0x0F) | (sec << 4)`, two bits again;
/// * **DXVA** — `DXVA_PicParams_AV1`'s `cdef_y_strength[i].secondary` IS a two-bit
///   bitfield.
///
/// So sending `4` is not "a bigger number": on three of those four it overflows a
/// two-bit field and the strength reads back as **0** — no secondary CDEF filtering
/// at all, on exactly the blocks that asked for the strongest. That is a small,
/// everywhere, in-loop pixel difference, which is the hardest kind to see and the
/// easiest kind to propagate: CDEF runs before the frame is stored as a reference.
///
/// Frame 0 of the vendored 25fps vector codes it (`cdef_y_sec_strength[3]` and
/// `cdef_uv_sec_strength[0]` are both 4), as do 68 of its 274 frames —
/// [`crate::av1::tests::the_cdef_secondary_strength_is_the_coded_value`] pins both
/// numbers.
///
/// Values `0..=2` are untouched by the fixup and pass through; a hand-built header
/// carrying the coded `3` passes through too. Anything wider is CLAMPED rather than
/// masked, because `& 3` is precisely the truncation this function exists to
/// prevent.
pub fn coded_cdef_sec_strength(parsed: u32) -> u8 {
    match parsed {
        0..=2 => parsed as u8,
        _ => 3,
    }
}

/// What this access unit does to the decoded-picture store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DpbUpdate {
    /// The id assigned to this AU's picture — allocate a surface for it. `None` for a
    /// `show_existing_frame` access unit, which decodes nothing.
    pub stored: Option<PicId>,
    /// Display-ready pictures, in output order.
    pub outputs: Vec<PicId>,
    /// Pictures no slot holds any more; free once displayed.
    pub removed: Vec<PicId>,
}

/// One tile group's payload, as a byte range in the access unit.
///
/// AV1 hands the hardware whole tile-group OBUs rather than the slice-by-slice
/// records H.264 and H.265 use, so the range is the OBU's data, and the backends
/// concatenate in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilePlan {
    pub data: Range<usize>,
    pub tg_start: u32,
    pub tg_end: u32,
}

/// Per-picture parameters a hardware picture-parameters struct wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PicturePlan {
    pub frame_type: FrameType,
    /// A key frame that refreshes every slot — the stream's re-anchor point.
    pub is_key: bool,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub order_hint: u32,
    /// Post-superres width; `frame_width` is the coded width before upscaling.
    pub upscaled_width: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    /// The display region — AV1's counterpart to a conformance window.
    pub render_width: u32,
    pub render_height: u32,
    pub bit_depth: u8,
    /// 0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4 — expressed in H.264's
    /// `chroma_format_idc` vocabulary so a backend's format decision is one function
    /// for all three codecs.
    pub chroma_format_idc: u8,
    /// Colour signalling, per picture and never latched — the same rule the other two
    /// planners follow, because a host can switch an HDR desktop to PQ/BT.2020 in band.
    pub colour: ColourDescription,
}

/// One planned access unit.
#[derive(Debug, Clone)]
pub struct AuPlan {
    pub picture: PicturePlan,
    pub tiles: Vec<TilePlan>,
    /// The references this frame names, **indexed by AV1 reference NAME** —
    /// position `i` is `ref_frame_idx[i]`, i.e. `LAST_FRAME + i`.
    ///
    /// `None` where the named slot held nothing: the reference is lost, it is also
    /// reported as [`PlanWarning::MissingReference`], and it leaves a HOLE. The
    /// array shape is the point. A `Vec` of the references that happened to resolve
    /// renumbers every name after the first loss — name 4 silently becomes name 3 —
    /// and every backend that read position-as-name then predicted from the wrong
    /// picture. Repeats are preserved for the same reason: a frame may legitimately
    /// point several of its seven names at one slot.
    pub refs: [Option<RefPic>; REFS_PER_FRAME],
    pub dpb: DpbUpdate,
    /// Every slot that holds a picture as this AU decodes — AV1's answer to the
    /// "marked DPB" the DXVA and VAAPI conversions want, and a superset of the
    /// pictures [`Self::refs`] names. Slot order, each slot once.
    pub dpb_refs: Vec<RefPic>,
    pub warnings: Vec<PlanWarning>,
    pub sequence: Rc<SequenceHeaderObu>,
    /// The frame header this plan was built from, whole.
    ///
    /// [`Self::picture`] is the digest the CLIENT needs — size, depth, colour,
    /// keyframe — while a hardware backend needs nearly all of the header:
    /// AV1 puts tile info, quantisation, segmentation, loop filter, CDEF, loop
    /// restoration, global motion and film grain in the per-frame header rather
    /// than in a parameter set, and every one of them reaches the driver. Carried
    /// whole for the same reason the H.264 and H.265 plans carry their activated
    /// SPS/PPS: a backend must build its structures from exactly what was parsed,
    /// never by re-reading the access unit.
    pub header: Rc<FrameHeaderObu>,
}

/// Concealment signals: planning continues, the session layer requests recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWarning {
    /// A frame named a slot holding no picture. Unlike H.264's equivalent this needs
    /// no interpretation: no AV1 process empties a slot behind the stream's back, so
    /// the reference was lost upstream.
    MissingReference { slot: u8, ref_index: u8 },
    /// `show_existing_frame` named an empty slot — nothing to display.
    MissingShowExisting { slot: u8 },
    /// The OBU walk stopped early: a malformed OBU with data behind it. The plan
    /// covers what was read; `offset` is where the walk stopped.
    TruncatedAu { offset: usize },
}

/// Why an access unit cannot be planned at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// No frame header in the access unit — nothing to decode or display.
    NoFrame,
    /// A frame arrived before any sequence header. Every dimension, depth and colour
    /// value lives there, so there is nothing to plan against.
    NoSequenceHeader,
    /// The parser rejected the bitstream.
    Parse(String),
    /// A frame outside this decoder's envelope.
    Unsupported(&'static str),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoFrame => write!(f, "the access unit carried no frame header"),
            PlanError::NoSequenceHeader => {
                write!(f, "a frame arrived before any sequence header")
            }
            PlanError::Parse(e) => write!(f, "AV1 parse: {e}"),
            PlanError::Unsupported(what) => write!(f, "outside the envelope: {what}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// The AV1 planner: the vendored parser plus this crate's reference ledger.
pub struct Av1Planner {
    parser: Parser,
    /// Slot → the picture it holds. AV1's whole reference model, and the reason this
    /// planner is bookkeeping rather than derivation.
    slots: [Option<RefPic>; NUM_REF_SLOTS],
    next_id: PicId,
    sequence: Option<Rc<SequenceHeaderObu>>,
}

impl Default for Av1Planner {
    fn default() -> Self {
        Self::new()
    }
}

impl Av1Planner {
    pub fn new() -> Av1Planner {
        Av1Planner {
            parser: Parser::default(),
            slots: [None; NUM_REF_SLOTS],
            next_id: 1,
            sequence: None,
        }
    }

    /// The slots currently holding pictures, in slot order.
    pub fn dpb_refs(&self) -> Vec<RefPic> {
        self.slots.iter().flatten().copied().collect()
    }

    /// Plan one access unit — **one temporal unit, which may carry SEVERAL frames**.
    ///
    /// That is why this returns a vector and its H.264/H.265 siblings do not. An AV1
    /// temporal unit is free to hold a hidden frame and the `show_existing_frame`
    /// that displays it, or several frames of a scalability layer; the vendored
    /// conformance vector puts 274 frames in 250 temporal units, so the case is not
    /// hypothetical even though punktfunk hosts (low-delay, no hidden frames) emit
    /// one frame per unit. Planning only the last header seen would silently drop
    /// the others — decoding fewer frames than the stream contains, with nothing to
    /// say so.
    ///
    /// Plans come back in decode order; each carries its own reference set and its
    /// own share of the store update.
    pub fn plan_au(&mut self, au: &[u8]) -> Result<Vec<AuPlan>, PlanError> {
        let mut warnings = Vec::new();
        let mut plans: Vec<AuPlan> = Vec::new();
        // The frame being accumulated: its header, and the tile groups seen since.
        let mut pending: Option<(FrameHeaderObu, Vec<TilePlan>)> = None;
        let mut consumed = 0usize;

        while consumed < au.len() {
            let action = match self.parser.read_obu(&au[consumed..]) {
                Ok(action) => action,
                Err(e) => {
                    // A malformed OBU with real data behind it is concealment
                    // material, not a parse failure, exactly as the other two
                    // planners treat a truncated NALU walk — but only once
                    // something has been read. Nothing at all is a hard error.
                    if pending.is_some() || !plans.is_empty() {
                        warnings.push(PlanWarning::TruncatedAu { offset: consumed });
                        break;
                    }
                    return Err(PlanError::Parse(e));
                }
            };
            let obu = match action {
                ObuAction::Process(obu) => obu,
                ObuAction::Drop(n) => {
                    consumed += n as usize;
                    continue;
                }
            };
            let used = obu.bytes_used;
            // The OBU's payload as a range in THIS access unit, so a backend can
            // hand the driver bytes without re-parsing.
            let obu_start = consumed;
            consumed += used;

            match self.parser.parse_obu(obu) {
                Ok(ParsedObu::SequenceHeader(seq)) => self.sequence = Some(seq),
                Ok(ParsedObu::FrameHeader(fh)) => {
                    // A new header ends the previous frame — its tile groups are
                    // all in by now.
                    if let Some((h, t)) = pending.take() {
                        plans.push(self.plan_one(h, t, std::mem::take(&mut warnings))?);
                    }
                    pending = Some((fh, Vec::new()));
                }
                Ok(ParsedObu::Frame(frame)) => {
                    // A Frame OBU is a header and its tile group in one, so it ends
                    // any previous frame and is itself complete.
                    if let Some((h, t)) = pending.take() {
                        plans.push(self.plan_one(h, t, std::mem::take(&mut warnings))?);
                    }
                    let tile = TilePlan {
                        data: obu_start..consumed,
                        tg_start: frame.tile_group.tg_start,
                        tg_end: frame.tile_group.tg_end,
                    };
                    plans.push(self.plan_one(
                        frame.header,
                        vec![tile],
                        std::mem::take(&mut warnings),
                    )?);
                }
                Ok(ParsedObu::TileGroup(tg)) => {
                    let tile = TilePlan {
                        data: obu_start..consumed,
                        tg_start: tg.tg_start,
                        tg_end: tg.tg_end,
                    };
                    match pending.as_mut() {
                        Some((_, tiles)) => tiles.push(tile),
                        // Tiles with no header ahead of them: the header was lost.
                        // Dropped rather than guessed at — there is no picture to
                        // attach them to.
                        None => warnings.push(PlanWarning::TruncatedAu { offset: obu_start }),
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if pending.is_some() || !plans.is_empty() {
                        warnings.push(PlanWarning::TruncatedAu { offset: obu_start });
                        break;
                    }
                    return Err(PlanError::Parse(e));
                }
            }
        }

        if let Some((h, t)) = pending.take() {
            plans.push(self.plan_one(h, t, std::mem::take(&mut warnings))?);
        }
        if plans.is_empty() {
            return Err(PlanError::NoFrame);
        }
        // Warnings raised after the last frame was planned (a truncated tail) still
        // belong to this access unit; attach them to the frame they cut short.
        if !warnings.is_empty() {
            if let Some(last) = plans.last_mut() {
                last.warnings.append(&mut warnings);
            }
        }
        Ok(plans)
    }

    fn plan_one(
        &mut self,
        header: FrameHeaderObu,
        tiles: Vec<TilePlan>,
        warnings: Vec<PlanWarning>,
    ) -> Result<AuPlan, PlanError> {
        let sequence = self.sequence.clone().ok_or(PlanError::NoSequenceHeader)?;
        self.plan_frame(header, sequence, tiles, warnings)
    }

    fn plan_frame(
        &mut self,
        header: FrameHeaderObu,
        sequence: Rc<SequenceHeaderObu>,
        tiles: Vec<TilePlan>,
        mut warnings: Vec<PlanWarning>,
    ) -> Result<AuPlan, PlanError> {
        // Shared with the plan: the backends need the whole header and there is no
        // reason for each to own a copy of a struct this size.
        let header = Rc::new(header);
        let dpb_refs = self.dpb_refs();

        // `show_existing_frame` decodes nothing: it displays a slot's contents.
        if header.show_existing_frame {
            let slot = header.frame_to_show_map_idx;
            let shown = self.slots.get(usize::from(slot)).copied().flatten();
            if shown.is_none() {
                warnings.push(PlanWarning::MissingShowExisting { slot });
            }
            // Showing a KEY frame this way resets the whole reference store (7.20):
            // the shown frame's state is loaded and every slot refreshed. Handled
            // through the same slot writer as an ordinary refresh so there is one
            // place removals are computed.
            let removed = if header.frame_type == FrameType::KeyFrame {
                match shown {
                    // The SHOWN picture's state is what every refreshed slot takes
                    // (7.20 loads the shown frame's state), not this header's —
                    // a show_existing_frame header carries none of its own.
                    Some(pic) => self.refresh_slots(0xff, pic.id, pic.state),
                    None => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let picture = picture_plan(&header, &sequence);
            return Ok(AuPlan {
                picture,
                tiles,
                refs: [None; REFS_PER_FRAME],
                dpb: DpbUpdate {
                    stored: None,
                    outputs: shown.map(|p| p.id).into_iter().collect(),
                    removed,
                },
                dpb_refs,
                header: header.clone(),
                warnings,
                sequence,
            });
        }

        // The references this frame names, BY NAME. A slot holding nothing leaves
        // its name empty rather than shortening the list (field docs): position is
        // the AV1 reference name and nothing may renumber it.
        let mut refs = [None; REFS_PER_FRAME];
        if !matches!(
            header.frame_type,
            FrameType::KeyFrame | FrameType::IntraOnlyFrame
        ) {
            for (ref_index, &slot) in header.ref_frame_idx.iter().enumerate() {
                match self.slots.get(usize::from(slot)).copied().flatten() {
                    Some(pic) => refs[ref_index] = Some(pic),
                    None => warnings.push(PlanWarning::MissingReference {
                        slot,
                        // Seven references; the cast cannot truncate.
                        ref_index: ref_index as u8,
                    }),
                }
            }
        }

        let id = self.next_id;
        self.next_id += 1;

        // The parser keeps its OWN reference state — sizes and order hints derived
        // from references — and it must be updated whether or not our ledger is
        // happy, or every later inter frame fails to parse.
        if let Err(e) = self.parser.ref_frame_update(&header) {
            return Err(PlanError::Parse(e));
        }
        let removed = self.refresh_slots(header.refresh_frame_flags, id, RefState::of(&header));

        let picture = picture_plan(&header, &sequence);
        let outputs = if header.show_frame {
            vec![id]
        } else {
            Vec::new()
        };
        Ok(AuPlan {
            picture,
            tiles,
            refs,
            dpb: DpbUpdate {
                stored: Some(id),
                outputs,
                removed,
            },
            dpb_refs,
            header: header.clone(),
            warnings,
            sequence,
        })
    }

    /// Write `id` into every slot `refresh_frame_flags` names, and report the
    /// pictures that no longer occupy ANY slot.
    ///
    /// The "any slot" part is the whole subtlety: one picture routinely occupies
    /// several slots at once (a key frame refreshes all eight), so a slot being
    /// overwritten does not mean its picture is gone. Reporting it as removed while
    /// another slot still holds it would free a surface the next frame references —
    /// which is the reference-loss shape this program exists to catch.
    fn refresh_slots(
        &mut self,
        refresh_frame_flags: u32,
        id: PicId,
        state: RefState,
    ) -> Vec<PicId> {
        let mut displaced: Vec<PicId> = Vec::new();
        for slot in 0..NUM_REF_SLOTS {
            if refresh_frame_flags & (1 << slot) == 0 {
                continue;
            }
            if let Some(old) = self.slots[slot] {
                if !displaced.contains(&old.id) {
                    displaced.push(old.id);
                }
            }
            self.slots[slot] = Some(RefPic {
                id,
                // Eight slots; the cast cannot truncate.
                slot: slot as u8,
                state,
            });
        }
        displaced.retain(|gone| !self.slots.iter().flatten().any(|held| held.id == *gone));
        displaced
    }
}

fn picture_plan(header: &FrameHeaderObu, sequence: &SequenceHeaderObu) -> PicturePlan {
    let color = &sequence.color_config;
    let bit_depth = if color.high_bitdepth {
        if color.twelve_bit {
            12
        } else {
            10
        }
    } else {
        8
    };
    // AV1 spells the sampling as two subsampling flags plus a monochrome flag;
    // every backend in this program decides formats in H.264's vocabulary, so the
    // translation happens once, here.
    let chroma_format_idc = match (color.mono_chrome, color.subsampling_x, color.subsampling_y) {
        (true, _, _) => 0,
        (false, true, true) => 1,
        (false, true, false) => 2,
        (false, false, false) => 3,
        // 4:4:0 (subsampling_y only) has no AV1 profile; report it as monochrome's
        // neighbour rather than silently calling it 4:2:0, and let the backend's
        // format decision refuse it.
        (false, false, true) => 4,
    };
    PicturePlan {
        frame_type: header.frame_type,
        is_key: header.frame_type == FrameType::KeyFrame,
        show_frame: header.show_frame,
        showable_frame: header.showable_frame,
        order_hint: header.order_hint,
        upscaled_width: header.upscaled_width,
        frame_width: header.frame_width,
        frame_height: header.frame_height,
        render_width: header.render_width,
        render_height: header.render_height,
        bit_depth,
        chroma_format_idc,
        colour: ColourDescription {
            colour_primaries: color.color_primaries as u8,
            transfer_characteristics: color.transfer_characteristics as u8,
            matrix_coefficients: color.matrix_coefficients as u8,
            video_full_range: color.color_range,
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use cros_codecs::bitstream_utils::IvfIterator;

    /// The vendored conformance vector: 250 temporal units, 274 frames — the same
    /// file the crate's vendor-pinning smoke test walks, here driven through the
    /// PLANNER instead of the parser.
    const AV1_25FPS: &[u8] =
        include_bytes!("../vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1");

    /// Walk the whole vector and check the plan is self-consistent at every frame.
    ///
    /// **Measured composition: 250 temporal units, 274 frames, 24 units carrying two
    /// frames, 250 displayed, and no `show_existing_frame` at all.** So the 24 extra
    /// frames are HIDDEN frames — decoded, not displayed, referenced later. That is
    /// what makes the multi-frame walk load-bearing rather than tidy: a planner that
    /// took only the last header in each unit would decode 250 frames and silently
    /// drop 24 REFERENCES, and the damage would surface later as missing-reference
    /// concealment on frames that were never damaged.
    ///
    /// ⚠ Coverage this vector does NOT give: `show_existing_frame` appears zero
    /// times, so [`Av1Planner::plan_frame`]'s display-only path — including the
    /// key-frame slot reset — is exercised by no test here. It needs a vector that
    /// uses it, or a synthesised one, before that path can be called verified.
    #[test]
    fn the_whole_vendored_vector_plans_and_the_frame_count_is_the_parsers() {
        let mut planner = Av1Planner::new();
        let (mut units, mut frames, mut shown, mut show_existing) = (0u32, 0u32, 0u32, 0u32);
        let mut multi_frame_units = 0u32;
        let mut warnings = 0usize;
        let mut max_refs = 0usize;

        for packet in IvfIterator::new(AV1_25FPS) {
            units += 1;
            let plans = planner
                .plan_au(packet)
                .unwrap_or_else(|e| panic!("temporal unit {units}: {e}"));
            if plans.len() > 1 {
                multi_frame_units += 1;
            }
            for plan in &plans {
                frames += 1;
                warnings += plan.warnings.len();
                shown += plan.dpb.outputs.len() as u32;
                if plan.dpb.stored.is_none() {
                    show_existing += 1;
                    assert!(
                        plan.tiles.is_empty(),
                        "a show_existing_frame decodes nothing and can carry no tiles"
                    );
                }
                max_refs = max_refs.max(plan.refs.iter().flatten().count());

                // Every tile range must lie inside the access unit it came from.
                for tile in &plan.tiles {
                    assert!(
                        tile.data.start < tile.data.end && tile.data.end <= packet.len(),
                        "frame {frames}: tile range {:?} is not inside a {}-byte unit",
                        tile.data,
                        packet.len()
                    );
                }
                // A reference must name a slot that holds the picture it claims,
                // and the name it sits under must be the one the bitstream coded.
                for (name, r) in plan.refs.iter().enumerate() {
                    let Some(r) = r else { continue };
                    assert!(usize::from(r.slot) < NUM_REF_SLOTS);
                    assert_eq!(
                        r.slot, plan.header.ref_frame_idx[name],
                        "frame {frames}: reference name {name} holds the picture in \
                         slot {}, but ref_frame_idx[{name}] names slot {}",
                        r.slot, plan.header.ref_frame_idx[name]
                    );
                    // The marked store is a superset of what this frame reads.
                    assert!(
                        plan.dpb_refs.iter().any(|d| d.id == r.id),
                        "frame {frames}: reference {} is not in the marked store",
                        r.id
                    );
                }
            }
        }

        assert_eq!(units, 250, "the vendored vector is 250 temporal units");
        assert_eq!(
            frames, 274,
            "the parser's own golden is 274 frames; a planner that sees fewer is \
             dropping frames a multi-frame temporal unit carried"
        );
        assert_eq!(
            multi_frame_units, 24,
            "the 24 units carrying two frames are the whole reason plan_au returns a \
             vector; if this reaches 0 the count above is being met some other way"
        );
        assert_eq!(
            warnings, 0,
            "a clean conformance vector must plan without concealment"
        );
        assert_eq!(
            shown, 250,
            "one displayed frame per temporal unit — the other 24 are hidden"
        );
        assert_eq!(
            show_existing, 0,
            "this vector uses no show_existing_frame; if that ever changes, the \
             display-only path stops being untested and the doc above must say so"
        );
        assert_eq!(
            max_refs, REFS_PER_FRAME,
            "an inter frame names all seven references"
        );
    }

    /// A picture can hold several slots at once, and losing ONE of them must not
    /// report the picture as removed.
    ///
    /// This is the whole reason [`Av1Planner::refresh_slots`] filters what it
    /// displaces: a key frame refreshes all eight slots, so the next frame to
    /// refresh a single slot displaces that picture from ONE slot while seven still
    /// hold it. Reporting it removed would free the surface under a live reference —
    /// the reference-loss shape this program exists to catch.
    #[test]
    fn a_picture_held_by_several_slots_is_not_removed_until_the_last_one_goes() {
        let mut planner = Av1Planner::new();
        let at = |order_hint: u32| RefState {
            order_hint,
            ..RefState::of(&FrameHeaderObu::default())
        };
        // A key frame in every slot.
        let removed = planner.refresh_slots(0xff, 1, at(0));
        assert!(removed.is_empty(), "nothing was there to displace");
        assert_eq!(planner.dpb_refs().len(), NUM_REF_SLOTS);

        // A frame takes one slot: picture 1 still holds the other seven.
        let removed = planner.refresh_slots(0b0000_0001, 2, at(1));
        assert!(
            removed.is_empty(),
            "picture 1 still occupies seven slots — reporting it removed would free \
             a surface every later frame still references"
        );

        // Take the rest: now it really is gone, and reported exactly once.
        let removed = planner.refresh_slots(0b1111_1110, 3, at(2));
        assert_eq!(removed, vec![1], "reported once, not once per slot");

        // And picture 2's single slot.
        let removed = planner.refresh_slots(0b0000_0001, 4, at(3));
        assert_eq!(removed, vec![2]);
    }

    /// A lost reference must leave a HOLE at its own name, not shorten the list.
    ///
    /// This is the defect the name-indexed [`AuPlan::refs`] closes, and it is worth
    /// a synthetic case because the clean vector never loses a reference: with a
    /// `Vec` of survivors, dropping the picture behind name 2 slid names 3..6 down
    /// one, and every backend that reads position-as-name then predicted LAST from
    /// the picture GOLDEN should have supplied. Nothing else in the plan would say
    /// so — the reference count is still plausible and every entry is still a real
    /// picture.
    #[test]
    fn a_lost_reference_leaves_its_name_empty_and_does_not_renumber_the_others() {
        // The vector's first unit is a key frame: it gives the vendored parser its
        // sequence header (`ref_frame_update` needs one) and fills all eight slots.
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let sequence = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .sequence
            .clone();
        assert_eq!(planner.dpb_refs().len(), NUM_REF_SLOTS);

        // Empty the slot name 2 will point at — a reference lost upstream.
        planner.slots[5] = None;

        let header = FrameHeaderObu {
            frame_type: FrameType::InterFrame,
            ref_frame_idx: [0, 1, 5, 3, 4, 2, 6],
            // Refresh nothing: this frame is here to be PLANNED, not to disturb
            // the ledger the assertions read.
            refresh_frame_flags: 0,
            ..Default::default()
        };
        let plan = planner
            .plan_frame(header, sequence, Vec::new(), Vec::new())
            .expect("an inter frame with a lost reference still plans");

        assert_eq!(
            plan.warnings,
            vec![PlanWarning::MissingReference {
                slot: 5,
                ref_index: 2
            }]
        );
        assert!(plan.refs[2].is_none(), "the lost name stays empty");
        let named: Vec<Option<u8>> = plan.refs.iter().map(|r| r.map(|p| p.slot)).collect();
        assert_eq!(
            named,
            vec![Some(0), Some(1), None, Some(3), Some(4), Some(2), Some(6)],
            "every surviving name must still sit at ITS OWN index — a compacted \
             list would read [0, 1, 3, 4, 2, 6] and rename four references"
        );
    }

    /// `RefFrameSignBias` must come out SPEC-indexed (bit 1 = `LAST_FRAME`), which
    /// the vendored parser's array is not.
    ///
    /// Recomputed here from `order_hints` — which the parser DOES index by
    /// reference name — through the spec's own `get_relative_dist` (5.9.3),
    /// transcribed rather than borrowed because cros-codecs' `helpers` module is
    /// private. So this does not restate [`RefState::of`]'s shift; it restates the
    /// spec, and the two must agree on every frame of the vector. Without the
    /// shift, ALTREF's bias lands on GOLDEN and `INTRA_FRAME` (bit 0, which the
    /// spec never sets) picks up LAST's.
    #[test]
    fn the_sign_bias_mask_is_spec_indexed_not_parser_indexed() {
        /// AV1 5.9.3 `get_relative_dist`, verbatim.
        fn get_relative_dist(enable_order_hint: bool, bits: i32, a: i32, b: i32) -> i32 {
            if !enable_order_hint {
                return 0;
            }
            let diff = a - b;
            let m = 1 << (bits - 1);
            (diff & (m - 1)) - (diff & m)
        }

        let mut planner = Av1Planner::new();
        let (mut frames, mut nonzero_masks, mut future_refs) = (0u32, 0u32, 0u32);
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                let h = &*plan.header;
                let seq = &*plan.sequence;
                let bits = seq.order_hint_bits_minus_1 + 1;
                let state = RefState::of(h);

                let mut expected = 0u8;
                if !h.frame_is_intra {
                    for name in 1..=REFS_PER_FRAME {
                        let dist = get_relative_dist(
                            seq.enable_order_hint,
                            bits,
                            h.order_hints[name] as i32,
                            h.order_hint as i32,
                        );
                        if dist > 0 {
                            expected |= 1 << name;
                            future_refs += 1;
                        }
                    }
                }
                assert_eq!(
                    state.ref_frame_sign_bias, expected,
                    "frame {frames}: sign-bias mask {:#010b} does not match the \
                     spec's own RefFrameSignBias[1..8] {expected:#010b}",
                    state.ref_frame_sign_bias
                );
                assert_eq!(
                    state.ref_frame_sign_bias & 1,
                    0,
                    "bit 0 is INTRA_FRAME and the spec never sets it — a set bit \
                     there is the parser's off-by-one leaking through"
                );
                if state.ref_frame_sign_bias != 0 {
                    nonzero_masks += 1;
                }
            }
        }
        assert_eq!(frames, 274);
        assert!(
            nonzero_masks > 0 && future_refs > 0,
            "this vector is the hidden-ALTREF one: if no frame ever biased a \
             reference into the future, this test compared zero against zero and \
             the shift above is untested"
        );
        eprintln!("frames {frames} · frames with a future reference {nonzero_masks}");
    }

    /// A reference carries ITS OWN frame type, not the frame reading it.
    #[test]
    fn a_reference_carries_its_own_frame_type() {
        let mut planner = Av1Planner::new();
        let mut mixed = 0u32;
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                for r in plan.refs.iter().flatten() {
                    if r.state.frame_type != plan.header.frame_type {
                        mixed += 1;
                    }
                }
            }
        }
        assert!(
            mixed > 0,
            "no frame of the vector ever referenced a picture of a DIFFERENT frame \
             type, so nothing here can tell the reference's own type from the \
             current frame's — the exact substitution this field exists to prevent"
        );
    }

    /// [`coded_cdef_sec_strength`] inverts the spec's in-place fixup — and the
    /// vendored vector really does code the value that needs it, on frame 0.
    ///
    /// Both halves matter. The mapping is three lines and could be asserted against
    /// itself forever; what makes it load-bearing is that the parser DOES hand out
    /// `4`, on the very first frame the parity leg compares, and on 68 of 274
    /// frames overall. If a re-synced vector ever stopped coding a secondary
    /// strength of 3, this test would be comparing a correction against a stream
    /// that never needs it, and the four hardware APIs' two-bit fields would be
    /// untested again.
    #[test]
    fn the_cdef_secondary_strength_is_the_coded_value() {
        // The fixup's inverse, and the identity everywhere else.
        assert_eq!(
            [0, 1, 2, 3, 4].map(coded_cdef_sec_strength),
            [0, 1, 2, 3, 3],
            "0..=2 pass through, the spec's 4 is the coded 3, and a hand-built 3 is \
             already coded"
        );

        let mut planner = Av1Planner::new();
        let (mut frames, mut needing_fixup, mut strengths) = (0u32, 0u32, 0u32);
        let mut frame0_raw: Vec<u32> = Vec::new();
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                let cdef = &plan.header.cdef_params;
                let coded = 1usize << cdef.cdef_bits;
                let mut any = false;
                for i in 0..coded {
                    for raw in [cdef.cdef_y_sec_strength[i], cdef.cdef_uv_sec_strength[i]] {
                        assert!(
                            raw <= 2 || raw == 4,
                            "frame {frames}: the parser can only hold 0, 1, 2 or the \
                             fixed-up 4 — {raw} means the vendored parse changed"
                        );
                        assert!(
                            coded_cdef_sec_strength(raw) <= 3,
                            "the corrected value must fit the two bits every hardware \
                             API gives it"
                        );
                        if raw == 4 {
                            any = true;
                            strengths += 1;
                        }
                    }
                }
                if any {
                    needing_fixup += 1;
                }
                if frames == 1 {
                    frame0_raw = cdef.cdef_y_sec_strength[..coded]
                        .iter()
                        .chain(cdef.cdef_uv_sec_strength[..coded].iter())
                        .copied()
                        .collect();
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            frame0_raw,
            vec![1, 2, 0, 4, 4, 0, 0, 0],
            "frame 0's four luma then four chroma secondary strengths — the first \
             frame the parity leg hashes, and it needs the correction"
        );
        assert_eq!(
            needing_fixup, 68,
            "68 of 274 frames of this vector carry a secondary strength the spec \
             fixed up; at zero the correction above is untested by any real stream"
        );
        eprintln!(
            "frames {frames} · frames needing the fixup {needing_fixup} · strengths \
             corrected {strengths}"
        );
    }

    #[test]
    fn an_access_unit_with_no_frame_is_refused() {
        let mut planner = Av1Planner::new();
        // A lone temporal delimiter: a valid OBU, no frame.
        assert_eq!(
            planner.plan_au(&[0x12, 0x00]).err(),
            Some(PlanError::NoFrame)
        );
    }
}
