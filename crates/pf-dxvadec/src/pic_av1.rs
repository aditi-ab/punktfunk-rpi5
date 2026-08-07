//! One AV1 [`AuPlan`] into the DXVA structures — M7's Windows conversion.
//!
//! The layouts it fills are measured against the Windows SDK's own `dxva.h`
//! ([`crate::dxva_av1`]); this module is where the AV1 frame header's meaning is
//! mapped onto them, and where the three places DXVA disagrees with the other
//! backends are handled.
//!
//! # The reference numbering — a FOURTH convention
//!
//! This program has now written down four spellings of "which pictures does this
//! frame use":
//!
//! * Vulkan H.265: DPB **slot** indices in `RefPicSetStCurr*`;
//! * DXVA H.265: **positions into `RefPicList[]`** in identically named arrays;
//! * VAAPI H.265: membership **flags** ORed onto each DPB entry;
//! * **DXVA AV1: two arrays that mean different things at once.**
//!   `frame_refs[7]` is indexed by reference NAME (`LAST`..`ALTREF`) and each entry
//!   carries a **reference SLOT** (`ref_frame_idx[name]`), that reference's own
//!   coded size, and that reference's own global motion; `RefFrameMapTextureIndex[8]`
//!   is indexed by that same slot and holds the **surface** — it states the whole
//!   reference store, the way `RefFrameList` does for the other two codecs. The
//!   driver dereferences one through the other, so the slot is the only thing
//!   `Index` may hold. Vulkan spells the first array's contents identically
//!   (`referenceNameSlotIndices` — slot indices by name); DXVA differs from it only
//!   in hanging the size and the warp off the same entry. Writing the surface into
//!   `Index` is not a refusal, it is a frame predicted from whatever picture sits
//!   in the slot numbered like that surface.
//!
//! # Global motion lives per reference
//!
//! Vulkan hangs one `StdVideoAV1GlobalMotion` block off the picture info, with an
//! eight-entry array inside it. DXVA puts each reference's warp parameters in that
//! reference's own `DXVA_PicEntry_AV1`. Both are indexed by reference NAME —
//! `global_motion_params()` loops `ref = LAST_FRAME..ALTREF_FRAME` — so the
//! Vulkan block is a straight copy and DXVA's per-entry read is
//! `gm_params[LAST_FRAME + name]`. Reading it by DPB SLOT instead is the exact
//! transposition that silently gives every warped reference somebody else's warp;
//! it agrees with the truth only while reference `i` happens to sit in slot `i+1`.

use pf_bitstream::av1::coded_cdef_sec_strength;
use pf_bitstream::av1::AuPlan;
use pf_bitstream::av1::FrameType;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::NUM_REF_SLOTS;
use pf_bitstream::av1::REFS_PER_FRAME;

use crate::dxva_av1::CdefAv1;
use crate::dxva_av1::CdefFlagsAv1;
use crate::dxva_av1::CdefStrength;
use crate::dxva_av1::CodingFlagsAv1;
use crate::dxva_av1::FilmGrainAv1;
use crate::dxva_av1::FilmGrainFlagsAv1;
use crate::dxva_av1::FormatFlagsAv1;
use crate::dxva_av1::GlobalMotionFlags;
use crate::dxva_av1::LoopFilterAv1;
use crate::dxva_av1::LoopFilterFlagsAv1;
use crate::dxva_av1::PicEntryAv1;
use crate::dxva_av1::PicParamsAv1;
use crate::dxva_av1::QuantizationAv1;
use crate::dxva_av1::QuantizationFlagsAv1;
use crate::dxva_av1::SegmentFeatureMask;
use crate::dxva_av1::SegmentationAv1;
use crate::dxva_av1::SegmentationFlagsAv1;
use crate::dxva_av1::TileAv1;
use crate::dxva_av1::TilesAv1;
use crate::dxva_av1::UNUSED_INDEX;
use crate::plan_bitstream;
use crate::Av1Bitstream;
use crate::Av1TileError;
use crate::SlotError;
use crate::SlotMap;

/// `DXVA_PicParams_AV1::tiles` holds at most 64 column and 64 row sizes.
pub const MAX_TILE_DIM: usize = 64;

/// As many `DXVA_Tile_AV1` records as one submission carries.
///
/// libavcodec's `MAX_TILES`, and its refusal is the whole comment: *"too many
/// tiles, exceeding all defined levels in the AV1 spec"* — `dxva2_av1_decode_slice`
/// answers `AVERROR(ENOSYS)` past it, and its `ctx_pic->tiles` is a fixed
/// 256-entry array. The 64x64 grid [`MAX_TILE_DIM`] admits 4096, which no AV1
/// level defines and no driver has been asked for.
pub const MAX_TILES: usize = 256;

/// `log2_restoration_unit_size` on a frame that restores nothing.
///
/// Not a meaningful size — every plane's `frame_restoration_type` is NONE and a
/// driver reading the field at all has nothing to apply it to. It is 8 because
/// that is what libavcodec's `dxva2_av1.c` writes, and 8 is the top of the range
/// `dxva.h` documents (6..8); the parser's own array is still zero here, whose
/// `trailing_zeros` would be 16.
const LOG2_RESTORATION_UNIT_SIZE_UNUSED: u16 = 8;

/// `qm_y`/`qm_u`/`qm_v` on a frame that uses no quantiser matrix.
///
/// `DXVA_PicParams_AV1::quantization` carries no `using_qmatrix` flag, so the three
/// indices have to say it themselves; `0xFF` is what libavcodec's `dxva2_av1.c`
/// writes and what `dxva.h` documents as the unused value. **Not** 0 — 0 selects a
/// real matrix.
const QM_UNUSED: u8 = 0xFF;

/// `LAST_FRAME` (AV1 spec): the first reference NAME, and the offset between a
/// position in `ref_frame_idx` and the index the spec's per-reference arrays
/// (global motion, order hints, sign bias) use. `INTRA_FRAME` is 0.
const LAST_FRAME: usize = 1;

/// Everything one AV1 `SubmitDecoderBuffers` call needs.
#[derive(Debug, Clone)]
pub struct DecodePlanDxvaAv1 {
    pub pic_params: PicParamsAv1,
    /// One record per **tile** — not per tile GROUP — in decode order across the
    /// frame's tile groups, exactly as libavcodec's `dxva2_av1.c` fills
    /// `ctx_pic->tiles[tile_num]` for `tile_num` in `tg_start..=tg_end`.
    ///
    /// `row`, `column` and `anchor_frame` are final. `DataOffset`/`DataSize` are
    /// ACCESS-UNIT-relative here and are replaced outright by
    /// [`mod@crate::pack_av1`], exactly as the H.264 and H.265 slice-control
    /// records are rebased by [`mod@crate::pack`].
    pub tiles: Vec<TileAv1>,
    /// Where the tiles and the tile-group regions are in the access unit — what
    /// the packer copies and what it rebases against.
    pub bitstream: Av1Bitstream,
    pub setup_slot: u8,
    pub setup_id: PicId,
    /// Pictures this frame's own `refresh_frame_flags` displaces from the store
    /// while THIS submission still NAMES them — their surfaces may not be recycled
    /// until the decode op has been issued, and the caller owes exactly that.
    ///
    /// AV1 applies `refresh_frame_flags` AFTER the frame is decoded (7.20), so
    /// `ref_frame_idx` resolves against the store as it stood BEFORE this frame and
    /// a frame that reads a slot it then overwrites is the ORDINARY case, not an
    /// exotic one: **268 of the vendored vector's 274 frames** do it, first at frame
    /// 6. Releasing such a picture inside this conversion — which is what the H.264
    /// and H.265 siblings do with their whole `removed` list, and what this one did
    /// until the parity harness caught it — hands its surface straight back to
    /// [`Self::setup_slot`], because [`SlotMap::assign`] takes the lowest free slot
    /// and the lowest free slot is the one just vacated. The submission then says
    /// `CurrPicTextureIndex = N` and `RefFrameMapTextureIndex[k] = N` in the same
    /// breath: decode into the surface you are predicting from.
    ///
    /// Neither vendored H.264 nor H.265 vector ever produces that shape (measured:
    /// zero on the 250-AU clips), which is why the eager release survived two
    /// hardware-proven codecs and opened on the first AV1 frame past the key frame's
    /// neighbourhood. The Vulkan rung carries the same contract for the same reason
    /// (`pf_vkdecode::pic_av1::DecodePlanVkAv1::release_after_decode`), and this
    /// rung's constraint is the STRICTER of the two: Vulkan binds only the
    /// references the frame names, while `RefFrameMapTextureIndex` declares the
    /// whole store, so every picture the store still names has to survive — not just
    /// the seven the frame reads.
    ///
    /// The ids are always a subset of the plan's `dpb.removed`, so applying them
    /// completes that plan's bookkeeping and never invents a removal. Empty on the
    /// overwhelming minority of frames that displace nothing they name; a caller
    /// that drops them leaks a surface per frame and runs the ledger dry within ten.
    pub release_after_decode: Vec<PicId>,
}

