//! [`VkAv1Decoder`]: the assembled native AV1 decoder — [`crate::decoder_h265`]
//! one codec over, over pf-bitstream's AV1 planner and M7's CPU half.
//!
//! Per access unit: `plan_au` → (per frame) `plan_to_vk_av1` → tile OBUs into the
//! bitstream ring → record (barriers, `vkCmdBeginVideoCodingKHR` with every bound
//! DPB slot, the one-time session RESET control, a caps-gated
//! `RESULT_STATUS_ONLY` query bracketing `vkCmdDecodeVideoKHR`) → submit on the
//! decode queue under the caller's [`QueueLock`] with a per-image timeline signal.
//!
//! Everything codec-agnostic is SHARED with the other two decoders rather than
//! re-implemented: the picture pool and its zero-copy hand-off contract
//! ([`crate::images`]), the bitstream ring, the op ring (command buffers + status
//! queries), the pending/ready/graveyard bookkeeping, `build_frame` and the DPB
//! settle (`settle_dpb_ids`, split off `settle_dpb` precisely so AV1's own
//! `DpbUpdate` type can share it). What is genuinely AV1's own lives here:
//!
//! - **One access unit is a TEMPORAL UNIT, and may carry several frames.**
//!   `Av1Planner::plan_au` returns a VECTOR — the vendored 250-packet vector holds
//!   274 frames, the extras being hidden ALTREFs. Every plan is decoded, in order;
//!   the frames they make ready queue up and `decode` hands back the first.
//! - **A `show_existing_frame` plan decodes nothing.** It has `dpb.stored == None`
//!   and displays `dpb.outputs` — a picture an earlier, hidden frame decoded. It
//!   is settled like any other DPB verdict and never reaches a submission.
//! - **`referenceNameSlotIndices` holds DPB SLOT indices, not positions in
//!   `pReferenceSlots`.** The two coincide for as long as references happen to land
//!   in slots `0..refs.len()` in `refs` order, which on a freshly keyed stream they
//!   do — and that is exactly how the HEVC RPS defect shipped. The plan computes
//!   slots ([`DecodePlanVkAv1::reference_name_slot_indices`]); this module lays
//!   `pReferenceSlots` out in [`DecodePlanVkAv1::refs`] order INDEPENDENTLY, and
//!   [`build_scope_av1`] fails closed when the two disagree about a slot the op
//!   binds.
//! - **A DPB slot this frame READS may not be recycled until its decode op is
//!   recorded.** `refresh_frame_flags` applies AFTER the frame decodes (7.20), so
//!   almost every inter frame of a low-delay stream overwrites a slot it is
//!   reading — 268 of the vendored vector's 274 frames. Releasing that slot inside
//!   the conversion, which is what H.264 and H.265 do with their whole `removed`
//!   list, gives it to this frame's own decode target: the reference then names
//!   the slot being written. [`DecodePlanVkAv1::release_after_decode`] carries
//!   those ids and this module releases them after the submission.
//! - **A lost reference is fatal, not degraded.** `AuPlan::refs` is indexed by
//!   reference NAME and a lost reference leaves a HOLE there, so nothing is
//!   renumbered and the conversion could in principle write
//!   [`REFERENCE_NAME_UNUSED`] for it and carry on. It does not: `-1` for a name
//!   the frame DOES reference is a spec violation, and what a driver's firmware
//!   then predicts from is undefined. The AU is refused
//!   ([`VkDecodeError::MissingReferenceAv1`], predicate [`lost_reference`]),
//!   recovery is latched and the stream re-anchors on the next key frame. Since
//!   the plan became name-indexed this is defence in depth rather than the only
//!   guard.
//! - **Tiles, not slices.** `VkVideoDecodeAV1PictureInfoKHR` wants a per-TILE
//!   offset and size into the uploaded buffer, and the plan carries whole
//!   tile-group (or frame) OBUs. [`plan_bitstream`] walks each OBU's tile-group
//!   header and per-tile size fields to recover the tile payloads, and it is those
//!   payloads — nothing else — that go into the ring slot
//!   ([`crate::ring::pack_av1_tiles`]).
//!
//! Codec dispatch (which decoder a stream gets) is the client wiring's job, not
//! this crate's: the public surface here mirrors [`crate::VkH265Decoder`]
//! method-for-method so the dispatch is a three-arm enum.
//!
//! # What the bitstream buffer contains
//!
//! Exactly the raw tile payloads, concatenated, with `frameHeaderOffset` at 0 —
//! libavcodec's `vulkan_av1.c` layout, byte for byte. Nothing else goes in: no OBU
//! headers, no frame header, none of the `tile_size_minus_1` fields between tiles.
//!
//! That is a deliberate choice over the spec-literal alternative (upload the whole
//! tile-group/frame OBUs, point `frameHeaderOffset` at the real frame header). The
//! spec-literal layout is not WRONG — the per-tile offsets and sizes are the part a
//! driver indexes by and they are identical either way, AV1 has no start-code
//! scanning to be confused by the extra bytes, and `frameHeaderOffset` is read by
//! no driver in this fleet (every one of them takes the whole frame header out of
//! `pStdPictureInfo`). But libavcodec is the implementation every driver was
//! validated against, so matching it removes the residual risk on the drivers
//! nobody here has tested, uploads fewer bytes per frame, and deletes the rebase
//! arithmetic that mapping in-OBU tile offsets to packed-buffer offsets needed.
//!
//! # `pTileOffsets` / `pTileSizes` are sized to the driver's read, not to tileCount
//!
//! ⚠ RADV reads `AV1_MAX_NUM_TILES` (256) entries out of both arrays
//! unconditionally — `radv_video.c`'s `for (i = 0; i < AV1_MAX_NUM_TILES; ++i)` —
//! and never looks at `tileCount`. libavcodec gets away with it because its
//! `tile_sizes` is a static `uint32_t[256]`. A `Vec` sized to the real tile count
//! (one, for every frame of the vendored vector) is a four-byte allocation the
//! driver reads a kilobyte deep. So both arrays are always 256 entries with the
//! tail zeroed, and `tileCount` is set separately — see [`SubmittedTiles`].

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::ops::Range;

use ash::vk;
use ash::vk::native as hh;
use cros_codecs::codec::av1::parser::FrameHeaderObu;
use pf_bitstream::av1::AuPlan;
use pf_bitstream::av1::Av1Planner;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::PlanWarning;
use pf_bitstream::av1::NUM_REF_SLOTS;
use pf_bitstream::h264::DisplayCrop;
use tracing::debug;
use tracing::trace;

use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps_av1::derive_caps_av1;
use crate::caps_av1::query_av1_caps;
use crate::caps_av1::Av1ProfileKey;
use crate::decoder::build_frame;
use crate::decoder::settle_dpb_ids;
use crate::decoder::wait_timeline;
use crate::decoder::DecodeStatus;
use crate::decoder::DecodedVkFrame;
use crate::decoder::OpRing;
use crate::decoder::PendingPic;
use crate::decoder::RetiredPool;
use crate::decoder::VkDecodeError;
use crate::decoder_h265::RecoveryLatch;
use crate::device::DecodeDevice;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::device::QueueSubmitGuard;
use crate::images::plan_pools;
use crate::images::DpbPool;
use crate::images::PicturePool;
use crate::pic_av1::plan_to_vk_av1;
use crate::pic_av1::DecodePlanVkAv1;
use crate::pic_av1::VkRefAv1;
use crate::pic_av1::REFERENCE_NAME_UNUSED;
use crate::ring::pack_av1_tiles;
use crate::ring::BitstreamRing;
use crate::ring::PackedAv1Tiles;
use crate::ring::RingLayout;
use crate::ring::UploadedAu;
use crate::ring::INITIAL_SLOT_SIZE;
use crate::ring::RING_SLOTS;
use crate::session_av1::ParamsActionAv1;
use crate::session_av1::SessionConfigAv1;
use crate::session_av1::VideoSessionAv1;
use crate::slots::SlotMap;

/// AV1's DPB depth: eight reference slots (`NUM_REF_FRAMES`) plus the picture
/// being decoded. Unlike H.264/H.265 this is a CONSTANT of the codec, not an SPS
/// field — so an AV1 session never renegotiates its DPB depth and
/// `plan_to_vk_av1` has no `CapacityMismatch` to answer.
const REQUIRED_SLOTS: u32 = NUM_REF_SLOTS as u32 + 1;

/// `OBU_TILE_GROUP` — the OBU type carrying tile data on its own.
const OBU_TILE_GROUP: u8 = 4;
/// `OBU_FRAME` — a frame header and its tile group in one OBU.
const OBU_FRAME: u8 = 6;

/// Why an access unit's tile OBUs cannot be turned into the per-tile byte ranges
/// `VkVideoDecodeAV1PictureInfoKHR` wants.
///
/// Every variant is a MALFORMED-INPUT verdict, and every one of them refuses the
/// AU. Submitting the whole OBU as if it were tile payload would hand the hardware
/// the OBU header, the tile-group header and the `tile_size_minus_1` fields as
/// entropy-coded data — plausible-looking garbage, which is the outcome this crate
/// exists to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Av1TileError {
    /// The OBU (or a field inside it) runs past the access unit.
    Truncated { obu: usize },
    /// `obu_forbidden_bit` was set: this is not an OBU header.
    NotAnObu { obu: usize },
    /// An OBU type the plan's tile list should never contain — only
    /// `OBU_TILE_GROUP` and `OBU_FRAME` carry tiles.
    UnexpectedObu { obu: usize, obu_type: u8 },
    /// The frame's tile info claims no tiles at all, so nothing can be located.
    NoTiles,
    /// The OBU's own `obu_size` field disagrees with the byte range the plan
    /// carries for it.
    ///
    /// Worth its own variant because it is the one cross-check the tile walk gets
    /// for free: the AV1 spec makes the LAST tile's size implicit (whatever is
    /// left), so a walk always ends flush with the payload no matter how wrong the
    /// preceding sizes were. `obu_size` is the only independent statement of where
    /// the payload ends, and a range that disagrees with it means every offset
    /// derived from that range is suspect.
    SizeMismatch {
        obu: usize,
        declared_end: usize,
        ranged_end: usize,
    },
    /// A tile offset or size beyond the `u32` fields Vulkan submits.
    Overflow,
    /// More tiles than [`AV1_MAX_NUM_TILES`], which is as many as the submission
    /// arrays hold and as many as libavcodec accepts.
    TooManyTiles { tiles: usize },
}

impl std::fmt::Display for Av1TileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Av1TileError::Truncated { obu } => {
                write!(f, "tile OBU {obu} runs past the access unit")
            }
            Av1TileError::NotAnObu { obu } => {
                write!(f, "tile OBU {obu} has obu_forbidden_bit set")
            }
            Av1TileError::UnexpectedObu { obu, obu_type } => {
                write!(
                    f,
                    "tile OBU {obu} has type {obu_type}, which carries no tiles"
                )
            }
            Av1TileError::NoTiles => write!(f, "the frame header codes no tiles"),
            Av1TileError::SizeMismatch {
                obu,
                declared_end,
                ranged_end,
            } => write!(
                f,
                "tile OBU {obu} declares its payload ending at {declared_end}, the \
                 plan's range ends at {ranged_end}"
            ),
            Av1TileError::Overflow => {
                write!(f, "a tile offset or size exceeds the u32 Vulkan submits")
            }
            Av1TileError::TooManyTiles { tiles } => write!(
                f,
                "{tiles} tiles exceed the {AV1_MAX_NUM_TILES} a submission carries"
            ),
        }
    }
}

impl std::error::Error for Av1TileError {}

/// The bitstream facts one AV1 frame's submission needs, in ACCESS-UNIT
/// coordinates: every tile's raw payload range, in decode order.
///
/// These ranges ARE what gets uploaded — the module docs' layout — so the packed
/// offsets fall straight out of the concatenation and there is nothing to rebase.
///
/// # Why [`Self::groups`] exists when this rung never reads it
///
/// The DXVA rung (`pf_dxvadec::pack_av1`, which depends on this crate — the link
/// only goes one way, so it cannot be a doc link) uploads a DIFFERENT layout: whole
/// `tile_data` regions, `tile_size_minus_1` fields and all, because that is
/// byte-for-byte what libavcodec's `dxva2_av1.c` hands a Windows driver and this
/// program's method there is to reproduce libavcodec rather than to reason from a
/// specification. The two layouts differ only in bytes NEITHER API's per-tile
/// offsets address, so the walk that finds the tiles is the same walk — and the
/// region each tile group contributes is a byte offset this function already
/// computes and used to throw away.
///
/// Publishing it here rather than duplicating the walk in pf-dxvadec is the same
/// call [`SlotMap`] records: a second copy of 150 lines of spec-literal byte
/// arithmetic buys one fewer crate edge and costs a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1Bitstream {
    /// Every tile's raw payload, in decode order across all of the frame's tile
    /// groups. Access-unit coordinates.
    pub tiles: Vec<Range<usize>>,
    /// One region per tile-group (or frame) OBU, in plan order: the OBU's
    /// `tile_data` — from the first tile's `tile_size_minus_1` field through the
    /// end of the OBU payload. Access-unit coordinates, and every range in
    /// [`Self::tiles`] lies inside exactly one of these.
    pub groups: Vec<Range<usize>>,
}

