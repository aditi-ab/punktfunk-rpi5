//! Native AV1 Vulkan Video decode over the `pf_bitstream` AV1 planner.
//!
//! Per temporal unit: `plan_au` (several frames) → `plan_to_vk_av1` →
//! tile-payload ring upload → record (barriers, `vkCmdBeginVideoCodingKHR`
//! with every bound DPB slot, one-shot session RESET, a caps-gated
//! `RESULT_STATUS_ONLY` query around `vkCmdDecodeVideoKHR`) → submit on the
//! decode queue under the caller's [`QueueLock`] with a per-image timeline
//! signal. `show_existing_frame` settles DPB and output with no GPU submit.
//!
//! `referenceNameSlotIndices` are DPB slot indices, not `pReferenceSlots`
//! positions; inconsistent bindings fail closed. AV1 §7.20: a slot this frame
//! reads cannot become its target until the decode is recorded —
//! [`DecodePlanVkAv1::release_after_decode`] delays that recycle. A named
//! reference with no image is fatal: latch recovery and wait for the next key
//! frame; never submit [`REFERENCE_NAME_UNUSED`].
//!
//! [`plan_bitstream`] uploads concatenated tile payloads only
//! (`frameHeaderOffset` 0). `pTileOffsets` / `pTileSizes` are 256-long
//! zero-tailed arrays (the driver reads the full arrays); `tileCount` is the
//! real count. Result-status queries only when the queue family advertises them.

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
use tracing::warn;

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

/// Eight `NUM_REF_FRAMES` plus the picture being decoded. Codec constant, not
/// an SPS field: an AV1 session never renegotiates DPB depth.
const REQUIRED_SLOTS: u32 = NUM_REF_SLOTS as u32 + 1;

/// Spec `obu_type` for a standalone tile group.
const OBU_TILE_GROUP: u8 = 4;
/// Spec `obu_type` for a frame header plus its tile group.
const OBU_FRAME: u8 = 6;

/// Malformed tile OBUs. Feeding the whole OBU as payload would treat headers
/// and `tile_size_minus_1` as entropy data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Av1TileError {
    Truncated {
        obu: usize,
    },
    NotAnObu {
        obu: usize,
    },
    /// Only `OBU_TILE_GROUP` and `OBU_FRAME` carry tiles.
    UnexpectedObu {
        obu: usize,
        obu_type: u8,
    },
    NoTiles,
    /// `obu_size` disagrees with the plan range. Last-tile size is implicit
    /// (whatever remains), so this is the only independent end-of-payload check.
    SizeMismatch {
        obu: usize,
        declared_end: usize,
        ranged_end: usize,
    },
    Overflow,
    /// More tiles than [`AV1_MAX_NUM_TILES`] (the submission arrays).
    TooManyTiles {
        tiles: usize,
    },
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

/// One frame's tile payload ranges in access-unit coordinates — these are the
/// uploaded bytes, so packed offsets are the concatenation with no rebase.
///
/// [`Self::groups`] is the `tile_data` region per tile-group OBU (size fields
/// included). This decoder does not upload that layout; `pf_dxvadec` does, and
/// must not re-walk the same spec arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1Bitstream {
    pub tiles: Vec<Range<usize>>,
    /// Per tile-group OBU: first `tile_size_minus_1` through OBU payload end.
    /// Every [`Self::tiles`] range sits in exactly one of these.
    pub groups: Vec<Range<usize>>,
}

fn leb128(au: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    // Spec caps leb128() at 8 bytes; a ninth continuation is malformed.
    for i in 0..8 {
        let byte = *au.get(at + i)?;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Walk the plan's tile OBUs into per-tile payload ranges.
///
/// `TilePlan::data` is a whole tile-group (or frame) OBU. Vulkan wants the
/// payloads only. The walk is spec `tile_group_obu()` (5.11.1): `header_bytes`
/// locates the tile-group start in an `OBU_FRAME`; `TileInfo` supplies
/// `NumTiles`, tg bit widths, and `TileSizeBytes`.
///
/// Last-tile size is implicit, so a walk always ends flush and "sizes add up"
/// is not a check. [`Av1TileError::SizeMismatch`] is `obu_size` vs the plan
/// range; overshoot is [`Av1TileError::Truncated`]; undershoot shortens the
/// last tile with nothing in the bitstream to contradict it.
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
        // Plan range is header + obu_size. Cross-check when the size field is
        // present (the only independent end); Annex-B omits it and the range stands.
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

        // OBU_FRAME: skip the frame header. The driver reads it from
        // `pStdPictureInfo`; the bitstream buffer holds tile payloads only.
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

        // Flag is coded only when NumTiles > 1. Read it; do not infer from the
        // plan's tg_start/tg_end — 0/NumTiles-1 has two spellings of different length.
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
        // `tile_data` starts here (`AV1RawTileGroup::tile_data` / DXVA memcpy).
        groups.push(cursor..payload_end);

        // Bound by NumTiles: a tg_end past NumTiles would walk off the payload.
        let count = tile_group
            .tg_end
            .checked_sub(tile_group.tg_start)
            .and_then(|span| span.checked_add(1))
            .filter(|count| *count <= num_tiles)
            .ok_or(Av1TileError::Truncated { obu: index })? as usize;
        // `TileSizeBytes` is 1..=4 when NumTiles > 1; 5.9.15 does not code it
        // for a single-tile frame (parser leaves 0).
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
                // le(TileSizeBytes) inside the OBU, not merely inside the AU.
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

/// 256: RADV reads that many entries regardless of `tileCount`.
pub(crate) const AV1_MAX_NUM_TILES: usize = 256;

/// Per-tile offsets/sizes. Arrays are 256 (what the driver reads); `count` is
/// the real tile count. ash's `tile_offsets()`/`tile_sizes()` set `tileCount`
/// from slice length, so a slice would fuse the two numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmittedTiles {
    pub(crate) offsets: [u32; AV1_MAX_NUM_TILES],
    pub(crate) sizes: [u32; AV1_MAX_NUM_TILES],
    pub(crate) count: u32,
}

/// Packed-tile offsets/sizes as `u32`. A miss here is silent corruption: the
/// hardware would start mid-tile.
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
        // Offset + size must not wrap: Vulkan would read past the packed buffer.
        offset.checked_add(size).ok_or(Av1TileError::Overflow)?;
        tiles.offsets[i] = *offset;
        tiles.sizes[i] = size;
    }
    Ok(tiles)
}