/// Why a plan cannot be expressed as DXVA AV1 buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToDxvaAv1Error {
    /// A `show_existing_frame` plan decodes nothing and has no submission.
    NoDecode,
    NoTiles,
    /// The access unit's tile OBUs could not be walked into per-tile payloads.
    Tiles(Av1TileError),
    /// The frame header's tile GRID and the tiles the access unit actually carried
    /// disagree — a dropped tile group, most likely, which nothing else reports.
    /// Submitting anyway declares `cols * rows` tiles over a shorter buffer.
    TileCountMismatch {
        /// Tile-control records built from the access unit's tile-group spans.
        records: usize,
        /// Tiles the bitstream walk found.
        walked: usize,
        /// `tile_cols * tile_rows` — what the picture parameters announce.
        grid: usize,
    },
    /// A reference the slot map does not hold.
    UnresolvedReference(PicId),
    /// More tile columns or rows than the picture parameters can express.
    TooManyTiles {
        cols: u32,
        rows: u32,
    },
    /// A field wider than its DXVA type.
    FieldOverflow {
        field: &'static str,
        value: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToDxvaAv1Error {
    fn from(e: SlotError) -> Self {
        PlanToDxvaAv1Error::Slot(e)
    }
}

impl std::fmt::Display for PlanToDxvaAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToDxvaAv1Error::NoDecode => {
                write!(f, "a show_existing_frame plan has no decode submission")
            }
            PlanToDxvaAv1Error::NoTiles => write!(f, "the frame carried no tile group"),
            PlanToDxvaAv1Error::Tiles(e) => write!(f, "tile walk: {e}"),
            PlanToDxvaAv1Error::TileCountMismatch {
                records,
                walked,
                grid,
            } => write!(
                f,
                "the frame header's tile grid is {grid} tiles; the access unit carried \
                 {walked} and produced {records} records"
            ),
            PlanToDxvaAv1Error::UnresolvedReference(id) => {
                write!(f, "reference picture {id} holds no DPB slot")
            }
            PlanToDxvaAv1Error::TooManyTiles { cols, rows } => write!(
                f,
                "{cols}x{rows} tiles exceed the {MAX_TILE_DIM}-entry picture parameters"
            ),
            PlanToDxvaAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its DXVA field")
            }
            PlanToDxvaAv1Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToDxvaAv1Error {}

fn narrow(field: &'static str, value: u32) -> Result<u8, PlanToDxvaAv1Error> {
    u8::try_from(value).map_err(|_| PlanToDxvaAv1Error::FieldOverflow { field, value })
}

fn narrow16(field: &'static str, value: u32) -> Result<u16, PlanToDxvaAv1Error> {
    u16::try_from(value).map_err(|_| PlanToDxvaAv1Error::FieldOverflow { field, value })
}