/// Read one LEB128 value at `at`, returning it and its byte length.
fn leb128(au: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    // The AV1 spec caps leb128() at 8 bytes; a ninth continuation byte is
    // malformed, not a bigger number.
    for i in 0..8 {
        let byte = *au.get(at + i)?;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Walk one access unit's tile OBUs into per-tile payload ranges.
///
/// # Why this is here rather than in the planner
///
/// `TilePlan::data` is a whole tile-group (or frame) OBU: the OBU header, then —
/// for `OBU_FRAME` — the frame header, then the tile-group header, then for every
/// tile but the last a `tile_size_minus_1` field followed by that tile's payload.
/// Vulkan wants the PAYLOADS, one offset and one size each, which is also what
/// libavcodec's Vulkan AV1 hwaccel submits. So the walk has to happen somewhere,
/// and it happens here because everything it needs is already in the plan:
///
/// - `FrameHeaderObu::header_bytes` is the frame header's length inside the OBU
///   payload — the vendored parser's own figure, the same one it uses to hand the
///   tile group its slice of an `OBU_FRAME` — so the tile-group header's start is
///   not guessed;
/// - `TileInfo` gives `TileCols`/`TileRows` (hence `NumTiles`), the two `log2`
///   fields the `tg_start`/`tg_end` bit width comes from, and `TileSizeBytes`.
///
/// The walk is the spec's `tile_group_obu()` byte layout (5.11.1) and nothing more;
/// it decodes no tile data. It takes the plan's PIECES rather than the plan so a
/// hand-built tile group can be walked in a unit test — the vendored vector is one
/// tile per frame, so the multi-tile arithmetic below has no other way to be
/// exercised.
///
/// ⚠ What this can and cannot catch: the AV1 spec makes the LAST tile's size
/// IMPLICIT — whatever is left of the payload — so a walk always ends flush with
/// the OBU no matter how wrong the preceding sizes were, and "the sizes add up"
/// is not a check that exists. What does exist is [`Av1TileError::SizeMismatch`]:
/// the OBU's own `obu_size` field against the byte range the plan carries. A coded
/// size that OVERSHOOTS the payload is caught too ([`Av1TileError::Truncated`]);
/// one that undershoots simply shortens the last tile, and nothing in the
/// bitstream contradicts it.
pub fn plan_bitstream(
    au: &[u8],
    plan_tiles: &[pf_bitstream::av1::TilePlan],
    header: &FrameHeaderObu,
) -> Result<Av1Bitstream, Av1TileError> {
    let tile_info = &header.tile_info;
    let num_tiles = tile_info
        .tile_cols
        .checked_mul(tile_info.tile_rows)
        .unwrap_or(0);
    if num_tiles == 0 {
        return Err(Av1TileError::NoTiles);
    }

    let mut tiles: Vec<Range<usize>> = Vec::with_capacity(num_tiles as usize);
    let mut groups: Vec<Range<usize>> = Vec::with_capacity(plan_tiles.len());

    for (index, tile_group) in plan_tiles.iter().enumerate() {
        let obu = &tile_group.data;
        if obu.end > au.len() || obu.start >= obu.end {
            return Err(Av1TileError::Truncated { obu: index });
        }
        // --- obu_header() + the leb128 obu_size ---
        let first = au[obu.start];
        if first & 0x80 != 0 {
            return Err(Av1TileError::NotAnObu { obu: index });
        }
        let obu_type = (first >> 3) & 0x0f;
        let extension_flag = (first >> 2) & 1 == 1;
        let has_size_field = (first >> 1) & 1 == 1;
        let mut cursor = obu
            .start
            .checked_add(1 + usize::from(extension_flag))
            .ok_or(Av1TileError::Truncated { obu: index })?;
        // The payload ends where the plan's range does: pf-bitstream builds that
        // range from the parser's `bytes_used`, which is header + obu_size. When
        // the OBU carries its own size field, the two are cross-checked — the only
        // independent statement of the payload's end there is (see the fn docs).
        // An Annex-B stream omits the field, and the range stands alone.
        let payload_end = obu.end;
        if has_size_field {
            let (size, len) = leb128(au, cursor).ok_or(Av1TileError::Truncated { obu: index })?;
            cursor += len;
            let declared_end = cursor
                .checked_add(usize::try_from(size).map_err(|_| Av1TileError::Overflow)?)
                .ok_or(Av1TileError::Overflow)?;
            if declared_end != payload_end {
                return Err(Av1TileError::SizeMismatch {
                    obu: index,
                    declared_end,
                    ranged_end: payload_end,
                });
            }
        }
        if cursor >= payload_end {
            return Err(Av1TileError::Truncated { obu: index });
        }

        // --- past the frame header, for an OBU_FRAME ---
        // Stepped OVER, never uploaded: the driver reads the frame header out of
        // `pStdPictureInfo` and the bitstream buffer holds tile payloads only
        // (module docs).
        match obu_type {
            OBU_FRAME => {
                cursor = cursor
                    .checked_add(header.header_bytes)
                    .ok_or(Av1TileError::Truncated { obu: index })?;
            }
            OBU_TILE_GROUP => {}
            other => {
                return Err(Av1TileError::UnexpectedObu {
                    obu: index,
                    obu_type: other,
                })
            }
        }
        if cursor >= payload_end {
            return Err(Av1TileError::Truncated { obu: index });
        }

        // --- tile_group_obu()'s own header ---
        // `tile_start_and_end_present_flag` is only coded when the frame has more
        // than one tile; when it IS coded and set, `tg_start`/`tg_end` follow at
        // `tile_cols_log2 + tile_rows_log2` bits each. Then byte_alignment().
        // (The flag has to be READ rather than inferred from the plan's tg_start /
        // tg_end: a single-tile-group frame codes 0/NumTiles-1 either way, and the
        // two spellings have different header lengths.)
        let mut header_bits = 0usize;
        if num_tiles > 1 {
            let present = au[cursor] & 0x80 != 0;
            header_bits += 1;
            if present {
                header_bits += 2 * (tile_info.tile_cols_log2 + tile_info.tile_rows_log2) as usize;
            }
        }
        cursor += header_bits.div_ceil(8);
        if cursor >= payload_end {
            return Err(Av1TileError::Truncated { obu: index });
        }
        // `tile_data` begins here — libavcodec's `AV1RawTileGroup::tile_data.data`,
        // which is exactly the pointer its DXVA hwaccel `memcpy`s (struct docs).
        groups.push(cursor..payload_end);

        // --- the tiles ---
        // `tg_start`/`tg_end` index tiles 0..NumTiles-1, so a group claiming more
        // than the frame has is malformed — and bounding the count here is also
        // what keeps a hostile header from steering the walk below by its own
        // arithmetic rather than by the payload.
        let count = tile_group
            .tg_end
            .checked_sub(tile_group.tg_start)
            .and_then(|span| span.checked_add(1))
            .filter(|count| *count <= num_tiles)
            .ok_or(Av1TileError::Truncated { obu: index })? as usize;
        // `TileSizeBytes` is `tile_size_bytes_minus_1 + 1` off two coded bits, so
        // it is 1..=4 — but ONLY when the frame has more than one tile. The field
        // is not coded at all for a single-tile frame (5.9.15), where the parser
        // leaves whatever it last saw (0 on a fresh one), and the vendored vector
        // is single-tile throughout: a width check applied unconditionally refuses
        // every frame of it. So it is checked exactly where it is USED, and an
        // out-of-range width is refused rather than shifted with (a debug panic,
        // and a silent wrap in release).
        let size_bytes = tile_info.tile_size_bytes as usize;
        if count > 1 && !(1..=4).contains(&size_bytes) {
            return Err(Av1TileError::Overflow);
        }
        for tile in 0..count {
            let last = tile + 1 == count;
            let size = if last {
                payload_end
                    .checked_sub(cursor)
                    .ok_or(Av1TileError::Truncated { obu: index })?
            } else {
                // le(TileSizeBytes): little-endian, TileSizeBytes wide — and read
                // from INSIDE the OBU, not merely inside the access unit.
                if cursor + size_bytes > payload_end {
                    return Err(Av1TileError::Truncated { obu: index });
                }
                let mut value = 0usize;
                for byte in 0..size_bytes {
                    value |= usize::from(au[cursor + byte]) << (8 * byte);
                }
                cursor += size_bytes;
                value + 1
            };
            let end = cursor
                .checked_add(size)
                .ok_or(Av1TileError::Truncated { obu: index })?;
            if end > payload_end {
                return Err(Av1TileError::Truncated { obu: index });
            }
            tiles.push(cursor..end);
            cursor = end;
        }
        debug_assert_eq!(
            cursor, payload_end,
            "the last tile's size is the payload remainder by construction"
        );
    }

    if tiles.is_empty() {
        return Err(Av1TileError::NoTiles);
    }
    Ok(Av1Bitstream { tiles, groups })
}

/// As many tiles as `pTileOffsets` / `pTileSizes` carry.
///
/// It is 256 because RADV reads 256 entries out of both arrays whatever
/// `tileCount` says (module docs), and because libavcodec refuses a frame with more
/// — "exceeding all defined levels in the AV1 spec".
pub(crate) const AV1_MAX_NUM_TILES: usize = 256;

/// The submission-final per-tile offsets and sizes.
///
/// Fixed 256-entry arrays with a zeroed tail and a separate `count`, because the
/// arrays are sized to what a DRIVER reads and `tileCount` states what is
/// meaningful — the two are not the same number (module docs). Handing ash a slice
/// would fuse them, since both `tile_offsets()` and `tile_sizes()` set `tileCount`
/// from the slice length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmittedTiles {
    pub(crate) offsets: [u32; AV1_MAX_NUM_TILES],
    pub(crate) sizes: [u32; AV1_MAX_NUM_TILES],
    pub(crate) count: u32,
}

/// Where each tile lands once packed into the ring slot, and how long it is.
///
/// A plain read-out of the packing: the uploaded segments ARE the tiles, so a
/// tile's offset is its segment's offset. What is left to check is that every
/// range stays inside the `u32` fields Vulkan submits — an offset that does not
/// land on the byte its tile starts at points the hardware into the middle of
/// somebody else's data, which is silent corruption rather than an error.
fn submitted_tiles(packed: &PackedAv1Tiles) -> Result<SubmittedTiles, Av1TileError> {
    if packed.segments.len() > AV1_MAX_NUM_TILES {
        return Err(Av1TileError::TooManyTiles {
            tiles: packed.segments.len(),
        });
    }
    let mut tiles = SubmittedTiles {
        offsets: [0; AV1_MAX_NUM_TILES],
        sizes: [0; AV1_MAX_NUM_TILES],
        count: packed.segments.len() as u32,
    };
    for (i, (segment, offset)) in packed.segments.iter().zip(&packed.offsets).enumerate() {
        let size = u32::try_from(segment.len()).map_err(|_| Av1TileError::Overflow)?;
        // The tile must end inside the packed buffer too — a size that overflows
        // its own offset would be a range Vulkan reads past the buffer.
        offset.checked_add(size).ok_or(Av1TileError::Overflow)?;
        tiles.offsets[i] = *offset;
        tiles.sizes[i] = size;
    }
    Ok(tiles)
}

/// `frameHeaderOffset`, which is always 0 here: the bitstream buffer holds tile
/// payloads only, so there is no frame header in it to point at. libavcodec
/// hardcodes the same 0, and no driver in this fleet reads the field — each takes
/// the whole frame header out of `pStdPictureInfo`.
const FRAME_HEADER_OFFSET: u32 = 0;

/// The submission-final `VkVideoDecodeAV1PictureInfoKHR`.
///
/// Split out of the recording so the wiring a driver actually reads — which array
/// each pointer targets, and what `tileCount` says about them — is exercised by a
/// test rather than only by a device.
///
/// ⚠ `tileCount` is assigned AFTER both setters, not left to them. ash's
/// `tile_offsets()` and `tile_sizes()` each set it from their slice length, and the
/// arrays here are deliberately longer than the tile count (module docs): letting
/// a setter win would tell the driver there are 256 tiles.
fn av1_picture_info<'a>(
    std_pic: &'a hh::StdVideoDecodeAV1PictureInfo,
    reference_name_slot_indices: [i32; pf_bitstream::av1::REFS_PER_FRAME],
    tiles: &'a SubmittedTiles,
) -> vk::VideoDecodeAV1PictureInfoKHR<'a> {
    let mut info = vk::VideoDecodeAV1PictureInfoKHR::default()
        .std_picture_info(std_pic)
        .reference_name_slot_indices(reference_name_slot_indices)
        .frame_header_offset(FRAME_HEADER_OFFSET)
        .tile_offsets(&tiles.offsets)
        .tile_sizes(&tiles.sizes);
    info.tile_count = tiles.count;
    info
}

/// The condition [`VkAv1Decoder::decode_planned`] refuses a whole access unit on:
/// the planner reported a reference the DPB no longer holds.
///
/// A named function rather than a `find_map` inlined at the call site because it
/// is THE guard for the AV1 corruption class — a name the frame references
/// resolving to `-1`, or (before the plan became name-indexed) to the wrong
/// picture entirely — and a test that re-implements the predicate stays green when
/// the real one is deleted. Production and test call this.
///
/// Note what it does NOT match: [`PlanWarning::TruncatedAu`] is concealment
/// material the planner already accounted for, and refusing on it would turn every
/// clipped access unit into a keyframe request.
pub(crate) fn lost_reference(warnings: &[PlanWarning]) -> Option<(u8, u8)> {
    warnings.iter().find_map(|w| match w {
        PlanWarning::MissingReference { slot, ref_index } => Some((*slot, *ref_index)),
        _ => None,
    })
}

/// What [`VkAv1Decoder::decode_planned`] did with one frame of a temporal unit.
///
/// Two outcomes rather than a bare `Ok(())`, because "the plan was honoured" and
/// "the decoder is waiting for a key frame and did nothing" are opposite
/// statements about the rung, and the caller has to count the second: a unit in
/// which EVERY frame was skipped produced no picture at all and comes back as
/// [`VkDecodeError::AwaitingKeyAv1`], while a unit where a key frame cleared the
/// wait partway through decoded normally (see [`VkAv1Decoder::awaiting_key`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameOutcome {
    /// The plan was carried out: submitted, or (a `show_existing_frame`) settled
    /// into a display verdict without a submission. Either way the unit produced
    /// this frame.
    Decoded,
    /// Skipped: the decoder is waiting for the next key frame after a failure and
    /// this frame is undecodable by construction.
    SkippedAwaitingKey,
}

/// Everything tied to ONE AV1 session generation. A stream renegotiation (extent
/// or profile — including a bit-depth, sampling or film-grain switch) retires it
/// and builds fresh.
struct SessionStateAv1 {
    session: VideoSessionAv1,
    slots: SlotMap,
    /// Distinct mode's reference-only DPB backing; `None` in coincide mode (the
    /// picture pool backs the DPB there).
    dpb: Option<DpbPool>,
    pool: PicturePool,
    ring: BitstreamRing,
    ops: OpRing,
    /// Last-known Std reference info per DPB slot — `vkCmdBeginVideoCodingKHR`
    /// wants codec reference info for EVERY bound slot, including ones this frame
    /// does not reference; refreshed from each plan's setup/ref entries.
    slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>>,
    /// Coincide mode: which pool image each DPB slot currently binds (rebound at
    /// every activation — the decoupling that keeps delivered images safe).
    slot_image: Vec<Option<usize>>,
    /// Per command-buffer completion tokens (reuse gate).
    cmd_marks: Vec<Option<(vk::Semaphore, u64)>>,
    /// Per query-slot submission ordinals (staleness validation).
    query_marks: Vec<u64>,
    /// Submissions recorded on this session (cmd/query indexing).
    submitted: u64,
    /// The newest submission's completion token (session drain).
    last_submit: Option<(vk::Semaphore, u64)>,
    /// The STREAM's coded extent (renegotiation comparison).
    coded_extent: vk::Extent2D,
    /// The granularity-aligned allocation extent (picture resources + frames).
    image_extent: vk::Extent2D,
}

/// The native Vulkan Video AV1 decoder. Mirrors [`crate::VkH265Decoder`]'s public
/// surface method-for-method.
pub struct VkAv1Decoder {
    dev: DecodeDevice,
    lock: Box<dyn QueueLock>,
    planner: Av1Planner,
    /// Caps per profile key, queried once per profile (a bit-depth or film-grain
    /// switch is a different key and re-queries).
    caps: Option<(Av1ProfileKey, DecodeCaps)>,
    state: Option<SessionStateAv1>,
    /// Decoded pictures awaiting their planner output verdict, keyed by [`PicId`].
    /// For AV1 this holds the HIDDEN frames: a `show_frame` picture is settled into
    /// `ready` by the very plan that decoded it.
    pending: BTreeMap<PicId, PendingPic>,
    /// Display-ready frames not yet handed out. Genuinely deeper than one here: a
    /// temporal unit carrying several shown frames makes several ready at once.
    ready: VecDeque<DecodedVkFrame>,
    /// Retired generations' pools with consumer-held images (die on their last
    /// release token).
    graveyard: Vec<RetiredPool>,
    /// The most recent access unit's warnings ([`Self::take_warnings`]) — the whole
    /// temporal unit's, concatenated in decode order.
    last_warnings: Vec<PlanWarning>,
    /// Pictures decoded so far — stamped onto each one as
    /// [`DecodedVkFrame::decode_order`]. Survives session rebuilds because it
    /// describes the STREAM, not the Vulkan objects.
    decoded: u64,
    /// Session generation: bumped on every rebuild, stamped into frames.
    generation: u64,
    device_lost: bool,
    /// Recovery owed after a failed frame whose planning had already advanced
    /// ([`RecoveryLatch`] docs for the whole argument).
    recovery: RecoveryLatch,
    /// Every frame until the next KEY frame is undecodable, and is skipped rather
    /// than converted.
    ///
    /// This exists because AV1's planner has no `flush`: when a failure forces
    /// [`Self::recover_dpb`] to empty this decoder's slot ledger and image
    /// bindings, the PLANNER's own eight-slot store still believes those pictures
    /// are resident and keeps handing out inter frames that reference them. Each
    /// would fail in `plan_to_vk_av1` with `UnresolvedReference` — a per-frame
    /// failure whose message describes a phantom reference gap rather than the
    /// wait that is really in progress, and which would drag every one of those
    /// frames through a conversion that cannot succeed.
    ///
    /// So the frames are skipped. What they are NOT is laundered into a clean
    /// answer: a temporal unit in which every frame was skipped comes back as
    /// [`VkDecodeError::AwaitingKeyAv1`], once per access unit, exactly as the
    /// H.264/H.265 decoders answer the same wait with their planners'
    /// `PlanError::AwaitingIdr`. The three codecs must be indistinguishable here,
    /// because the consumer's demotion streak is the only thing that turns "this
    /// rung produces no picture" into "fall through to the next rung": a clean
    /// `Ok(None)` RESETS that streak once per frame, so a rung whose every key
    /// frame fails would never reach the threshold and the session would keep a
    /// frozen screen with a clean bill of health. During a recovery wait the
    /// decoder really has stopped working, and that is what the streak must see.
    ///
    /// A DECODED key frame (which references nothing and refreshes all eight
    /// slots) clears it and decoding resumes — including one that arrives partway
    /// through a temporal unit, which is why the skip is per FRAME while the error
    /// is per ACCESS UNIT.
    awaiting_key: bool,
}

