//! Converts one AV1 [`AuPlanAv1`] into libva picture and tile buffers.
//! `ref_frame_map` is slot-indexed and contains `VASurfaceID`s; `ref_frame_idx`
//! and global motion are reference-name-indexed, with `ref_frame_idx` containing slots.
//! libva has no per-reference dimensions: drivers recover them from each surface.
//! These rules come from `va_dec_av1.h`, AV1 §7.11.3.3, and libavcodec `vaapi_av1.c`.
//!
//! Frames with `apply_grain` are refused: correct synthesis requires distinct decode
//! and display surfaces. The gate is per frame, not `film_grain_params_present`.
//! For published stores, missing references are replaced with a live surface and
//! reported in [`DecodePlanVaAv1::substituted_refs`], never submitted as an invalid ID;
//! shown key frames retain libavcodec's deliberately invalid, unused reference map.
//!
//! Slot removals and assignment precede tile walking and film-grain refusal so this
//! ledger remains aligned with [`Av1Planner::plan_au`](pf_bitstream::av1::Av1Planner::plan_au).
//! After refusal the caller must bind no surface to the assigned slot.
//!
//! Each tile-group OBU produces one parameter buffer containing one record per tile
//! and one data buffer for the complete `tile_data`; every `slice_data_offset` is
//! relative to that group's data buffer, as required by `va_dec_av1.h` and libavcodec.

use std::ops::Range;

use pf_bitstream::av1::coded_cdef_sec_strength;
use pf_bitstream::av1::AuPlan as AuPlanAv1;
use pf_bitstream::av1::FrameType;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::NUM_REF_SLOTS;
use pf_bitstream::av1::REFS_PER_FRAME;

use crate::va::VA_INVALID_SURFACE;
use crate::va::VA_SLICE_DATA_FLAG_ALL;
use crate::va_av1::FilmGrainInfoFieldsAV1;
use crate::va_av1::LoopFilterInfoFieldsAV1;
use crate::va_av1::LoopRestorationFieldsAV1;
use crate::va_av1::ModeControlFieldsAV1;
use crate::va_av1::PicInfoFieldsAV1;
use crate::va_av1::QmatrixFieldsAV1;
use crate::va_av1::SegmentInfoFieldsAV1;
use crate::va_av1::SeqInfoFieldsAV1;
use crate::va_av1::VaDecPictureParameterBufferAV1;
use crate::va_av1::VaSegmentationStructAV1;
use crate::va_av1::VaSliceParameterBufferAV1;
use crate::va_av1::VaWarpedMotionParamsAV1;
use crate::va_av1::ANCHOR_FRAME_UNUSED;
use crate::va_av1::LAST_FRAME;
use crate::va_av1::SUPERRES_NUM;
use crate::va_av1::TILE_SBS_LEN;
use crate::SlotError;
use crate::SlotMap;
use pf_vkdecode::plan_bitstream;
use pf_vkdecode::Av1Bitstream;
use pf_vkdecode::Av1TileError;

/// AV1's tile ceiling in one frame — `MAX_TILE_COLS` × `MAX_TILE_ROWS` is 4096,
/// which no AV1 level defines; libavcodec refuses past 256 ("exceeding all defined
/// levels in the AV1 spec") and so does the shared walk.
pub const MAX_TILES: usize = 256;

/// AV1's `MAX_TILE_COLS` / `MAX_TILE_ROWS`, and the bound the parser's own
/// `TileInfo` arrays are sized to.
pub const MAX_TILE_DIM: usize = 64;

/// One tile-group OBU's submission: the records for its tiles, and the byte range
/// (ACCESS-UNIT coordinates) of the `tile_data` region they address.
///
/// The pairing is the point. `vaRenderPicture` establishes which data buffer a
/// parameter buffer's `slice_data_offset` is relative to by being handed the two
/// together, so the records and their region must travel as one thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileGroupVa {
    pub tiles: Vec<VaSliceParameterBufferAV1>,
    pub data: Range<usize>,
}

/// Everything one AV1 `vaRenderPicture` sequence needs.
#[derive(Debug, Clone)]
pub struct DecodePlanVaAv1 {
    pub pic_params: VaDecPictureParameterBufferAV1,
    /// One entry per tile-group (or frame) OBU, in decode order.
    pub tile_groups: Vec<TileGroupVa>,
    /// The ledger slot this picture took — or `None` when the picture refreshes no
    /// reference slot and the conversion gave the slot straight back (see the
    /// `refresh_frame_flags == 0` note in [`plan_to_va_av1`]).
    pub setup_slot: Option<u8>,
    pub setup_id: PicId,
    /// Which `ref_frame_map` entries were empty and got a live surface instead — bit
    /// `i` for AV1 reference slot `i` (module docs, "A lost reference gets a LIVE
    /// surface").
    ///
    /// Non-zero means this frame is being concealed: it decodes from at least one
    /// substitute. Reported rather than silent because it is the one thing about a
    /// submission that a log cannot otherwise tell from a clean decode, and because a
    /// clean stream must never produce it — `pf-vaadec`'s vector test asserts 0 across
    /// all 274 frames.
    pub substituted_refs: u8,
}

/// Why an AV1 plan cannot be expressed as VAAPI buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVaAv1Error {
    /// A `show_existing_frame` plan decodes nothing and has no submission. Not a
    /// failure: the caller displays a surface it already holds.
    NoDecode,
    NoTiles,
    /// The access unit's tile OBUs could not be walked into per-tile payloads.
    Tiles(Av1TileError),
    /// The frame header's tile GRID and the tiles the access unit actually carried
    /// disagree — a dropped tile group, most likely, which nothing else reports.
    TileCountMismatch {
        records: usize,
        walked: usize,
        grid: usize,
    },
    /// More tile columns or rows than AV1 defines.
    TooManyTiles {
        cols: u32,
        rows: u32,
    },
    /// A tile's payload is not inside the tile-group region the records address.
    TileOutsideGroup {
        tile: usize,
    },
    /// The frame applies film grain, which needs a second surface this rung does not
    /// allocate (module docs).
    FilmGrain,
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// A picture the marked store holds has no ledger slot, so no surface can be put
    /// in `ref_frame_map` for it.
    UnresolvedReference(PicId),
    SurfaceOutOfRange {
        slot: u8,
        surfaces: usize,
    },
    /// A header value wider than the libva field that carries it.
    FieldOverflow {
        field: &'static str,
        value: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVaAv1Error {
    fn from(e: SlotError) -> Self {
        PlanToVaAv1Error::Slot(e)
    }
}

impl PlanToVaAv1Error {
    /// This refusal is the shape a LOST TILE GROUP makes.
    ///
    /// The distinction the caller needs, and the reason it is decided here rather than
    /// by matching an enum at the call site: on a plan that already carries an
    /// integrity warning these five are damage, not a defect — the access unit simply
    /// did not carry the tiles its frame header announced — and the rung's answer to
    /// damage is concealment, exactly as it is for every warning the planner raises.
    /// On an UNDAMAGED plan the same five mean this conversion or the shared tile walk
    /// disagrees with a stream that arrived whole, which is a defect and must surface
    /// as one.
    ///
    /// Everything else stays a refusal either way: a capacity mismatch, an unresolved
    /// reference or a field overflow says something about this rung's own state that
    /// concealing would bury.
    pub fn lost_tiles(&self) -> bool {
        matches!(
            self,
            PlanToVaAv1Error::NoTiles
                | PlanToVaAv1Error::Tiles(_)
                | PlanToVaAv1Error::TileCountMismatch { .. }
                | PlanToVaAv1Error::TooManyTiles { .. }
                | PlanToVaAv1Error::TileOutsideGroup { .. }
        )
    }
}

impl std::fmt::Display for PlanToVaAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVaAv1Error::NoDecode => {
                write!(f, "a show_existing_frame access unit decodes nothing")
            }
            PlanToVaAv1Error::NoTiles => write!(f, "the access unit planned no tiles"),
            PlanToVaAv1Error::Tiles(e) => write!(f, "tile walk: {e}"),
            PlanToVaAv1Error::TileCountMismatch {
                records,
                walked,
                grid,
            } => write!(
                f,
                "{records} tile records and {walked} walked tiles for a {grid}-tile \
                 grid — a tile group was lost"
            ),
            PlanToVaAv1Error::TooManyTiles { cols, rows } => {
                write!(f, "a {cols}x{rows} tile grid is outside AV1's limits")
            }
            PlanToVaAv1Error::TileOutsideGroup { tile } => write!(
                f,
                "tile {tile}'s payload is not inside its tile group's data region"
            ),
            PlanToVaAv1Error::FilmGrain => write!(
                f,
                "this frame applies film grain, which needs a separate display \
                 surface this rung does not allocate"
            ),
            PlanToVaAv1Error::CapacityMismatch { required, capacity } => write!(
                f,
                "the slot map holds {capacity} slots, AV1 needs {required}"
            ),
            PlanToVaAv1Error::UnresolvedReference(id) => {
                write!(f, "picture {id} holds a reference slot but no surface")
            }
            PlanToVaAv1Error::SurfaceOutOfRange { slot, surfaces } => {
                write!(
                    f,
                    "ledger slot {slot} has no surface in a table of {surfaces}"
                )
            }
            PlanToVaAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its libva field")
            }
            PlanToVaAv1Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVaAv1Error {}