/// Always 0: the buffer holds tile payloads only. The driver takes the frame
/// header from `pStdPictureInfo`.
const FRAME_HEADER_OFFSET: u32 = 0;

/// `VkVideoDecodeAV1PictureInfoKHR` for the decode op.
///
/// Assign `tileCount` after both setters. ash's `tile_offsets()`/`tile_sizes()`
/// each set it from slice length; the arrays are 256 long, so a setter win would
/// tell the driver there are 256 tiles.
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

/// Planner-reported missing DPB reference. Does not match
/// [`PlanWarning::TruncatedAu`]: that is concealment the planner already
/// applied; refusing it would turn every clipped AU into a keyframe request.
pub(crate) fn lost_reference(warnings: &[PlanWarning]) -> Option<(u8, u8)> {
    warnings.iter().find_map(|w| match w {
        PlanWarning::MissingReference { slot, ref_index } => Some((*slot, *ref_index)),
        _ => None,
    })
}

/// One planned frame: decoded, or skipped while waiting for a key. The caller
/// counts skips; a unit of only skips is [`VkDecodeError::AwaitingKeyAv1`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameOutcome {
    /// Submitted, or `show_existing_frame` settled without a submit.
    Decoded,
    SkippedAwaitingKey,
}

/// One session generation. Extent or profile change (bit-depth, sampling,
/// film-grain) retires it.
struct SessionStateAv1 {
    session: VideoSessionAv1,
    slots: SlotMap,
    /// Distinct-mode DPB backing. `None` in coincide (the picture pool is the DPB).
    dpb: Option<DpbPool>,
    pool: PicturePool,
    ring: BitstreamRing,
    ops: OpRing,
    /// Std ref info per DPB slot. Begin-coding wants it for every bound slot,
    /// including ones this frame does not reference.
    slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>>,
    /// Coincide: pool image bound to each DPB slot. Rebound on activation.
    slot_image: Vec<Option<usize>>,
    /// Per command-buffer completion tokens (reuse gate).
    cmd_marks: Vec<Option<(vk::Semaphore, u64)>>,
    /// Per query-slot submission ordinals (staleness).
    query_marks: Vec<u64>,
    submitted: u64,
    last_submit: Option<(vk::Semaphore, u64)>,
    /// Stream coded extent (renegotiation comparison).
    coded_extent: vk::Extent2D,
    /// Granularity-aligned allocation extent (picture resources and frames).
    image_extent: vk::Extent2D,
}

/// Native Vulkan Video AV1 decoder. Public surface matches [`crate::VkH265Decoder`].
pub struct VkAv1Decoder {
    dev: DecodeDevice,
    lock: Box<dyn QueueLock>,
    planner: Av1Planner,
    /// Caps per profile key. Bit-depth or film-grain change is a new key.
    caps: Option<(Av1ProfileKey, DecodeCaps)>,
    state: Option<SessionStateAv1>,
    /// Hidden frames waiting on a later `show_existing_frame`. A `show_frame`
    /// picture is settled into `ready` by the plan that decoded it.
    pending: BTreeMap<PicId, PendingPic>,
    /// Display-ready frames not yet handed out. A temporal unit can fill several.
    ready: VecDeque<DecodedVkFrame>,
    /// Retired generations' pools with consumer-held images (die on last token).
    graveyard: Vec<RetiredPool>,
    last_warnings: Vec<PlanWarning>,
    /// Decode-order ordinal stamped onto frames. Survives rebuilds: it describes
    /// the stream, not the Vulkan objects.
    decoded: u64,
    generation: u64,
    device_lost: bool,
    recovery: RecoveryLatch,
    /// Skip until the next decoded key. The planner has no `flush`, so after
    /// [`Self::recover_dpb`] its store still names the emptied slots.
    /// Per-frame skip, per-AU error: [`VkDecodeError::AwaitingKeyAv1`].
    /// `Ok(None)` would reset the demotion streak.
    awaiting_key: bool,
    /// Over-declared-level warning, once per decoder (`ensure_state` runs per AU).
    level_advisory_warned: bool,
}