impl VkAv1Decoder {
    /// Wrap the borrowed device. Sessions/pools are built lazily from the first
    /// frame's sequence header (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// The full [`DeviceHandles`] caller contract (liveness, enabled extensions
    /// and features, truthful queue families) — held for this decoder's whole
    /// lifetime, not just this call. The device must additionally have been
    /// created with `VK_KHR_video_decode_av1` enabled; that part of the contract
    /// is checked below AS FAR AS IT CAN BE — the check reads the decode queue
    /// family's advertised `videoCodecOperations`, which is the device's own claim
    /// about the family, not proof that the client enabled the extension at
    /// `vkCreateDevice`. Getting it wrong is undefined behaviour at session
    /// creation rather than an error, which is why the family check runs before
    /// anything is queried or created.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        dev.require_codec_op(vk::VideoCodecOperationFlagsKHR::DECODE_AV1, "AV1 decode")?;
        Ok(Self {
            dev,
            lock,
            planner: Av1Planner::new(),
            caps: None,
            state: None,
            pending: BTreeMap::new(),
            ready: VecDeque::new(),
            graveyard: Vec::new(),
            last_warnings: Vec::new(),
            decoded: 0,
            generation: 0,
            device_lost: false,
            recovery: RecoveryLatch::default(),
            awaiting_key: false,
        })
    }

    /// Ask the device, BEFORE a single AU is fed, whether it can decode a stream of
    /// the negotiated shape — the construction-time half of what the lazy
    /// `ensure_state` path would otherwise only discover at the first sequence
    /// header.
    ///
    /// `film_grain` is the load-bearing argument. Grain synthesis is part of the
    /// AV1 decode PROFILE, and a device that decodes AV1 need not offer the
    /// grain-enabled one; discovering that lazily makes the refusal a mid-stream
    /// error streak, which demotes past the FFmpeg rungs, where discovering it here
    /// is a construction failure the client's ladder answers by falling through to
    /// the next rung with the session's hardware decode intact.
    ///
    /// The negotiated facts are a HINT (the in-band sequence header is
    /// authoritative), so this is deliberately not a promise that decode will
    /// succeed: the level ceiling and a sequence header that disagrees with the
    /// Welcome still surface at the first AU.
    pub fn probe_stream_support(
        &self,
        chroma_format_idc: u8,
        bit_depth: u8,
        film_grain: bool,
    ) -> Result<(), VkDecodeError> {
        let key = Av1ProfileKey::from_negotiated(chroma_format_idc, bit_depth, film_grain)?;
        // SAFETY: the constructor's `DeviceHandles` contract holds for this
        // decoder's whole lifetime, so the physical device is live — the same
        // proof `ensure_state`'s identical call carries.
        let raw =
            unsafe { query_av1_caps(&self.dev, key) }.map_err(|r| caps_query_error(r, key))?;
        let wanted = key
            .output_format()
            .expect("from_negotiated gated the sampling/depth combination");
        derive_caps_av1(&raw, wanted)?;
        Ok(())
    }

    /// Decode one access unit — one TEMPORAL UNIT, which may carry several frames.
    /// Returns the next display-ready frame, if the planner declared one; drain the
    /// rest with [`Self::take_ready`].
    ///
    /// A temporal unit whose every frame was skipped while [`Self::awaiting_key`]
    /// is set comes back as [`VkDecodeError::AwaitingKeyAv1`] — the same kind of
    /// answer the H.264/H.265 decoders give for the same wait, and for the reason
    /// [`Self::awaiting_key`]'s docs carry. A `show_existing_frame` naming an empty
    /// slot is NOT that: the planner reports it as a warning and it simply displays
    /// nothing.
    ///
    /// Never panics. `VkDecodeError::DeviceLost` latches: every later call fails
    /// fast until the owner rebuilds the decoder on fresh handles.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        if self.device_lost {
            return Err(VkDecodeError::DeviceLost);
        }
        let result = self.decode_inner(au);
        if matches!(result, Err(VkDecodeError::DeviceLost)) {
            self.device_lost = true;
        }
        result
    }

    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        // A previous frame failed after its planning had advanced: clear the stale
        // DPB residency BEFORE planning this AU, or every later frame referencing
        // the stranded picture fails forever ([`RecoveryLatch`] docs).
        if self.recovery.take() {
            self.recover_dpb();
        }
        // Cleared BEFORE planning so an AU that fails to PLAN cannot leave the
        // previous one's warnings to be re-read as fresh damage.
        self.last_warnings.clear();
        let plans = match self.planner.plan_au(au) {
            Ok(plans) => plans,
            Err(e) => return Err(VkDecodeError::PlanAv1(e)),
        };
        // The whole temporal unit's warnings, in decode order — `take_warnings`
        // answers per ACCESS UNIT, and one unit's frames share a concealment
        // verdict as far as the integration layer is concerned.
        for plan in &plans {
            for warning in &plan.warnings {
                trace!(?warning, "plan warning");
            }
            self.last_warnings.extend(plan.warnings.iter().cloned());
        }

        let mut skipped = 0usize;
        for plan in &plans {
            // From here the PLANNER has already advanced past this frame — its
            // store holds the picture whatever happens next — so any failure below
            // leaves the planner's store and this decoder's ledgers able to
            // disagree. Latch the recovery rather than returning into a permanently
            // wedged state.
            match self.decode_planned(plan, au) {
                Ok(FrameOutcome::Decoded) => {}
                Ok(FrameOutcome::SkippedAwaitingKey) => skipped += 1,
                Err(e) => {
                    self.recovery.latch();
                    return Err(e);
                }
            }
        }
        // Nothing in this unit decoded and nothing was displayed, because the
        // decoder is still waiting for a key frame. That is an ERROR per access
        // unit — [`VkDecodeError::AwaitingKeyAv1`] and [`Self::awaiting_key`] carry
        // the argument — and deliberately not a latch: `recover_dpb` has already
        // run, the ledgers are consistent, and re-latching would re-flush an empty
        // ledger once per frame for the whole wait.
        //
        // Counted rather than short-circuited inside the loop, because a key frame
        // may sit BEHIND a skipped frame in the same temporal unit: returning at
        // the first skip would never reach it, and the wait would never end.
        if whole_unit_skipped(plans.len(), skipped) {
            return Err(VkDecodeError::AwaitingKeyAv1);
        }
        Ok(self.ready.pop_front())
    }

    /// One planned frame of a temporal unit.
    fn decode_planned(&mut self, plan: &AuPlan, au: &[u8]) -> Result<FrameOutcome, VkDecodeError> {
        // A key frame re-anchors everything: it references nothing and refreshes
        // all eight slots, so it is decodable no matter what came before.
        //
        // A DECODED one, specifically. `show_existing_frame` of a key frame also
        // resets the planner's store (7.20) but decodes nothing, so it leaves this
        // decoder with an empty ledger against a full planner store — resuming
        // there would fail on the very next inter frame and re-arm the wait, one
        // error per frame, which is the storm this flag exists to avoid.
        if self.awaiting_key && clears_awaiting_key(plan) {
            debug!("AV1 key frame reached — decoding resumes");
            self.awaiting_key = false;
        }
        if self.awaiting_key {
            trace!(
                show_existing = plan.dpb.stored.is_none(),
                "frame skipped while awaiting the next AV1 key frame"
            );
            return Ok(FrameOutcome::SkippedAwaitingKey);
        }

        // `show_existing_frame`: no decode at all. It displays a slot's contents —
        // a picture some earlier hidden frame put there — so its DPB verdicts are
        // settled and nothing is submitted.
        let Some(setup_id) = plan.dpb.stored else {
            self.settle(&plan.dpb.outputs, &plan.dpb.removed);
            if let Some(state) = &mut self.state {
                for &id in &plan.dpb.removed {
                    state.slots.release(id);
                }
            }
            // Decoded: nothing was submitted, but the plan was HONOURED — it
            // declared a picture displayable, which is a frame the unit produced.
            return Ok(FrameOutcome::Decoded);
        };

        // A reference the planner could not resolve: refuse before anything is
        // converted (module docs, and [`lost_reference`]).
        if let Some((slot, ref_index)) = lost_reference(&plan.warnings) {
            return Err(VkDecodeError::MissingReferenceAv1 { slot, ref_index });
        }

        // One picture per plan: stamp its DECODE-order ordinal before anything can
        // reorder it (see `DecodedVkFrame::decode_order`).
        self.decoded = self.decoded.saturating_add(1);
        let decode_order = self.decoded;

        self.ensure_state(plan)?;

        // A parameters RECREATE over an EXISTING object destroys it, which an
        // in-flight decode may still be executing against: drain first. The FIRST
        // one of a session's life destroys nothing (the session is created without
        // a parameters object — `session_av1` module docs) and needs no drain.
        {
            let session = &self.state.as_ref().expect("ensure_state built it").session;
            if session.parameters_action(&plan.sequence) == ParamsActionAv1::Recreate
                && session.has_parameters()
            {
                self.drain_gpu()?;
            }
        }
        let state = self.state.as_mut().expect("ensure_state built it");
        // SAFETY: live device (constructor contract); the drain above satisfies
        // ensure_parameters' Recreate contract, and Current touches nothing a
        // submitted decode reads.
        unsafe { state.session.ensure_parameters(&plan.sequence)? };

        // The bitstream layout, decided BEFORE the DPB ledger is touched: a
        // malformed tile group must not leave a half-applied slot map behind.
        let bitstream =
            plan_bitstream(au, &plan.tiles, &plan.header).map_err(VkDecodeError::TilesAv1)?;

        let vk_plan = plan_to_vk_av1(plan, &mut state.slots).map_err(VkDecodeError::ConvertAv1)?;

        // The per-AU active-reference gate: the session was created with
        // maxActiveReferencePictures; binding more in one decode op would be a
        // silent VUID violation on the drivers that matter most.
        let max_active = state.session.config.max_active_references as usize;
        if vk_plan.refs.len() > max_active {
            return Err(VkDecodeError::Unsupported(format!(
                "frame references {} pictures, session allows {max_active} active references",
                vk_plan.refs.len()
            )));
        }

        // Coincide binding sync: slots the planner released no longer bind their
        // images (the pictures may still be pending/held — untouched), and the
        // setup slot's PREVIOUS binding is cleared before it binds fresh.
        let setup = usize::from(vk_plan.setup_slot);
        if state.dpb.is_none() {
            let unbound =
                sync_slot_bindings(&state.slots, &mut state.slot_image, vk_plan.setup_slot);
            for picture in unbound {
                state.pool.pictures[picture].bound = false;
            }
        }

        // The decode target: a FREE pool image (never one a consumer holds — the
        // whole point of the pool model).
        let Some(dst) = state.pool.free_index() else {
            debug!(
                held = state.pool.held_total(),
                "picture pool exhausted — release_frame owed"
            );
            return Err(VkDecodeError::NoFreeSlot);
        };

        // Cross-queue waits (the AVVkFrame contract): the dst image's last known
        // timeline value (covers a presenter write-back after release), plus —
        // coincide mode — every referenced image's value, so reference reads order
        // after any presenter layout restore already reported back.
        let mut waits: Vec<(vk::Semaphore, u64)> = Vec::new();
        {
            let dst_pic = &state.pool.pictures[dst];
            if dst_pic.value > 0 {
                waits.push((dst_pic.semaphore, dst_pic.value));
            }
        }
        if state.dpb.is_none() {
            for r in &vk_plan.refs {
                if let Some(picture) = state.slot_image[usize::from(r.slot)] {
                    let pic = &state.pool.pictures[picture];
                    if pic.value > 0 && !waits.iter().any(|(sem, _)| *sem == pic.semaphore) {
                        waits.push((pic.semaphore, pic.value));
                    }
                }
            }
        }
        let signal_value = state.pool.pictures[dst].value + 1;

        // Command buffer + query slot for this submission.
        let submission = state.submitted;
        let cmd_index = (submission % state.ops.cmds.len() as u64) as usize;
        if let Some((sem, value)) = state.cmd_marks[cmd_index] {
            // SAFETY: live device; the token is a pool image's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "command buffer reuse")? };
        }
        let query_index = (submission % u64::from(state.ops.query_count)) as u32;

        // Upload the raw TILE PAYLOADS — nothing else goes in the buffer (module
        // docs) — recycling/growing the ring against submission-completion tokens.
        // AV1 has no start codes and nothing to strip: the tiles go in verbatim.
        let Some(packed) = pack_av1_tiles(&bitstream.tiles) else {
            return Err(VkDecodeError::Unsupported(
                "packed tile data exceeds the u32 offsets Vulkan submits".into(),
            ));
        };
        let tiles = submitted_tiles(&packed).map_err(VkDecodeError::TilesAv1)?;

        let device = self.dev.ash().clone();
        let mut poll = |token: &(vk::Semaphore, u64)| -> Result<bool, VkDecodeError> {
            // SAFETY: live device; the token's semaphore is a pool semaphore.
            let current = unsafe { device.get_semaphore_counter_value(token.0) }
                .map_err(VkDecodeError::from)?;
            Ok(current >= token.1)
        };
        let device2 = self.dev.ash().clone();
        let mut wait = |token: &(vk::Semaphore, u64)| -> Result<(), VkDecodeError> {
            // SAFETY: as above.
            unsafe { wait_timeline(&device2, token.0, token.1, "bitstream slot drain") }
        };
        // SAFETY: live device; the segments are the plan's own in-bounds OBU
        // ranges; every pending token is the completion signal of the submission
        // that consumed the slot.
        let upload = unsafe {
            state
                .ring
                .upload(&self.dev, au, &packed.segments, &mut poll, &mut wait)?
        };

        // Record + submit, signalling the dst image's next timeline value.
        // SAFETY: live device; every handle recorded below belongs to this session
        // generation, and the packed OBUs sit uploaded in the ring slot.
        unsafe {
            record_and_submit_av1(
                &self.dev,
                &*self.lock,
                state,
                &vk_plan,
                &tiles,
                &upload,
                dst,
                cmd_index,
                query_index,
                &waits,
                signal_value,
            )?;
        }

        // Post-submit bookkeeping.
        let dst_sem = state.pool.pictures[dst].semaphore;
        state.pool.pictures[dst].value = signal_value;
        state.pool.pictures[dst].pending = true;
        if state.dpb.is_none() {
            state.pool.pictures[dst].bound = true;
            state.slot_image[setup] = Some(dst);
        }
        state.cmd_marks[cmd_index] = Some((dst_sem, signal_value));
        state.query_marks[query_index as usize] = submission;
        state.submitted += 1;
        state.last_submit = Some((dst_sem, signal_value));
        state
            .ring
            .pending
            .set_pending(upload.slot, (dst_sem, signal_value));

        // Refresh the per-slot reference cache from this frame's facts.
        state.slot_refs[setup] = Some(vk_plan.setup_ref);
        for r in &vk_plan.refs {
            state.slot_refs[usize::from(r.slot)] = Some(r.std);
        }

        // The slots this frame's own refresh displaced while it was still READING
        // them. Held through the conversion and the submission above so neither the
        // setup assignment nor the binding sync could take them
        // (`DecodePlanVkAv1::release_after_decode`); free now that the decode op is
        // recorded, so the next frame may have them. Their pool images stay pinned
        // by `bound` until that frame's sync, which is the same one-frame grace
        // every other released slot's image gets.
        for &id in &vk_plan.release_after_decode {
            if !state.slots.release(id) {
                trace!(id, "deferred release of an id the slot map no longer holds");
            }
        }

        self.pending.insert(
            vk_plan.setup_id,
            PendingPic {
                image: dst,
                submission,
                query_slot: query_index,
                timeline_value: signal_value,
                crop: DisplayCrop {
                    x: 0,
                    y: 0,
                    // AV1's display region is `render_width`/`render_height`, its
                    // answer to a conformance window — the decoded picture is the
                    // (post-superres) `upscaled_width` x `frame_height`.
                    //
                    // ⚠ CLAMPED, because AV1's render size is a display HINT and
                    // not a window: 5.9.6 puts no upper bound on
                    // `render_width_minus_1`, so a stream may legally ask to be
                    // shown at more than it coded (that is how a decoder is told to
                    // upscale on output). Used as a crop unclamped it addresses
                    // rows and columns the decoded image does not have.
                    width: plan.picture.render_width.min(plan.picture.upscaled_width),
                    height: plan.picture.render_height.min(plan.picture.frame_height),
                },
                colour: plan.picture.colour,
                // AV1 has no POC. `OrderHint` is the closest thing the stream
                // states and is what a consumer ordering frames would compare;
                // it is a small wrapping counter, not a monotone one.
                poc: plan.picture.order_hint as i32,
                // AV1's re-anchor point is the KEY frame — there is no IDR and no
                // recovery point SEI, so this is the only clean point a consumer
                // freezing on loss ever sees.
                is_idr: plan.picture.is_key,
                recovery: crate::recovery::RecoveryMark::NONE,
                decode_order,
            },
        );

        // The plan's DPB verdicts over the pending map.
        self.settle(&plan.dpb.outputs, &plan.dpb.removed);

        // A frame that refreshes NO slot enters the planner's store nowhere, so the
        // planner can never report it removed — while `plan_to_vk_av1` did assign
        // it a slot in this decoder's ledger. Left alone that slot is held for the
        // session's whole life, and nine such frames exhaust the ledger with
        // `SlotError::Full`. It is legal AV1 (a frame shown once and never
        // referenced), it does not occur in the vendored vector, and it costs one
        // release to close.
        if plan.header.refresh_frame_flags == 0 {
            let state = self.state.as_mut().expect("ensured above");
            state.slots.release(setup_id);
            // If it was not shown either, nothing can ever display or reference it:
            // free its image instead of leaving the picture pending forever.
            if let Some(entry) = self.pending.remove(&setup_id) {
                trace!(
                    id = setup_id,
                    "frame refreshes no slot and is not shown — freeing its image"
                );
                state.pool.pictures[entry.image].pending = false;
            }
        }
        Ok(FrameOutcome::Decoded)
    }

    /// Apply one plan's DPB verdicts: outputs become ready frames (their images
    /// move pending → held), removed-but-never-shown pictures free their images.
    fn settle(&mut self, outputs: &[PicId], removed: &[PicId]) {
        let (ready, dropped) = settle_dpb_ids(&mut self.pending, outputs, removed);
        let Some(state) = self.state.as_mut() else {
            return;
        };
        for entry in ready {
            let frame = build_frame(
                &mut state.pool,
                state.dpb.is_none(),
                state.image_extent,
                &entry,
                self.generation,
            );
            self.ready.push_back(frame);
        }
        for entry in dropped {
            debug!(
                order_hint = entry.poc,
                "picture displaced from every slot without being shown — freeing its image"
            );
            state.pool.pictures[entry.image].pending = false;
        }
    }

    /// Hand a delivered frame back. `presenter_signaled` reports whether the
    /// consumer SAMPLED the image (and therefore enqueued the `value + 1` timeline
    /// signal per the [`DecodedVkFrame`] contract) — the decoder then waits that
    /// write-back before the image's next use. Every frame `decode`/`take_ready`
    /// returns must come back exactly once, including stale-generation frames
    /// (their retired pool dies on its last release token).
    pub fn release_frame(
        &mut self,
        frame: &DecodedVkFrame,
        presenter_signaled: bool,
    ) -> Result<(), VkDecodeError> {
        let pool = if frame.generation == self.generation {
            match &mut self.state {
                Some(state) => &mut state.pool,
                None => {
                    return Err(VkDecodeError::StaleFrame {
                        frame_generation: frame.generation,
                        current_generation: self.generation,
                    })
                }
            }
        } else {
            match self
                .graveyard
                .iter_mut()
                .find(|r| r.generation == frame.generation)
            {
                Some(retired) => &mut retired.pool,
                None => {
                    return Err(VkDecodeError::StaleFrame {
                        frame_generation: frame.generation,
                        current_generation: self.generation,
                    })
                }
            }
        };
        let index = frame.picture as usize;
        if index >= pool.pictures.len() {
            return Err(VkDecodeError::StaleFrame {
                frame_generation: frame.generation,
                current_generation: self.generation,
            });
        }
        let picture = &mut pool.pictures[index];
        match picture.held.checked_sub(1) {
            Some(remaining) => picture.held = remaining,
            None => {
                debug!(index, "frame released more often than delivered");
                return Ok(());
            }
        }
        if presenter_signaled {
            picture.value = picture.value.max(frame.value + 1);
        }
        // A retired pool dies on its last token (presenter fence-waited before the
        // token per the release contract; decode work drained at retirement).
        if frame.generation != self.generation {
            self.graveyard
                .retain(|r| r.generation != frame.generation || r.pool.held_total() > 0);
        }
        Ok(())
    }

    /// A display-ready frame beyond the one `decode` returned, if any. Drain after
    /// every decode; frames left here still occupy pool images. Genuinely needed on
    /// AV1: one temporal unit can make several frames ready.
    pub fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        self.ready.pop_front()
    }

    /// The warnings of the most recent successfully planned access unit — every
    /// frame's, concatenated in decode order (concealment signals: the integration
    /// layer's want_keyframe hook). Cleared by the next `decode`.
    pub fn take_warnings(&mut self) -> Vec<PlanWarning> {
        std::mem::take(&mut self.last_warnings)
    }

    /// The current session generation ([`DecodedVkFrame::generation`] of newly
    /// delivered frames).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The DECODE-order ordinal of the most recently decoded picture — the
    /// watermark a consumer compares [`DecodedVkFrame::decode_order`] against to
    /// tell a frame decoded before a loss from one decoded after it. 0 before the
    /// first frame decodes; `show_existing_frame` plans do not advance it, because
    /// they decode nothing.
    pub fn decode_order(&self) -> u64 {
        self.decoded
    }

    /// One-line state snapshot for failure paths and field logs (not a stable
    /// format).
    pub fn debug_snapshot(&self) -> String {
        let recovery = if self.recovery.is_latched() {
            " recovery=owed"
        } else {
            ""
        };
        let awaiting = if self.awaiting_key {
            " awaiting=key"
        } else {
            ""
        };
        match &self.state {
            None => format!("gen={}{recovery}{awaiting} <no session>", self.generation),
            Some(state) => {
                let occupancy: Vec<String> = state
                    .pool
                    .pictures
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        format!(
                            "{i}:{}{}h{}",
                            if p.bound { "B" } else { "-" },
                            if p.pending { "P" } else { "-" },
                            p.held
                        )
                    })
                    .collect();
                format!(
                    "av1 gen={}{recovery}{awaiting} mode={} slots_held={}/{} pool=[{}] \
                     pending={} ready={} graveyard={}",
                    self.generation,
                    if state.dpb.is_none() {
                        "coincide"
                    } else {
                        "distinct"
                    },
                    state.slots.active(),
                    state.slots.capacity(),
                    occupancy.join(" "),
                    self.pending.len(),
                    self.ready.len(),
                    self.graveyard.len(),
                )
            }
        }
    }

    /// Read `frame`'s decode status WITHOUT waiting.
    ///
    /// [`DecodeStatus::Failed`] covers driver-reported errors AND a query slot
    /// re-armed before it was read (the status is then unprovable — same
    /// conservative verdict).
    ///
    /// On drivers whose decode family lacks `queryResultStatusSupport` (RADV)
    /// there is no per-op verdict to read: `Ok` then means "the decode op
    /// COMPLETED on the timeline" — the same information FFmpeg has on every
    /// driver, no worse.
    pub fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, false)
    }

    /// Does this decode queue family answer per-op `RESULT_STATUS` queries at all?
    /// The fact is the DEVICE's, identical for every codec, and it is what tells a
    /// clean integrity report apart from an undetectable one.
    pub fn status_queries(&self) -> bool {
        self.dev.result_status_queries()
    }

    /// [`Self::poll_status`], but WAITs for the op to complete first.
    pub fn wait_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, true)
    }

    fn read_status(&mut self, frame: &DecodedVkFrame, block: bool) -> DecodeStatus {
        if frame.generation != self.generation {
            trace!(
                frame_generation = frame.generation,
                current = self.generation,
                "status asked for a stale-generation frame — Failed, without \
                 touching the new pools"
            );
            return DecodeStatus::Failed;
        }
        let Some(state) = &self.state else {
            return DecodeStatus::Failed;
        };
        let Some(query_pool) = state.ops.query_pool else {
            // No queries on this driver: the verdict degrades to timeline
            // completion (poll_status docs).
            if block {
                // SAFETY: live device; pool-owned semaphore.
                return match unsafe {
                    wait_timeline(self.dev.ash(), frame.semaphore, frame.value, "status wait")
                } {
                    Ok(()) => DecodeStatus::Ok,
                    Err(VkDecodeError::DeviceLost) => {
                        self.device_lost = true;
                        DecodeStatus::Failed
                    }
                    Err(_) => DecodeStatus::Failed,
                };
            }
            // SAFETY: live device; pool-owned semaphore.
            return match unsafe { self.dev.ash().get_semaphore_counter_value(frame.semaphore) } {
                Ok(current) if current >= frame.value => DecodeStatus::Ok,
                Ok(_) => DecodeStatus::Pending,
                Err(vk::Result::ERROR_DEVICE_LOST) => {
                    self.device_lost = true;
                    DecodeStatus::Failed
                }
                Err(_) => DecodeStatus::Failed,
            };
        };
        let slot = frame.query_slot as usize;
        if slot >= state.query_marks.len() || state.query_marks[slot] != frame.submission {
            trace!(
                slot,
                "status query slot re-armed before it was read — unprovable, reported Failed"
            );
            return DecodeStatus::Failed;
        }
        let flags = if block {
            vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR
        } else {
            vk::QueryResultFlags::WITH_STATUS_KHR
        };
        let mut status = [0i32; 1];
        // SAFETY: live device; the query pool is this session generation's own and
        // `frame.query_slot` indexes within its count (checked above against the
        // marks array it is sized to).
        let result = unsafe {
            self.dev
                .ash()
                .get_query_pool_results(query_pool, frame.query_slot, &mut status, flags)
        };
        match result {
            // VkQueryResultStatusKHR: >0 complete, 0 not ready, <0 error.
            Ok(()) if status[0] > 0 => DecodeStatus::Ok,
            Ok(()) if status[0] == 0 => DecodeStatus::Pending,
            Ok(()) => DecodeStatus::Failed,
            Err(vk::Result::NOT_READY) => DecodeStatus::Pending,
            Err(vk::Result::ERROR_DEVICE_LOST) => {
                self.device_lost = true;
                DecodeStatus::Failed
            }
            Err(r) => {
                debug!(?r, "status query read failed");
                DecodeStatus::Failed
            }
        }
    }

    /// Wait — bounded by `timeout_ns` — for a delivered frame's decode-complete
    /// signal. Pure measurement (the integration layer's sampled decode-latency
    /// stat): touches no decoder state. `frame` must be unreleased, which pins its
    /// pool — and with it the semaphore — alive.
    pub fn wait_decoded(&self, frame: &DecodedVkFrame, timeout_ns: u64) -> bool {
        if frame.generation != self.generation {
            return false;
        }
        let semaphores = [frame.semaphore];
        let values = [frame.value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&semaphores)
            .values(&values);
        // SAFETY: live device (constructor contract); the semaphore is a pool
        // semaphore the unreleased frame keeps alive (fn docs); the info arrays
        // are locals outliving the call.
        unsafe { self.dev.ash().wait_semaphores(&info, timeout_ns) }.is_ok()
    }

    /// Drain this decoder (teardown / stream discontinuity).
    ///
    /// AV1's flush is a DISCARD, not a bump, and that is the codec's doing rather
    /// than a shortcut: there is no reorder buffer and no bumping process, so a
    /// picture still `pending` here is a HIDDEN frame — one the stream decoded with
    /// `show_frame = 0` and would only ever have displayed through a later
    /// `show_existing_frame`. Handing those to the consumer would show frames the
    /// stream deliberately hid, out of order. Their images are freed instead.
    ///
    /// The decoder is left [`Self::awaiting_key`], because the PLANNER's own
    /// eight-slot store is untouched by this (it has no `flush`) and now disagrees
    /// with an emptied ledger — see that field's docs.
    pub fn flush(&mut self) {
        if let Some(state) = &mut self.state {
            for (_, entry) in std::mem::take(&mut self.pending) {
                state.pool.pictures[entry.image].pending = false;
            }
            let unbound = reset_slot_bindings(
                &mut state.slots,
                &mut state.slot_image,
                &mut state.slot_refs,
            );
            for picture in unbound {
                state.pool.pictures[picture].bound = false;
            }
        } else {
            self.pending.clear();
        }
        self.awaiting_key = true;
    }

    /// Clear the DPB state a failed frame left behind, so decoding resumes at the
    /// next key frame instead of erroring on residency nothing can honour.
    ///
    /// Three ledgers have to agree and, after a post-planning failure, do not: the
    /// PLANNER's eight-slot store, this decoder's [`SlotMap`], and the slot→image
    /// bindings. [`Self::flush`] empties the last two (and arms
    /// [`Self::awaiting_key`], which covers the first — the planner keeps its store
    /// and is simply not asked to decode anything until the key frame refreshes it).
    ///
    /// Deliberately not a session rebuild: the session, pools and ring are all
    /// still valid — only the DPB bookkeeping is stale — and a rebuild would churn
    /// every image allocation for a condition a key frame fixes anyway.
    fn recover_dpb(&mut self) {
        debug!(
            snapshot = %self.debug_snapshot(),
            "recovering from a failed AV1 frame — skipping to the next key frame"
        );
        self.flush();
    }

    /// Session/caps for THIS plan exist and match its extent + profile, and the
    /// stream sits inside the device's level ceiling.
    fn ensure_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        let key = profile_key_for(plan)?;
        if self.caps.as_ref().map(|(k, _)| *k) != Some(key) {
            let wanted = key
                .output_format()
                .expect("from_stream gated the sampling/depth combination");
            // SAFETY: live device (constructor contract).
            let raw =
                unsafe { query_av1_caps(&self.dev, key) }.map_err(|r| caps_query_error(r, key))?;
            self.caps = Some((key, derive_caps_av1(&raw, wanted)?));
        }
        // The level gate. AV1's `StdVideoAV1Level` is index-coded exactly like the
        // bitstream's `seq_level_idx` (2.0 = 0 … 7.3 = 23) and ascends with the
        // level, so this is a plain comparison — of AV1 code points against an AV1
        // ceiling, the pairing `MaxLevelIdc`'s tag exists to keep honest.
        let caps_max_level = self.caps.as_ref().expect("queried above").1.max_level_idc;
        let stream_level = u32::from(stream_level_idx(plan));
        if stream_level > caps_max_level.code_point() {
            return Err(VkDecodeError::Unsupported(format!(
                "stream level (seq_level_idx {stream_level}) above the device's \
                 maxLevel ({caps_max_level})"
            )));
        }
        let coded = coded_extent(plan);
        match &self.state {
            Some(state) if state.coded_extent == coded && state.session.config.profile == key => {
                Ok(())
            }
            _ => self.rebuild_state(plan),
        }
    }

    /// Tear down the current session generation (draining its decode work, retiring
    /// its picture pool to the graveyard when the consumer still holds images) and
    /// build a fresh one shaped by `plan`, bumping [`Self::generation`] so frames
    /// of the old one route to the graveyard.
    fn rebuild_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        self.drain_gpu()?;
        if let Some(state) = self.state.take() {
            debug!("rebuilding AV1 decode session (stream renegotiation)");
            let SessionStateAv1 { mut pool, .. } = state;
            for frame in self.ready.drain(..) {
                let picture = &mut pool.pictures[frame.picture as usize];
                picture.held = picture.held.saturating_sub(1);
            }
            for (_, entry) in std::mem::take(&mut self.pending) {
                pool.pictures[entry.image].pending = false;
            }
            for picture in &mut pool.pictures {
                picture.bound = false;
            }
            let held = pool.held_total();
            if held > 0 {
                debug!(
                    held,
                    generation = self.generation,
                    "consumer still holds images of the retired generation — graveyarding"
                );
                self.graveyard.push(RetiredPool {
                    generation: self.generation,
                    pool,
                });
            }
        }
        self.generation += 1;

        let (key, caps) = self.caps.as_ref().expect("ensure_state queried caps");
        let key = *key;
        if REQUIRED_SLOTS > caps.max_dpb_slots {
            return Err(VkDecodeError::Unsupported(format!(
                "AV1 needs {REQUIRED_SLOTS} DPB slots, device caps at {}",
                caps.max_dpb_slots
            )));
        }
        let coded = coded_extent(plan);
        // Bounds-checked at the ALLOCATION extent (granularity-rounded): that is
        // what the images are created at and what maxCodedExtent must cover.
        let image_extent = caps.aligned_extent(coded);
        if coded.width < caps.min_coded_extent.width
            || coded.height < caps.min_coded_extent.height
            || image_extent.width > caps.max_coded_extent.width
            || image_extent.height > caps.max_coded_extent.height
        {
            return Err(VkDecodeError::Unsupported(format!(
                "coded extent {}x{} (allocated {}x{}) outside device range {}x{}..{}x{}",
                coded.width,
                coded.height,
                image_extent.width,
                image_extent.height,
                caps.min_coded_extent.width,
                caps.min_coded_extent.height,
                caps.max_coded_extent.width,
                caps.max_coded_extent.height
            )));
        }

        let config = SessionConfigAv1 {
            max_coded_extent: image_extent,
            max_dpb_slots: REQUIRED_SLOTS,
            max_active_references: (REQUIRED_SLOTS - 1).min(caps.max_active_references),
            profile: key,
        };
        let mut pool_plan = plan_pools(caps, REQUIRED_SLOTS);
        // TEST-ONLY readback hook, exactly as the other two decoders': a parity
        // test copies decoded pictures back to hash them, and
        // `vkCmdCopyImageToBuffer` needs TRANSFER_SRC on the source — a bit the
        // zero-copy production pools deliberately do not carry.
        if std::env::var("PF_VKD_TEST_READBACK").is_ok_and(|v| v == "1") {
            pool_plan.picture_usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        let decode_profile = DecodeProfile::Av1(key);
        // SAFETY: live device per the constructor contract, for every create in
        // this block; each created half is owned by a Drop type the moment it
        // exists, so a mid-build failure unwinds cleanly.
        let state = unsafe {
            let session = VideoSessionAv1::create(&self.dev, caps, config)?;
            let dpb = if caps.coincide {
                None
            } else {
                Some(
                    DpbPool::create(&self.dev, caps, &pool_plan, image_extent, decode_profile)
                        .map_err(VkDecodeError::from)?,
                )
            };
            let pool =
                PicturePool::create(&self.dev, caps, &pool_plan, image_extent, decode_profile)
                    .map_err(VkDecodeError::from)?;
            let ring = BitstreamRing::create(
                &self.dev,
                RingLayout::new(
                    INITIAL_SLOT_SIZE,
                    RING_SLOTS,
                    caps.min_bitstream_offset_alignment,
                    caps.min_bitstream_size_alignment,
                ),
                decode_profile,
            )
            .map_err(VkDecodeError::from)?;
            let ops = OpRing::create(
                &self.dev,
                decode_profile,
                pool_plan.picture_count,
                RING_SLOTS,
            )
            .map_err(VkDecodeError::from)?;
            SessionStateAv1 {
                session,
                slots: SlotMap::new(NUM_REF_SLOTS),
                slot_refs: vec![None; REQUIRED_SLOTS as usize],
                slot_image: vec![None; REQUIRED_SLOTS as usize],
                cmd_marks: vec![None; RING_SLOTS as usize],
                query_marks: vec![u64::MAX; pool_plan.picture_count as usize],
                submitted: 0,
                last_submit: None,
                coded_extent: coded,
                image_extent,
                dpb,
                pool,
                ring,
                ops,
            }
        };
        self.state = Some(state);
        Ok(())
    }

    /// Wait out every in-flight decode submission of the current session.
    fn drain_gpu(&mut self) -> Result<(), VkDecodeError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        if let Some((sem, value)) = state.last_submit {
            // SAFETY: live device; the token is a pool image's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "session drain")? };
        }
        Ok(())
    }
}