fn narrow(field: &'static str, value: u32) -> Result<u8, PlanToVaAv1Error> {
    u8::try_from(value).map_err(|_| PlanToVaAv1Error::FieldOverflow { field, value })
}

fn narrow16(field: &'static str, value: u32) -> Result<u16, PlanToVaAv1Error> {
    u16::try_from(value).map_err(|_| PlanToVaAv1Error::FieldOverflow { field, value })
}

fn narrow32(field: &'static str, value: usize) -> Result<u32, PlanToVaAv1Error> {
    u32::try_from(value).map_err(|_| PlanToVaAv1Error::FieldOverflow {
        field,
        value: u32::MAX,
    })
}

/// Convert one planned AV1 frame.
///
/// `au` is the access unit `plan` was planned from: the tile records need per-TILE
/// byte ranges, and finding those means walking each tile group's header and its
/// `tile_size_minus_1` fields — a walk over the bitstream, not over the plan. It is
/// [`plan_bitstream`], shared with the Vulkan and DXVA rungs.
///
/// `surfaces` is the caller's ledger-slot → `VASurfaceID` table and `setup_surface`
/// is the surface this picture decodes INTO — the same parameter contract
/// [`crate::pic::plan_to_va`] documents, and for the same reason: the decode target
/// comes off the caller's free list at activation time and is bound to its slot
/// afterwards, because a slot freed by this access unit's own removals is free
/// again by the time the ledger is asked.
///
/// # The frame that refreshes nothing
///
/// A frame with `refresh_frame_flags == 0` is legal AV1 — shown once, referenced
/// never — and it enters the planner's store NOWHERE, so the planner can never
/// report it removed. It still needs a ledger slot while it is converted (that is
/// how a later frame would resolve its surface), so this function assigns one and
/// gives it straight back, exactly as [`crate::pic_h265::plan_to_va_h265`] does for
/// a picture its own access unit evicts. [`DecodePlanVaAv1::setup_slot`] is then
/// `None`, which is the caller's signal that nothing binds the surface and only its
/// pending-output claim keeps it off the free list.
///
/// That the release can happen HERE rather than in the caller is a property of this
/// backend: a VAAPI ledger slot is not a surface (the DXVA rung's `setup_slot` IS
/// its surface index, which is why `pf_dxvadec` has to hold the slot until the frame
/// has been read). Nine such frames would otherwise exhaust a nine-slot ledger and
/// kill a session on correct streams.
///
/// # What a refusal leaves behind, and what the caller owes it
///
/// ⚠ `slots` is mutated BEFORE the tile walk and before the film grain gate, so a
/// refusal from either of those has already applied this access unit's removals and
/// assigned the setup picture its slot. That is deliberate and the module docs say
/// why: the planner stored the picture before this function was called, and a refusal
/// that skipped the assignment would desynchronise the ledger from the planner's store
/// permanently.
///
/// What the caller owes in return is that on a refusal it binds **nothing** to the
/// assigned slot — no surface was written, and leaving the slot's PREVIOUS binding in
/// place would make the next frame predict from a picture that is not the one the
/// bitstream named. An unbound slot reads back as `VA_INVALID_SURFACE` in
/// `surfaces` and is then substituted (module docs), which is the concealment libva
/// documents.
///
/// The refusals that can still fire before any mutation — [`PlanToVaAv1Error::NoDecode`],
/// [`PlanToVaAv1Error::CapacityMismatch`], [`PlanToVaAv1Error::SurfaceOutOfRange`],
/// [`PlanToVaAv1Error::UnresolvedReference`] and the `RefPic::slot` overflow — leave
/// `slots` untouched, so the same "bind nothing" answer is correct for them too.
pub fn plan_to_va_av1(
    plan: &AuPlanAv1,
    au: &[u8],
    slots: &mut SlotMap,
    surfaces: &[u32],
    setup_surface: u32,
) -> Result<DecodePlanVaAv1, PlanToVaAv1Error> {
    let setup_id = plan.dpb.stored.ok_or(PlanToVaAv1Error::NoDecode)?;
    let h = &*plan.header;
    let seq = &*plan.sequence;
    let color = &seq.color_config;

    // AV1's DPB depth is a constant of the codec: eight reference slots plus the
    // picture being decoded.
    let required = NUM_REF_SLOTS + 1;
    if slots.capacity() != required {
        return Err(PlanToVaAv1Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }
    // A pre-check, so the caller's post-call bind of `setup_surface` to the returned
    // slot is always in range.
    if surfaces.len() < slots.capacity() {
        return Err(PlanToVaAv1Error::SurfaceOutOfRange {
            slot: (slots.capacity() - 1) as u8,
            surfaces: surfaces.len(),
        });
    }

    // --- the reference store, by AV1 SLOT, holding SURFACES -------------------
    //
    // ⚠ Indexed by `RefPic::slot` (the bitstream's own 0..8 reference slot), NOT by
    // the ledger slot — the ledger is only how a PicId finds its surface. Writing
    // the ledger slot's number here would name a different reference on every frame
    // whose store is not in ledger order, which is every frame after the first
    // eviction.
    let mut ref_frame_map = [VA_INVALID_SURFACE; NUM_REF_SLOTS];
    // ⚠ A SHOWN KEY FRAME publishes an empty store. libavcodec:
    // `if (frame_type == AV1_FRAME_KEY && frame_header->show_frame)
    //      pic_param.ref_frame_map[i] = VA_INVALID_ID;`
    // — the frame decodes from nothing and refreshes every slot, so the surfaces the
    // store held a moment ago are not references for it. Ours would still list them
    // (the plan's `dpb_refs` is the store BEFORE this frame's refresh), and the
    // difference is exactly the one place drivers have been exercised.
    let publishes_store = !(h.frame_type == FrameType::KeyFrame && h.show_frame);
    if publishes_store {
        for r in &plan.dpb_refs {
            let ledger = slots
                .slot_of(r.id)
                .ok_or(PlanToVaAv1Error::UnresolvedReference(r.id))?;
            let surface =
                *surfaces
                    .get(usize::from(ledger))
                    .ok_or(PlanToVaAv1Error::SurfaceOutOfRange {
                        slot: ledger,
                        surfaces: surfaces.len(),
                    })?;
            let slot = usize::from(r.slot);
            if slot >= NUM_REF_SLOTS {
                return Err(PlanToVaAv1Error::FieldOverflow {
                    field: "RefPic::slot",
                    value: u32::from(r.slot),
                });
            }
            ref_frame_map[slot] = surface;
        }
    }

    // ⚠ An empty entry is pointed at a LIVE surface — the header's own prescription
    // for a missing reference, quoted in the module docs. Two different losses land
    // here and both need it: a slot the planner reports empty (its picture never
    // arrived) and a slot whose picture this rung refused to convert, which the caller
    // signals by binding no surface to it.
    //
    // ⚠ A resolved reference is preferred over `setup_surface`. Both are live and
    // correctly sized, but the decode target is the surface the driver is about to
    // WRITE, and naming it as its own reference is a shape some drivers validate
    // against; a picture that actually decoded is the better substitute and is the
    // "alternative frame buffer" the header means. The target is the fallback for the
    // one case with nothing else to reach for — a store that resolved nothing at all.
    let mut substituted_refs = 0u8;
    if publishes_store {
        let alternative = ref_frame_map
            .iter()
            .copied()
            .find(|&s| s != VA_INVALID_SURFACE)
            .unwrap_or(setup_surface);
        for (slot, entry) in ref_frame_map.iter_mut().enumerate() {
            if *entry == VA_INVALID_SURFACE {
                *entry = alternative;
                substituted_refs |= 1 << slot;
            }
        }
    }

    // The seven reference NAMES, each holding the SLOT it reads — which is
    // `ref_frame_idx[name]` verbatim, and libavcodec copies it unconditionally
    // (a key or intra-only frame reads no references and the driver ignores it).
    //
    // ⚠ Deliberately NOT taken from `plan.refs`: a lost reference leaves a hole
    // there, and a hole is not a slot. The name still points at the slot the
    // bitstream coded, and the concealment is done one level down — that slot's
    // `ref_frame_map` entry is the substituted surface above, so the name resolves to a
    // live picture rather than to nothing.
    let ref_frame_idx = h.ref_frame_idx;

    // --- mutations, once the references have resolved ------------------------
    //
    // ⚠ HERE, and not after the tile walk. Every fallible step below leaves the ledger
    // in step with the planner's store, which is what a refusal needs (fn docs); doing
    // it the other way round turns one lost tile group into a hard `Err` on every
    // frame until the next shown key frame.
    //
    // ⚠ But not before the loop above either: a reference resolves against the store as
    // it stood BEFORE this access unit's removals, and releasing first would lose the
    // picture a name still points at.

    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        let _ = slots.release(id);
    }
    let assigned = slots.assign(setup_id)?;
    // The frame that refreshes nothing never enters the store, so nothing will ever
    // ask the ledger for it again (fn docs).
    let setup_slot = if h.refresh_frame_flags == 0 {
        slots.release(setup_id);
        None
    } else {
        Some(assigned)
    };

    // Film grain: refused, not approximated (module docs). After the mutations, so the
    // refusal costs this frame and not the rest of the GOP.
    if seq.film_grain_params_present && h.film_grain_params.apply_grain {
        return Err(PlanToVaAv1Error::FilmGrain);
    }

    // --- tiles ---------------------------------------------------------------
    //
    // A frame header whose tile groups did not arrive is the everyday shape of a lost
    // packet: `PlanWarning::TruncatedAu`, a plan that still stores its picture, and an
    // empty or short tile list. It refuses here — there is nothing to submit — and
    // [`PlanToVaAv1Error::lost_tiles`] is how the caller tells that damage apart from a
    // defect.
    if plan.tiles.is_empty() {
        return Err(PlanToVaAv1Error::NoTiles);
    }
    let t = &h.tile_info;
    if t.tile_cols == 0 || t.tile_rows == 0 {
        return Err(PlanToVaAv1Error::NoTiles);
    }
    let grid = (t.tile_cols as usize).saturating_mul(t.tile_rows as usize);
    if t.tile_cols as usize > MAX_TILE_DIM || t.tile_rows as usize > MAX_TILE_DIM {
        return Err(PlanToVaAv1Error::TooManyTiles {
            cols: t.tile_cols,
            rows: t.tile_rows,
        });
    }
    if grid > MAX_TILES {
        return Err(PlanToVaAv1Error::Tiles(Av1TileError::TooManyTiles {
            tiles: grid,
        }));
    }
    let bitstream: Av1Bitstream =
        plan_bitstream(au, &plan.tiles, h).map_err(PlanToVaAv1Error::Tiles)?;

    let mut tile_groups: Vec<TileGroupVa> = Vec::with_capacity(plan.tiles.len());
    let mut walked = 0usize;
    // `plan_bitstream` pushes one region per plan tile group, in order, so `index`
    // addresses this group's region — `get` rather than `[]` because a panic in a
    // decode thread is a worse answer than a refusal even for a case the walk cannot
    // produce.
    for (index, tg) in plan.tiles.iter().enumerate() {
        let region = bitstream
            .groups
            .get(index)
            .ok_or(PlanToVaAv1Error::TileCountMismatch {
                records: walked,
                walked: bitstream.tiles.len(),
                grid,
            })?
            .clone();
        // A group whose end precedes its start is malformed; the walk refuses it
        // too, so this saturates rather than growing a second refusal path.
        let count = tg.tg_end.saturating_sub(tg.tg_start).saturating_add(1);
        let mut records = Vec::with_capacity(count as usize);
        for step in 0..count {
            let tile_num = tg.tg_start.saturating_add(step);
            let payload = bitstream.tiles.get(walked).ok_or({
                PlanToVaAv1Error::TileCountMismatch {
                    records: walked,
                    walked: bitstream.tiles.len(),
                    grid,
                }
            })?;
            walked += 1;
            // The offset libva wants is relative to the DATA BUFFER, which is this
            // group's whole `tile_data` region — not to the access unit, and not to
            // the tile. Rebased here rather than by a packer, because unlike DXVA
            // this rung uploads the region itself and has nothing to rebase against
            // later.
            if payload.start < region.start || payload.end > region.end {
                return Err(PlanToVaAv1Error::TileOutsideGroup { tile: walked - 1 });
            }
            records.push(VaSliceParameterBufferAV1 {
                slice_data_size: narrow32("slice_data_size", payload.end - payload.start)?,
                slice_data_offset: narrow32("slice_data_offset", payload.start - region.start)?,
                slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
                // Tile numbering is libavcodec's: `tile_row = tile_num / tile_cols`,
                // `tile_column = tile_num % tile_cols`, with `tile_num` running
                // `tg_start..=tg_end` across the frame's groups.
                tile_row: narrow16("tile_row", tile_num / t.tile_cols)?,
                tile_column: narrow16("tile_column", tile_num % t.tile_cols)?,
                // `va_deprecated`, and libavcodec fills both anyway.
                tg_start: narrow16("tg_start", tg.tg_start)?,
                tg_end: narrow16("tg_end", tg.tg_end)?,
                anchor_frame_idx: ANCHOR_FRAME_UNUSED,
                tile_idx_in_tile_list: 0,
                va_reserved: [0; 4],
            });
        }
        tile_groups.push(TileGroupVa {
            tiles: records,
            data: region,
        });
    }
    // The independent cross-check, and the same one the DXVA rung makes: the tile
    // GRID comes from the frame header and is what `tile_cols`/`tile_rows` announce
    // to the driver, while the record count comes from the tile groups the access
    // unit actually carried. A dropped tile group raises no warning anywhere else —
    // the OBU walk simply never sees it — and submitting anyway declares a grid the
    // tile buffers are short for.
    if walked != grid || bitstream.tiles.len() != grid {
        return Err(PlanToVaAv1Error::TileCountMismatch {
            records: walked,
            walked: bitstream.tiles.len(),
            grid,
        });
    }

    // --- the picture parameter blocks ----------------------------------------

    let lf = &h.loop_filter_params;
    let q = &h.quantization_params;
    let c = &h.cdef_params;
    let lr = &h.loop_restoration_params;
    let sp = &h.segmentation_params;
    let gm = &h.global_motion_params;

    let mut seg_info = VaSegmentationStructAV1::zeroed();
    seg_info.segment_info_fields = SegmentInfoFieldsAV1 {
        enabled: sp.segmentation_enabled,
        update_map: sp.segmentation_update_map,
        temporal_update: sp.segmentation_temporal_update,
        update_data: sp.segmentation_update_data,
    }
    .pack();
    for segment in 0..8 {
        let mut mask = 0u8;
        for (feature, enabled) in sp.feature_enabled[segment].iter().enumerate() {
            if *enabled {
                mask |= 1 << feature;
            }
        }
        seg_info.feature_mask[segment] = mask;
        // No clipping here: libva wants `FeatureData` AFTER 5.9.14's Clip3, and the
        // vendored parser clips as it reads (`helpers::clip3` against the spec's
        // `FEATURE_MAX`). Clipping again would be a no-op; not clipping at all would
        // have been the bug, which is why this says which side did it.
        seg_info.feature_data[segment] = sp.feature_data[segment];
    }

    let mut cdef_y_strengths = [0u8; crate::va_av1::CDEF_MAX];
    let mut cdef_uv_strengths = [0u8; crate::va_av1::CDEF_MAX];
    for i in 0..crate::va_av1::CDEF_MAX {
        // The header's own formula: `(pri << 2) | (sec & 0x03)`.
        //
        // ⚠ `sec` must be the CODED two-bit read. AV1 5.9.19 rewrites the syntax
        // element in place (a coded 3 becomes 4) and cros-codecs follows the spec, so
        // masking the parser's value with 3 would turn the STRONGEST secondary filter
        // into NO filter — on 68 of the vendored vector's 274 frames, including
        // frame 0. `coded_cdef_sec_strength` is the inverse; its docs carry the
        // evidence.
        let pri_y = narrow("cdef_y_pri_strength", c.cdef_y_pri_strength[i])?;
        let pri_uv = narrow("cdef_uv_pri_strength", c.cdef_uv_pri_strength[i])?;
        cdef_y_strengths[i] = (pri_y << 2) | coded_cdef_sec_strength(c.cdef_y_sec_strength[i]);
        cdef_uv_strengths[i] = (pri_uv << 2) | coded_cdef_sec_strength(c.cdef_uv_sec_strength[i]);
    }

    let mut width_in_sbs_minus_1 = [0u16; TILE_SBS_LEN];
    let mut height_in_sbs_minus_1 = [0u16; TILE_SBS_LEN];
    // ⚠ Clamped to 63 entries. The arrays ARE 63 long and the header says why — the
    // last tile's size is derived from the others and the frame size — but
    // libavcodec loops to `tile_cols`, which writes index 63 on a 64-column frame.
    // That is a one-element overrun in libavcodec, not a layout we should reproduce.
    for (out, coded) in width_in_sbs_minus_1
        .iter_mut()
        .zip(&t.width_in_sbs_minus_1[..(t.tile_cols as usize).min(TILE_SBS_LEN)])
    {
        *out = narrow16("width_in_sbs_minus_1", *coded)?;
    }
    for (out, coded) in height_in_sbs_minus_1
        .iter_mut()
        .zip(&t.height_in_sbs_minus_1[..(t.tile_rows as usize).min(TILE_SBS_LEN)])
    {
        *out = narrow16("height_in_sbs_minus_1", *coded)?;
    }

    let mut wm = [VaWarpedMotionParamsAV1::zeroed(); REFS_PER_FRAME];
    for (name, entry) in wm.iter_mut().enumerate() {
        // ⚠ Global motion is indexed by reference NAME, never by DPB slot. AV1's
        // `global_motion_params()` loops `ref = LAST_FRAME..ALTREF_FRAME` and the
        // vendored parser stores it that way; libavcodec's `vaapi_av1.c` writes
        // `pic_param.wm[i - 1]` for `i = LAST_FRAME..=ALTREF_FRAME`. Reading by slot
        // agrees with the truth only while reference `i` happens to sit in slot
        // `i + 1`, and silently hands every warped reference somebody else's warp
        // the moment it does not.
        let gm_name = LAST_FRAME + name;
        entry.wmtype = gm.gm_type[gm_name] as u32;
        // Six warp parameters, not eight: 5.9.24 codes six and libavcodec copies
        // `for (j = 0; j < 6; j++)`. `wmmat[6]`/`wmmat[7]` stay zero.
        entry.wmmat[..6].copy_from_slice(&gm.gm_params[gm_name]);
        // `warp_valid` is the parser's `setup_shear` verdict — a warp whose shear
        // parameters are out of range is unusable — and libva's flag is its inverse.
        entry.invalid = u8::from(!gm.warp_valid[gm_name]);
    }

    let bit_depth_idx = if color.high_bitdepth {
        if color.twelve_bit {
            2
        } else {
            1
        }
    } else {
        0
    };

    let mut pic_params = VaDecPictureParameterBufferAV1::zeroed();
    pic_params.profile = seq.seq_profile as u8;
    // ⚠ The parser types this `i32` and leaves it **-1** when `enable_order_hint` is
    // 0 (`parser.rs`: `s.order_hint_bits_minus_1 = -1`). `as u8` on that is 255 — a
    // decoder told the order hints are 256 bits wide — so the disabled case sends 0,
    // which is what libavcodec's CBS holds for a field it never read.
    pic_params.order_hint_bits_minus_1 = if seq.enable_order_hint {
        narrow(
            "order_hint_bits_minus_1",
            u32::try_from(seq.order_hint_bits_minus_1).map_err(|_| {
                PlanToVaAv1Error::FieldOverflow {
                    field: "order_hint_bits_minus_1",
                    value: 0,
                }
            })?,
        )?
    } else {
        0
    };
    pic_params.bit_depth_idx = bit_depth_idx;
    pic_params.matrix_coefficients = color.matrix_coefficients as u8;
    pic_params.seq_info_fields = SeqInfoFieldsAV1 {
        still_picture: seq.still_picture,
        use_128x128_superblock: seq.use_128x128_superblock,
        enable_filter_intra: seq.enable_filter_intra,
        enable_intra_edge_filter: seq.enable_intra_edge_filter,
        enable_interintra_compound: seq.enable_interintra_compound,
        enable_masked_compound: seq.enable_masked_compound,
        enable_dual_filter: seq.enable_dual_filter,
        enable_order_hint: seq.enable_order_hint,
        enable_jnt_comp: seq.enable_jnt_comp,
        enable_cdef: seq.enable_cdef,
        mono_chrome: color.mono_chrome,
        color_range: color.color_range,
        subsampling_x: color.subsampling_x,
        subsampling_y: color.subsampling_y,
        chroma_sample_position: color.chroma_sample_position as u8,
        film_grain_params_present: seq.film_grain_params_present,
    }
    .pack();
    pic_params.current_frame = setup_surface;
    // Equal to `current_frame` because `apply_grain` is 0 on every frame that
    // reaches here (module docs); libva then ignores this field entirely.
    pic_params.current_display_picture = setup_surface;
    pic_params.anchor_frames_num = 0;
    pic_params.anchor_frames_list = std::ptr::null_mut();
    // The UPSCALED width — the same quantity libavcodec sends as the coded
    // `frame_width_minus_1`, which AV1 5.9.8 reads into `UpscaledWidth` before
    // superres divides it down into `FrameWidth`.
    pic_params.frame_width_minus1 = narrow16(
        "frame_width_minus1",
        h.upscaled_width
            .checked_sub(1)
            .ok_or(PlanToVaAv1Error::FieldOverflow {
                field: "upscaled_width",
                value: 0,
            })?,
    )?;
    pic_params.frame_height_minus1 = narrow16(
        "frame_height_minus1",
        h.frame_height
            .checked_sub(1)
            .ok_or(PlanToVaAv1Error::FieldOverflow {
                field: "frame_height",
                value: 0,
            })?,
    )?;
    pic_params.ref_frame_map = ref_frame_map;
    pic_params.ref_frame_idx = ref_frame_idx;
    pic_params.primary_ref_frame = narrow("primary_ref_frame", h.primary_ref_frame)?;
    pic_params.order_hint = narrow("order_hint", h.order_hint)?;
    pic_params.seg_info = seg_info;
    pic_params.tile_cols = narrow("tile_cols", t.tile_cols)?;
    pic_params.tile_rows = narrow("tile_rows", t.tile_rows)?;
    pic_params.width_in_sbs_minus_1 = width_in_sbs_minus_1;
    pic_params.height_in_sbs_minus_1 = height_in_sbs_minus_1;
    pic_params.context_update_tile_id =
        narrow16("context_update_tile_id", t.context_update_tile_id)?;
    pic_params.pic_info_fields = PicInfoFieldsAV1 {
        frame_type: h.frame_type as u8,
        show_frame: h.show_frame,
        showable_frame: h.showable_frame,
        error_resilient_mode: h.error_resilient_mode,
        disable_cdf_update: h.disable_cdf_update,
        allow_screen_content_tools: h.allow_screen_content_tools != 0,
        force_integer_mv: h.force_integer_mv != 0,
        allow_intrabc: h.allow_intrabc,
        use_superres: h.use_superres,
        allow_high_precision_mv: h.allow_high_precision_mv,
        is_motion_mode_switchable: h.is_motion_mode_switchable,
        use_ref_frame_mvs: h.use_ref_frame_mvs,
        disable_frame_end_update_cdf: h.disable_frame_end_update_cdf,
        uniform_tile_spacing_flag: t.uniform_tile_spacing_flag,
        allow_warped_motion: h.allow_warped_motion,
        large_scale_tile: false,
    }
    .pack();
    // The REAL denominator, not the coded one, and `SUPERRES_NUM` when superres is
    // off — libva documents 8 there and 9..=16 otherwise, so a 0 would be outside
    // the field's stated range.
    pic_params.superres_scale_denominator = if h.use_superres {
        narrow("superres_denom", h.superres_denom)?
    } else {
        SUPERRES_NUM
    };
    pic_params.interp_filter = h.interpolation_filter as u8;
    pic_params.filter_level = [lf.loop_filter_level[0], lf.loop_filter_level[1]];
    pic_params.filter_level_u = lf.loop_filter_level[2];
    pic_params.filter_level_v = lf.loop_filter_level[3];
    pic_params.loop_filter_info_fields = LoopFilterInfoFieldsAV1 {
        sharpness_level: lf.loop_filter_sharpness,
        mode_ref_delta_enabled: lf.loop_filter_delta_enabled,
        mode_ref_delta_update: lf.loop_filter_delta_update,
    }
    .pack();
    pic_params.ref_deltas = lf.loop_filter_ref_deltas;
    pic_params.mode_deltas = lf.loop_filter_mode_deltas;
    pic_params.base_qindex = narrow("base_qindex", q.base_q_idx)?;
    // The five deltas are `su(1+6)` reads, so the parser cannot hand out anything
    // outside -63..=63 and the narrowing cannot truncate.
    pic_params.y_dc_delta_q = q.delta_q_y_dc as i8;
    pic_params.u_dc_delta_q = q.delta_q_u_dc as i8;
    pic_params.u_ac_delta_q = q.delta_q_u_ac as i8;
    pic_params.v_dc_delta_q = q.delta_q_v_dc as i8;
    pic_params.v_ac_delta_q = q.delta_q_v_ac as i8;
    pic_params.qmatrix_fields = QmatrixFieldsAV1 {
        using_qmatrix: q.using_qmatrix,
        // No 0xFF sentinel here, unlike DXVA: libva carries `using_qmatrix` itself,
        // so a frame without a matrix simply leaves these ignored.
        qm_y: narrow("qm_y", q.qm_y)?,
        qm_u: narrow("qm_u", q.qm_u)?,
        qm_v: narrow("qm_v", q.qm_v)?,
    }
    .pack();
    pic_params.mode_control_fields = ModeControlFieldsAV1 {
        delta_q_present_flag: q.delta_q_present,
        log2_delta_q_res: narrow("delta_q_res", q.delta_q_res)?,
        delta_lf_present_flag: lf.delta_lf_present,
        log2_delta_lf_res: lf.delta_lf_res,
        delta_lf_multi: lf.delta_lf_multi,
        tx_mode: h.tx_mode as u8,
        reference_select: h.reference_select,
        reduced_tx_set_used: h.reduced_tx_set,
        skip_mode_present: h.skip_mode_present,
    }
    .pack();
    // The parser holds `CdefDamping` (coded + 3); libva wants the coded value.
    pic_params.cdef_damping_minus_3 =
        narrow("cdef_damping_minus_3", c.cdef_damping.saturating_sub(3))?;
    pic_params.cdef_bits = narrow("cdef_bits", c.cdef_bits)?;
    pic_params.cdef_y_strengths = cdef_y_strengths;
    pic_params.cdef_uv_strengths = cdef_uv_strengths;
    pic_params.loop_restoration_fields = LoopRestorationFieldsAV1 {
        // The parser's `FrameRestorationType` IS the spec's, so no remap — see
        // [`LoopRestorationFieldsAV1`]'s docs for why libavcodec appears to remap
        // and this does not.
        yframe_restoration_type: lr.frame_restoration_type[0] as u8,
        cbframe_restoration_type: lr.frame_restoration_type[1] as u8,
        crframe_restoration_type: lr.frame_restoration_type[2] as u8,
        lr_unit_shift: lr.lr_unit_shift,
        lr_uv_shift: lr.lr_uv_shift,
    }
    .pack();
    pic_params.wm = wm;
    // Left zero, and deliberately: `apply_grain` is 0 on every frame that reaches
    // here, which libva documents as "all the rest parameters should be set to zero
    // and ignored".
    pic_params.film_grain_info.film_grain_info_fields = FilmGrainInfoFieldsAV1::default().pack();

    Ok(DecodePlanVaAv1 {
        pic_params,
        tile_groups,
        setup_slot,
        setup_id,
        substituted_refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::Av1Planner;
    use std::collections::HashMap;

    const AV1_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// A surface table with a recognisable value per ledger slot, so a wrong index
    /// is a wrong NUMBER rather than a plausible one.
    fn surface_table() -> Vec<u32> {
        (0..NUM_REF_SLOTS as u32 + 1).map(|i| 0x1000 + i).collect()
    }

    /// The caller's binding step, reproduced: re-derive the ledger-slot → surface
    /// table from the ledger (a slot the conversion released binds nothing), then
    /// bind the picture just converted.
    ///
    /// This is `video_vaapi_native`'s `bind_setup` + `sync_slot_bindings` field for
    /// field, down to asking the LEDGER where the picture landed rather than being told
    /// — and the test has to do it because the surface table the NEXT frame resolves
    /// its references through is exactly this table. A fixed table would let the
    /// reference checks below pass while reading somebody else's surface.
    ///
    /// `surface` is `None` for the refusal path, where the conversion assigned the slot
    /// but nothing was decoded into a surface for it. Binding nothing is the caller's
    /// half of [`plan_to_va_av1`]'s contract.
    fn bind(
        slot_surface: &mut [u32],
        slots: &SlotMap,
        stored: Option<PicId>,
        surface: Option<u32>,
    ) {
        let live: std::collections::HashSet<u8> = slots.held().map(|(slot, _)| slot).collect();
        for (index, bound) in slot_surface.iter_mut().enumerate() {
            if !live.contains(&(index as u8)) {
                *bound = VA_INVALID_SURFACE;
            }
        }
        if let Some(slot) = stored.and_then(|id| slots.slot_of(id)) {
            slot_surface[usize::from(slot)] = surface.unwrap_or(VA_INVALID_SURFACE);
        }
    }

    /// The whole vendored vector, converted — and every statement that could be
    /// transposed checked against an independently kept shadow of the truth.
    ///
    /// The load-bearing assertions are the two indexings this API gets wrong most
    /// easily: `ref_frame_map` is by AV1 SLOT (checked against a `PicId → surface`
    /// map this test keeps itself, so a ledger-slot index would read a different
    /// surface), and `wm[]` is by reference NAME (checked against
    /// `gm_params[name + 1]`, so the off-by-one that hands every reference its
    /// neighbour's warp fails here).
    #[test]
    fn the_whole_vendored_vector_converts_and_the_indexings_hold() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        // The caller's ledger-slot → surface bindings, maintained exactly as the
        // client maintains them.
        let mut slot_surface = vec![VA_INVALID_SURFACE; NUM_REF_SLOTS + 1];
        // Our own PicId → surface record, kept INDEPENDENTLY of the ledger — so a
        // conversion that indexed the store by the wrong thing reads a surface this
        // map disagrees with.
        let mut surface_of: HashMap<u64, u32> = HashMap::new();

        let (mut frames, mut inter, mut warped, mut cdef_fixups) = (0u32, 0u32, 0u32, 0u32);
        let mut multi_ref_slot_pictures = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                let Some(id) = plan.dpb.stored else {
                    continue;
                };
                frames += 1;
                // The caller's free-list choice, faked deterministically: a surface
                // number that is unique per picture, so a stale binding is visible.
                let setup_surface = 0x9000 + frames;
                // The table is snapshotted BEFORE the conversion, as the client does
                // — references resolve against the pre-removal bindings.
                let surfaces = slot_surface.clone();
                let va = plan_to_va_av1(&plan, packet, &mut slots, &surfaces, setup_surface)
                    .unwrap_or_else(|e| panic!("frame {frames}: {e}"));
                surface_of.insert(id, setup_surface);
                bind(&mut slot_surface, &slots, Some(id), Some(setup_surface));

                assert_eq!(va.pic_params.current_frame, setup_surface);
                assert_eq!(
                    va.pic_params.current_display_picture, setup_surface,
                    "no film grain means the display surface IS the decode target"
                );

                let h = &*plan.header;
                let shown_key = h.frame_type == FrameType::KeyFrame && h.show_frame;

                // --- ref_frame_map is by AV1 SLOT and holds SURFACES ---------
                let mut expected = [VA_INVALID_SURFACE; NUM_REF_SLOTS];
                if !shown_key {
                    for r in &plan.dpb_refs {
                        expected[usize::from(r.slot)] = *surface_of
                            .get(&r.id)
                            .unwrap_or_else(|| panic!("frame {frames}: no surface for {}", r.id));
                    }
                }
                assert_eq!(
                    va.pic_params.ref_frame_map, expected,
                    "frame {frames}: the store must be indexed by AV1 slot and hold \
                     the surface each slot's picture decoded into"
                );
                assert_eq!(
                    va.substituted_refs, 0,
                    "frame {frames}: a stream that lost nothing must conceal nothing — \
                     a substitution here means the reference plumbing is inventing \
                     surfaces on a clean vector"
                );
                if shown_key {
                    assert!(
                        va.pic_params
                            .ref_frame_map
                            .iter()
                            .all(|&s| s == VA_INVALID_SURFACE),
                        "frame {frames}: a shown key frame publishes an empty store"
                    );
                }
                // One picture in SEVERAL AV1 slots is what makes the indexing above
                // falsifiable: while every picture holds exactly one slot, an
                // AV1-slot index and a per-picture index cannot be told apart.
                let distinct_slots: std::collections::HashSet<u8> =
                    plan.dpb_refs.iter().map(|r| r.slot).collect();
                assert_eq!(
                    distinct_slots.len(),
                    plan.dpb_refs.len(),
                    "the marked store lists each slot once"
                );
                let distinct_ids: std::collections::HashSet<u64> =
                    plan.dpb_refs.iter().map(|r| r.id).collect();
                if distinct_ids.len() < plan.dpb_refs.len() {
                    multi_ref_slot_pictures += 1;
                }

                // --- ref_frame_idx is by NAME and holds a SLOT ---------------
                assert_eq!(
                    va.pic_params.ref_frame_idx, h.ref_frame_idx,
                    "frame {frames}: the name table is the header's own slot list"
                );
                if !h.frame_is_intra {
                    inter += 1;
                    for (name, r) in plan.refs.iter().enumerate() {
                        let r = r.expect("the clean vector loses no reference");
                        assert_eq!(
                            va.pic_params.ref_frame_idx[name], r.slot,
                            "frame {frames}: name {name} must carry its SLOT"
                        );
                        assert_eq!(
                            va.pic_params.ref_frame_map[usize::from(r.slot)],
                            surface_of[&r.id],
                            "frame {frames}: following name {name} through the slot \
                             table must reach that reference's own surface"
                        );
                    }
                }

                // --- global motion is by NAME, one step off the parser -------
                for name in 0..REFS_PER_FRAME {
                    let gm = &h.global_motion_params;
                    assert_eq!(
                        va.pic_params.wm[name].wmmat[..6],
                        gm.gm_params[name + 1],
                        "frame {frames}: wm[{name}] must be reference name \
                         {}'s warp, not slot {name}'s",
                        name + 1
                    );
                    assert_eq!(va.pic_params.wm[name].wmmat[6..], [0, 0]);
                    assert_eq!(va.pic_params.wm[name].wmtype, gm.gm_type[name + 1] as u32);
                    assert_eq!(
                        va.pic_params.wm[name].invalid,
                        u8::from(!gm.warp_valid[name + 1])
                    );
                    if va.pic_params.wm[name].wmtype != 0 {
                        warped += 1;
                    }
                }

                // --- the tile records address the tile PAYLOADS --------------
                let grid = (h.tile_info.tile_cols * h.tile_info.tile_rows) as usize;
                let records: usize = va.tile_groups.iter().map(|g| g.tiles.len()).sum();
                assert_eq!(records, grid, "frame {frames}: one record per tile");
                for group in &va.tile_groups {
                    let region = &packet[group.data.clone()];
                    for tile in &group.tiles {
                        let start = tile.slice_data_offset as usize;
                        let end = start + tile.slice_data_size as usize;
                        assert!(
                            end <= region.len(),
                            "frame {frames}: a record runs past its data buffer"
                        );
                        // The bytes the record addresses must BE a tile payload —
                        // and specifically not the group's own header, which is
                        // where the region starts and the payload does not.
                        assert!(
                            !region[start..end].is_empty(),
                            "frame {frames}: an empty tile"
                        );
                    }
                    // The whole region must be accounted for: every tile's payload
                    // plus one `TileSizeBytes` field per tile EXCEPT the last. This
                    // is `tile_group_obu()`'s own arithmetic and is a fact about the
                    // bitstream rather than about this conversion, which is what
                    // makes it independent of the offsets it checks.
                    let size_bytes = if grid > 1 {
                        h.tile_info.tile_size_bytes as usize
                    } else {
                        0
                    };
                    let payloads: usize =
                        group.tiles.iter().map(|t| t.slice_data_size as usize).sum();
                    assert_eq!(
                        payloads + (group.tiles.len() - 1) * size_bytes,
                        group.data.end - group.data.start,
                        "frame {frames}: the group's tiles and its size fields must \
                         account for the region exactly"
                    );
                    assert_eq!(
                        group.tiles[0].slice_data_offset, 0,
                        "the first tile's payload starts the tile_data region"
                    );
                }

                // --- the scalar traps ----------------------------------------
                assert_eq!(
                    va.pic_params.frame_width_minus1 as u32,
                    h.upscaled_width - 1,
                    "frame {frames}: the width field is the UPSCALED width"
                );
                assert_eq!(va.pic_params.frame_height_minus1 as u32, h.frame_height - 1);
                assert_eq!(
                    va.pic_params.superres_scale_denominator, SUPERRES_NUM,
                    "this vector uses no superres, so the denominator is 8 — never 0"
                );
                assert_eq!(
                    va.pic_params.order_hint_bits_minus_1 as i32,
                    plan.sequence.order_hint_bits_minus_1,
                    "the vector enables order hints, so the field is the parser's"
                );
                let coded = 1usize << h.cdef_params.cdef_bits;
                for i in 0..coded {
                    let sec = va.pic_params.cdef_y_strengths[i] & 0x3;
                    let pri = va.pic_params.cdef_y_strengths[i] >> 2;
                    assert_eq!(pri as u32, h.cdef_params.cdef_y_pri_strength[i]);
                    if h.cdef_params.cdef_y_sec_strength[i] == 4 {
                        cdef_fixups += 1;
                        assert_eq!(
                            sec, 3,
                            "frame {frames}: the spec's in-place 4 is the coded 3, \
                             and masking it with 3 would send 0"
                        );
                    } else {
                        assert_eq!(sec as u32, h.cdef_params.cdef_y_sec_strength[i]);
                    }
                }
            }
        }

        assert_eq!(frames, 274, "every frame of the vector converted");
        assert!(
            inter > 0,
            "no inter frame: the name-table checks were vacuous"
        );
        assert!(
            multi_ref_slot_pictures > 0,
            "no picture ever occupied two reference slots at once, so nothing here \
             could tell an AV1-slot index from a per-picture one"
        );
        assert!(
            cdef_fixups > 0,
            "no frame coded a secondary strength needing the fixup, so the CDEF \
             packing above compared a correction against a stream that never needs it"
        );
        // ⚠ Honest about what this vector does NOT cover: it codes no global motion
        // at all, so the wm[] comparison above proves the INDEXING (each entry is
        // read from `gm_params[name + 1]`) but every value compared is the identity
        // warp. A transposition of two identical zeros is invisible.
        eprintln!(
            "frames {frames} · inter {inter} · non-identity warps {warped} \
             (0 means the warp VALUES are untested; the indexing is not) · \
             cdef fixups {cdef_fixups}"
        );
    }

    /// A `show_existing_frame` plan has no submission — and the caller must be able
    /// to tell that apart from a failure.
    ///
    /// ⚠ Built by hand, because the vendored vector uses `show_existing_frame`
    /// **zero times** (pf-bitstream's own planner test says so and asserts it stays
    /// 0). So what is exercised here is this function's `dpb.stored == None` arm and
    /// nothing about the parser's display-only path.
    #[test]
    fn a_show_existing_frame_plan_is_not_a_decode() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();
        let display_only = AuPlanAv1 {
            dpb: pf_bitstream::av1::DpbUpdate {
                stored: None,
                outputs: vec![1],
                removed: Vec::new(),
            },
            tiles: Vec::new(),
            ..key
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        assert_eq!(
            plan_to_va_av1(&display_only, first, &mut slots, &surface_table(), 7).err(),
            Some(PlanToVaAv1Error::NoDecode)
        );
        assert_eq!(slots.active(), 0, "a refusal must not touch the ledger");
    }

    /// Film grain is refused, not approximated (module docs) — and the refusal costs
    /// this frame only.
    ///
    /// ⚠ The ledger assertion is the load-bearing half now. The gate sits AFTER the
    /// mutation block precisely so a grained frame in the middle of a GOP does not
    /// leave the ledger one picture short of the planner's store, which would turn
    /// every later reference to it into a hard `UnresolvedReference` — a refusal that
    /// can never repair itself.
    #[test]
    fn a_frame_that_applies_film_grain_is_refused() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();

        // The vector codes neither, so both halves of the gate are set by hand.
        let mut seq = (*key.sequence).clone();
        seq.film_grain_params_present = true;
        let mut header = (*key.header).clone();
        header.film_grain_params.apply_grain = true;
        let grained = AuPlanAv1 {
            sequence: std::rc::Rc::new(seq.clone()),
            header: std::rc::Rc::new(header),
            ..key.clone()
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        assert_eq!(
            plan_to_va_av1(&grained, first, &mut slots, &surface_table(), 7).err(),
            Some(PlanToVaAv1Error::FilmGrain)
        );
        let stored = grained.dpb.stored.expect("the key frame is stored");
        assert_eq!(
            slots.slot_of(stored),
            Some(0),
            "the refusal must leave the ledger holding the picture the PLANNER stored \
             — the planner has no idea this rung said no, and every later frame that \
             names this picture resolves through this ledger"
        );

        // A sequence that DECLARES the tool but a frame that does not apply it is
        // ordinary: the declaration alone must not cost the session this rung.
        let declared_only = AuPlanAv1 {
            sequence: std::rc::Rc::new(seq),
            ..key
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let va = plan_to_va_av1(&declared_only, first, &mut slots, &surface_table(), 7)
            .expect("a declared-but-unused film grain tool still decodes");
        assert_eq!(
            va.pic_params.film_grain_info,
            crate::va_av1::VaFilmGrainStructAV1::zeroed(),
            "apply_grain is 0, which libva documents as 'set the rest to zero'"
        );
        assert_eq!(
            va.pic_params.seq_info_fields & (1 << 15),
            1 << 15,
            "the sequence's declaration is still reported"
        );
    }

    /// `order_hint_bits_minus_1` must be 0 — not 255 — when order hints are off.
    ///
    /// The parser stores **-1** there, and `as u8` on that is 255: a decoder told
    /// its order hints are 256 bits wide. Worth its own test because no vector here
    /// disables order hints, so nothing else would ever exercise the branch.
    #[test]
    fn order_hints_off_sends_zero_not_the_parsers_minus_one() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();
        assert!(
            key.sequence.enable_order_hint,
            "the vector enables order hints; this test is about the other branch"
        );
        let mut seq = (*key.sequence).clone();
        seq.enable_order_hint = false;
        seq.order_hint_bits_minus_1 = -1;
        seq.order_hint_bits = 0;
        let plan = AuPlanAv1 {
            sequence: std::rc::Rc::new(seq),
            ..key
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let va = plan_to_va_av1(&plan, first, &mut slots, &surface_table(), 7).expect("converts");
        assert_eq!(va.pic_params.order_hint_bits_minus_1, 0);
        assert_eq!(
            va.pic_params.seq_info_fields & (1 << 7),
            0,
            "and the sequence flag says so too"
        );
    }

    /// A frame that refreshes no slot gives its ledger slot straight back.
    ///
    /// Nine such frames would otherwise fill a nine-slot ledger and kill the session
    /// with `SlotError::Full` on a perfectly legal stream, which is the defect the
    /// Vulkan and DXVA rungs each had to close separately.
    #[test]
    fn a_frame_that_refreshes_nothing_returns_its_slot() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let surfaces = surface_table();

        // The real key frame refreshes all eight slots and keeps its ledger slot.
        let va = plan_to_va_av1(&key, first, &mut slots, &surfaces, 7).expect("converts");
        assert_eq!(va.setup_slot, Some(0));
        assert_eq!(slots.active(), 1);

        // The same frame with `refresh_frame_flags == 0`: converted, then released.
        let mut header = (*key.header).clone();
        header.refresh_frame_flags = 0;
        let ephemeral = AuPlanAv1 {
            header: std::rc::Rc::new(header),
            dpb: pf_bitstream::av1::DpbUpdate {
                stored: Some(999),
                outputs: vec![999],
                removed: Vec::new(),
            },
            ..key
        };
        let va = plan_to_va_av1(&ephemeral, first, &mut slots, &surfaces, 8).expect("converts");
        assert_eq!(
            va.setup_slot, None,
            "nothing binds the surface — only the pending output claims it"
        );
        assert_eq!(
            slots.active(),
            1,
            "the ledger is back where it was; a ninth such frame must still fit"
        );
        assert_eq!(slots.slot_of(999), None);
    }

    /// A ledger sized for another codec is refused rather than silently overflowed.
    #[test]
    fn a_ledger_of_the_wrong_capacity_is_refused() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("plans")
            .first()
            .expect("a frame")
            .clone();
        // An H.264-shaped ledger (4-frame DPB) cannot hold AV1's eight slots.
        let mut slots = SlotMap::new(4);
        assert_eq!(
            plan_to_va_av1(&key, first, &mut slots, &surface_table(), 7).err(),
            Some(PlanToVaAv1Error::CapacityMismatch {
                required: 9,
                capacity: 5
            })
        );
    }

    /// A short surface table is refused BEFORE the ledger is touched, so the caller's
    /// post-call bind is always in range.
    #[test]
    fn a_short_surface_table_is_refused_before_any_mutation() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("plans")
            .first()
            .expect("a frame")
            .clone();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let short = vec![0u32; NUM_REF_SLOTS];
        assert!(matches!(
            plan_to_va_av1(&key, first, &mut slots, &short, 7).err(),
            Some(PlanToVaAv1Error::SurfaceOutOfRange { .. })
        ));
        assert_eq!(slots.active(), 0);
    }

    /// **The lost-packet regression.** A frame header whose tile groups did not arrive
    /// refuses — and the GOP survives it.
    ///
    /// This is the shape one lost UDP packet makes, and pf-bitstream produces it
    /// deliberately: `plan_au` pushes a plan whose picture IS stored and whose tile list
    /// is short or empty, with a `TruncatedAu` warning saying so. The planner's own
    /// reference store already holds that picture by then, so a refusal that skipped
    /// this rung's `slots.assign` would leave the two permanently one picture apart —
    /// and the very next frame that names it would refuse with `UnresolvedReference`,
    /// which is ALSO before the assignment and so can never repair. Every frame to the
    /// next shown key frame would hard-error: one lost packet, one lost GOP.
    ///
    /// So this test asserts the three things that stop that: the refusal is
    /// recognisable as damage ([`PlanToVaAv1Error::lost_tiles`]), the ledger holds the
    /// picture the planner stored, and the NEXT access unit converts — with the lost
    /// picture's slot concealed by a live surface rather than resolved to the surface
    /// of whatever picture held that slot before.
    #[test]
    fn a_truncated_access_unit_refuses_but_leaves_the_ledger_in_step() {
        let packets: Vec<&[u8]> = IvfIterator::new(AV1_25FPS).take(3).collect();
        assert_eq!(packets.len(), 3, "the vector has at least three packets");
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut slot_surface = vec![VA_INVALID_SURFACE; NUM_REF_SLOTS + 1];

        // --- packet 0: an ordinary shown key frame -----------------------------
        let key = planner.plan_au(packets[0]).expect("plans").remove(0);
        let key_id = key.dpb.stored.expect("the key frame decodes");
        plan_to_va_av1(&key, packets[0], &mut slots, &slot_surface.clone(), 0x9001)
            .expect("the key frame converts");
        bind(&mut slot_surface, &slots, Some(key_id), Some(0x9001));

        // --- packet 1: two frames, and the FIRST loses its tile groups ---------
        //
        // Packet 1 of this vector is the standard AV1 shape: a hidden ALTREF that later
        // frames predict from, then the frame that displays. Losing the hidden one is
        // the worst case — nothing shows it, so nothing else would ever notice.
        let mut unit = planner.plan_au(packets[1]).expect("plans");
        assert_eq!(
            unit.len(),
            2,
            "packet 1 is a hidden ALTREF plus a shown frame"
        );
        let shown = unit.remove(1);
        let mut lost = unit.remove(0);
        let lost_id = lost.dpb.stored.expect("a truncated frame is still STORED");
        assert!(
            !lost.header.show_frame,
            "the frame this test loses is the hidden one"
        );
        assert!(
            !lost.tiles.is_empty(),
            "the vector's own packet carries tiles; this test takes them away"
        );
        // Exactly what pf-bitstream hands over when the tile-group OBUs are gone: the
        // picture is stored, the tile list is empty, and the warning says damage.
        lost.tiles.clear();
        lost.warnings
            .push(pf_bitstream::av1::PlanWarning::TruncatedAu { offset: 0 });
        assert!(
            lost.warnings.iter().any(crate::is_integrity_warning_av1),
            "the plan this test drives must be one the rung CONCEALS"
        );

        let refusal = plan_to_va_av1(&lost, packets[1], &mut slots, &slot_surface.clone(), 0x9002)
            .expect_err("no tiles, nothing to submit");
        assert_eq!(refusal, PlanToVaAv1Error::NoTiles);
        assert!(
            refusal.lost_tiles(),
            "the caller tells damage from a defect through this predicate; a refusal \
             it does not recognise is a hard error and demotes the rung"
        );
        let ledger_slot = slots.slot_of(lost_id).expect(
            "THE REGRESSION: the planner stored this picture, so the ledger must hold \
             it too — without this every later reference to it is a hard Err that \
             never repairs",
        );
        // The caller's half of the contract: the slot is live, but NOTHING is bound to
        // it, because nothing was decoded.
        bind(&mut slot_surface, &slots, Some(lost_id), None);
        assert_eq!(
            slot_surface[usize::from(ledger_slot)],
            VA_INVALID_SURFACE,
            "a picture that never decoded must not inherit the surface of whatever \
             held its ledger slot before"
        );

        // --- the rest of the unit, and the next one, must still convert --------
        let mut substituted_somewhere = false;
        for (plan, packet, surface) in [
            (&shown, packets[1], 0x9003u32),
            (
                &planner.plan_au(packets[2]).expect("plans").remove(0),
                packets[2],
                0x9004,
            ),
        ] {
            let id = plan.dpb.stored.expect("decodes");
            assert!(
                plan.dpb_refs.iter().any(|r| r.id == lost_id),
                "this frame must reference the truncated picture or it proves nothing"
            );
            let va = plan_to_va_av1(plan, packet, &mut slots, &slot_surface.clone(), surface)
                .expect("a frame after a lost one converts — it does not hard-error");

            // Every slot the lost picture holds is concealed with a LIVE surface, and
            // the surface chosen is a picture that really decoded (the key frame's),
            // never the `VA_INVALID_SURFACE` a driver would dereference.
            for r in &plan.dpb_refs {
                let bit = 1u8 << r.slot;
                if r.id == lost_id {
                    substituted_somewhere = true;
                    assert_eq!(
                        va.substituted_refs & bit,
                        bit,
                        "slot {} holds a picture with no surface and must be reported \
                         substituted",
                        r.slot
                    );
                    assert_eq!(
                        va.pic_params.ref_frame_map[usize::from(r.slot)],
                        0x9001,
                        "and it must point at a picture that DECODED — the key \
                         frame's surface — rather than at nothing"
                    );
                } else {
                    assert_eq!(
                        va.substituted_refs & bit,
                        0,
                        "slot {} resolved; substituting it would hide a real reference",
                        r.slot
                    );
                }
            }
            bind(&mut slot_surface, &slots, Some(id), Some(surface));
        }
        assert!(
            substituted_somewhere,
            "no frame ever named the lost picture, so nothing above was checked"
        );
    }

    /// A store that resolved NOTHING still submits live surfaces.
    ///
    /// The fallback arm of the substitution, which the truncated-AU test above cannot
    /// reach: with no decoded reference to reach for, the decode target itself is the
    /// "alternative frame buffer" `va_dec_av1.h:352` prescribes. What must never
    /// happen is `VA_INVALID_SURFACE` reaching a driver the same header says is *"not
    /// responsible to validate reference frames' id"*.
    #[test]
    fn a_store_with_no_surfaces_at_all_falls_back_to_the_decode_target() {
        let packets: Vec<&[u8]> = IvfIterator::new(AV1_25FPS).take(2).collect();
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);

        let key = planner.plan_au(packets[0]).expect("plans").remove(0);
        let key_id = key.dpb.stored.expect("decodes");
        plan_to_va_av1(&key, packets[0], &mut slots, &surface_table(), 0x9001).expect("converts");
        assert!(slots.slot_of(key_id).is_some());

        // The key frame's picture holds every slot, and the caller bound none of them —
        // the state after a whole access unit was refused.
        let unbound = vec![VA_INVALID_SURFACE; NUM_REF_SLOTS + 1];
        let next = planner.plan_au(packets[1]).expect("plans").remove(0);
        assert_eq!(next.dpb_refs.len(), NUM_REF_SLOTS, "a full store");
        let va = plan_to_va_av1(&next, packets[1], &mut slots, &unbound, 0x9002).expect("converts");
        assert_eq!(
            va.substituted_refs, 0xff,
            "every slot of the store was concealed"
        );
        assert_eq!(
            va.pic_params.ref_frame_map, [0x9002; NUM_REF_SLOTS],
            "with nothing else live, the decode target is the substitute"
        );

        // ⚠ And a SHOWN KEY FRAME is exempt: libavcodec publishes an all-invalid map
        // there deliberately, and that is the one path every driver is exercised on.
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let va = plan_to_va_av1(&key, packets[0], &mut slots, &unbound, 0x9001).expect("converts");
        assert_eq!(va.substituted_refs, 0);
        assert_eq!(va.pic_params.ref_frame_map, [VA_INVALID_SURFACE; 8]);
    }
}