impl VkAv1Decoder {
    /// Wrap the borrowed device. Sessions/pools are built lazily from the first
    /// sequence header (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// The [`DeviceHandles`] contract (liveness, extensions, features, truthful
    /// queue families) holds for this decoder's lifetime. `VK_KHR_video_decode_av1`
    /// must be enabled. The family `videoCodecOperations` check is the device's
    /// claim, not proof the client passed the extension to `vkCreateDevice`.
    /// Missing it is UB at session create, so the family check runs first.
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
            level_advisory_warned: false,
        })
    }

    /// Caps check before any AU. `film_grain` is part of the AV1 decode profile;
    /// missing it here is a construction failure, not a mid-stream error streak.
    ///
    /// Negotiated facts are a hint (the sequence header is authoritative). Extent,
    /// DPB depth, and a disagreeing header still fail at the first AU. Declared
    /// level is advisory; `ensure_state` only warns.
    pub fn probe_stream_support(
        &self,
        chroma_format_idc: u8,
        bit_depth: u8,
        film_grain: bool,
    ) -> Result<(), VkDecodeError> {
        let key = Av1ProfileKey::from_negotiated(chroma_format_idc, bit_depth, film_grain)?;
        // SAFETY: the constructor `DeviceHandles` contract holds for this lifetime.
        let raw =
            unsafe { query_av1_caps(&self.dev, key) }.map_err(|r| caps_query_error(r, key))?;
        let wanted = key
            .output_format()
            .expect("from_negotiated gated the sampling/depth combination");
        derive_caps_av1(&raw, wanted)?;
        Ok(())
    }

    /// Decode one temporal unit (may carry several frames). Returns the next
    /// display-ready frame; drain the rest with [`Self::take_ready`].
    ///
    /// Every frame skipped while [`Self::awaiting_key`] is [`VkDecodeError::AwaitingKeyAv1`].
    /// A `show_existing_frame` naming an empty slot is a warning and displays
    /// nothing. `DeviceLost` latches until the owner rebuilds on fresh handles.
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
        // Recover before planning: a stranded DPB picture would fail every later
        // reference ([`RecoveryLatch`]).
        if self.recovery.take() {
            self.recover_dpb();
        }
        // Clear before planning so a plan failure cannot re-report the previous AU.
        self.last_warnings.clear();
        let plans = match self.planner.plan_au(au) {
            Ok(plans) => plans,
            Err(e) => return Err(VkDecodeError::PlanAv1(e)),
        };
        // Concatenate per AU: one unit's frames share a concealment verdict.
        for plan in &plans {
            for warning in &plan.warnings {
                trace!(?warning, "plan warning");
            }
            self.last_warnings.extend(plan.warnings.iter().cloned());
        }

        let mut skipped = 0usize;
        for plan in &plans {
            // Planner store already holds this picture. Failure below can disagree
            // with the ledgers; latch recovery rather than leave them split.
            match self.decode_planned(plan, au) {
                Ok(FrameOutcome::Decoded) => {}
                Ok(FrameOutcome::SkippedAwaitingKey) => skipped += 1,
                Err(e) => {
                    self.recovery.latch();
                    return Err(e);
                }
            }
        }
        // Count skips; do not return at the first. A key may sit behind a skipped
        // frame in the same unit. Not a latch: `recover_dpb` already ran.
        if whole_unit_skipped(plans.len(), skipped) {
            return Err(VkDecodeError::AwaitingKeyAv1);
        }
        Ok(self.ready.pop_front())
    }

    fn decode_planned(&mut self, plan: &AuPlan, au: &[u8]) -> Result<FrameOutcome, VkDecodeError> {
        // Only a decoded key clears the wait. `show_existing_frame` of a key
        // resets the planner store (7.20) but decodes nothing — empty ledger vs
        // full store, then the next inter fails and re-arms the wait.
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

        // `show_existing_frame`: settle DPB and output with no GPU submit.
        let Some(setup_id) = plan.dpb.stored else {
            self.settle(&plan.dpb.outputs, &plan.dpb.removed);
            if let Some(state) = &mut self.state {
                for &id in &plan.dpb.removed {
                    state.slots.release(id);
                }
            }
            return Ok(FrameOutcome::Decoded);
        };

        if let Some((slot, ref_index)) = lost_reference(&plan.warnings) {
            return Err(VkDecodeError::MissingReferenceAv1 { slot, ref_index });
        }

        // Stamp decode-order before anything can reorder it.
        self.decoded = self.decoded.saturating_add(1);
        let decode_order = self.decoded;

        self.ensure_state(plan)?;

        // Recreate destroys an existing parameters object; drain first. The
        // session's first parameters create has nothing to destroy.
        {
            let session = &self.state.as_ref().expect("ensure_state built it").session;
            if session.parameters_action(&plan.sequence) == ParamsActionAv1::Recreate
                && session.has_parameters()
            {
                self.drain_gpu()?;
            }
        }
        let state = self.state.as_mut().expect("ensure_state built it");
        // SAFETY: live device; drain above satisfies Recreate; Current touches
        // nothing a submitted decode reads.
        unsafe { state.session.ensure_parameters(&plan.sequence)? };

        // Walk tiles before the DPB ledger: a malformed group must not half-apply.
        let bitstream =
            plan_bitstream(au, &plan.tiles, &plan.header).map_err(VkDecodeError::TilesAv1)?;

        let vk_plan = plan_to_vk_av1(plan, &mut state.slots).map_err(VkDecodeError::ConvertAv1)?;

        // Binding more than maxActiveReferencePictures is a silent VUID miss.
        let max_active = state.session.config.max_active_references as usize;
        if vk_plan.refs.len() > max_active {
            return Err(VkDecodeError::Unsupported(format!(
                "frame references {} pictures, session allows {max_active} active references",
                vk_plan.refs.len()
            )));
        }

        // Coincide: unbind released slots; drop the setup slot's previous image.
        let setup = usize::from(vk_plan.setup_slot);
        if state.dpb.is_none() {
            let unbound =
                sync_slot_bindings(&state.slots, &mut state.slot_image, vk_plan.setup_slot);
            for picture in unbound {
                state.pool.pictures[picture].bound = false;
            }
        }

        // Free pool image, never one a consumer holds.
        let Some(dst) = state.pool.free_index() else {
            debug!(
                held = state.pool.held_total(),
                "picture pool exhausted — release_frame owed"
            );
            return Err(VkDecodeError::NoFreeSlot);
        };

        // Dst's last timeline (presenter write-back), plus coincide refs so
        // reads follow any reported layout restore.
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

        let submission = state.submitted;
        let cmd_index = (submission % state.ops.cmds.len() as u64) as usize;
        if let Some((sem, value)) = state.cmd_marks[cmd_index] {
            // SAFETY: live device; token is a pool image's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "command buffer reuse")? };
        }
        let query_index = (submission % u64::from(state.ops.query_count)) as u32;

        // Tile payloads only; AV1 has no start codes to strip.
        let Some(packed) = pack_av1_tiles(&bitstream.tiles) else {
            return Err(VkDecodeError::Unsupported(
                "packed tile data exceeds the u32 offsets Vulkan submits".into(),
            ));
        };
        let tiles = submitted_tiles(&packed).map_err(VkDecodeError::TilesAv1)?;

        let device = self.dev.ash().clone();
        let mut poll = |token: &(vk::Semaphore, u64)| -> Result<bool, VkDecodeError> {
            // SAFETY: live device; token semaphore is a pool semaphore.
            let current = unsafe { device.get_semaphore_counter_value(token.0) }
                .map_err(VkDecodeError::from)?;
            Ok(current >= token.1)
        };
        let device2 = self.dev.ash().clone();
        let mut wait = |token: &(vk::Semaphore, u64)| -> Result<(), VkDecodeError> {
            // SAFETY: live device; token semaphore is a pool semaphore.
            unsafe { wait_timeline(&device2, token.0, token.1, "bitstream slot drain") }
        };
        // SAFETY: live device; segments are in-bounds plan ranges; pending tokens
        // are the completion signals of the submissions that consumed the slots.
        let upload = unsafe {
            state
                .ring
                .upload(&self.dev, au, &packed.segments, &mut poll, &mut wait)?
        };

        // SAFETY: live device; recorded handles belong to this generation; packed
        // tiles sit in the ring slot.
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

        state.slot_refs[setup] = Some(vk_plan.setup_ref);
        for r in &vk_plan.refs {
            state.slot_refs[usize::from(r.slot)] = Some(r.std);
        }

        // Slots this frame still read; held through convert/submit so setup
        // assignment cannot recycle them. Free now that the decode is recorded.
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
                    // Render size is a display hint (5.9.6 has no upper bound),
                    // not a window. Unclamped it would crop past the decoded image.
                    width: plan.picture.render_width.min(plan.picture.upscaled_width),
                    height: plan.picture.render_height.min(plan.picture.frame_height),
                },
                colour: plan.picture.colour,
                // No POC. OrderHint wraps; it is not a monotone counter.
                poc: plan.picture.order_hint as i32,
                // Key is the only re-anchor (no IDR, no recovery-point SEI).
                is_idr: plan.picture.is_key,
                recovery: crate::recovery::RecoveryMark::NONE,
                decode_order,
                references_clean: plan.picture.references_clean,
            },
        );

        self.settle(&plan.dpb.outputs, &plan.dpb.removed);

        // refresh_frame_flags == 0 never enters the planner store, so it is never
        // reported removed. The ledger still assigned a slot; leave it and nine
        // such frames hit `SlotError::Full`.
        if plan.header.refresh_frame_flags == 0 {
            let state = self.state.as_mut().expect("ensured above");
            state.slots.release(setup_id);
            // Not shown either: nothing can display or reference it.
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

    /// Outputs become ready (pending → held); removed-never-shown free their images.
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

    /// Return a delivered frame. `presenter_signaled` means the consumer sampled
    /// and signaled `value + 1` ([`DecodedVkFrame`]); wait that write-back before
    /// reuse. Every `decode`/`take_ready` frame comes back once, including stale
    /// generations (retired pool dies on its last token).
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
        // Retired pool dies on last token (presenter waited; decode drained).
        if frame.generation != self.generation {
            self.graveyard
                .retain(|r| r.generation != frame.generation || r.pool.held_total() > 0);
        }
        Ok(())
    }

    /// Next display-ready frame after the one `decode` returned. Drain after every
    /// decode; leftover frames occupy pool images. A temporal unit can fill several.
    pub fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        self.ready.pop_front()
    }

    /// Last planned AU's warnings, decode order. Cleared by the next `decode`.
    pub fn take_warnings(&mut self) -> Vec<PlanWarning> {
        std::mem::take(&mut self.last_warnings)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Decode-order watermark. 0 before the first decode; `show_existing_frame`
    /// does not advance it.
    pub fn decode_order(&self) -> u64 {
        self.decoded
    }

    /// One-line state snapshot (not a stable format).
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

    /// Decode status without waiting. [`DecodeStatus::Failed`] is a driver error
    /// or a query slot re-armed before it was read. Without
    /// `queryResultStatusSupport` (RADV), `Ok` means the timeline completed.
    pub fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, false)
    }

    pub fn status_queries(&self) -> bool {
        self.dev.result_status_queries()
    }

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
            // No queries: degrade to timeline completion.
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
        // SAFETY: live device; query pool is this generation's; `query_slot` is
        // in range (checked against the marks array it is sized to).
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

    /// Bounded wait for a delivered frame's decode-complete signal. Touches no
    /// decoder state. `frame` must be unreleased (pins the pool semaphore).
    pub fn wait_decoded(&self, frame: &DecodedVkFrame, timeout_ns: u64) -> bool {
        if frame.generation != self.generation {
            return false;
        }
        let semaphores = [frame.semaphore];
        let values = [frame.value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&semaphores)
            .values(&values);
        // SAFETY: live device; unreleased frame pins the pool semaphore; info
        // arrays are locals that outlive the call.
        unsafe { self.dev.ash().wait_semaphores(&info, timeout_ns) }.is_ok()
    }

    /// Discard pending pictures (teardown / discontinuity). AV1 has no reorder
    /// buffer: pending entries are hidden frames. Leaves [`Self::awaiting_key`]
    /// — the planner has no `flush` and still names the emptied slots.
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

    /// Align the three DPB ledgers after a post-planning failure: planner store,
    /// [`SlotMap`], slot→image. [`Self::flush`] empties the last two and arms
    /// [`Self::awaiting_key`]. Not a session rebuild — pools stay valid.
    fn recover_dpb(&mut self) {
        debug!(
            snapshot = %self.debug_snapshot(),
            "recovering from a failed AV1 frame — skipping to the next key frame"
        );
        self.flush();
    }

    /// Session/caps match this plan's extent and profile. A declared level above
    /// the device ceiling warns once and proceeds (`seq_level_idx` 31 is not a
    /// level).
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
        // Declared level above maxLevel is not a refusal: extent and DPB depth
        // are the physical facts. `seq_level_idx` 31 is Annex A's "maximum
        // parameters", not a level; sequence headers carry none to the driver.
        let caps_max_level = self.caps.as_ref().expect("queried above").1.max_level_idc;
        let stream_level = u32::from(stream_level_idx(plan));
        if stream_level > caps_max_level.code_point() && !self.level_advisory_warned {
            self.level_advisory_warned = true;
            warn!(
                stream_level,
                ceiling = %caps_max_level,
                "stream declares an AV1 level above the device ceiling — the declared \
                 level is advisory (seq_level_idx 31 means \"maximum parameters\", and \
                 encoders over-declare); proceeding, since the level never reaches the \
                 driver"
            );
        }
        let coded = coded_extent(plan);
        match &self.state {
            Some(state) if state.coded_extent == coded && state.session.config.profile == key => {
                Ok(())
            }
            _ => self.rebuild_state(plan),
        }
    }

    /// Drain the current generation, graveyard any consumer-held images, and build
    /// a fresh session. Bumps [`Self::generation`] so old frames route there.
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
        // Bounds at the allocation extent: that is what images are created at.
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
        // Test-only: `vkCmdCopyImageToBuffer` needs TRANSFER_SRC; production pools
        // do not carry it.
        if std::env::var("PF_VKD_TEST_READBACK").is_ok_and(|v| v == "1") {
            pool_plan.picture_usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        let decode_profile = DecodeProfile::Av1(key);
        // SAFETY: live device; each created half is owned by a Drop type at birth,
        // so a mid-build failure unwinds cleanly.
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

    fn drain_gpu(&mut self) -> Result<(), VkDecodeError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        if let Some((sem, value)) = state.last_submit {
            // SAFETY: live device; token is a pool image's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "session drain")? };
        }
        Ok(())
    }
}