impl Drop for VkAv1Decoder {
    fn drop(&mut self) {
        // Best-effort decode drain so the pools' Drop impls never destroy in-flight
        // decode work; a wedged driver falls through after the bounded timeout.
        // Presenter-side sampling of graveyarded/held images is the CALLER's
        // teardown contract (the H.264 decoder's Drop docs).
        if let Err(e) = self.drain_gpu() {
            debug!(error = %e, "drain on drop failed; tearing down anyway");
        }
        if !self.graveyard.is_empty() {
            debug!(
                pools = self.graveyard.len(),
                "graveyard not fully token-drained at decoder drop — destroying anyway \
                 (upstream teardown forfeited its bounded wait)"
            );
        }
    }
}

/// Turn a failed AV1 capabilities query into an error that names the likely cause.
///
/// A device that decodes AV1 but not the FILM-GRAIN profile answers the very first
/// query with a profile-unsupported result, and that is by far the most probable
/// reason a caps query fails at all here (every other input to it is a shape the
/// profile builder already gated). Saying so is what turns a bare `VkResult` in a
/// field log into the ladder's named demote — and the refusal is deliberate: the
/// alternative, re-querying with grain turned off, would decode the stream's
/// pictures and silently drop the grain the encoder relied on.
fn caps_query_error(r: vk::Result, key: Av1ProfileKey) -> VkDecodeError {
    if key.film_grain {
        VkDecodeError::Unsupported(format!(
            "AV1 decode capabilities query failed with {r:?}; this stream applies film \
             grain, and a device that cannot host the film-grain AV1 decode profile \
             fails exactly here — decoding it without grain is not offered"
        ))
    } else {
        VkDecodeError::from(r)
    }
}