/// Convert one planned AV1 frame.
///
/// `au` is the access unit `plan` was planned from: the tile-control records need
/// the per-TILE byte ranges, and finding those means walking each tile group's
/// header and its `tile_size_minus_1` fields — which is a walk over the bitstream,
/// not over the plan. (The H.264 and H.265 conversions need no such thing: a slice
/// NALU's range IS what the driver reads.)
///
/// ⚠ There is no `status_id` parameter, unlike the H.264 and H.265 conversions:
/// `StatusReportFeedbackNumber` is left **zero** for AV1 (see where it is filled
/// below), so a caller passing one would be handing over a number that goes
/// nowhere. Nothing mutates `slots` until every fallible step has passed.
pub fn plan_to_dxva_av1(
    au: &[u8],
    plan: &AuPlan,
    slots: &mut SlotMap,
) -> Result<DecodePlanDxvaAv1, PlanToDxvaAv1Error> {
    let setup_id = plan.dpb.stored.ok_or(PlanToDxvaAv1Error::NoDecode)?;
    if plan.tiles.is_empty() {
        return Err(PlanToDxvaAv1Error::NoTiles);
    }
    let h = &*plan.header;
    let seq = &*plan.sequence;

    // --- resolve, before any mutation ------------------------------------
    // The reference store, by SLOT. This is `RefFrameMapTextureIndex`, and it is a
    // statement about the whole store — the same thing `RefFrameList` is for the
    // other two codecs, and the reason an LTR no slice names still has to appear.
    let mut ref_frame_map = [UNUSED_INDEX; NUM_REF_SLOTS];
    for r in &plan.dpb_refs {
        let slot = slots
            .slot_of(r.id)
            .ok_or(PlanToDxvaAv1Error::UnresolvedReference(r.id))?;
        ref_frame_map[usize::from(r.slot)] = slot;
    }

    // The seven reference NAMES. Each carries a reference SLOT, that reference's
    // own coded size, and that reference's own global motion (module docs).
    //
    // `plan.refs` is indexed BY NAME and a lost reference leaves a hole, so the
    // name comes off the iterator and holes are skipped — they keep DXVA's
    // `UNUSED_INDEX`. A compacted list (which is what this loop used to receive)
    // renamed every reference after the first loss.
    let mut frame_refs = [PicEntryAv1::zeroed(); REFS_PER_FRAME];
    let inter = !matches!(
        h.frame_type,
        FrameType::KeyFrame | FrameType::IntraOnlyFrame
    );
    if inter {
        for (name, r) in plan.refs.iter().enumerate() {
            let Some(r) = r else { continue };
            // The reference must still be in the store — this rung's ledger has to
            // hold a surface for it, or `ref_frame_map` above named nothing at
            // `r.slot` and the driver would follow `Index` to an empty entry.
            slots
                .slot_of(r.id)
                .ok_or(PlanToDxvaAv1Error::UnresolvedReference(r.id))?;
            // ⚠ Global motion is indexed by reference NAME, never by DPB slot.
            // AV1's `global_motion_params()` loops `ref = LAST_FRAME..ALTREF_FRAME`
            // and the vendored parser stores it that way; libavcodec's
            // `dxva2_av1.c` reads `gm_params[AV1_REF_FRAME_LAST + i]` for
            // `frame_refs[i]`. Reading by slot instead happens to agree only while
            // reference `i` sits in slot `i + 1`, and silently hands every warped
            // reference somebody else's warp the moment it does not.
            let gm_name = LAST_FRAME + name;
            let gm = &h.global_motion_params;
            frame_refs[name] = PicEntryAv1 {
                // ⚠ The REFERENCE's own size, never this frame's. libavcodec:
                // `pp->frame_refs[i].width = ref_frame->width` off the reference's
                // `AVFrame`. AV1 lets every frame pick its own size up to the
                // sequence maximum, and these two fields are how the driver knows
                // to SCALE motion out of a differently-sized reference (7.11.3.3
                // `xStep`/`yStep` are computed from `RefUpscaledWidth[refIdx]`).
                // Sending the current frame's size makes every scaled prediction
                // read as unscaled, and agrees with the truth only while nothing
                // resizes.
                width: r.state.upscaled_width,
                height: r.state.frame_height,
                wmmat: gm.gm_params[gm_name],
                global_motion_flags: GlobalMotionFlags {
                    // `warp_valid` is the parser's `setup_shear` verdict — a warp
                    // whose shear parameters are out of range is unusable, and
                    // DXVA's flag is the inverse.
                    wminvalid: !gm.warp_valid[gm_name],
                    wmtype: gm.gm_type[gm_name] as u8,
                }
                .pack(),
                // ⚠⚠ The AV1 reference SLOT — `ref_frame_idx[name]`, 0..8 — and NOT
                // the surface index. `Index` is a subscript INTO
                // `RefFrameMapTextureIndex`, which the loop above already filled by
                // slot, so the driver resolves the surface itself. libavcodec:
                // `pp->frame_refs[i].Index = ref_frame ? ref_idx : 0xFF` with
                // `ref_idx = frame_header->ref_frame_idx[i]`; Chromium's
                // `d3d11_av1_accelerator.cc` writes the same thing. `RefPic::slot`
                // IS that index (`Av1Planner` reads the store at
                // `ref_frame_idx[name]` and the entry carries the slot it sits in).
                index: r.slot,
                reserved16: 0,
            };
        }
    }

    // --- tiles ------------------------------------------------------------
    let t = &h.tile_info;
    if t.tile_cols as usize > MAX_TILE_DIM || t.tile_rows as usize > MAX_TILE_DIM {
        return Err(PlanToDxvaAv1Error::TooManyTiles {
            cols: t.tile_cols,
            rows: t.tile_rows,
        });
    }
    let mut tiles = TilesAv1::zeroed();
    tiles.cols = narrow("tiles.cols", t.tile_cols)?;
    tiles.rows = narrow("tiles.rows", t.tile_rows)?;
    tiles.context_update_id = t.context_update_tile_id as u16;
    // `widths`/`heights` are each tile's size in SUPERBLOCKS — a count, where the
    // parser (and the AV1 syntax) records `*_in_sbs_minus_1`. ⚠ The `+ 1` is the
    // whole of it: libavcodec's `dxva2_av1.c` writes
    // `pp->tiles.widths[i] = frame_header->width_in_sbs_minus_1[i] + 1`, and
    // Chromium's `d3d11_av1_accelerator.cc` independently writes a count too.
    // Sending the coded minus-one value understates EVERY tile by one superblock,
    // on every frame — the vendored vector is five superblocks wide in one tile
    // and would have told the driver four.
    for i in 0..t.tile_cols as usize {
        tiles.widths[i] = narrow16("tiles.widths", t.width_in_sbs_minus_1[i].saturating_add(1))?;
    }
    for i in 0..t.tile_rows as usize {
        tiles.heights[i] = narrow16(
            "tiles.heights",
            t.height_in_sbs_minus_1[i].saturating_add(1),
        )?;
    }

    // The tile records. ONE PER TILE — `dxva2_av1.c` sizes its array
    // `tile_cols * tile_rows` and fills it `for (tile_num = h->tg_start; tile_num
    // <= h->tg_end; tile_num++)`, so a frame whose four tiles arrive in a single
    // tile group is four records with four different `row`/`column` pairs. One
    // record per tile GROUP pointing at the whole OBU is not a coarser version of
    // this: it hands the driver the OBU header and the tile-group header as
    // entropy-coded tile data.
    //
    // The BYTES come from the walk (`plan_bitstream`, shared with the Vulkan rung)
    // and the tile NUMBERING comes from the plan's own tile-group spans, which is
    // how libav numbers them.
    //
    // ⚠ The cross-check that matters is against the tile GRID, not between those
    // two: both are computed from the same `tg_start`/`tg_end` pair, so comparing
    // them is comparing an expression with itself. `tile_cols * tile_rows` is an
    // independent statement — it comes from the frame header, it is what
    // `pic_params.tiles.cols`/`rows` announce to the driver, and it is exactly
    // libavcodec's own guard (`ctx_pic->tile_count = frame_header->tile_cols *
    // frame_header->tile_rows; if (ctx_pic->tile_count > MAX_TILES) return
    // AVERROR(ENOSYS)`).
    //
    // The failure it catches is a DROPPED TILE GROUP: an access unit that lost one
    // in transit carries no `TruncatedAu` warning (the OBU walk simply never sees
    // it), so nothing else in this rung notices — and the submission then declares
    // a grid the tile-control buffer has too few records for, which is a driver
    // reading past `DataSize`.
    let cols = t.tile_cols.max(1);
    let rows = t.tile_rows.max(1);
    let grid = (cols as usize).saturating_mul(rows as usize);
    if grid > MAX_TILES {
        return Err(PlanToDxvaAv1Error::Tiles(Av1TileError::TooManyTiles {
            tiles: grid,
        }));
    }
    let bitstream = plan_bitstream(au, &plan.tiles, h).map_err(PlanToDxvaAv1Error::Tiles)?;
    let mut tile_records = Vec::with_capacity(bitstream.tiles.len());
    for tg in &plan.tiles {
        // A group whose end precedes its start is malformed; the walk refuses it
        // too, so this saturates rather than growing a second refusal path.
        let count = tg.tg_end.saturating_sub(tg.tg_start).saturating_add(1);
        for step in 0..count {
            let tile_num = tg.tg_start.saturating_add(step);
            tile_records.push(TileAv1 {
                // Filled from the walk below, in ACCESS-UNIT coordinates;
                // `pack_av1` then replaces both fields with buffer-relative ones
                // (field docs).
                data_offset: 0,
                data_size: 0,
                row: (tile_num / cols) as u16,
                column: (tile_num % cols) as u16,
                reserved16: 0,
                // libavcodec writes `0xFF` on every tile: `anchor_frame` selects a
                // reference for large-scale tile decoding, which no punktfunk
                // stream and no conformance vector here uses.
                anchor_frame: UNUSED_INDEX,
                reserved8: 0,
            });
        }
    }
    if tile_records.len() != grid || bitstream.tiles.len() != grid {
        return Err(PlanToDxvaAv1Error::TileCountMismatch {
            records: tile_records.len(),
            walked: bitstream.tiles.len(),
            grid,
        });
    }
    for (record, tile) in tile_records.iter_mut().zip(&bitstream.tiles) {
        record.data_offset =
            u32::try_from(tile.start).map_err(|_| PlanToDxvaAv1Error::FieldOverflow {
                field: "tile.DataOffset",
                value: u32::MAX,
            })?;
        record.data_size = u32::try_from(tile.end - tile.start).map_err(|_| {
            PlanToDxvaAv1Error::FieldOverflow {
                field: "tile.DataSize",
                value: u32::MAX,
            }
        })?;
    }

    // --- the blocks -------------------------------------------------------
    let lf = &h.loop_filter_params;
    let mut loop_filter = LoopFilterAv1::zeroed();
    loop_filter.filter_level = [lf.loop_filter_level[0], lf.loop_filter_level[1]];
    loop_filter.filter_level_u = lf.loop_filter_level[2];
    loop_filter.filter_level_v = lf.loop_filter_level[3];
    loop_filter.sharpness_level = lf.loop_filter_sharpness;
    loop_filter.control_flags = LoopFilterFlagsAv1 {
        mode_ref_delta_enabled: lf.loop_filter_delta_enabled,
        mode_ref_delta_update: lf.loop_filter_delta_update,
        delta_lf_multi: lf.delta_lf_multi,
        delta_lf_present: lf.delta_lf_present,
    }
    .pack();
    loop_filter.ref_deltas = lf.loop_filter_ref_deltas;
    loop_filter.mode_deltas = lf.loop_filter_mode_deltas;
    loop_filter.delta_lf_res = lf.delta_lf_res;
    // Loop restoration.
    //
    // ⚠ DXVA wants the LOG2 of the unit size where the parser records the size
    // itself — and the parser records NOTHING when restoration is off. AV1 5.9.20
    // only computes `LoopRestorationSize` inside `if ( UsesLr )`, so on a frame with
    // every plane's restoration type NONE the vendored parser's array is still
    // `[0, 0, 0]`, and `0u16.trailing_zeros()` is **16** — a restoration unit of
    // 65536 samples, in a field `dxva.h` documents as 6, 7 or 8. That is 271 of the
    // vendored vector's 274 frames.
    //
    // libavcodec's `dxva2_av1.c` sends `uses_lr ? 6 + lr_unit_shift : 8` for luma
    // and `uses_lr ? 6 + lr_unit_shift - lr_uv_shift : 8` for the two chroma planes,
    // and libavcodec is the implementation every driver was validated against, so
    // the OFF value is 8 rather than 0 or 16. With restoration on, the parser's own
    // `loop_restoration_size[i]` already carries the per-plane `>> lr_uv_shift`, so
    // its `trailing_zeros` IS `6 + lr_unit_shift - lr_uv_shift` — the two agree
    // wherever the field is read at all.
    let lr = &h.loop_restoration_params;
    for i in 0..3 {
        loop_filter.frame_restoration_type[i] = lr.frame_restoration_type[i] as u8;
        loop_filter.log2_restoration_unit_size[i] = if lr.uses_lr {
            lr.loop_restoration_size[i].trailing_zeros() as u16
        } else {
            LOG2_RESTORATION_UNIT_SIZE_UNUSED
        };
    }

    let q = &h.quantization_params;
    let mut quantization = QuantizationAv1::zeroed();
    quantization.control_flags = QuantizationFlagsAv1 {
        delta_q_present: q.delta_q_present,
        delta_q_res: narrow("delta_q_res", q.delta_q_res)?,
    }
    .pack();
    quantization.base_qindex = narrow("base_qindex", q.base_q_idx)?;
    quantization.y_dc_delta_q = q.delta_q_y_dc as i8;
    quantization.u_dc_delta_q = q.delta_q_u_dc as i8;
    quantization.v_dc_delta_q = q.delta_q_v_dc as i8;
    quantization.u_ac_delta_q = q.delta_q_u_ac as i8;
    quantization.v_ac_delta_q = q.delta_q_v_ac as i8;
    // ⚠ The quantiser-matrix indices need a SENTINEL when the frame uses no matrix.
    // `DXVA_PicParams_AV1::quantization` has no `using_qmatrix` bit — 0xFF is the
    // only way to say "none" — and the vendored parser only assigns `qm_y`/`qm_u`/
    // `qm_v` inside `if using_qmatrix`, so a frame without one carries **0**, which
    // is a perfectly valid matrix index. Left alone the driver dequantizes against
    // matrix 0 on every such frame, which is every frame of both vendored vectors.
    // libavcodec: `pp->quantization.qm_y = frame_header->using_qmatrix ?
    // frame_header->qm_y : 0xFF` (Chromium the same).
    let (qm_y, qm_u, qm_v) = if q.using_qmatrix {
        (
            narrow("qm_y", q.qm_y)?,
            narrow("qm_u", q.qm_u)?,
            narrow("qm_v", q.qm_v)?,
        )
    } else {
        (QM_UNUSED, QM_UNUSED, QM_UNUSED)
    };
    quantization.qm_y = qm_y;
    quantization.qm_u = qm_u;
    quantization.qm_v = qm_v;

    let c = &h.cdef_params;
    let mut cdef = CdefAv1::zeroed();
    cdef.control_flags = CdefFlagsAv1 {
        damping: narrow("cdef_damping", c.cdef_damping.saturating_sub(3))?,
        bits: narrow("cdef_bits", c.cdef_bits)?,
    }
    .pack();
    // Two fields to a byte (module docs) — not the parallel arrays AV1's syntax
    // and Vulkan's Std block use.
    //
    // ⚠ `secondary` gets TWO bits here, and the parser's value does not fit them:
    // AV1 5.9.19 rewrites the syntax element in place (a coded 3 becomes 4) and
    // cros-codecs follows the spec, while `DXVA_PicParams_AV1` — like VA-API,
    // NVDEC and Vulkan — wants the coded two-bit read, which is what libavcodec's
    // `dxva2_av1.c` sends. Passing the parser's 4 through `pack`'s `& 0x3` would
    // turn the STRONGEST secondary filter into NO filter, silently, on every frame
    // that codes one. `coded_cdef_sec_strength` is the inverse; its docs carry the
    // evidence.
    for i in 0..8 {
        cdef.y_strengths[i] = CdefStrength {
            primary: c.cdef_y_pri_strength[i] as u8,
            secondary: coded_cdef_sec_strength(c.cdef_y_sec_strength[i]),
        }
        .pack();
        cdef.uv_strengths[i] = CdefStrength {
            primary: c.cdef_uv_pri_strength[i] as u8,
            secondary: coded_cdef_sec_strength(c.cdef_uv_sec_strength[i]),
        }
        .pack();
    }

    let s = &h.segmentation_params;
    let mut segmentation = SegmentationAv1::zeroed();
    segmentation.control_flags = SegmentationFlagsAv1 {
        enabled: s.segmentation_enabled,
        update_map: s.segmentation_update_map,
        update_data: s.segmentation_update_data,
        temporal_update: s.segmentation_temporal_update,
    }
    .pack();
    for seg in 0..8 {
        let e = &s.feature_enabled[seg];
        segmentation.feature_mask[seg] = SegmentFeatureMask {
            alt_q: e[0],
            alt_lf_y_v: e[1],
            alt_lf_y_h: e[2],
            alt_lf_u: e[3],
            alt_lf_v: e[4],
            ref_frame: e[5],
            skip: e[6],
            globalmv: e[7],
        }
        .pack();
        segmentation.feature_data[seg] = s.feature_data[seg];
    }

    // Film grain: only where the sequence enables it AND the frame applies it —
    // the same gate the Vulkan conversion uses, and for the same reason.
    let fg_on = seq.film_grain_params_present && h.film_grain_params.apply_grain;
    let mut film_grain = FilmGrainAv1::zeroed();
    if fg_on {
        let fg = &h.film_grain_params;
        film_grain.control_flags = FilmGrainFlagsAv1 {
            apply_grain: true,
            scaling_shift_minus8: fg.grain_scaling_minus_8,
            chroma_scaling_from_luma: fg.chroma_scaling_from_luma,
            ar_coeff_lag: narrow("ar_coeff_lag", fg.ar_coeff_lag)?,
            ar_coeff_shift_minus6: fg.ar_coeff_shift_minus_6,
            grain_scale_shift: fg.grain_scale_shift,
            overlap_flag: fg.overlap_flag,
            clip_to_restricted_range: fg.clip_to_restricted_range,
            matrix_coeff_is_identity: seq.color_config.matrix_coefficients as u32 == 0,
        }
        .pack();
        film_grain.grain_seed = fg.grain_seed;
        // ⚠ DXVA wants [value, scaling] PAIRS where the parser (and Vulkan) keep
        // two parallel arrays. The counts are bounded by the DXVA capacity, and an
        // over-count is refused rather than truncated: fewer scaling points than
        // the stream declared is different grain, not less grain.
        let pts = |name: &'static str, n: u8, cap: usize| -> Result<usize, PlanToDxvaAv1Error> {
            if usize::from(n) > cap {
                return Err(PlanToDxvaAv1Error::FieldOverflow {
                    field: name,
                    value: u32::from(n),
                });
            }
            Ok(usize::from(n))
        };
        let ny = pts(
            "num_y_points",
            fg.num_y_points,
            film_grain.scaling_points_y.len(),
        )?;
        let ncb = pts(
            "num_cb_points",
            fg.num_cb_points,
            film_grain.scaling_points_cb.len(),
        )?;
        let ncr = pts(
            "num_cr_points",
            fg.num_cr_points,
            film_grain.scaling_points_cr.len(),
        )?;
        for i in 0..ny {
            film_grain.scaling_points_y[i] = [fg.point_y_value[i], fg.point_y_scaling[i]];
        }
        for i in 0..ncb {
            film_grain.scaling_points_cb[i] = [fg.point_cb_value[i], fg.point_cb_scaling[i]];
        }
        for i in 0..ncr {
            film_grain.scaling_points_cr[i] = [fg.point_cr_value[i], fg.point_cr_scaling[i]];
        }
        film_grain.num_y_points = fg.num_y_points;
        film_grain.num_cb_points = fg.num_cb_points;
        film_grain.num_cr_points = fg.num_cr_points;
        film_grain
            .ar_coeffs_y
            .copy_from_slice(&fg.ar_coeffs_y_plus_128[..24]);
        film_grain
            .ar_coeffs_cb
            .copy_from_slice(&fg.ar_coeffs_cb_plus_128[..25]);
        film_grain
            .ar_coeffs_cr
            .copy_from_slice(&fg.ar_coeffs_cr_plus_128[..25]);
        film_grain.cb_mult = fg.cb_mult;
        film_grain.cb_luma_mult = fg.cb_luma_mult;
        film_grain.cr_mult = fg.cr_mult;
        film_grain.cr_luma_mult = fg.cr_luma_mult;
        film_grain.cb_offset = fg.cb_offset as i16;
        film_grain.cr_offset = fg.cr_offset as i16;
    }

    // --- mutations, after every fallible step -----------------------------
    // ⚠ A picture this submission NAMES may be displaced by this same frame's
    // refresh. Its surface is still in `ref_frame_map` above, so releasing it here
    // would hand that very surface to `setup_slot` below and the frame would decode
    // into a picture it predicts from. Held back for the caller instead — see
    // `DecodePlanDxvaAv1::release_after_decode` for the measurement and for why
    // this rung's test is `dpb_refs` (the whole store `RefFrameMapTextureIndex`
    // declares) rather than the Vulkan rung's narrower `refs`.
    let release_after_decode: Vec<PicId> = plan
        .dpb
        .removed
        .iter()
        .copied()
        .filter(|id| *id != setup_id && plan.dpb_refs.iter().any(|r| r.id == *id))
        .collect();
    for &id in &plan.dpb.removed {
        if id == setup_id || release_after_decode.contains(&id) {
            continue;
        }
        let _ = slots.release(id);
    }
    let setup_slot = match slots.slot_of(setup_id) {
        Some(existing) => existing,
        None => slots.assign(setup_id)?,
    };

    let color = &seq.color_config;
    let mut pic_params = PicParamsAv1::zeroed();
    // ⚠ UPSCALED width, where libavcodec sends the CODED one — a divergence that is
    // inert on every stream that exists here and is written down rather than
    // "fixed" because nothing can measure it.
    //
    // `dxva2_av1.c` sends `avctx->width`, and `update_context_with_frame_header`
    // sets that from `frame_width_minus_1 + 1` — FrameWidth, the pre-superres coded
    // width. The same goes for `frame_refs[i].width`, which libav reads off the
    // reference's `AVFrame`. With superres OFF the two are equal by definition
    // (7.20: `UpscaledWidth = FrameWidth` when `use_superres` is 0), which is every
    // frame of both vendored vectors and every frame a punktfunk host emits — no
    // encoder in this program codes superres. So the 250/250 parity result on two
    // vendors says nothing either way about which is right, and changing it would
    // be an unmeasured change to a rung that is finally proven. Revisit with a
    // superres vector and a driver-by-driver measurement, not by reading.
    pic_params.width = h.upscaled_width;
    pic_params.height = h.frame_height;
    pic_params.max_width = u32::from(seq.max_frame_width_minus_1) + 1;
    pic_params.max_height = u32::from(seq.max_frame_height_minus_1) + 1;
    pic_params.curr_pic_texture_index = setup_slot;
    // The superres denominator as DXVA wants it: the real one, not the coded one,
    // and SUPERRES_NUM when superres is off.
    pic_params.superres_denom = if h.use_superres {
        narrow("superres_denom", h.superres_denom)?
    } else {
        SUPERRES_NUM
    };
    pic_params.bitdepth = if color.high_bitdepth {
        if color.twelve_bit {
            12
        } else {
            10
        }
    } else {
        8
    };
    pic_params.seq_profile = seq.seq_profile as u8;
    pic_params.tiles = tiles;
    pic_params.coding = CodingFlagsAv1 {
        use_128x128_superblock: seq.use_128x128_superblock,
        intra_edge_filter: seq.enable_intra_edge_filter,
        interintra_compound: seq.enable_interintra_compound,
        masked_compound: seq.enable_masked_compound,
        warped_motion: h.allow_warped_motion,
        dual_filter: seq.enable_dual_filter,
        jnt_comp: seq.enable_jnt_comp,
        screen_content_tools: h.allow_screen_content_tools != 0,
        integer_mv: h.force_integer_mv != 0,
        cdef: seq.enable_cdef,
        restoration: seq.enable_restoration,
        film_grain: seq.film_grain_params_present,
        intrabc: h.allow_intrabc,
        high_precision_mv: h.allow_high_precision_mv,
        switchable_motion_mode: h.is_motion_mode_switchable,
        filter_intra: seq.enable_filter_intra,
        disable_frame_end_update_cdf: h.disable_frame_end_update_cdf,
        disable_cdf_update: h.disable_cdf_update,
        reference_mode: h.reference_select,
        skip_mode: h.skip_mode_present,
        reduced_tx_set: h.reduced_tx_set,
        superres: h.use_superres,
        tx_mode: h.tx_mode as u8,
        use_ref_frame_mvs: h.use_ref_frame_mvs,
        enable_ref_frame_mvs: seq.enable_ref_frame_mvs,
        // ⚠ A literal 1, and NOT `refresh_frame_flags != 0`. libavcodec writes
        // `pp->coding.reference_frame_update = 1` unconditionally; Chromium writes
        // `!(show_existing_frame && frame_type == KEY_FRAME)`, which is also 1
        // everywhere this function runs (a `show_existing_frame` unit decodes
        // nothing and is refused above with `NoDecode`). So both references agree on
        // the value for every frame that reaches here, and a frame refreshing no
        // slot — legal AV1, and what `refresh_frame_flags != 0` would have sent 0
        // for — is not the exception either.
        reference_frame_update: true,
    }
    .pack();
    pic_params.format = FormatFlagsAv1 {
        frame_type: h.frame_type as u8,
        show_frame: h.show_frame,
        showable_frame: h.showable_frame,
        subsampling_x: color.subsampling_x,
        subsampling_y: color.subsampling_y,
        mono_chrome: color.mono_chrome,
    }
    .pack();
    pic_params.primary_ref_frame = narrow("primary_ref_frame", h.primary_ref_frame)?;
    pic_params.order_hint = narrow("order_hint", h.order_hint)?;
    pic_params.order_hint_bits = if seq.enable_order_hint {
        // The parser types this one signed; a negative value would be a parse bug,
        // and turning it into a huge unsigned one here would hide that.
        narrow(
            "order_hint_bits",
            u32::try_from(seq.order_hint_bits_minus_1).map_err(|_| {
                PlanToDxvaAv1Error::FieldOverflow {
                    field: "order_hint_bits_minus_1",
                    value: 0,
                }
            })? + 1,
        )?
    } else {
        0
    };
    pic_params.frame_refs = frame_refs;
    pic_params.ref_frame_map_texture_index = ref_frame_map;
    pic_params.loop_filter = loop_filter;
    pic_params.quantization = quantization;
    pic_params.cdef = cdef;
    pic_params.interp_filter = h.interpolation_filter as u8;
    pic_params.segmentation = segmentation;
    pic_params.film_grain = film_grain;
    // ⚠ `StatusReportFeedbackNumber` stays ZERO — the `zeroed()` value, written
    // nowhere. This is AV1-SPECIFIC: libavcodec DOES tag its H.264 and HEVC
    // submissions, and `dxva2_av1.c` alone has the line commented out with the
    // reason —
    //
    //     // XXX: Setting the StatusReportFeedbackNumber breaks decoding on some
    //     // drivers (tested on NVIDIA 457.09)
    //     // Status Reporting is not used by FFmpeg, hence not providing a number
    //     // does not cause any issues
    //     //pp->StatusReportFeedbackNumber = 1 + DXVA_CONTEXT_REPORT_ID(avctx, ctx)++;
    //
    // Chromium's `d3d11_av1_accelerator.cc` reaches the same place from the other
    // direction: "should not be equal to 0 ... but it crashes :|". Two independent
    // implementations both ship the zero, so this rung ships it too — and does not
    // even accept a number to drop (fn docs).

    Ok(DecodePlanDxvaAv1 {
        pic_params,
        tiles: tile_records,
        bitstream,
        setup_slot,
        setup_id,
        release_after_decode,
    })
}