impl Drop for VkAv1Decoder {
    fn drop(&mut self) {
        // Best-effort drain so pool Drop does not destroy in-flight work. Presenter
        // sampling of held/graveyarded images is the caller's teardown contract.
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

/// Name a failed caps query. Do not re-query with grain off: that would decode
/// the pictures and silently drop the grain the encoder relied on.
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

fn profile_key_for(plan: &AuPlan) -> Result<Av1ProfileKey, VkDecodeError> {
    Av1ProfileKey::from_stream(
        plan.sequence.seq_profile as u8,
        plan.picture.chroma_format_idc,
        plan.picture.bit_depth,
        plan.sequence.film_grain_params_present,
    )
    .map_err(VkDecodeError::ParamsAv1)
}

/// Decoded key frame (references nothing, refreshes all eight slots).
/// `show_existing_frame` of a key resets the planner store (7.20) but decodes
/// nothing — empty ledger vs full store.
fn clears_awaiting_key(plan: &AuPlan) -> bool {
    plan.picture.is_key && plan.dpb.stored.is_some()
}

/// [`VkDecodeError::AwaitingKeyAv1`] when every planned frame was skipped.
/// `planned == 0` is `Ok(None)` (metadata-only AU). `skipped < planned` is not
/// a skip: a key may sit behind a skipped frame in the same unit.
fn whole_unit_skipped(planned: usize, skipped: usize) -> bool {
    planned > 0 && skipped == planned
}

/// Operating point 0 is the full stream (non-scalable default; hosts emit one).
fn stream_level_idx(plan: &AuPlan) -> u8 {
    plan.sequence.operating_points[0].seq_level_idx
}

/// Decode output extent: superres upscales after reconstruction, so pool images
/// hold `upscaled_width` × `frame_height`. One extent per session generation;
/// `ensure_state` rebuilds on change. Mid-sequence size override with scaled
/// refs is outside the envelope: rebuild + [`VkAv1Decoder::awaiting_key`].
fn coded_extent(plan: &AuPlan) -> vk::Extent2D {
    vk::Extent2D {
        width: plan.picture.upscaled_width,
        height: plan.picture.frame_height,
    }
}

/// Empty DPB residency, slot→image, and cached ref info together. Leaving ref
/// info would let [`build_scope_av1`] bind a slot the planner no longer knows.
/// Returns the pool images the cleared bindings pinned.
fn reset_slot_bindings(
    slots: &mut SlotMap,
    slot_image: &mut [Option<usize>],
    slot_refs: &mut [Option<hh::StdVideoDecodeAV1ReferenceInfo>],
) -> Vec<usize> {
    // `held` borrows the map that `release` mutates.
    for (_slot, id) in slots.held().collect::<Vec<_>>() {
        slots.release(id);
    }
    let unbound = slot_image.iter_mut().filter_map(Option::take).collect();
    for cached in slot_refs.iter_mut() {
        *cached = None;
    }
    unbound
}

/// Coincide: unbind slots the ledger no longer holds, and the setup slot's
/// previous image, before it binds fresh. Pictures stay pending/held on those
/// flags ([`crate::images`]). A referenced slot must still bind after this.
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

fn slot_view(state: &SessionStateAv1, slot: u8) -> Option<vk::ImageView> {
    match &state.dpb {
        Some(dpb) => Some(dpb.dpb_view(slot)),
        None => state.slot_image[usize::from(slot)].map(|p| state.pool.pictures[p].view),
    }
}

/// One bound-slot entry. `-1` is the setup activation. No derived `Eq`: the Std
/// bindgen struct has none; tests compare the fields that matter.
#[derive(Debug, Clone, Copy)]
struct ScopeEntryAv1 {
    slot_index: i32,
    view: vk::ImageView,
    std: hh::StdVideoDecodeAV1ReferenceInfo,
}

/// Bound-slot list: `refs` in order (decode-op prefix), other held slots, then
/// setup as `-1`. Fail closed: a named slot with no image, or a name not in
/// `refs` (every non-negative `referenceNameSlotIndices` entry must equal some
/// `pReferenceSlots` `slotIndex`).
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
    for name in reference_name_slot_indices {
        if *name == REFERENCE_NAME_UNUSED {
            continue;
        }
        // Negative-but-not-UNUSED is not a slot. `u8::MAX` is past the nine-slot
        // ledger, so the refusal reads as "a name nothing binds".
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
            // Every held slot was a setup slot once.
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

/// Record one AV1 decode op and submit under the queue lock. Image waits per the
/// pool contract; dst timeline signals `signal_value`.
///
/// # Safety
///
/// Live device; `vk_plan` derived against this generation's `SlotMap`; `dst` a
/// free pool image; tiles resident in `upload`'s ring slot; the command buffer's
/// previous submission completed (caller waited its mark).
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
    // SAFETY: previous submission completed; pool allows per-buffer reset.
    unsafe {
        device
            .begin_command_buffer(cmd, &begin_info)
            .map_err(VkDecodeError::from)?
    };

    let memory_barriers = [vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
        .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        .dst_access_mask(
            vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
        )];
    // Fully overwritten: UNDEFINED discard plus a dep on earlier ops.
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
    // SAFETY: recording into the begun buffer; synchronization2 is enabled.
    unsafe { device.cmd_pipeline_barrier2(cmd, &dependency) };

    // Reset before the coding scope. Skip when the family has no status queries
    // (RADV: recording a query hangs VCN).
    if let Some(query_pool) = state.ops.query_pool {
        // SAFETY: recording; `query_index` is within the pool's count.
        unsafe { device.cmd_reset_query_pool(cmd, query_pool, query_index, 1) };
    }

    // Stage resources → std infos → dpb infos → slot infos. Each vector is
    // complete before the next borrows it, so nothing reallocates under a pointer.
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
    // Decode-op prefix: this frame's refs in plan order. Names are DPB slots,
    // not list indices; every named slot is in this prefix (`build_scope_av1`).
    let decode_refs: Vec<vk::VideoReferenceSlotInfoKHR<'_>> =
        begin_slots[..reference_count].to_vec();

    // Setup uses its real slot index; the begin list's twin entry carries -1.
    let setup_std = vk_plan.setup_ref;
    let mut setup_dpb = vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(&setup_std);
    let setup_resource = resources[scope.len() - 1];
    let setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(i32::from(vk_plan.setup_slot))
        .picture_resource(&setup_resource)
        .push_next(&mut setup_dpb);

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
    // Consume RESET here; re-arm if this command buffer never reaches the queue.
    let did_reset = state.session.take_needs_reset();
    // SAFETY: recording through end_command_buffer; pointed-to structs outlive
    // the calls; session/parameters handles are this generation's.
    let recorded: Result<(), vk::Result> = unsafe {
        (dev.video_queue().fp().cmd_begin_video_coding_khr)(cmd, &begin_coding);
        if did_reset {
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
    // SAFETY: decode queue is the device's; the guard externally synchronizes it.
    let result = unsafe { device.queue_submit2(dev.decode_queue(), &submits, vk::Fence::null()) };
    drop(guard);
    if let Err(e) = result {
        // Recorded RESET never executed; the next recording must redo it.
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

    fn std_ref(order_hint: u8, frame_type: u8) -> hh::StdVideoDecodeAV1ReferenceInfo {
        // SAFETY: Std bindgen struct; all-zero is valid for every field.
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

    /// Distinct fake view per slot (never dereferenced).
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    fn names(slots: &[u8]) -> [i32; 7] {
        let mut out = [REFERENCE_NAME_UNUSED; 7];
        for (name, slot) in slots.iter().enumerate() {
            out[name] = i32::from(*slot);
        }
        out
    }

    #[test]
    fn the_scopes_leading_entries_are_the_refs_in_plan_order() {
        // `refs` is first-appearance order, not slot/name order. Do not sort:
        // `pReferenceSlots` is exactly this prefix.
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
        let refs = vec![vk_ref(4, 10)];
        let slot_refs = vec![Some(std_ref(0, 1)); 9];
        let err = build_scope_av1(
            &refs,
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

    /// RADV reads 256 entries regardless of `tileCount`; the tail must be zero.
    /// `tileCount` is the real count — ash setters would write 256 from slice len.
    #[test]
    fn the_picture_info_carries_padded_tile_arrays_with_the_real_tile_count() {
        let packed = pack_av1_tiles(&[100..1000, 1000..1600, 1600..2100]).expect("fits u32");
        let tiles = submitted_tiles(&packed).expect("three tiles fit");
        assert_eq!(tiles.count, 3);
        assert_eq!(&tiles.offsets[..3], &[0, 900, 1500]);
        assert_eq!(&tiles.sizes[..3], &[900, 600, 500]);

        // SAFETY: Std bindgen struct; all-zero is valid; no pointer is read.
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
        // SAFETY: pointers from `tiles`/`std_pic`, alive here; arrays are
        // AV1_MAX_NUM_TILES long, the length this read uses.
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

        let std = std_ref(17, 1);
        let dpb_info = vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(&std);
        assert_eq!(
            dpb_info.s_type,
            vk::StructureType::VIDEO_DECODE_AV1_DPB_SLOT_INFO_KHR
        );
        // SAFETY: pointer taken from `std`, alive for this scope.
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

        // 256 tiles fit; one more is refused, not truncated.
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

    /// [`plan_bitstream`] vs the parser's independent `Tile::tile_offset` /
    /// `tile_size`. A whole-OBU or off-by-header split fails here.
    #[test]
    fn every_tile_of_the_vector_splits_to_the_parsers_own_offsets_and_sizes() {
        let mut planner = Av1Planner::new();
        // Second parser: `Cow::Borrowed` slices point into the packet, so offsets
        // are pointer differences — no re-walk of the same arithmetic.
        let mut reference = Parser::default();
        let (mut frames, mut tiles_checked) = (0u32, 0u32);
        let mut frame_obus = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
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
                        // Advance parser ref state or later inter frames fail.
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

    /// Slot holds tile payloads only. Offsets alone pass for whole-OBU upload too;
    /// slot length is what distinguishes the layouts.
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

    /// Hand-built two-tile group: the vendored vector is one tile per frame, so
    /// `tile_size_minus_1` / present-flag / header alignment need this.
    fn two_tile_group(
        flag_present: bool,
    ) -> (Vec<u8>, FrameHeaderObu, Vec<pf_bitstream::av1::TilePlan>) {
        let mut header = FrameHeaderObu::default();
        header.tile_info.tile_cols = 2;
        header.tile_info.tile_rows = 1;
        header.tile_info.tile_cols_log2 = 1;
        header.tile_info.tile_rows_log2 = 0;
        header.tile_info.tile_size_bytes = 2;

        // NumTiles > 1 codes the present flag. Clear: one padded bit. Set:
        // flag + tg_start/tg_end at 1 bit each = 3 bits, still one byte.
        let tg_header: u8 = if flag_present {
            // flag=1, tg_start=0, tg_end=1 from the MSB.
            0b1010_0000
        } else {
            0b0000_0000
        };
        let tile0 = [0xA1u8, 0xA2, 0xA3];
        let tile1 = [0xB1u8, 0xB2];
        let mut payload = vec![tg_header];
        // le(TileSizeBytes=2) of tile_size_minus_1 for every tile but the last.
        payload.extend_from_slice(&[(tile0.len() as u8) - 1, 0]);
        payload.extend_from_slice(&tile0);
        payload.extend_from_slice(&tile1);

        // obu_header: OBU_TILE_GROUP, no extension, has_size_field.
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
            // 2 OBU header + 1 tile-group header + 2 size bytes = 5.
            assert_eq!(ranges, vec![5..8, 8..10], "flag_present={flag_present}");
            assert_eq!(&au[ranges[0].clone()], &[0xA1, 0xA2, 0xA3]);
            assert_eq!(&au[ranges[1].clone()], &[0xB1, 0xB2]);
        }

        // Overshoot is refused. Undershoot cannot be: the last tile absorbs it.
        let (mut au, header, tiles) = two_tile_group(false);
        au[3] = 0x40; // tile_size_minus_1 = 64 ⇒ 65 bytes in an 8-byte payload
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Truncated { obu: 0 })
        );

        // TileSizeBytes is coded only for multi-tile; width 0 would read nothing.
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
        assert!(plan_bitstream(packet, &plan.tiles, &plan.header).is_ok());

        // Plan range shortened by one; bitstream `obu_size` still names the old
        // end. Last-tile size is implicit, so this is the only end check.
        let mut damaged = plan.tiles.clone();
        damaged[0].data.end -= 1;
        assert!(
            matches!(
                plan_bitstream(packet, &damaged, &plan.header),
                Err(Av1TileError::SizeMismatch { .. })
            ),
            "a range disagreeing with obu_size must be refused"
        );

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

        let mut no_tiles = (*plan.header).clone();
        no_tiles.tile_info.tile_cols = 0;
        assert_eq!(
            plan_bitstream(packet, &plan.tiles, &no_tiles),
            Err(Av1TileError::NoTiles)
        );
    }

    #[test]
    fn a_leb128_without_a_terminator_is_refused_rather_than_read_forever() {
        // Spec caps leb128() at eight continuation bytes.
        let au = [0x80u8; 16];
        assert_eq!(leb128(&au, 0), None);
        let au = [0x81u8, 0x02];
        assert_eq!(leb128(&au, 0), Some((0x101, 2)));
        assert_eq!(leb128(&[0x80], 0), None);
        assert_eq!(leb128(&[], 0), None);
    }

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

        let key = profile_key_for(&plan).expect("inside the envelope");
        assert_eq!(key.output_format(), Some(crate::caps::NV12));
        assert!(!key.film_grain);

        // Operating point 0; this vector is a real Std level, not the 31 sentinel.
        assert!(stream_level_idx(&plan) <= 23);
    }

    /// `seq_level_idx` 31 is Annex A "maximum parameters", not a level above 7.3.
    /// `StdVideoAV1Level` stops at 23; 31 > 23 is not "too demanding".
    #[test]
    fn the_av1_max_parameters_sentinel_is_not_a_level_above_the_ceiling() {
        let ceiling = crate::caps::MaxLevelIdc::Av1(hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_7_3);
        assert_eq!(ceiling.code_point(), 23, "the Std enum's top code point");

        for idx in 0..=ceiling.code_point() {
            assert!(idx <= ceiling.code_point());
        }
        // 24..30 reserved; 31 is "maximum parameters". Not a capability test.
        for idx in (ceiling.code_point() + 1)..=31 {
            assert!(
                idx > ceiling.code_point(),
                "seq_level_idx {idx} is outside the Std range, not a more demanding level"
            );
        }

        assert!(31 > ceiling.code_point());
        assert_eq!(format!("{ceiling}"), "AV1 Std level 23");
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

        // `show_existing_frame` of a key must not resume (full store, empty ledger).
        let mut shown = first.clone();
        shown.dpb.stored = None;
        assert!(shown.picture.is_key);
        assert!(!clears_awaiting_key(&shown));

        let inter = planner
            .plan_au(IvfIterator::new(AV1_25FPS).nth(1).expect("a second packet"))
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");
        assert!(!inter.picture.is_key);
        assert!(!clears_awaiting_key(&inter));
    }

    /// Skip is per frame; error is per AU. `Ok(None)` would reset demotion.
    /// `planned == 0` is metadata, not a wait.
    #[test]
    fn a_unit_reports_the_key_frame_wait_only_when_it_decoded_nothing_at_all() {
        assert!(whole_unit_skipped(1, 1), "a single-frame unit");
        assert!(whole_unit_skipped(2, 2), "and a two-frame one");

        // Key behind a skip in the same unit: an early return would miss it.
        assert!(!whole_unit_skipped(2, 1));
        assert!(!whole_unit_skipped(3, 2));
        assert!(!whole_unit_skipped(2, 0));
        assert!(!whole_unit_skipped(0, 0));
    }

    /// Wait error must not share text with the loss that latched recovery.
    #[test]
    fn the_key_frame_wait_names_itself_in_the_error_text() {
        let waiting = format!("{}", VkDecodeError::AwaitingKeyAv1);
        assert!(waiting.contains("key frame"), "{waiting}");
        assert!(waiting.contains("skipped"), "{waiting}");
        let lost = format!(
            "{}",
            VkDecodeError::MissingReferenceAv1 {
                slot: 3,
                ref_index: 2
            }
        );
        assert_ne!(waiting, lost);
    }

    /// This vector never hits `refresh_frame_flags == 0`.
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

    /// Production [`lost_reference`], not a re-implemented `find_map`.
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

        // TruncatedAu is concealment the planner already applied.
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

    /// Clean vector must not trip [`lost_reference`], or every frame would refuse.
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

    /// Vector through convert + [`sync_slot_bindings`] + [`build_scope_av1`].
    /// A referenced slot must still bind the image it was decoded into — bound
    /// to the setup picture is also wrong, and silent on the GPU.
    #[test]
    fn slot_recycling_waits_for_the_decode_op() {
        #[derive(Clone, Default)]
        struct SimPicture {
            bound: bool,
            pending: bool,
            held: u32,
        }
        // Distinct view per pool image (never dereferenced).
        let image_view = |picture: usize| vk::ImageView::from_raw(picture as u64 + 1);

        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut slot_image: Vec<Option<usize>> = vec![None; REQUIRED_SLOTS as usize];
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>> =
            vec![None; REQUIRED_SLOTS as usize];
        let mut pictures =
            vec![SimPicture::default(); (REQUIRED_SLOTS + crate::images::HOLD_HEADROOM) as usize];
        let mut pending: BTreeMap<PicId, usize> = BTreeMap::new();
        let mut image_of: BTreeMap<PicId, usize> = BTreeMap::new();

        let (mut frames, mut deferring, mut scope_refs) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                let Some(setup_id) = plan.dpb.stored else {
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

                // Held slots except setup must bind, or the scope drops them.
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

                // Held slots once each, plus setup as `-1`. Setup is held: assigned.
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

                // Bound to the setup picture is still bound — to the wrong image.
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
        // 268 of 274 displace a picture they still read. Zero means the asserts
        // above compare empty lists.
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