/// The Vulkan profile this frame's stream needs — Std profile, sampling, bit depth
/// and the sequence's film-grain flag, all of which the sequence header carries and
/// the profile must restate.
fn profile_key_for(plan: &AuPlan) -> Result<Av1ProfileKey, VkDecodeError> {
    Av1ProfileKey::from_stream(
        plan.sequence.seq_profile as u8,
        plan.picture.chroma_format_idc,
        plan.picture.bit_depth,
        plan.sequence.film_grain_params_present,
    )
    .map_err(VkDecodeError::ParamsAv1)
}

/// Whether this plan ends an outstanding [`VkAv1Decoder::awaiting_key`] wait.
///
/// A DECODED key frame, specifically: it references nothing and refreshes all
/// eight reference slots, so it re-anchors both the planner's store and this
/// decoder's ledger in one step. A `show_existing_frame` OF a key frame resets the
/// planner's store too (7.20) while decoding nothing — resuming there would leave
/// an empty ledger against a full store and fail on the very next inter frame.
fn clears_awaiting_key(plan: &AuPlan) -> bool {
    plan.picture.is_key && plan.dpb.stored.is_some()
}

/// Did a temporal unit of `planned` frames produce NOTHING because every one of
/// them was skipped waiting for a key frame — the
/// [`VkDecodeError::AwaitingKeyAv1`] condition?
///
/// A named function rather than the expression inlined at the call site because
/// both of its edges are load-bearing and neither is obvious:
///
/// * `planned == 0` is not a skip. A temporal unit can plan no frames at all (one
///   carrying only metadata or a sequence header), and that is an ordinary
///   `Ok(None)` — turning it into an error would fail access units on a perfectly
///   healthy stream.
/// * `skipped < planned` is not a skip either, and this is the case an early
///   return inside the loop would have got wrong: a key frame may sit BEHIND a
///   skipped frame in the same unit, clears the wait when it is reached, and
///   decodes. Reporting the unit as skipped there would answer an error for an
///   access unit that really did decode a picture.
///
/// Pure, so the aggregation is CPU-testable without a device.
fn whole_unit_skipped(planned: usize, skipped: usize) -> bool {
    planned > 0 && skipped == planned
}

/// The stream's level, as the sequence header's FIRST operating point states it.
///
/// Operating point 0 is the full stream — the one a non-scalable decoder decodes
/// and the one the vendored parser selects by default. A punktfunk host emits a
/// single operating point.
fn stream_level_idx(plan: &AuPlan) -> u8 {
    plan.sequence.operating_points[0].seq_level_idx
}

/// The extent the decode output has: AV1's superres upscales horizontally AFTER
/// reconstruction, so a superres frame is coded at `frame_width` and comes out at
/// `upscaled_width`, and it is the output the pool images have to hold.
///
/// It is also the ONE extent a session generation has. Every picture resource a
/// coding scope binds — the setup slot, this frame's references, the other held
/// slots — is described with it, which is sound because `ensure_state` rebuilds
/// the session the moment the extent changes: within a generation no two pictures
/// were decoded at different sizes.
///
/// The cost of that is worth stating: AV1 permits a mid-sequence frame-size change
/// (`frame_size_override_flag`) with references SCALED to the new size, and this
/// rung answers it with a session rebuild — a fresh, empty slot ledger against a
/// planner store that still holds the old pictures, so the stream re-anchors on the
/// next key frame ([`VkAv1Decoder::awaiting_key`]). Reference scaling is outside
/// the punktfunk envelope (a host renegotiates with a new sequence header, which
/// rebuilds anyway); a stream that used it would decode, with a hitch at each size
/// change rather than a wrong picture.
fn coded_extent(plan: &AuPlan) -> vk::Extent2D {
    vk::Extent2D {
        width: plan.picture.upscaled_width,
        height: plan.picture.frame_height,
    }
}

/// Empty the three per-slot ledgers a recovery resets: DPB residency, the
/// slot→image bindings and the cached per-slot reference info. Returns the pool
/// image indices the cleared bindings were pinning, for the caller to unbind (pure
/// over the ledgers so the recovery is testable without a device — the pool is the
/// one piece that needs one).
///
/// All three are emptied TOGETHER on purpose: leaving reference info behind would
/// let [`build_scope_av1`] bind a slot the planner no longer knows about, which is
/// the same "plausible-looking picture in the wrong place" the unbound-reference
/// refusal exists to prevent.
fn reset_slot_bindings(
    slots: &mut SlotMap,
    slot_image: &mut [Option<usize>],
    slot_refs: &mut [Option<hh::StdVideoDecodeAV1ReferenceInfo>],
) -> Vec<usize> {
    // `release` is the only way a slot is freed (SlotMap docs); the collect is
    // because `held` borrows the map the releases mutate.
    for (_slot, id) in slots.held().collect::<Vec<_>>() {
        slots.release(id);
    }
    let unbound = slot_image.iter_mut().filter_map(Option::take).collect();
    for cached in slot_refs.iter_mut() {
        *cached = None;
    }
    unbound
}

/// Coincide-mode binding sync, run once per frame between the plan conversion and
/// the decode target's allocation: a slot the ledger no longer holds stops binding
/// its pool image, and the setup slot's PREVIOUS binding is cleared before it binds
/// fresh. Returns the pool images that lost a binding — the caller clears their
/// `bound` flag, which is what puts them back in reach of the free list.
///
/// The pictures themselves are untouched: one still `pending` or held by a consumer
/// stays off the free list on those flags alone (the decoupled-pool contract in
/// [`crate::images`]).
///
/// A free function rather than four lines inline, because it is half of the
/// invariant `build_scope_av1` refuses on: a slot this frame REFERENCES must still
/// bind an image once this has run. Driving the two together over the vendored
/// vector is what `slot_recycling_waits_for_the_decode_op` does, and what no
/// hardware-free test could do while this lived inside `decode_planned`.
fn sync_slot_bindings(
    slots: &SlotMap,
    slot_image: &mut [Option<usize>],
    setup_slot: u8,
) -> Vec<usize> {
    let mut held = vec![false; slot_image.len()];
    for (slot, _id) in slots.held() {
        held[usize::from(slot)] = true;
    }
    let setup = usize::from(setup_slot);
    let mut unbound = Vec::new();
    for (slot, binding) in slot_image.iter_mut().enumerate() {
        if binding.is_some() && (!held[slot] || slot == setup) {
            unbound.extend(binding.take());
        }
    }
    unbound
}

/// The picture resource view bound for DPB `slot`: the bound pool image (coincide)
/// or the DPB array layer (distinct).
fn slot_view(state: &SessionStateAv1, slot: u8) -> Option<vk::ImageView> {
    match &state.dpb {
        Some(dpb) => Some(dpb.dpb_view(slot)),
        None => state.slot_image[usize::from(slot)].map(|p| state.pool.pictures[p].view),
    }
}

/// One entry of a coding scope's bound-slot list: the DPB slot index it binds
/// (`-1` for the setup ACTIVATION entry), the picture resource view, and the codec
/// reference info that slot's association carries.
/// (No derived equality: `StdVideoDecodeAV1ReferenceInfo` is a plain-C bindgen
/// struct without it. Assertions compare the fields that carry meaning.)
#[derive(Debug, Clone, Copy)]
struct ScopeEntryAv1 {
    slot_index: i32,
    view: vk::ImageView,
    std: hh::StdVideoDecodeAV1ReferenceInfo,
}

/// Build the coding scope's bound-slot list and say how many leading entries are
/// this frame's references.
///
/// The layout:
///
/// 1. every entry of `refs`, IN ORDER — the decode op takes exactly this prefix;
/// 2. every other still-held slot, so its association survives the scope;
/// 3. the setup slot as the activation entry, slot index `-1`.
///
/// Two things fail the whole op rather than being skipped:
///
/// - a reference whose slot binds no image — `referenceNameSlotIndices` names DPB
///   SLOTS, and dropping the entry that binds one leaves the hardware with a named
///   slot this op never bound;
/// - a `referenceNameSlotIndices` entry naming a slot the reference list does NOT
///   bind. That is the Vulkan rule stated the other way round (every non-negative
///   entry must equal the `slotIndex` of one of `pReferenceSlots`), and checking it
///   here is what would have caught the HEVC RPS class at the point of submission
///   rather than on a driver.
#[allow(clippy::too_many_arguments)]
fn build_scope_av1(
    refs: &[VkRefAv1],
    reference_name_slot_indices: &[i32],
    held_slots: impl Iterator<Item = u8>,
    setup_slot: u8,
    setup_view: vk::ImageView,
    setup_ref: hh::StdVideoDecodeAV1ReferenceInfo,
    slot_refs: &[Option<hh::StdVideoDecodeAV1ReferenceInfo>],
    view_of: impl Fn(u8) -> Option<vk::ImageView>,
) -> Result<(Vec<ScopeEntryAv1>, usize), VkDecodeError> {
    let mut scope: Vec<ScopeEntryAv1> = Vec::with_capacity(refs.len() + slot_refs.len() + 1);
    for r in refs {
        match view_of(r.slot) {
            Some(view) => scope.push(ScopeEntryAv1 {
                slot_index: i32::from(r.slot),
                view,
                std: r.std,
            }),
            None => return Err(VkDecodeError::UnboundReferenceSlot { slot: r.slot }),
        }
    }
    let reference_count = scope.len();
    // Every reference NAME must resolve to a slot the op binds.
    for name in reference_name_slot_indices {
        if *name == REFERENCE_NAME_UNUSED {
            continue;
        }
        // Negative-but-not-UNUSED is not a slot at all; `u8::MAX` is a slot no
        // session of this crate has (the ledger tops out at nine), so the refusal
        // reads as "a name nothing binds" — which is what it is.
        let Ok(slot) = u8::try_from(*name) else {
            return Err(VkDecodeError::UnboundReferenceSlot { slot: u8::MAX });
        };
        if !refs.iter().any(|r| r.slot == slot) {
            return Err(VkDecodeError::UnboundReferenceSlot { slot });
        }
    }
    for slot in held_slots {
        if slot == setup_slot || refs.iter().any(|r| r.slot == slot) {
            continue;
        }
        match (
            slot_refs.get(usize::from(slot)).copied().flatten(),
            view_of(slot),
        ) {
            (Some(std), Some(view)) => scope.push(ScopeEntryAv1 {
                slot_index: i32::from(slot),
                view,
                std,
            }),
            // Unreachable in practice: every held slot was a setup slot once.
            _ => trace!(
                slot,
                "held slot without reference info/binding — left unbound"
            ),
        }
    }
    scope.push(ScopeEntryAv1 {
        slot_index: -1,
        view: setup_view,
        std: setup_ref,
    });
    Ok((scope, reference_count))
}