/// `SUPERRES_NUM` (AV1 spec): the denominator that means "no upscaling".
const SUPERRES_NUM: u8 = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptors::descriptors_av1;
    use crate::descriptors::BUFFER_BITSTREAM;
    use crate::descriptors::BUFFER_PICTURE_PARAMETERS;
    use crate::descriptors::BUFFER_SLICE_CONTROL;
    use crate::dxva::BITSTREAM_ALIGN;
    use crate::pack_av1::pack_av1;
    use crate::pack_av1::packed_size_av1;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::Av1Planner;

    const AV1_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// Convert one plan the way the RUNG must: the conversion, then the releases it
    /// defers past the decode op ([`DecodePlanDxvaAv1::release_after_decode`]).
    ///
    /// Not a convenience — it is the caller's half of the contract, and the same
    /// helper the Vulkan rung's tests carry for the same reason. A loop that
    /// converts without it holds a surface on 268 of this vector's 274 frames and
    /// runs the nine-slot ledger dry inside ten.
    fn convert(au: &[u8], plan: &AuPlan, slots: &mut SlotMap) -> DecodePlanDxvaAv1 {
        let dx = plan_to_dxva_av1(au, plan, slots).expect("the clean vector converts");
        for &id in &dx.release_after_decode {
            assert!(
                slots.release(id),
                "a deferred release named picture {id}, which holds no surface"
            );
        }
        dx
    }

    /// The decode target never shares a surface with a picture the submission names.
    ///
    /// The defect this pins is the one the Windows parity harness caught and nothing
    /// on the CPU could see. AV1 applies `refresh_frame_flags` AFTER the frame is
    /// decoded (7.20), so a frame that reads a slot it then overwrites is ordinary —
    /// **268 of this vector's 274 frames**, first at frame 6 — and releasing the
    /// displaced picture inside the conversion handed its surface straight to
    /// `setup_slot`, because [`SlotMap::assign`] takes the lowest free slot and the
    /// lowest free slot is the one just vacated. The submission then said
    /// `CurrPicTextureIndex = N` and `RefFrameMapTextureIndex[k] = N` at once.
    ///
    /// Measured on hardware before the fix: Intel Arc decoded 245 of 250 delivered
    /// frames wrong (47% of luma at the first bad frame, max |delta| 242, chroma
    /// wrong too — a frame predicted from the wrong picture), and the only late
    /// frame it got right was the one intra frame, which names no reference and so
    /// could not alias. NVIDIA tolerated it.
    ///
    /// The assertion is against the WHOLE STORE, not just the seven names this frame
    /// reads: `RefFrameMapTextureIndex` declares every occupied slot, so a driver is
    /// entitled to consult one the frame never names.
    #[test]
    fn the_decode_target_never_aliases_a_surface_the_submission_names() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut deferring, mut deferred) = (0u32, 0u32, 0u32);
        let mut peak = 0usize;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                // Deliberately NOT `convert` — this test applies the deferred
                // releases itself, after checking each one.
                let dx = plan_to_dxva_av1(packet, &plan, &mut slots).expect("the vector converts");
                frames += 1;
                peak = peak.max(slots.active());

                // `#[repr(packed)]` — copy the fields out before reading them.
                let curr = dx.pic_params.curr_pic_texture_index;
                let store = dx.pic_params.ref_frame_map_texture_index;
                assert!(
                    store.iter().all(|surface| *surface != curr),
                    "frame {frames}: surface {curr} is both CurrPicTextureIndex and a \
                     RefFrameMapTextureIndex entry — the frame decodes into a picture \
                     it predicts from"
                );
                // Every deferred id is one this plan really removed AND the store
                // really names — never an invented removal, never a live picture.
                for &id in &dx.release_after_decode {
                    assert!(
                        plan.dpb.removed.contains(&id),
                        "frame {frames}: deferred picture {id} is not in this plan's \
                         removed list"
                    );
                    assert!(
                        plan.dpb_refs.iter().any(|r| r.id == id),
                        "frame {frames}: picture {id} is deferred without being named \
                         by the store — only a picture the submission points at earns \
                         the reprieve"
                    );
                    assert!(
                        slots.slot_of(id).is_some(),
                        "frame {frames}: a deferred picture must still hold its surface"
                    );
                }
                if !dx.release_after_decode.is_empty() {
                    deferring += 1;
                    deferred += dx.release_after_decode.len() as u32;
                }
                for &id in &dx.release_after_decode {
                    assert!(slots.release(id));
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            deferring, 268,
            "268 of 274 frames of this vector displace a picture their own submission \
             names; at zero this test compares an empty list against itself and the \
             deferral could be deleted without a single assertion noticing"
        );
        assert_eq!(deferred, 268, "one displaced picture per frame here");
        // The nine-slot ledger is `NUM_REF_SLOTS + 1`, and holding a displaced
        // picture one frame longer is exactly what that spare is for — the pool is
        // allocated `SlotMap::capacity()` surfaces (`pf_dxvadec::pool_size`), so a
        // peak above it would be a submission naming a surface that does not exist.
        assert!(
            peak <= slots.capacity(),
            "peak {peak} surfaces held exceeds the {} the pool allocates",
            slots.capacity()
        );
        eprintln!(
            "frames {frames} · deferring {deferring} · peak surfaces held {peak}/{}",
            slots.capacity()
        );
    }

    /// The whole vector, converted **and packed** — the closest a CPU gate gets to
    /// the hardware leg, and the test that would have caught the defect this
    /// module shipped with.
    ///
    /// The load-bearing assertion is the last one: the bytes each
    /// `DXVA_Tile_AV1` addresses inside the packed buffer must equal that tile's
    /// payload in the access unit. A record pointing at the whole tile-group OBU
    /// satisfies every OTHER check here — it is in range, it is inside the buffer,
    /// its size is consistent — and hands the driver the OBU header, the frame
    /// header and the tile-group header as entropy-coded tile data. There is no
    /// way to see that from the picture parameters, and no way to see it from a
    /// smoke test either: it decodes, and it decodes to noise.
    ///
    /// ⚠ That assertion is nonetheless WEAKER than it looks, which is why the
    /// tile-group ARITHMETIC is checked separately below. `pack_av1` computes a
    /// record's offset as `base + (tile.start - group.start)` from the very ranges
    /// this compares against, so the two sides descend from one expression: a walk
    /// that mistook where a tile begins satisfies it exactly. The independent
    /// statement is `tile_group_obu()`'s own accounting — every tile's payload plus
    /// one `TileSizeBytes` field per tile EXCEPT THE LAST fills the group's region
    /// with nothing over and nothing short — and it is a fact about the bitstream
    /// rather than about the packer.
    #[test]
    fn the_whole_vendored_vector_packs_into_a_three_buffer_submission() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut dst = vec![0u8; 1 << 20];
        let mut frames = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                // Poison the mapping so a record can only be "right" by pointing
                // at bytes this pack actually wrote.
                dst.fill(0xCC);
                let packed = pack_av1(packet, &dx.bitstream, &dx.tiles, &mut dst).expect("packs");

                assert_eq!(
                    packed.data_size as usize % BITSTREAM_ALIGN,
                    0,
                    "frame {frames}: the bitstream buffer is padded to the granule"
                );
                assert_eq!(packed.tiles.len(), dx.bitstream.tiles.len());

                for (record, tile) in packed.tiles.iter().zip(&dx.bitstream.tiles) {
                    // `#[repr(packed)]` — copy the fields out before using them.
                    let (offset, size) = (record.data_offset as usize, record.data_size as usize);
                    assert!(
                        offset + size <= packed.data_size as usize,
                        "frame {frames}: a tile record runs past the buffer's DataSize"
                    );
                    assert_eq!(
                        &dst[offset..offset + size],
                        &packet[tile.clone()],
                        "frame {frames}: the bytes a tile record addresses must BE that \
                         tile's payload"
                    );
                    // …and specifically NOT the tile group's OBU header, which is
                    // where the payload does not start.
                    assert!(
                        plan.tiles
                            .iter()
                            .all(|tg| tile.start != tg.data.start || tile.end != tg.data.end),
                        "frame {frames}: a tile record covers a whole tile-group OBU"
                    );
                }

                // `tile_group_obu()`'s accounting, per GROUP — the check the byte
                // comparison above cannot make (fn docs). `TileSizeBytes` is only
                // coded when the frame has more than one tile, so a single-tile
                // group carries no size field at all and the sum is the group.
                let size_bytes =
                    if plan.header.tile_info.tile_cols * plan.header.tile_info.tile_rows > 1 {
                        plan.header.tile_info.tile_size_bytes as usize
                    } else {
                        0
                    };
                for group in &dx.bitstream.groups {
                    let in_group: Vec<_> = dx
                        .bitstream
                        .tiles
                        .iter()
                        .filter(|t| group.start <= t.start && t.end <= group.end)
                        .collect();
                    assert!(!in_group.is_empty(), "frame {frames}: an empty tile group");
                    let payloads: usize = in_group.iter().map(|t| t.end - t.start).sum();
                    assert_eq!(
                        payloads + (in_group.len() - 1) * size_bytes,
                        group.end - group.start,
                        "frame {frames}: the group's {} tiles plus its {} size fields \
                         must account for the region EXACTLY — a short sum is a tile \
                         boundary read in the wrong place, which every offset after it \
                         inherits",
                        in_group.len(),
                        in_group.len() - 1
                    );
                }

                let descs = descriptors_av1(&packed);
                assert_eq!(
                    descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
                    vec![
                        BUFFER_PICTURE_PARAMETERS,
                        BUFFER_BITSTREAM,
                        BUFFER_SLICE_CONTROL,
                    ],
                    "frame {frames}: AV1 submits three buffers and never a matrix"
                );
                // Only the first of these is independent of `descriptors_av1`'s own
                // arithmetic — the other two would compare `packed.data_size` and
                // `16 * tiles.len()` with the expressions they were built from. So
                // they are asserted against the BYTES instead: what the packer wrote,
                // and the record size measured out of the Windows SDK's `dxva.h`.
                assert_eq!(descs[0].data_size, 912, "DXVA_PicParams_AV1, measured");
                assert_eq!(
                    descs[1].data_size as usize % BITSTREAM_ALIGN,
                    0,
                    "frame {frames}: the bitstream descriptor states the PADDED size"
                );
                assert!(
                    descs[1].data_size as usize >= packed_size_av1(&dx.bitstream),
                    "frame {frames}: the bitstream descriptor is at least the tile data"
                );
                assert_eq!(
                    descs[2].data_size as usize,
                    size_of::<TileAv1>() * dx.tiles.len(),
                    "frame {frames}: sixteen bytes per TILE"
                );
                assert!(descs.iter().all(|d| d.num_mbs_in_buffer == 0));
            }
        }
        assert_eq!(frames, 274);
    }

    /// Convert every frame of the vendored vector and check what a driver reads.
    ///
    /// The anti-vacuity assertions matter as much as the checks: a run that never
    /// saw an inter frame, or never saw the reference store hold a picture the
    /// frame does not name, would pass every check below while exercising none of
    /// the code that makes them interesting.
    #[test]
    fn the_whole_vendored_vector_converts() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut inter, mut store_beyond_refs) = (0u32, 0u32, 0u32);
        let mut gm_by_slot_would_differ = 0u32;
        let mut index_by_surface_would_differ = 0u32;
        let mut ref_size_would_differ = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                // Would writing the SURFACE into `Index` have been visible at all?
                // Only where the two numbers differ — so this is counted BEFORE the
                // conversion, which is when the ledger holds what the conversion
                // reads (it releases displaced pictures on its way out).
                for r in plan.refs.iter().flatten() {
                    let surface = slots.slot_of(r.id).expect("a named reference is held");
                    if surface != r.slot {
                        index_by_surface_would_differ += 1;
                    }
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;

                // Tile records must describe TILE PAYLOAD ranges inside the access
                // unit — the bytes after each tile's `tile_size_minus_1` field,
                // never the whole tile-group OBU. A record covering the OBU would
                // hand the driver the OBU header and the frame header as
                // entropy-coded tile data.
                assert_eq!(dx.tiles.len(), dx.bitstream.tiles.len());
                for (rec, range) in dx.tiles.iter().zip(&dx.bitstream.tiles) {
                    assert_eq!(rec.data_offset as usize, range.start);
                    assert_eq!(rec.data_size as usize, range.end - range.start);
                    assert!(range.end <= packet.len());
                    // Inside its own tile-group region, which is what the packer
                    // rebases against.
                    assert!(dx
                        .bitstream
                        .groups
                        .iter()
                        .any(|g| g.start <= range.start && range.end <= g.end));
                }
                for tg in &plan.tiles {
                    // Every tile record lies strictly INSIDE its OBU, never at its
                    // first byte: the OBU header alone is one or two bytes.
                    assert!(dx
                        .tiles
                        .iter()
                        .all(|rec| rec.data_offset as usize != tg.data.start));
                }

                // The store: every named slot resolves to a real surface, and any
                // slot with no picture stays UNUSED. `0` is a valid surface, so a
                // slot left at 0 by accident would point at a live picture.
                let named = dx
                    .pic_params
                    .ref_frame_map_texture_index
                    .iter()
                    .filter(|i| **i != UNUSED_INDEX)
                    .count();
                assert_eq!(named, plan.dpb_refs.len());
                let referenced = plan.refs.iter().flatten().count();
                if named > referenced {
                    store_beyond_refs += 1;
                }

                if referenced > 0 {
                    inter += 1;
                    // Every reference NAME must carry the SLOT the frame header
                    // named, that slot must hold a surface, that reference's own
                    // coded size must travel with it, and its global motion must be
                    // the entry the AV1 syntax codes for THAT name.
                    for (name, r) in plan.refs.iter().enumerate() {
                        let e = dx.pic_params.frame_refs[name];
                        let Some(named_ref) = r else {
                            assert_eq!(
                                e.index, UNUSED_INDEX,
                                "an unnamed reference must stay unused, not read as \
                                 slot 0"
                            );
                            continue;
                        };
                        assert_eq!(
                            e.index, named_ref.slot,
                            "reference name {name} must carry ref_frame_idx[{name}] — \
                             the SLOT — because `Index` subscripts \
                             RefFrameMapTextureIndex; a surface index there predicts \
                             from whatever sits in the slot of that number"
                        );
                        assert_ne!(
                            dx.pic_params.ref_frame_map_texture_index[usize::from(e.index)],
                            UNUSED_INDEX,
                            "reference name {name} points at an empty slot"
                        );
                        // The REFERENCE's own size, not this frame's — a distinction
                        // this vector cannot show (nothing resizes), so it is
                        // asserted against the planner's per-reference state rather
                        // than against a difference.
                        let (w, h) = (e.width, e.height);
                        assert_eq!(
                            (w, h),
                            (named_ref.state.upscaled_width, named_ref.state.frame_height),
                            "reference name {name} must carry its OWN coded size"
                        );
                        if named_ref.state.upscaled_width != plan.header.upscaled_width
                            || named_ref.state.frame_height != plan.header.frame_height
                        {
                            ref_size_would_differ += 1;
                        }
                        let gm = &plan.header.global_motion_params;
                        // `PicEntryAv1` is `#[repr(packed)]`, so its fields are
                        // copied out before being compared — a reference to one
                        // may be unaligned.
                        let (wmmat, flags) = (e.wmmat, e.global_motion_flags);
                        assert_eq!(
                            wmmat,
                            gm.gm_params[LAST_FRAME + name],
                            "reference name {name} must carry gm_params[LAST_FRAME \
                             + {name}], not the entry at its DPB slot"
                        );
                        assert_eq!(
                            flags,
                            GlobalMotionFlags {
                                wminvalid: !gm.warp_valid[LAST_FRAME + name],
                                wmtype: gm.gm_type[LAST_FRAME + name] as u8,
                            }
                            .pack()
                        );
                        // Would reading by DPB SLOT have given the same answer?
                        let slot = usize::from(named_ref.slot);
                        if gm.gm_params[LAST_FRAME + name] != gm.gm_params[slot]
                            || gm.gm_type[LAST_FRAME + name] != gm.gm_type[slot]
                        {
                            gm_by_slot_would_differ += 1;
                        }
                    }
                }
                assert_eq!(dx.pic_params.curr_pic_texture_index, dx.setup_slot);

                // Both native rungs take AV1's RENDER size as a display crop and
                // clamp it to the decoded picture, because 5.9.6 puts no upper
                // bound on `render_width_minus_1` — it is a hint, not a window.
                // This vector never exercises the clamp, and saying so here is the
                // point: the Vulkan rung's 250/250 bit-identical parity result
                // cannot have moved when the clamp was added.
                assert!(
                    plan.picture.render_width <= plan.picture.upscaled_width
                        && plan.picture.render_height <= plan.picture.frame_height,
                    "frame {frames}: this vector's render region fits inside the \
                     decoded picture, so the display-size clamp is inert on it"
                );
            }
        }

        assert_eq!(frames, 274);
        eprintln!("gm reads where name and slot disagree: {gm_by_slot_would_differ}");
        eprintln!(
            "reference entries where the surface is not the slot: \
             {index_by_surface_would_differ}"
        );
        assert!(
            index_by_surface_would_differ > 0,
            "no reference of this vector ever sat in a slot whose number differs from \
             its surface index, so `Index` cannot be told from a surface index here — \
             which is exactly how the surface read shipped"
        );
        assert_eq!(
            ref_size_would_differ, 0,
            "this vector never resizes, so `frame_refs[].width` cannot be told from \
             the current frame's width by VALUE; it is pinned against \
             `RefPic::state` instead, and this counter says so rather than leaving \
             the reader to wonder"
        );
        assert!(
            gm_by_slot_would_differ > 0,
            "reading global motion by DPB SLOT never disagreed with reading it by \
             reference NAME on this vector, so the assertions above cannot tell the \
             two apart — which is how the slot read shipped in the first place"
        );
        assert!(inter > 0, "a 274-frame vector must have inter frames");
        assert!(
            store_beyond_refs > 0,
            "the reference store never held a picture the frame did not name — so \
             this run never exercised the difference between RefFrameMapTextureIndex \
             (the whole store) and frame_refs (what this frame uses), which is the \
             distinction the Ally X class of bug lives in"
        );
    }

    /// The CHROMA deblocking levels reach `filter_level_u` / `filter_level_v`, and
    /// `log2_restoration_unit_size` is never the parser's silence.
    ///
    /// Two halves of the same block, both measured on hardware rather than argued.
    ///
    /// The levels first. ⚠ The AV1 Vulkan rung's frame-0 parity leg came back `luma
    /// IDENTICAL, chroma 319/38400 bytes differ` and that signature was reproduced
    /// EXACTLY — count, `|delta|` histogram and the first six differing bytes with
    /// their values — by decoding the vector's frame 0 with `loop_filter_level[2]`
    /// and `[3]` forced to zero in the bitstream. **That divergence turned out NOT
    /// to be a levels bug** (it was a freed sequence header making the driver treat
    /// the frame as monochrome — `pf_vkdecode::session_av1`), so do not cite it as
    /// evidence that a rung got the pair wrong. What it does establish, and what
    /// keeps this test, is the SIGNATURE: frame 0 codes `[1, 7, 8, 12]`, two luma
    /// levels and two chroma ones, and dropping only the chroma pair is invisible
    /// to luma and to every other plane statistic. A rung that lost the pair would
    /// fail Windows parity in a way nothing else here would notice, and this rung's
    /// `[2]` and `[3]` reads are four characters from `[0]` and `[1]`.
    ///
    /// Then the restoration unit size, which is a units defect the vendored parser
    /// invites: `LoopRestorationSize` is only computed inside `if ( UsesLr )`
    /// (5.9.20), so the array is `[0, 0, 0]` on a frame that restores nothing and
    /// `trailing_zeros` turns that into **16** — 271 of these 274 frames, in a field
    /// `dxva.h` documents as 6..8. libavcodec sends 8.
    #[test]
    fn the_chroma_loop_filter_levels_and_the_restoration_unit_size_reach_the_driver() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_chroma_lf, mut with_lr) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let lf = &plan.header.loop_filter_params;
                // `#[repr(packed)]` — copy the block out before reading its fields.
                let sent = dx.pic_params.loop_filter;
                assert_eq!(
                    (
                        sent.filter_level[0],
                        sent.filter_level[1],
                        sent.filter_level_u,
                        sent.filter_level_v
                    ),
                    (
                        lf.loop_filter_level[0],
                        lf.loop_filter_level[1],
                        lf.loop_filter_level[2],
                        lf.loop_filter_level[3]
                    ),
                    "frame {frames}: DXVA splits AV1's four levels into a luma PAIR \
                     plus two named chroma fields — U is index 2 and V is index 3"
                );
                if lf.loop_filter_level[2] != 0 || lf.loop_filter_level[3] != 0 {
                    with_chroma_lf += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        (sent.filter_level, sent.filter_level_u, sent.filter_level_v),
                        ([1, 7], 8, 12),
                        "frame 0's levels, and the pair whose loss the Vulkan rung's \
                         frame-0 divergence was reproduced from"
                    );
                }

                let lr = &plan.header.loop_restoration_params;
                let sizes = sent.log2_restoration_unit_size;
                if lr.uses_lr {
                    with_lr += 1;
                    assert_eq!(lr.loop_restoration_size, [128, 128, 128]);
                    assert_eq!(sizes, [7, 7, 7], "6 + lr_unit_shift, per plane");
                } else {
                    assert_eq!(
                        sizes, [LOG2_RESTORATION_UNIT_SIZE_UNUSED; 3],
                        "frame {frames}: restores nothing, so the size is \
                         libavcodec's 8 — never the parser's zero read as 16"
                    );
                }
                assert!(
                    sizes.iter().all(|s| (6..=8).contains(s)),
                    "frame {frames}: log2_restoration_unit_size is 6, 7 or 8"
                );
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            with_chroma_lf, 123,
            "123 of 274 frames of this vector deblock chroma; at zero the levels \
             above are all zero anyway and this test could not tell a dropped pair \
             from a carried one"
        );
        assert_eq!(
            with_lr, 3,
            "three frames use loop restoration, so both branches of the size are \
             exercised"
        );
    }

    /// The packed CDEF strength bytes carry the CODED secondary strength.
    ///
    /// `CdefStrength::pack` gives `secondary` TWO bits, and the vendored parser
    /// holds the AV1 spec's post-fixup value — 4 where the stream coded 3 (5.9.19
    /// rewrites the syntax element in place). `& 0x3` then turns the STRONGEST
    /// secondary filter into no filter at all, silently, on 68 of this vector's 274
    /// frames including the first. libavcodec's `dxva2_av1.c` assigns CBS's
    /// unmodified two-bit read into the same bitfield, which is the convention every
    /// driver was validated against.
    ///
    /// Asserted against the packed BYTE rather than the intermediate struct,
    /// because the truncation is what `pack` does and a test that stopped at the
    /// struct would not have seen it.
    #[test]
    fn cdef_secondary_strengths_survive_the_two_bit_pack() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut corrected_frames) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let raw = &plan.header.cdef_params;
                // `#[repr(packed)]` — copy the arrays out before indexing them.
                let cdef = dx.pic_params.cdef;
                let (y, uv) = (cdef.y_strengths, cdef.uv_strengths);
                let mut corrected = false;
                for i in 0..8 {
                    let want_y = coded_cdef_sec_strength(raw.cdef_y_sec_strength[i]);
                    let want_uv = coded_cdef_sec_strength(raw.cdef_uv_sec_strength[i]);
                    assert_eq!(
                        (y[i] >> 6, uv[i] >> 6),
                        (want_y, want_uv),
                        "frame {frames}: the secondary strength must survive the \
                         two-bit field — the parser's 4 packs to 0"
                    );
                    assert_eq!(
                        (y[i] & 0x3f, uv[i] & 0x3f),
                        (
                            raw.cdef_y_pri_strength[i] as u8,
                            raw.cdef_uv_pri_strength[i] as u8
                        ),
                        "the PRIMARY strengths are not fixed up by the spec and must \
                         reach the driver untouched"
                    );
                    if raw.cdef_y_sec_strength[i] == 4 || raw.cdef_uv_sec_strength[i] == 4 {
                        corrected = true;
                    }
                }
                if corrected {
                    corrected_frames += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        (y[3] >> 6, uv[0] >> 6),
                        (3, 3),
                        "frame 0 codes the strongest secondary strength twice, and \
                         the uncorrected conversion packed both as 0"
                    );
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            corrected_frames, 68,
            "68 of 274 frames of this vector need the correction; at zero this test \
             compares an untouched conversion against itself"
        );
    }

    /// A key frame names no reference, and must say so with the unused sentinel
    /// rather than with slot 0.
    #[test]
    fn a_key_frame_names_no_reference() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plans = planner.plan_au(first).expect("the first unit plans");
        let plan = plans.first().expect("a frame");
        assert!(plan.picture.is_key, "the vector opens on a key frame");
        let dx = plan_to_dxva_av1(first, plan, &mut slots).expect("converts");
        assert!(dx
            .pic_params
            .frame_refs
            .iter()
            .all(|e| e.index == UNUSED_INDEX));
    }

    /// The tile sizes are COUNTS of superblocks, not the coded minus-one values.
    ///
    /// A units defect the parser's field names invite, and the reason it needs its
    /// own test is that nothing else can see it: every offset, every size and every
    /// descriptor stays right, the picture decodes, and the driver has simply been
    /// told each tile is one superblock narrower and shorter than it is.
    ///
    /// The number is checked against the FRAME rather than against the field it came
    /// from: this vector is one tile, so the tile's width in superblocks is the whole
    /// frame's, `ceil(320 / 64) = 5` columns by `ceil(240 / 64) = 4` rows at 64x64
    /// superblocks. A conversion that shipped the minus-one value would say 4 by 3.
    #[test]
    fn the_tile_sizes_are_superblock_counts_not_the_coded_minus_one() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut frames = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let t = &plan.header.tile_info;
                // `#[repr(packed)]` — copy the block out before reading its arrays.
                let tiles = dx.pic_params.tiles;
                assert_eq!((tiles.cols, tiles.rows), (1, 1), "this vector is one tile");
                let sb = if plan.sequence.use_128x128_superblock {
                    128
                } else {
                    64
                };
                assert_eq!(
                    (tiles.widths[0], tiles.heights[0]),
                    (
                        plan.header.frame_width.div_ceil(sb) as u16,
                        plan.header.frame_height.div_ceil(sb) as u16
                    ),
                    "frame {frames}: the single tile spans the whole frame in \
                     superblocks — libav sends `width_in_sbs_minus_1[i] + 1`"
                );
                assert_eq!(
                    (tiles.widths[0], tiles.heights[0]),
                    (
                        t.width_in_sbs_minus_1[0] as u16 + 1,
                        t.height_in_sbs_minus_1[0] as u16 + 1
                    ),
                    "frame {frames}: and that is the coded value plus one"
                );
                // Past the frame's tile grid the arrays stay zero — a driver reading
                // `cols` entries never sees them, and a phantom `1` would be a tile
                // where the frame has none. (`#[repr(packed)]`: the arrays are
                // copied out whole before being iterated.)
                let (widths, heights) = (tiles.widths, tiles.heights);
                assert!(widths[1..].iter().all(|w| *w == 0));
                assert!(heights[1..].iter().all(|h| *h == 0));
            }
        }
        assert_eq!(frames, 274);
    }

    /// Three fields whose correct value is a SENTINEL or a constant, on every frame
    /// of the vector — none of which any other assertion here would notice.
    ///
    /// * `StatusReportFeedbackNumber` **zero**: libavcodec has the assignment
    ///   commented out for AV1 alone ("breaks decoding on some drivers (tested on
    ///   NVIDIA 457.09)") and Chromium ships the zero too ("should not be equal to
    ///   0 ... but it crashes :|"). This rung does not even accept a number.
    /// * `qm_y`/`qm_u`/`qm_v` **0xFF** where the frame uses no quantiser matrix.
    ///   The struct has no `using_qmatrix` bit, and the parser leaves the indices at
    ///   0 — a VALID matrix — so the sentinel is the only thing standing between
    ///   every frame of this vector and a dequantisation against matrix 0.
    /// * `reference_frame_update` **1**, which libavcodec writes as a literal.
    #[test]
    fn the_three_fields_whose_right_answer_is_a_constant() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut without_qmatrix, mut without_refresh) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let pp = &dx.pic_params;
                let status = pp.status_report_feedback_number;
                assert_eq!(
                    status, 0,
                    "frame {frames}: AV1 submits a zero StatusReportFeedbackNumber"
                );

                let q = pp.quantization;
                let (qm_y, qm_u, qm_v) = (q.qm_y, q.qm_u, q.qm_v);
                if plan.header.quantization_params.using_qmatrix {
                    assert_eq!(
                        (qm_y, qm_u, qm_v),
                        (
                            plan.header.quantization_params.qm_y as u8,
                            plan.header.quantization_params.qm_u as u8,
                            plan.header.quantization_params.qm_v as u8
                        )
                    );
                } else {
                    without_qmatrix += 1;
                    assert_eq!(
                        (qm_y, qm_u, qm_v),
                        (QM_UNUSED, QM_UNUSED, QM_UNUSED),
                        "frame {frames}: with no quantiser matrix the indices are the \
                         0xFF sentinel — 0 is matrix zero, which the driver would \
                         dequantize against"
                    );
                }

                // `reference_frame_update` is bit 22 of the coding flags — read back
                // through `pack` rather than spelled as a magic mask.
                let coding = pp.coding;
                let on = CodingFlagsAv1 {
                    reference_frame_update: true,
                    ..Default::default()
                }
                .pack();
                assert_eq!(coding & on, on, "frame {frames}: libav writes a literal 1");
                if plan.header.refresh_frame_flags == 0 {
                    without_refresh += 1;
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            without_qmatrix, 274,
            "no frame of this vector uses a quantiser matrix, so the sentinel is what \
             the driver reads on every one of them — at zero this test proves nothing"
        );
        // Not an anti-vacuity assertion but a note about what this vector CANNOT
        // show: `reference_frame_update` only differs from `refresh_frame_flags != 0`
        // on a frame that refreshes nothing, and this vector has none.
        assert_eq!(without_refresh, 0);
    }

    /// Every picture a temporal unit decodes is still addressable once the unit ends.
    ///
    /// A precondition of the Windows AV1 parity harness rather than of this crate.
    /// That harness drives the production entry point, which takes a whole temporal
    /// unit and plans it internally, so it reaches a HIDDEN frame's pixels by asking
    /// the slot map where that picture went after the unit is done. Sound only if a
    /// unit never displaces a picture it decoded itself — a fact about this vector,
    /// not about AV1 — and the harness needs a GPU while this does not, so the check
    /// lives here where every leg runs it.
    #[test]
    fn no_unit_of_the_vector_displaces_a_picture_it_decoded_itself() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut units, mut multi_frame) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            let plans = planner.plan_au(packet).expect("the clean vector plans");
            units += 1;
            let mut decoded = Vec::new();
            for plan in &plans {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, plan, &mut slots);
                decoded.push((dx.setup_id, dx.setup_slot));
            }
            if decoded.len() > 1 {
                multi_frame += 1;
            }
            for (id, slot) in decoded {
                assert_eq!(
                    slots.slot_of(id),
                    Some(slot),
                    "unit {units}: picture {id} left surface {slot} before its own \
                     unit finished, so a per-unit readback could not find it"
                );
            }
        }
        assert_eq!(units, 250);
        assert_eq!(
            multi_frame, 24,
            "24 units carry a hidden frame as well as the shown one — at zero this \
             check never saw the case it exists for"
        );
    }

    /// A frame whose tile groups do not add up to its tile GRID is refused.
    ///
    /// The failure this stands in for is a dropped tile group: the OBU walk never
    /// sees it, so no `TruncatedAu` warning is raised and nothing else in the rung
    /// notices that the submission is short of what `pic_params.tiles` announces.
    /// Simulated by removing a tile-group plan, which is what such a loss leaves
    /// behind.
    #[test]
    fn a_frame_short_of_its_tile_grid_is_refused_rather_than_submitted() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plans = planner.plan_au(first).expect("the first unit plans");
        let mut plan = plans.into_iter().next().expect("a frame");

        // The unmodified frame converts, so the refusal below is about the tiles and
        // not about the frame.
        plan_to_dxva_av1(first, &plan, &mut slots).expect("the untouched frame converts");

        // Now claim a two-tile grid the access unit has one tile for.
        let header = std::rc::Rc::make_mut(&mut plan.header);
        header.tile_info.tile_cols = 2;
        header.tile_info.tile_rows = 1;
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        assert_eq!(
            plan_to_dxva_av1(first, &plan, &mut slots).err(),
            Some(PlanToDxvaAv1Error::TileCountMismatch {
                records: 1,
                walked: 1,
                grid: 2,
            })
        );
    }
}