/// Record one AV1 decode op into the chosen command buffer and submit it under the
/// queue lock: image waits per the pool contract, the dst image's timeline signal
/// at `signal_value`.
///
/// # Safety
///
/// Live device; `state` is the current session generation with `vk_plan` derived
/// against its `SlotMap`, `dst` a free pool image, the tile OBUs resident in
/// `upload`'s ring slot, and the command buffer's previous submission completed
/// (caller waited its mark).
#[allow(clippy::too_many_arguments)]
unsafe fn record_and_submit_av1(
    dev: &DecodeDevice,
    lock: &dyn QueueLock,
    state: &mut SessionStateAv1,
    vk_plan: &DecodePlanVkAv1,
    tiles: &SubmittedTiles,
    upload: &UploadedAu,
    dst: usize,
    cmd_index: usize,
    query_index: u32,
    waits: &[(vk::Semaphore, u64)],
    signal_value: u64,
) -> Result<(), VkDecodeError> {
    let device = dev.ash();
    let cmd = state.ops.cmds[cmd_index];
    let coded_extent = state.coded_extent;
    let coincide = state.dpb.is_none();

    // ---- the reference layout, decided BEFORE anything is recorded ----
    let setup_view = if coincide {
        state.pool.pictures[dst].view
    } else {
        state
            .dpb
            .as_ref()
            .expect("distinct mode")
            .dpb_view(vk_plan.setup_slot)
    };
    let held_slots: Vec<u8> = state.slots.held().map(|(slot, _id)| slot).collect();
    let (scope, reference_count) = build_scope_av1(
        &vk_plan.refs,
        &vk_plan.reference_name_slot_indices,
        held_slots.into_iter(),
        vk_plan.setup_slot,
        setup_view,
        vk_plan.setup_ref,
        &state.slot_refs,
        |slot| slot_view(state, slot),
    )?;

    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the buffer's previous submission completed (fn contract) and its
    // pool allows per-buffer reset, so begin implicitly resets it.
    unsafe {
        device
            .begin_command_buffer(cmd, &begin_info)
            .map_err(VkDecodeError::from)?
    };

    // ---- barriers (outside the video coding scope) ----
    let memory_barriers = [vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
        .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        .dst_access_mask(
            vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
        )];
    // Decode targets are fully overwritten: discard via UNDEFINED with an
    // execution+memory dependency on earlier ops that touched them.
    let decode_layer_barrier = |image: vk::Image, layer: u32, new_layout: vk::ImageLayout| {
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .src_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            )
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .dst_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            )
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(new_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: layer,
                layer_count: 1,
            })
    };
    let dst_image = state.pool.pictures[dst].image;
    let mut image_barriers = Vec::new();
    if coincide {
        // The dst pool image IS the setup DPB picture.
        image_barriers.push(decode_layer_barrier(
            dst_image,
            0,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
        ));
    } else {
        let dpb = state.dpb.as_ref().expect("distinct mode");
        let (setup_image, setup_layer) = dpb.dpb_target(vk_plan.setup_slot);
        image_barriers.push(decode_layer_barrier(
            setup_image,
            setup_layer,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
        ));
        image_barriers.push(decode_layer_barrier(
            dst_image,
            0,
            vk::ImageLayout::VIDEO_DECODE_DST_KHR,
        ));
    }
    let dependency = vk::DependencyInfo::default()
        .memory_barriers(&memory_barriers)
        .image_memory_barriers(&image_barriers);
    // SAFETY: recording into the begun buffer; synchronization2 is enabled per
    // the DeviceHandles feature contract.
    unsafe { device.cmd_pipeline_barrier2(cmd, &dependency) };

    // This op's status query slot, reset before the coding scope (encoder idiom).
    // None on drivers without queryResultStatusSupport (RADV — recording a query
    // there hangs the VCN; OpRing docs). NEVER remove this gate.
    if let Some(query_pool) = state.ops.query_pool {
        // SAFETY: recording; `query_index` is within the pool's count (fn contract).
        unsafe { device.cmd_reset_query_pool(cmd, query_pool, query_index, 1) };
    }

    // ---- bound-slot staging ----
    // Staged arrays over the scope decided above: resources → std infos → codec
    // slot infos → slot infos. Each vector is fully built before the next borrows
    // it, so nothing reallocates under a stored pointer.
    let resources: Vec<vk::VideoPictureResourceInfoKHR<'_>> = scope
        .iter()
        .map(|entry| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(coded_extent)
                .base_array_layer(0)
                .image_view_binding(entry.view)
        })
        .collect();
    let std_refs: Vec<hh::StdVideoDecodeAV1ReferenceInfo> =
        scope.iter().map(|entry| entry.std).collect();
    let mut dpb_infos: Vec<vk::VideoDecodeAV1DpbSlotInfoKHR<'_>> = std_refs
        .iter()
        .map(|std| vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(std))
        .collect();
    let mut begin_slots: Vec<vk::VideoReferenceSlotInfoKHR<'_>> = Vec::with_capacity(scope.len());
    for (index, entry) in scope.iter().enumerate() {
        begin_slots.push(
            vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(entry.slot_index)
                .picture_resource(&resources[index]),
        );
    }
    for (slot_info, dpb_info) in begin_slots.iter_mut().zip(dpb_infos.iter_mut()) {
        *slot_info = (*slot_info).push_next(dpb_info);
    }
    // The decode op's reference list: exactly this frame's references, in `refs`
    // order. `referenceNameSlotIndices` does NOT index into it — it names DPB slots
    // — but every slot it names has to BE in it, which build_scope_av1 checked.
    let decode_refs: Vec<vk::VideoReferenceSlotInfoKHR<'_>> =
        begin_slots[..reference_count].to_vec();

    // The setup slot as the decode op sees it: its REAL index (the begin list's
    // twin entry carries -1), same resource, its own codec info chain.
    let setup_std = vk_plan.setup_ref;
    let mut setup_dpb = vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(&setup_std);
    let setup_resource = resources[scope.len() - 1];
    let setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(i32::from(vk_plan.setup_slot))
        .picture_resource(&setup_resource)
        .push_next(&mut setup_dpb);

    // Decode destination: the setup picture itself (coincide) or the pool image
    // (distinct).
    let dst_resource = if coincide {
        setup_resource
    } else {
        vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(coded_extent)
            .base_array_layer(0)
            .image_view_binding(state.pool.pictures[dst].view)
    };

    let mut av1_pic = av1_picture_info(
        vk_plan.pic.std(),
        vk_plan.reference_name_slot_indices,
        tiles,
    );
    let mut decode_info = vk::VideoDecodeInfoKHR::default()
        .src_buffer(state.ring.buffer())
        .src_buffer_offset(upload.offset)
        .src_buffer_range(upload.range)
        .dst_picture_resource(dst_resource)
        .setup_reference_slot(&setup_slot_info)
        .push_next(&mut av1_pic);
    if reference_count > 0 {
        decode_info = decode_info.reference_slots(&decode_refs);
    }

    let begin_coding = vk::VideoBeginCodingInfoKHR::default()
        .video_session(state.session.session())
        .video_session_parameters(state.session.parameters())
        .reference_slots(&begin_slots);
    // The one-shot session RESET, consumed HERE but re-armed on every error path
    // below — a RESET recorded into a command buffer that never reaches the queue
    // initialized nothing, and the next successful recording must carry it or the
    // session runs its whole life uninitialized.
    let did_reset = state.session.take_needs_reset();
    // SAFETY: recording into the begun buffer, through end_command_buffer; every
    // pointed-to struct above is a local (or session-state field) that outlives the
    // calls; the session/parameters handles are this generation's own.
    let recorded: Result<(), vk::Result> = unsafe {
        (dev.video_queue().fp().cmd_begin_video_coding_khr)(cmd, &begin_coding);
        if did_reset {
            // Session first-use initialization — ONCE, before its first decode.
            let control = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::RESET);
            (dev.video_queue().fp().cmd_control_video_coding_khr)(cmd, &control);
        }
        if let Some(query_pool) = state.ops.query_pool {
            device.cmd_begin_query(cmd, query_pool, query_index, vk::QueryControlFlags::empty());
        }
        (dev.video_decode_queue().fp().cmd_decode_video_khr)(cmd, &decode_info);
        if let Some(query_pool) = state.ops.query_pool {
            device.cmd_end_query(cmd, query_pool, query_index);
        }
        (dev.video_queue().fp().cmd_end_video_coding_khr)(
            cmd,
            &vk::VideoEndCodingInfoKHR::default(),
        );
        device.end_command_buffer(cmd)
    };
    if let Err(e) = recorded {
        if did_reset {
            state.session.re_arm_reset();
        }
        return Err(VkDecodeError::from(e));
    }

    // ---- submit, under the caller's queue lock ----
    let cmd_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
    let wait_infos: Vec<vk::SemaphoreSubmitInfo<'_>> = waits
        .iter()
        .map(|&(semaphore, value)| {
            vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .value(value)
                .stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        })
        .collect();
    let signals = [vk::SemaphoreSubmitInfo::default()
        .semaphore(state.pool.pictures[dst].semaphore)
        .value(signal_value)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submits = [vk::SubmitInfo2::default()
        .command_buffer_infos(&cmd_infos)
        .wait_semaphore_infos(&wait_infos)
        .signal_semaphore_infos(&signals)];
    let guard = QueueSubmitGuard::acquire(lock);
    // SAFETY: the decode queue is the device's own (DeviceHandles contract) and
    // externally synchronized by the guard; the submit arrays are locals.
    let result = unsafe { device.queue_submit2(dev.decode_queue(), &submits, vk::Fence::null()) };
    drop(guard);
    if let Err(e) = result {
        // The recorded RESET never executed: the next recording must redo it.
        if did_reset {
            state.session.re_arm_reset();
        }
        return Err(VkDecodeError::from(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle as _;
    use cros_codecs::bitstream_utils::IvfIterator;
    use cros_codecs::codec::av1::parser::ObuAction;
    use cros_codecs::codec::av1::parser::ParsedObu;
    use cros_codecs::codec::av1::parser::Parser;

    use super::*;

    const AV1_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// A reference-info value carrying just the fields the assertions read.
    fn std_ref(order_hint: u8, frame_type: u8) -> hh::StdVideoDecodeAV1ReferenceInfo {
        // SAFETY: StdVideoDecodeAV1ReferenceInfo is a plain-C bindgen struct of a
        // bitfield word, three small integers and a byte array; all-zero is valid
        // for every field.
        let mut std: hh::StdVideoDecodeAV1ReferenceInfo = unsafe { std::mem::zeroed() };
        std.OrderHint = order_hint;
        std.frame_type = frame_type;
        std
    }

    fn vk_ref(slot: u8, order_hint: u8) -> VkRefAv1 {
        VkRefAv1 {
            slot,
            std: std_ref(order_hint, 1),
            id: u64::from(slot) + 100,
        }
    }

    /// A distinguishable fake view per slot (never dereferenced — the scope only
    /// carries handles around).
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    /// Names 0..7 pointing at `slots`, `-1` for the rest.
    fn names(slots: &[u8]) -> [i32; 7] {
        let mut out = [REFERENCE_NAME_UNUSED; 7];
        for (name, slot) in slots.iter().enumerate() {
            out[name] = i32::from(*slot);
        }
        out
    }

    #[test]
    fn the_scopes_leading_entries_are_the_refs_in_plan_order() {
        // `refs` is the plan's DEDUPED reference list in first-appearance order,
        // which is neither slot order nor name order — the scope must not sort or
        // re-order it, because `pReferenceSlots` is exactly this prefix.
        let refs = vec![vk_ref(5, 40), vk_ref(1, 60), vk_ref(3, 8)];
        let slot_refs = vec![Some(std_ref(0, 1)); 9];
        let (scope, reference_count) = build_scope_av1(
            &refs,
            &names(&[5, 1, 3, 5, 1, 3, 5]),
            [1u8, 3, 5, 7].into_iter(),
            2,
            fake_view(2),
            std_ref(50, 0),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();

        assert_eq!(reference_count, 3, "exactly this frame's references lead");
        assert_eq!(
            scope[..reference_count]
                .iter()
                .map(|e| e.slot_index)
                .collect::<Vec<_>>(),
            vec![5, 1, 3],
            "plan order, not slot order"
        );
        for (entry, r) in scope.iter().zip(&refs) {
            assert_eq!(entry.view, fake_view(r.slot));
            assert_eq!(entry.std.OrderHint, r.std.OrderHint);
        }

        // Then the other still-held slot (7), then the setup ACTIVATION entry.
        assert_eq!(scope[3].slot_index, 7);
        let last = scope.last().unwrap();
        assert_eq!(
            last.slot_index, -1,
            "the setup slot binds its resource without a current association"
        );
        assert_eq!(last.view, fake_view(2));
        assert_eq!(last.std.OrderHint, 50);
        assert_eq!(
            scope.len(),
            5,
            "3 refs + 1 other held slot + the activation"
        );
    }

    #[test]
    fn a_reference_slot_without_a_bound_image_fails_the_whole_op() {
        let refs = vec![vk_ref(4, 10), vk_ref(6, 20)];
        let slot_refs = vec![Some(std_ref(0, 1)); 9];
        let err = build_scope_av1(
            &refs,
            &names(&[4, 6]),
            [4u8, 6].into_iter(),
            0,
            fake_view(0),
            std_ref(30, 1),
            &slot_refs,
            |slot| (slot != 6).then(|| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 6 }),
            "{err}"
        );
    }

    #[test]
    fn a_reference_name_pointing_outside_the_bound_slots_fails_the_whole_op() {
        // The HEVC class, stated for AV1: a name resolving to a slot the decode op
        // does not bind is unresolvable for the hardware — it can only answer by
        // guessing. Refuse at submission time rather than discover it on a driver.
        let refs = vec![vk_ref(4, 10)];
        let slot_refs = vec![Some(std_ref(0, 1)); 9];
        let err = build_scope_av1(
            &refs,
            // Name 1 points at slot 7, which `refs` does not contain.
            &names(&[4, 7]),
            [4u8, 7].into_iter(),
            0,
            fake_view(0),
            std_ref(30, 1),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 7 }),
            "{err}"
        );

        // And the same list with slot 7 actually bound is fine — so the assertion
        // above measures the name check, not an unrelated refusal.
        let refs = vec![vk_ref(4, 10), vk_ref(7, 11)];
        let (_scope, reference_count) = build_scope_av1(
            &refs,
            &names(&[4, 7]),
            [4u8, 7].into_iter(),
            0,
            fake_view(0),
            std_ref(30, 1),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 2);
    }

    #[test]
    fn held_slots_are_bound_once_and_the_setup_slot_never_twice() {
        // Slot 3 is BOTH a reference and still held; slot 2 is the setup slot and
        // also held (the previous picture in it). Neither may appear twice: a
        // duplicate slot index in one coding scope is invalid.
        let refs = vec![vk_ref(3, 12)];
        let slot_refs = vec![Some(std_ref(99, 1)); 9];
        let (scope, reference_count) = build_scope_av1(
            &refs,
            &names(&[3, 3, 3, 3, 3, 3, 3]),
            [1u8, 2, 3].into_iter(),
            2,
            fake_view(2),
            std_ref(24, 1),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 1);
        let indices: Vec<i32> = scope.iter().map(|e| e.slot_index).collect();
        assert_eq!(indices, vec![3, 1, -1]);
        assert_eq!(
            indices.iter().filter(|&&i| i == 3).count(),
            1,
            "a referenced slot is bound exactly once even when seven names use it"
        );
        assert!(
            !indices.contains(&2),
            "the setup slot is bound only as the -1 activation entry"
        );
    }

    #[test]
    fn a_key_frames_scope_is_the_activation_entry_alone() {
        let slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>> = vec![None; 9];
        let (scope, reference_count) = build_scope_av1(
            &[],
            &names(&[]),
            std::iter::empty(),
            0,
            fake_view(0),
            std_ref(0, 0),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "a key frame references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![-1]
        );
    }

    #[test]
    fn resetting_the_slot_bindings_frees_every_ledger_and_hands_back_the_pinned_images() {
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        slots.assign(100).unwrap();
        slots.assign(200).unwrap();
        let mut slot_image: Vec<Option<usize>> = vec![Some(7), Some(8), None, None];
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>> =
            vec![Some(std_ref(10, 1)); 4];

        let unbound = reset_slot_bindings(&mut slots, &mut slot_image, &mut slot_refs);
        assert_eq!(
            unbound,
            vec![7, 8],
            "the pool images the stale bindings pinned go back on the free list"
        );
        assert_eq!(slots.active(), 0);
        assert_eq!(
            slots.capacity(),
            REQUIRED_SLOTS as usize,
            "capacity survives — no session rebuild"
        );
        assert!(slot_image.iter().all(Option::is_none));
        assert!(
            slot_refs.iter().all(Option::is_none),
            "cached reference info goes too, or build_scope_av1 could bind a slot \
             the planner no longer knows about"
        );
    }

    /// The submission-final picture info, built by the PRODUCTION function.
    ///
    /// Two things are load-bearing here and neither is visible without a driver:
    ///
    /// * `pTileOffsets` / `pTileSizes` must address 256 readable entries, because
    ///   RADV reads that many whatever `tileCount` says. So the arrays are checked
    ///   past the tile count, and the tail must be zero rather than whatever was in
    ///   the previous frame's allocation;
    /// * `tileCount` must nevertheless be the REAL count. ash's `tile_offsets()`
    ///   and `tile_sizes()` both write it from their slice length, so the
    ///   production function assigns it afterwards — and this test would catch a
    ///   refactor that dropped that line, because it would read 256.
    #[test]
    fn the_picture_info_carries_padded_tile_arrays_with_the_real_tile_count() {
        let packed = pack_av1_tiles(&[100..1000, 1000..1600, 1600..2100]).expect("fits u32");
        let tiles = submitted_tiles(&packed).expect("three tiles fit");
        assert_eq!(tiles.count, 3);
        assert_eq!(&tiles.offsets[..3], &[0, 900, 1500]);
        assert_eq!(&tiles.sizes[..3], &[900, 600, 500]);

        // SAFETY: StdVideoDecodeAV1PictureInfo is a plain-C bindgen struct of a
        // bitfield word, integers, byte arrays and const pointers; all-zero is
        // valid and no pointer is dereferenced here.
        let mut std_pic: hh::StdVideoDecodeAV1PictureInfo = unsafe { std::mem::zeroed() };
        std_pic.OrderHint = 42;
        let picture_info = av1_picture_info(&std_pic, names(&[5, 1, 3]), &tiles);

        assert_eq!(
            picture_info.tile_count, 3,
            "tileCount is the real count, not the array length ash's setters would \
             have written"
        );
        assert_eq!(
            picture_info.frame_header_offset, FRAME_HEADER_OFFSET,
            "the buffer holds tile payloads only, so there is no header to point at"
        );
        assert_eq!(
            picture_info.s_type,
            vk::StructureType::VIDEO_DECODE_AV1_PICTURE_INFO_KHR
        );
        assert_eq!(picture_info.reference_name_slot_indices[0], 5);
        assert_eq!(picture_info.reference_name_slot_indices[6], -1);
        // SAFETY: the three pointers were taken from `tiles`/`std_pic`, both alive
        // for this scope; the arrays behind the first two are AV1_MAX_NUM_TILES
        // long by construction, which is exactly the length read here.
        unsafe {
            let offsets =
                std::slice::from_raw_parts(picture_info.p_tile_offsets, AV1_MAX_NUM_TILES);
            let sizes = std::slice::from_raw_parts(picture_info.p_tile_sizes, AV1_MAX_NUM_TILES);
            assert_eq!(&offsets[..3], &[0, 900, 1500]);
            assert_eq!(&sizes[..3], &[900, 600, 500]);
            assert!(
                offsets[3..].iter().all(|o| *o == 0) && sizes[3..].iter().all(|s| *s == 0),
                "the tail a driver reads past tileCount must be zeroed, not \
                 whatever the allocator handed back"
            );
            assert_eq!((*picture_info.p_std_picture_info).OrderHint, 42);
        }

        // And a DPB slot info chains the AV1 reference info, not another codec's.
        let std = std_ref(17, 1);
        let dpb_info = vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(&std);
        assert_eq!(
            dpb_info.s_type,
            vk::StructureType::VIDEO_DECODE_AV1_DPB_SLOT_INFO_KHR
        );
        // SAFETY: the pointer was just taken from `std`, alive for this scope.
        unsafe {
            assert_eq!((*dpb_info.p_std_reference_info).OrderHint, 17);
        }
    }

    #[test]
    fn packed_tiles_land_end_to_end_and_more_than_the_arrays_hold_is_refused() {
        let packed = pack_av1_tiles(&[100..200, 500..560]).unwrap();
        assert_eq!(packed.offsets, vec![0, 100]);
        let tiles = submitted_tiles(&packed).expect("two tiles fit");
        assert_eq!(tiles.count, 2);
        assert_eq!(&tiles.offsets[..2], &[0, 100]);
        assert_eq!(&tiles.sizes[..2], &[100, 60]);

        // Exactly full is fine; one more is refused rather than truncated — a
        // silently dropped tile decodes as garbage in that part of the frame.
        let ranges: Vec<Range<usize>> = (0..AV1_MAX_NUM_TILES).map(|i| i * 4..i * 4 + 4).collect();
        let full = submitted_tiles(&pack_av1_tiles(&ranges).unwrap()).expect("256 tiles fit");
        assert_eq!(full.count, AV1_MAX_NUM_TILES as u32);
        let ranges: Vec<Range<usize>> = (0..AV1_MAX_NUM_TILES + 1)
            .map(|i| i * 4..i * 4 + 4)
            .collect();
        assert_eq!(
            submitted_tiles(&pack_av1_tiles(&ranges).unwrap()),
            Err(Av1TileError::TooManyTiles {
                tiles: AV1_MAX_NUM_TILES + 1
            })
        );
    }

    /// Every tile of the vendored vector, split and cross-checked against the
    /// vendored PARSER's own per-tile figures.
    ///
    /// This is the anti-vacuity assertion for [`plan_bitstream`]: the walk it does
    /// (OBU header, frame header length, tile-group header, `tile_size_minus_1`
    /// fields) is re-derived here from the parser's `Tile::tile_offset` /
    /// `Tile::tile_size` — which are computed by an INDEPENDENT code path inside
    /// cros-codecs — and the two must agree byte for byte on all 274 frames. A
    /// split that merely "looked plausible" (whole OBUs, say, or an off-by-the-OBU-
    /// header start) fails here rather than on a driver.
    #[test]
    fn every_tile_of_the_vector_splits_to_the_parsers_own_offsets_and_sizes() {
        let mut planner = Av1Planner::new();
        // A SECOND parser instance, walking the same bytes to recover the tile
        // ranges the plan does not carry. Its `Cow::Borrowed` payload slices point
        // into the packet, so their absolute offsets come out of the pointer
        // difference — no unsafe, and no re-implementation of the walk.
        let mut reference = Parser::default();
        let (mut frames, mut tiles_checked) = (0u32, 0u32);
        let mut frame_obus = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            // What the parser says the tiles are, in decode order.
            let mut expected: Vec<Range<usize>> = Vec::new();
            let mut consumed = 0usize;
            while consumed < packet.len() {
                let action = reference
                    .read_obu(&packet[consumed..])
                    .expect("the clean vector parses");
                let obu = match action {
                    ObuAction::Process(obu) => obu,
                    ObuAction::Drop(n) => {
                        consumed += n as usize;
                        continue;
                    }
                };
                consumed += obu.bytes_used;
                match reference.parse_obu(obu).expect("the clean vector parses") {
                    ParsedObu::Frame(frame) => {
                        frame_obus += 1;
                        let payload = frame.tile_group.obu.as_ref();
                        let base = payload.as_ptr() as usize - packet.as_ptr() as usize;
                        for tile in &frame.tile_group.tiles {
                            let start = base + tile.tile_offset as usize;
                            expected.push(start..start + tile.tile_size as usize);
                        }
                        // The parser keeps its own reference state and needs it
                        // advanced, exactly as `Av1Planner` does, or every later
                        // inter frame fails to parse.
                        if !frame.header.show_existing_frame {
                            reference
                                .ref_frame_update(&frame.header)
                                .expect("the clean vector updates");
                        }
                    }
                    ParsedObu::TileGroup(tg) => {
                        let payload = tg.obu.as_ref();
                        let base = payload.as_ptr() as usize - packet.as_ptr() as usize;
                        for tile in &tg.tiles {
                            let start = base + tile.tile_offset as usize;
                            expected.push(start..start + tile.tile_size as usize);
                        }
                    }
                    ParsedObu::FrameHeader(fh) if !fh.show_existing_frame => {
                        reference
                            .ref_frame_update(&fh)
                            .expect("the clean vector updates");
                    }
                    _ => {}
                }
            }

            let mut produced: Vec<Range<usize>> = Vec::new();
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                let bitstream = plan_bitstream(packet, &plan.tiles, &plan.header)
                    .expect("every tile group splits");
                produced.extend(bitstream.tiles);
            }
            tiles_checked += produced.len() as u32;
            assert_eq!(
                produced, expected,
                "the split disagrees with the parser's own tile offsets/sizes"
            );
        }

        assert_eq!(frames, 274, "every frame of the vector must split");
        assert_eq!(
            tiles_checked, 274,
            "this vector is one tile per frame; the count pins that the comparison \
             above actually compared something"
        );
        assert!(
            frame_obus > 0,
            "the vector must exercise the OBU_FRAME path — where the frame header \
             sits INSIDE the tile OBU and the split has to step over it"
        );
    }

    /// Over the vector: the ring slot must contain the TILE PAYLOADS AND NOTHING
    /// ELSE, and every submitted offset must land exactly on its tile's first byte
    /// inside it.
    ///
    /// The "nothing else" half is the layout assertion. Uploading whole OBUs also
    /// produced correct per-tile offsets — it is what this rung did until the M7
    /// review — so an offsets-only check passes against either layout. What
    /// distinguishes them is the slot LENGTH: libavcodec's layout uploads the sum
    /// of the tile sizes, and the OBU layout uploads the OBU headers, the frame
    /// headers and the `tile_size_minus_1` fields with them.
    #[test]
    fn the_ring_slot_holds_the_tile_payloads_and_nothing_else() {
        let mut planner = Av1Planner::new();
        let (mut checked, mut bytes_saved) = (0u32, 0usize);
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let bitstream = plan_bitstream(packet, &plan.tiles, &plan.header).expect("splits");
                let packed = pack_av1_tiles(&bitstream.tiles).expect("fits u32");
                let tiles = submitted_tiles(&packed).expect("within the tile limit");

                // Build the slot bytes exactly as the ring would.
                let mut slot: Vec<u8> = Vec::new();
                for segment in &packed.segments {
                    slot.extend_from_slice(&packet[segment.clone()]);
                }
                let obu_bytes: usize = plan.tiles.iter().map(|t| t.data.len()).sum();
                assert_eq!(
                    slot.len(),
                    bitstream.tiles.iter().map(Range::len).sum::<usize>(),
                    "the slot is the tile payloads exactly"
                );
                assert!(
                    slot.len() < obu_bytes,
                    "the tile payloads must be SHORTER than the OBUs that carried \
                     them, or this frame proves nothing about the layout"
                );
                bytes_saved += obu_bytes - slot.len();

                assert_eq!(tiles.count as usize, bitstream.tiles.len());
                for (i, range) in bitstream.tiles.iter().enumerate() {
                    let start = tiles.offsets[i] as usize;
                    let end = start + tiles.sizes[i] as usize;
                    assert!(end <= slot.len(), "a tile range reaches past the slot");
                    assert_eq!(
                        &slot[start..end],
                        &packet[range.clone()],
                        "the submitted offset does not address this tile's bytes"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 274, "every tile of the vector was addressed");
        eprintln!("bytes not uploaded across the vector: {bytes_saved}");
    }

    /// A hand-built TWO-tile tile group: the only way the coded-size arithmetic
    /// gets exercised at all.
    ///
    /// The vendored vector is one tile per frame, so every `tile_size_minus_1`
    /// read, the `tile_start_and_end_present_flag` bit and the tile-group header's
    /// byte alignment are dead code as far as
    /// `every_tile_of_the_vector_splits_to_the_parsers_own_offsets_and_sizes` is
    /// concerned. This builds the bytes by hand from the spec's `tile_group_obu()`
    /// layout and checks the ranges land on the payloads.
    fn two_tile_group(
        flag_present: bool,
    ) -> (Vec<u8>, FrameHeaderObu, Vec<pf_bitstream::av1::TilePlan>) {
        let mut header = FrameHeaderObu::default();
        header.tile_info.tile_cols = 2;
        header.tile_info.tile_rows = 1;
        header.tile_info.tile_cols_log2 = 1;
        header.tile_info.tile_rows_log2 = 0;
        header.tile_info.tile_size_bytes = 2;

        // tile_group_obu(): NumTiles = 2 > 1, so tile_start_and_end_present_flag
        // is coded. Clear ⇒ the group is the whole frame (tg 0..1) and the header
        // is one bit padded to one byte; set ⇒ tg_start/tg_end follow at
        // (tile_cols_log2 + tile_rows_log2) = 1 bit each, so 3 bits, still one byte.
        let tg_header: u8 = if flag_present {
            // flag=1, tg_start=0, tg_end=1 ⇒ bits 1 0 1 from the MSB.
            0b1010_0000
        } else {
            0b0000_0000
        };
        let tile0 = [0xA1u8, 0xA2, 0xA3];
        let tile1 = [0xB1u8, 0xB2];
        let mut payload = vec![tg_header];
        // le(TileSizeBytes = 2) of tile_size_minus_1 for every tile but the last.
        payload.extend_from_slice(&[(tile0.len() as u8) - 1, 0]);
        payload.extend_from_slice(&tile0);
        payload.extend_from_slice(&tile1);

        // obu_header(): type = OBU_TILE_GROUP (4), no extension, has_size_field.
        let mut au = vec![0x22u8, payload.len() as u8];
        let payload_start = au.len();
        au.extend_from_slice(&payload);
        let tiles = vec![pf_bitstream::av1::TilePlan {
            data: 0..au.len(),
            tg_start: 0,
            tg_end: 1,
        }];
        assert_eq!(payload_start, 2);
        (au, header, tiles)
    }

    #[test]
    fn a_multi_tile_group_splits_at_the_coded_tile_sizes() {
        for flag_present in [false, true] {
            let (au, header, tiles) = two_tile_group(flag_present);
            let bitstream = plan_bitstream(&au, &tiles, &header).expect("splits");
            let ranges = bitstream.tiles;
            // 2 OBU header bytes + 1 tile-group header byte + 2 size bytes = 5.
            assert_eq!(ranges, vec![5..8, 8..10], "flag_present={flag_present}");
            assert_eq!(&au[ranges[0].clone()], &[0xA1, 0xA2, 0xA3]);
            assert_eq!(&au[ranges[1].clone()], &[0xB1, 0xB2]);
        }

        // A coded size that OVERSHOOTS the payload is refused rather than
        // producing a range past the OBU. (A size that UNDERSHOOTS cannot be
        // caught — the last tile absorbs it; plan_bitstream's docs say so.)
        let (mut au, header, tiles) = two_tile_group(false);
        au[3] = 0x40; // tile_size_minus_1 = 64 ⇒ 65 bytes in an 8-byte payload
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Truncated { obu: 0 })
        );

        // `TileSizeBytes` is only CODED for a multi-tile frame, so it is only
        // checked there — a width of 0 (what the parser leaves on a single-tile
        // frame) would shift by 0..0 and read nothing.
        let (au, mut header, tiles) = two_tile_group(false);
        header.tile_info.tile_size_bytes = 0;
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Overflow)
        );
        header.tile_info.tile_size_bytes = 9;
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Overflow),
            "a width past 4 would overflow the shift"
        );

        // A tile group claiming more tiles than the frame has is malformed.
        let (au, header, mut tiles) = two_tile_group(false);
        tiles[0].tg_end = 7;
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Truncated { obu: 0 })
        );
    }

    #[test]
    fn an_obu_whose_declared_size_disagrees_with_the_plans_range_is_refused() {
        let mut planner = Av1Planner::new();
        let packet = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plan = planner
            .plan_au(packet)
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");
        // The clean article splits.
        assert!(plan_bitstream(packet, &plan.tiles, &plan.header).is_ok());

        // Now lie about the OBU's extent by one byte. `obu_size` inside the
        // bitstream still says the old end, and the disagreement is caught — the
        // one cross-check a walk with an implicit last tile size can have.
        let mut damaged = plan.tiles.clone();
        damaged[0].data.end -= 1;
        assert!(
            matches!(
                plan_bitstream(packet, &damaged, &plan.header),
                Err(Av1TileError::SizeMismatch { .. })
            ),
            "a range disagreeing with obu_size must be refused"
        );

        // An OBU type that carries no tiles at all is named rather than walked.
        let start = plan.tiles[0].data.start;
        let mut au = packet.to_vec();
        // OBU_METADATA (5) in the type field.
        au[start] = (au[start] & !0x78) | (5 << 3);
        assert_eq!(
            plan_bitstream(&au, &plan.tiles, &plan.header),
            Err(Av1TileError::UnexpectedObu {
                obu: 0,
                obu_type: 5
            })
        );

        // And a frame header claiming no tiles refuses before any byte is read.
        let mut no_tiles = (*plan.header).clone();
        no_tiles.tile_info.tile_cols = 0;
        assert_eq!(
            plan_bitstream(packet, &plan.tiles, &no_tiles),
            Err(Av1TileError::NoTiles)
        );
    }

    #[test]
    fn a_leb128_without_a_terminator_is_refused_rather_than_read_forever() {
        // Nine continuation bytes: the AV1 spec caps leb128() at eight.
        let au = [0x80u8; 16];
        assert_eq!(leb128(&au, 0), None);
        // A well-formed multi-byte value reads back exactly.
        let au = [0x81u8, 0x02];
        assert_eq!(leb128(&au, 0), Some((0x101, 2)));
        // And a value running off the end is a miss, not a panic.
        assert_eq!(leb128(&[0x80], 0), None);
        assert_eq!(leb128(&[], 0), None);
    }

    /// The extents and the level the session is shaped by, read off real plans.
    #[test]
    fn the_session_shape_comes_off_the_stream_not_a_constant() {
        let mut planner = Av1Planner::new();
        let packet = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plan = planner
            .plan_au(packet)
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");

        let extent = coded_extent(&plan);
        assert_eq!(
            (extent.width, extent.height),
            (plan.picture.upscaled_width, plan.picture.frame_height),
            "the decode output is the POST-superres width"
        );
        assert!(extent.width > 0 && extent.height > 0);

        // The vector is Main 4:2:0 8-bit without film grain.
        let key = profile_key_for(&plan).expect("inside the envelope");
        assert_eq!(key.output_format(), Some(crate::caps::NV12));
        assert!(!key.film_grain);

        // The level gate reads operating point 0 and stays inside the Std range.
        assert!(stream_level_idx(&plan) <= 23);
    }

    #[test]
    fn only_a_decoded_key_frame_ends_the_wait_for_one() {
        let mut planner = Av1Planner::new();
        let packet = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let first = planner
            .plan_au(packet)
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");
        assert!(first.picture.is_key, "the vector opens on a key frame");
        assert!(
            clears_awaiting_key(&first),
            "a decoded key frame is what resumes decoding"
        );

        // The same key frame as a `show_existing_frame` plan decodes nothing, so
        // it must NOT resume: the planner's store would be full and this decoder's
        // ledger empty, and the next inter frame would fail immediately.
        let mut shown = first.clone();
        shown.dpb.stored = None;
        assert!(shown.picture.is_key);
        assert!(!clears_awaiting_key(&shown));

        // An ordinary inter frame never resumes either.
        let inter = planner
            .plan_au(IvfIterator::new(AV1_25FPS).nth(1).expect("a second packet"))
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");
        assert!(!inter.picture.is_key);
        assert!(!clears_awaiting_key(&inter));
    }

    /// A recovery WAIT must reach the consumer as an ERROR, once per access unit —
    /// the same answer H.264/H.265 give through their planners'
    /// `PlanError::AwaitingIdr`, and the reason [`VkAv1Decoder::awaiting_key`]'s
    /// docs carry: a clean `Ok(None)` resets the consumer's demotion streak once
    /// per frame, so a rung whose every key frame fails (film grain on a device
    /// without the grain profile; a level above `maxLevelIdc`; a sequence header
    /// disagreeing with the negotiation) would never demote and the session would
    /// hold a frozen screen with a clean bill of health.
    ///
    /// What this pins is the AGGREGATION, which is where the naive fix goes wrong:
    /// the error is per ACCESS UNIT while the skip is per FRAME, because a key
    /// frame can sit behind a skipped frame in the same temporal unit — the
    /// vendored vector has 24 units carrying two frames each.
    #[test]
    fn a_unit_reports_the_key_frame_wait_only_when_it_decoded_nothing_at_all() {
        // The wait itself: every frame of the unit skipped.
        assert!(whole_unit_skipped(1, 1), "a single-frame unit");
        assert!(whole_unit_skipped(2, 2), "and a two-frame one");

        // A key frame arrived partway through the unit and decoded: NOT the wait,
        // whatever came before it. An early return at the first skip would have
        // answered an error here and never reached the key frame at all.
        assert!(!whole_unit_skipped(2, 1));
        assert!(!whole_unit_skipped(3, 2));
        // Nothing was skipped: the ordinary decoding case.
        assert!(!whole_unit_skipped(2, 0));
        // A unit that planned no frames (metadata / a sequence header on its own)
        // is a clean `Ok(None)`, never an error.
        assert!(!whole_unit_skipped(0, 0));
    }

    /// The wait's error must be DISTINGUISHABLE from the failure that started it —
    /// a support engineer reading a field log has to be able to tell "the AU could
    /// not be decoded" from "the decoder is waiting to re-anchor", and the two ride
    /// the same `Err` channel.
    #[test]
    fn the_key_frame_wait_names_itself_in_the_error_text() {
        let waiting = format!("{}", VkDecodeError::AwaitingKeyAv1);
        assert!(waiting.contains("key frame"), "{waiting}");
        assert!(waiting.contains("skipped"), "{waiting}");
        // …and it is not the same message as the loss that latched the recovery.
        let lost = format!(
            "{}",
            VkDecodeError::MissingReferenceAv1 {
                slot: 3,
                ref_index: 2
            }
        );
        assert_ne!(waiting, lost);
    }

    /// The `refresh_frame_flags == 0` leg is real AV1 and this vector has none of
    /// it — which is worth PROVING rather than assuming, because it is exactly the
    /// sort of "cannot happen" that quietly exhausts a nine-slot ledger in the
    /// field. The measurement is the point: it says plainly which arm the vendored
    /// vector exercises and which one only the code review covers.
    #[test]
    fn every_frame_of_the_vector_refreshes_a_slot_so_the_orphan_arm_is_review_only() {
        let mut planner = Av1Planner::new();
        let (mut frames, mut orphans) = (0u32, 0u32);
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                if plan.header.refresh_frame_flags == 0 {
                    orphans += 1;
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            orphans, 0,
            "if this ever fires the orphan release IS exercised — turn this into a \
             ledger-occupancy assertion rather than deleting it"
        );
    }

    /// The refusal predicate itself — [`lost_reference`], the PRODUCTION function
    /// `decode_planned` calls.
    ///
    /// It used to be re-implemented inline here, which meant deleting the real
    /// refusal left this green: the test asserted that a `find_map` over a
    /// hand-built array found what the array contained. The guard it is supposed
    /// to cover is the one that keeps a frame from being decoded against a
    /// reference the DPB does not hold.
    #[test]
    fn a_lost_reference_is_the_condition_the_decoder_refuses_on() {
        assert_eq!(
            lost_reference(&[
                PlanWarning::TruncatedAu { offset: 12 },
                PlanWarning::MissingReference {
                    slot: 3,
                    ref_index: 2,
                },
            ]),
            Some((3, 2)),
            "a missing reference must be found even behind another warning"
        );

        // A truncated tail alone is NOT this condition — it is concealment
        // material the planner already accounted for, and refusing on it would
        // turn every clipped AU into a keyframe request.
        assert_eq!(
            lost_reference(&[PlanWarning::TruncatedAu { offset: 12 }]),
            None
        );
        assert_eq!(
            lost_reference(&[PlanWarning::MissingShowExisting { slot: 4 }]),
            None,
            "a show_existing_frame naming an empty slot decodes nothing, so there \
             is no reference set to be wrong about"
        );
        assert_eq!(lost_reference(&[]), None);
    }

    /// And the whole vector goes through that predicate without tripping it — the
    /// anti-vacuity half: if the clean vector DID report a lost reference, every
    /// frame of it would be refused and the tests above would be measuring a
    /// decoder that decodes nothing.
    #[test]
    fn no_frame_of_the_clean_vector_trips_the_refusal() {
        let mut planner = Av1Planner::new();
        let mut frames = 0u32;
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                frames += 1;
                assert_eq!(
                    lost_reference(&plan.warnings),
                    None,
                    "frame {frames} of a clean conformance vector must not be refused"
                );
            }
        }
        assert_eq!(frames, 274);
    }

    /// The whole vendored vector through the DPB bookkeeping `decode_planned`
    /// runs — conversion, [`sync_slot_bindings`], [`build_scope_av1`] — with no
    /// GPU anywhere.
    ///
    /// This is the test that was missing, and the defect it closes reached an
    /// RTX 5070 Ti before anything on this machine noticed: 172 unit tests, clippy
    /// clean, and `AU 4: DPB slot 2 is referenced by this AU but binds no image`
    /// on the first hardware contact. Everything needed to see it was on the CPU.
    /// What was not on the CPU was a test that ran the three pieces TOGETHER: the
    /// conversion was tested against a `SlotMap`, the scope builder against
    /// hand-made reference lists, and the binding sync against nothing at all (it
    /// was four lines inline in `decode_planned`). Each was right about its own
    /// half and the seam between them was where the picture went missing.
    ///
    /// So this walks the real vector through the real functions and asserts what
    /// the hardware asserts:
    ///
    /// * every slot this frame references still binds an image when the scope is
    ///   built (the refusal that fired on the driver);
    /// * the image it binds is the one that reference was DECODED into — the
    ///   assertion that matters more, because a slot recycled into the setup
    ///   picture is *bound*, just to the wrong picture, and the hardware would
    ///   have predicted from the frame it was in the middle of writing without
    ///   ever reporting an error;
    /// * no held slot is left without a binding, which `build_scope_av1` only
    ///   traces as "unreachable in practice".
    #[test]
    fn slot_recycling_waits_for_the_decode_op() {
        #[derive(Clone, Default)]
        struct SimPicture {
            bound: bool,
            pending: bool,
            held: u32,
        }
        // A distinguishable view per POOL IMAGE (never dereferenced), so a scope
        // entry can be traced back to the picture that image holds.
        let image_view = |picture: usize| vk::ImageView::from_raw(picture as u64 + 1);

        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut slot_image: Vec<Option<usize>> = vec![None; REQUIRED_SLOTS as usize];
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>> =
            vec![None; REQUIRED_SLOTS as usize];
        let mut pictures =
            vec![SimPicture::default(); (REQUIRED_SLOTS + crate::images::HOLD_HEADROOM) as usize];
        // Decoded pictures awaiting an output verdict, and the pool image each
        // picture was decoded into (for as long as anything can reference it).
        let mut pending: BTreeMap<PicId, usize> = BTreeMap::new();
        let mut image_of: BTreeMap<PicId, usize> = BTreeMap::new();

        let (mut frames, mut deferring, mut scope_refs) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                let Some(setup_id) = plan.dpb.stored else {
                    // show_existing_frame decodes nothing; `decode_planned`
                    // settles it and releases its removals directly.
                    let (ready, dropped) =
                        settle_dpb_ids(&mut pending, &plan.dpb.outputs, &plan.dpb.removed);
                    for image in ready.into_iter().chain(dropped) {
                        pictures[image].pending = false;
                    }
                    for &id in &plan.dpb.removed {
                        slots.release(id);
                        image_of.remove(&id);
                    }
                    continue;
                };
                frames += 1;

                let vk = plan_to_vk_av1(&plan, &mut slots).expect("the clean vector converts");
                let setup = usize::from(vk.setup_slot);
                if !vk.release_after_decode.is_empty() {
                    deferring += 1;
                }

                for picture in sync_slot_bindings(&slots, &mut slot_image, vk.setup_slot) {
                    pictures[picture].bound = false;
                }
                let dst = pictures
                    .iter()
                    .position(|p| !p.bound && !p.pending && p.held == 0)
                    .unwrap_or_else(|| panic!("frame {frames}: picture pool exhausted"));

                // Every held slot but the setup one must bind an image, or
                // `build_scope_av1` silently drops it from the coding scope.
                for (slot, _id) in slots.held() {
                    if usize::from(slot) == setup {
                        continue;
                    }
                    assert!(
                        slot_image[usize::from(slot)].is_some(),
                        "frame {frames}: held slot {slot} binds no image"
                    );
                }

                let held_slots: Vec<u8> = slots.held().map(|(slot, _id)| slot).collect();
                let (scope, reference_count) = build_scope_av1(
                    &vk.refs,
                    &vk.reference_name_slot_indices,
                    held_slots.iter().copied(),
                    vk.setup_slot,
                    image_view(dst),
                    vk.setup_ref,
                    &slot_refs,
                    |slot| slot_image[usize::from(slot)].map(image_view),
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "frame {frames}: {e}\n  setup_slot={setup} setup_id={setup_id}\n  \
                         refs={:?}\n  names={:?}\n  bindings={slot_image:?}",
                        vk.refs.iter().map(|r| (r.slot, r.id)).collect::<Vec<_>>(),
                        vk.reference_name_slot_indices,
                    )
                });

                // The scope binds every held slot but the setup one exactly once,
                // plus the setup slot as the `-1` activation entry — nothing
                // dropped, nothing duplicated. (The setup slot is always held by
                // now: `plan_to_vk_av1` assigned it.)
                assert_eq!(reference_count, vk.refs.len());
                let mut bound: Vec<i32> = scope.iter().map(|e| e.slot_index).collect();
                assert_eq!(bound.pop(), Some(-1), "frame {frames}: no activation entry");
                bound.sort_unstable();
                let mut expected: Vec<i32> = held_slots
                    .iter()
                    .filter(|slot| usize::from(**slot) != setup)
                    .map(|slot| i32::from(*slot))
                    .collect();
                expected.sort_unstable();
                assert_eq!(
                    bound, expected,
                    "frame {frames}: the coding scope must bind exactly the held \
                     slots, once each"
                );

                // THE assertion: each reference's scope entry must carry the image
                // that reference was decoded into. A slot recycled into the setup
                // picture binds an image too — the wrong one — and nothing but this
                // would say so.
                for (entry, r) in scope[..reference_count].iter().zip(&vk.refs) {
                    let decoded_into = image_of[&r.id];
                    assert_eq!(
                        entry.view,
                        image_view(decoded_into),
                        "frame {frames}: reference picture {} (slot {}) binds pool \
                         image {:?}, but it was decoded into image {decoded_into}",
                        r.id,
                        r.slot,
                        slot_image[usize::from(r.slot)]
                    );
                    assert_ne!(
                        decoded_into, dst,
                        "frame {frames}: reference picture {} resolves to the image \
                         this very frame is decoding into",
                        r.id
                    );
                    scope_refs += 1;
                }

                // Post-submit bookkeeping, in `decode_planned`'s order.
                pictures[dst].pending = true;
                pictures[dst].bound = true;
                slot_image[setup] = Some(dst);
                slot_refs[setup] = Some(vk.setup_ref);
                for r in &vk.refs {
                    slot_refs[usize::from(r.slot)] = Some(r.std);
                }
                for &id in &vk.release_after_decode {
                    assert!(slots.release(id), "frame {frames}: deferred release missed");
                }
                pending.insert(setup_id, dst);
                image_of.insert(setup_id, dst);

                let (ready, dropped) =
                    settle_dpb_ids(&mut pending, &plan.dpb.outputs, &plan.dpb.removed);
                for image in ready.into_iter().chain(dropped) {
                    // A consumer that displays and releases at once: the harshest
                    // case for the pool, because an image comes back free the
                    // instant nothing else pins it.
                    pictures[image].pending = false;
                }
                for &id in &plan.dpb.removed {
                    image_of.remove(&id);
                }
                if plan.header.refresh_frame_flags == 0 {
                    slots.release(setup_id);
                    if let Some(image) = pending.remove(&setup_id) {
                        pictures[image].pending = false;
                    }
                    image_of.remove(&setup_id);
                }
            }
        }

        assert_eq!(frames, 274, "every frame of the vector must decode");
        // Anti-vacuity. Releasing a displaced reference eagerly — the shape every
        // codec in this crate shipped with — gave its slot to the decode target on
        // 268 of these 274 frames, measured, the first at frame 6 (AU 4 of the
        // stream, which is the AU the driver refused). So if this count ever
        // reaches 0 the vector stopped exercising the case and the assertions above
        // are comparing empty lists.
        assert_eq!(
            deferring, 268,
            "268 of 274 frames displace a picture they are reading; at zero, \
             `release_after_decode` could be deleted and nothing here would fail"
        );
        assert_eq!(
            scope_refs, 1616,
            "the references actually bound into a coding scope across the vector"
        );
        eprintln!(
            "frames {frames} · scope references {scope_refs} · deferred releases {deferring}"
        );
    }
}
