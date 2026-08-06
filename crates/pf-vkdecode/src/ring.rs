//! Host-visible bitstream upload ring: one persistent-mapped `VIDEO_DECODE_SRC`
//! buffer cut into equal slots, honouring the profile's
//! `minBitstreamBufferOffsetAlignment`/`SizeAlignment`.
//!
//! Split like the rest of the crate: [`RingLayout`] + [`SlotStates`] are the pure,
//! unit-tested halves (offset/alignment math including growth, and the recycle
//! bookkeeping); [`BitstreamRing`] is the thin Vulkan half that allocates the
//! buffer and copies AU bytes. Slots recycle when the timeline value of the submit
//! that consumed them completes; an AU larger than the slot size grows the ring by
//! RECREATING the buffer (after draining every in-flight slot) — growth is rare
//! (an IDR burst outsizing the initial slots) and a stall there beats permanently
//! oversized slots.

use ash::vk;
use tracing::debug;

use crate::caps::DecodeProfile;
use crate::device::find_memory_type;
use crate::device::AllocError;
use crate::device::DecodeDevice;

/// Initial per-slot capacity. Sized for comfort at streaming bitrates (a 4K IDR at
/// punktfunk rates is a few hundred KiB); the ring grows on first contact with a
/// larger AU rather than pre-reserving worst cases.
pub const INITIAL_SLOT_SIZE: u64 = 2 * 1024 * 1024;
/// Slot count: enough to keep uploads ahead of a couple of in-flight decodes; the
/// pipeline depth itself is bounded by the output/query rings, not by this.
pub const RING_SLOTS: u32 = 4;

/// `x` rounded up to a multiple of power-of-two `align`.
const fn align_up(x: u64, align: u64) -> u64 {
    (x + align - 1) & !(align - 1)
}

/// Pure geometry of the ring buffer. Both Vulkan alignments are powers of two per
/// the spec's alignment-value convention, which [`RingLayout::new`] debug-asserts;
/// the slot size is a multiple of BOTH, so every slot offset satisfies the offset
/// alignment and every full-slot range satisfies the size alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    pub slot_size: u64,
    pub slots: u32,
    pub offset_alignment: u64,
    pub size_alignment: u64,
}

impl RingLayout {
    pub fn new(min_slot_size: u64, slots: u32, offset_alignment: u64, size_alignment: u64) -> Self {
        debug_assert!(
            offset_alignment.is_power_of_two() && size_alignment.is_power_of_two(),
            "Vulkan alignment values are powers of two"
        );
        debug_assert!(slots > 0 && min_slot_size > 0);
        let align = offset_alignment.max(size_alignment);
        Self {
            slot_size: align_up(min_slot_size, align),
            slots,
            offset_alignment,
            size_alignment,
        }
    }

    /// Byte offset of `slot` — a `minBitstreamBufferOffsetAlignment` multiple by
    /// construction.
    pub fn offset_of(&self, slot: u32) -> u64 {
        debug_assert!(slot < self.slots);
        u64::from(slot) * self.slot_size
    }

    /// Whether an AU of `len` bytes fits one slot (its aligned range included).
    pub fn fits(&self, len: u64) -> bool {
        self.record_range(len) <= self.slot_size
    }

    /// The `srcBufferRange` to record for an AU of `len` bytes: the length rounded
    /// up to `minBitstreamBufferSizeAlignment`.
    pub fn record_range(&self, len: u64) -> u64 {
        align_up(len, self.size_alignment)
    }

    /// Total buffer size.
    pub fn buffer_size(&self) -> u64 {
        self.slot_size * u64::from(self.slots)
    }

    /// The layout a recreation adopts so an AU of `len` bytes fits with headroom:
    /// slot size doubles from the current one until sufficient (geometric growth —
    /// one recreation per size class, not one per oversized AU).
    pub fn grown_for(&self, len: u64) -> Self {
        let mut slot = self.slot_size.max(1);
        while align_up(len, self.size_alignment) > slot {
            slot *= 2;
        }
        Self::new(slot, self.slots, self.offset_alignment, self.size_alignment)
    }
}

/// One AU's slice NALUs as they will actually sit in a ring slot: the AU byte
/// ranges to concatenate, and the offsets those ranges land at.
///
/// The two are produced TOGETHER by [`pack_slices`] and consumed together
/// ([`BitstreamRing::upload`] writes `segments`, the recording layer submits
/// `offsets`) precisely because they cannot be allowed to disagree: an offset
/// that does not land on the byte the segment starts at points the hardware at
/// the middle of somebody else's slice, which is silent corruption rather than
/// an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackedSlices {
    /// The AU ranges to concatenate, each starting at a THREE-byte Annex-B start
    /// code (see [`three_byte_prefix`]).
    pub(crate) segments: Vec<std::ops::Range<usize>>,
    /// The `pSliceOffsets` / `pSliceSegmentOffsets` those ranges land at once
    /// concatenated — one per segment, in the same order.
    pub(crate) offsets: Vec<u32>,
}

/// `segment` with any Annex-B `zero_byte`s ahead of its start code dropped, so it
/// begins at exactly `00 00 01`.
///
/// **This is load-bearing, not tidying.** Annex-B admits both a three-byte start
/// code and a four-byte one (a `zero_byte` ahead of it, B.1.2/B.2.2) and leaves the
/// choice to the encoder, which is exactly the trap: the vendored H.265 vector
/// carries `00 00 00 01` on 249 of its 250 slice segments, while its H.264 twin
/// carries `00 00 01` on all 500 of its. Both planners hand out ranges that begin at
/// whichever form the stream happened to use, so WITHOUT this normalisation the
/// codec that works and the codec that corrupts are decided by the encoder that
/// produced the file — which is precisely how this shipped: the H.264 rung was
/// bit-exact on 250/250 frames while H.265 failed on 247/250, through the same ring.
/// NOTHING structural protects H.264; it is latently exposed to any stream whose
/// encoder prefixes slices with four bytes (NVENC and AMF among them).
///
/// Vulkan's slice offsets, however, are consumed by drivers written against
/// libavcodec's `ff_vk_decode_add_slice`, which DISCARDS the stream's prefix and
/// writes its own `{ 0x00, 0x00, 0x01 }` before each slice, pointing the offset at
/// that. A three-byte prefix at the offset is therefore the only byte pattern any
/// driver has been validated on, and NVIDIA's takes it literally: it reads the
/// slice segment header at `offset + 3 + 2` (prefix plus the two-byte H.265 NAL
/// unit header) rather than scanning for the prefix. A four-byte prefix shifts its
/// bit reader one byte early — onto the NAL header's SECOND byte — and every
/// syntax element it decodes afterwards is garbage; the driver says so
/// (`Invalid PPS/SPS id in slice header (pps_id=115)`, 115 and 119 being what
/// `nuh_temporal_id_plus1 = 1` followed by this vector's two slice-header first
/// bytes decode to as `ue(v)`).
///
/// The sibling DXVA packer (`pf-dxvadec`'s `pack`) normalises to three bytes for
/// exactly this reason and says so; the Vulkan path did not, which is the bug this
/// function fixes. Dropping leading zeros achieves the same normalisation as
/// FFmpeg's rewrite without a second copy: the prefix shrinks to `00 00 01` and the
/// NALU behind it is untouched.
///
/// A range that is not a start code at all (a hand-built plan) is returned
/// unchanged — the loop only ever drops a zero that is followed by two more zeros,
/// so it can never eat into `00 00 01` itself.
fn three_byte_prefix(au: &[u8], segment: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    let mut start = segment.start;
    while segment.end - start > 3 && au[start..start + 3] == [0, 0, 0] {
        start += 1;
    }
    start..segment.end
}

/// How `segments` of `au` pack into one ring slot: prefixes normalised, offsets
/// rebased out of AU coordinates.
///
/// The rebase exists because the bitstream buffer carries the SLICE NALUs ONLY
/// (module docs: non-VCL NALUs in the submitted range hang VCN firmware), while
/// both planners hand out AU-RELATIVE offsets. Submitting the plan's offsets
/// unchanged would point the hardware at bytes that were never uploaded — for
/// H.265 that is the whole reason [`crate::DecodePlanVkH265::slice_offsets`]
/// documents itself as "NOT submission-final". Both codecs' recording paths call
/// this, so the packing is written (and tested) once.
///
/// Offsets are `u32` because Vulkan's are; a packed AU large enough to overflow
/// one cannot fit any ring slot this crate allocates (4 GiB of slice data), and
/// the sum is taken in `u64` so the check is real rather than a wrapped compare.
pub(crate) fn pack_slices(au: &[u8], segments: &[std::ops::Range<usize>]) -> Option<PackedSlices> {
    let mut packed = Vec::with_capacity(segments.len());
    let mut offsets = Vec::with_capacity(segments.len());
    let mut cursor: u64 = 0;
    for segment in segments {
        let segment = three_byte_prefix(au, segment);
        offsets.push(u32::try_from(cursor).ok()?);
        cursor += segment.len() as u64;
        packed.push(segment);
    }
    Some(PackedSlices {
        segments: packed,
        offsets,
    })
}

/// One AU's AV1 tile payloads as they will sit in a ring slot: the AU byte ranges
/// to concatenate, and the offset each lands at.
///
/// [`PackedSlices`]' AV1 twin, and a separate type rather than a flag because the
/// two differ in exactly the thing that must never be confused: an Annex-B slice
/// gets its start-code prefix NORMALISED ([`three_byte_prefix`]) and an AV1 tile
/// must not be touched at all. AV1 has no start codes — a tile payload is entropy-
/// coded bytes that may legitimately begin `00 00 00`, and trimming those would
/// silently shorten the tile the driver decodes.
///
/// The segments are the RAW TILE PAYLOADS, not the OBUs that carried them: the
/// bitstream buffer holds nothing else (see [`crate::decoder_av1`]), so `offsets[i]`
/// is directly tile `i`'s `pTileOffsets` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackedAv1Tiles {
    /// The AU ranges to concatenate, verbatim and in order.
    pub(crate) segments: Vec<std::ops::Range<usize>>,
    /// The offset each range lands at once concatenated — one per segment, in the
    /// same order.
    pub(crate) offsets: Vec<u32>,
}

/// How AV1 `tiles` of `au` pack into one ring slot: verbatim, with the offset each
/// lands at.
///
/// The offsets exist for the reason [`pack_slices`]' do — the plan's ranges are
/// AU-relative and the buffer holds only what was uploaded — but the packing itself
/// is a plain concatenation: see [`PackedAv1Tiles`] for why no prefix normalisation
/// happens (or may happen) here.
///
/// Offsets are `u32` because Vulkan's are; a packed AU large enough to overflow
/// one cannot fit any ring slot this crate allocates, and the sum is taken in
/// `u64` so the check is real rather than a wrapped compare.
pub(crate) fn pack_av1_tiles(tiles: &[std::ops::Range<usize>]) -> Option<PackedAv1Tiles> {
    let mut offsets = Vec::with_capacity(tiles.len());
    let mut cursor: u64 = 0;
    for tile in tiles {
        offsets.push(u32::try_from(cursor).ok()?);
        cursor += tile.len() as u64;
    }
    // The END of the last segment must also be expressible: `pTileSizes` and the
    // recorded `srcBufferRange` are read against it.
    u32::try_from(cursor).ok()?;
    Some(PackedAv1Tiles {
        segments: tiles.to_vec(),
        offsets,
    })
}

/// Concatenate `segments` of `au` into `dst`, zeroing whatever is left of it.
///
/// The zero tail matters: `dst` is a whole recorded `srcBufferRange` (the packed
/// length rounded up to `minBitstreamBufferSizeAlignment`), so without it the
/// driver would be handed the previous AU's bytes past this one's end.
///
/// Shared with [`BitstreamRing::upload`] rather than inlined there so the CPU
/// tests assert against the bytes the ring ACTUALLY receives.
///
/// # Panics
///
/// If `segments` are not in-bounds ranges of `au`, or their total length exceeds
/// `dst` — both caller invariants [`BitstreamRing::upload`] establishes from the
/// layout (and a panic beats a wild write either way).
pub(crate) fn pack_into(dst: &mut [u8], au: &[u8], segments: &[std::ops::Range<usize>]) {
    let mut cursor = 0usize;
    for segment in segments {
        let bytes = &au[segment.clone()];
        dst[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        cursor += bytes.len();
    }
    dst[cursor..].fill(0);
}

/// Pure recycle bookkeeping: which slots are free, which carry an in-flight token.
/// Generic over the token so the FIFO/recycle behaviour is testable without a
/// device (the ring instantiates `T = (vk::Semaphore, u64)`).
#[derive(Debug)]
pub(crate) struct SlotStates<T> {
    pending: Vec<Option<T>>,
    /// Round-robin cursor: slots are handed out in order, so the slot AT the
    /// cursor is always the oldest in-flight one — the right one to wait on.
    cursor: usize,
}

impl<T> SlotStates<T> {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            pending: (0..slots).map(|_| None).collect(),
            cursor: 0,
        }
    }

    /// Acquire the next slot in round-robin order. `is_done` is consulted when the
    /// slot still carries a token (`Ok(true)` frees it); returning `Ok(false)`
    /// yields `Ok(None)` — the caller then waits on [`Self::oldest`]'s token and
    /// retries. Errors pass through untouched.
    pub(crate) fn acquire<E>(
        &mut self,
        mut is_done: impl FnMut(&T) -> Result<bool, E>,
    ) -> Result<Option<usize>, E> {
        let slot = self.cursor;
        if let Some(token) = &self.pending[slot] {
            if !is_done(token)? {
                return Ok(None);
            }
            self.pending[slot] = None;
        }
        self.cursor = (self.cursor + 1) % self.pending.len();
        Ok(Some(slot))
    }

    /// The oldest in-flight token (the one blocking [`Self::acquire`]), if any.
    pub(crate) fn oldest(&self) -> Option<&T> {
        self.pending[self.cursor].as_ref()
    }

    /// Record `token` as `slot`'s in-flight use.
    pub(crate) fn set_pending(&mut self, slot: usize, token: T) {
        debug_assert!(
            self.pending[slot].is_none(),
            "slot handed out while pending"
        );
        self.pending[slot] = Some(token);
    }

    /// All in-flight tokens (drain-before-recreate walks these).
    pub(crate) fn in_flight(&self) -> impl Iterator<Item = &T> {
        self.pending.iter().filter_map(Option::as_ref)
    }

    /// Forget every token (after the caller has drained them).
    pub(crate) fn clear(&mut self) {
        for p in &mut self.pending {
            *p = None;
        }
        self.cursor = 0;
    }
}

/// One uploaded AU: what `vkCmdDecodeVideoKHR` needs plus the slot to mark pending
/// once the submit's timeline token exists.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UploadedAu {
    pub offset: u64,
    pub range: u64,
    pub slot: usize,
}

/// The in-flight token a used slot waits on: a timeline (semaphore, value) pair —
/// the same pair the submit that consumed the slot signalled.
pub(crate) type Token = (vk::Semaphore, u64);

/// The Vulkan half: buffer + memory + persistent map. Created against the session's
/// video profile (the spec requires the src buffer to be profile-listed).
pub(crate) struct BitstreamRing {
    device: ash::Device,
    layout: RingLayout,
    profile: DecodeProfile,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    pub(crate) pending: SlotStates<Token>,
}

impl BitstreamRing {
    /// Allocate the buffer for `layout`.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        layout: RingLayout,
        profile: DecodeProfile,
    ) -> Result<Self, AllocError> {
        // SAFETY: live device; allocate_backing only creates objects it returns.
        let (buffer, memory, ptr) = unsafe { Self::allocate_backing(dev, &layout, profile)? };
        Ok(Self {
            device: dev.ash().clone(),
            layout,
            profile,
            buffer,
            memory,
            ptr,
            pending: SlotStates::new(layout.slots as usize),
        })
    }

    pub(crate) fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// # Safety
    ///
    /// As [`Self::create`].
    unsafe fn allocate_backing(
        dev: &DecodeDevice,
        layout: &RingLayout,
        decode_profile: DecodeProfile,
    ) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), AllocError> {
        let mut chain = decode_profile.chain();
        let profile = chain.wire();
        let mut profile_list =
            vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
        let ci = vk::BufferCreateInfo::default()
            .size(layout.buffer_size())
            .usage(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut profile_list);
        // SAFETY: live device; `ci` roots a chain of locals outliving the call.
        let buffer = unsafe { dev.ash().create_buffer(&ci, None)? };
        // SAFETY: `buffer` was just created on this device.
        let req = unsafe { dev.ash().get_buffer_memory_requirements(buffer) };
        let mem_props = dev.memory_properties();
        let type_index = match find_memory_type(
            &mem_props,
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        ) {
            Ok(index) => index,
            Err(e) => {
                // SAFETY: destroying the just-created, never-bound buffer.
                unsafe { dev.ash().destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(type_index);
        // SAFETY: live device; on failure the buffer is destroyed before returning
        // so nothing leaks.
        let memory = match unsafe { dev.ash().allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: destroying the just-created, never-bound buffer.
                unsafe { dev.ash().destroy_buffer(buffer, None) };
                return Err(e.into());
            }
        };
        // SAFETY: fresh buffer + fresh memory of at least the required size.
        if let Err(e) = unsafe { dev.ash().bind_buffer_memory(buffer, memory, 0) } {
            // SAFETY: unwinding the two objects created above (unbound/unused).
            unsafe {
                dev.ash().destroy_buffer(buffer, None);
                dev.ash().free_memory(memory, None);
            }
            return Err(e.into());
        }
        // SAFETY: `memory` is HOST_VISIBLE and unmapped; WHOLE_SIZE maps its full
        // range for the buffer's lifetime (vkFreeMemory implicitly unmaps).
        let ptr = match unsafe {
            dev.ash()
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
        } {
            Ok(p) => p.cast::<u8>(),
            Err(e) => {
                // SAFETY: unwinding the two objects created above.
                unsafe {
                    dev.ash().destroy_buffer(buffer, None);
                    dev.ash().free_memory(memory, None);
                }
                return Err(e.into());
            }
        };
        Ok((buffer, memory, ptr))
    }

    /// Upload one AU, recycling or growing as needed.
    ///
    /// `poll`/`wait` bridge to the caller's timeline-semaphore facts: `poll`
    /// answers "has this token completed?" without blocking; `wait` blocks until
    /// it has (bounded by the caller's timeout policy). The split keeps this
    /// module free of any semaphore knowledge.
    ///
    /// `segments` are the byte ranges of `au` to upload, CONCATENATED — the
    /// decoder passes the SLICE NALUs only. The buffer must contain nothing but
    /// slice data: the VCN firmware scans the submitted range itself, and
    /// non-slice NALUs (AUD/SEI/SPS/PPS, which real AUs open with) in the range
    /// hang it — the 2026-08 .25 `vcn_unified_0 ring timeout`. FFmpeg's decoder
    /// feeds slices-only for the same reason; parameter sets ride the session
    /// parameters object instead.
    ///
    /// They must be [`pack_slices`]' output, not the plan's raw ranges: the
    /// offsets the recording layer submits are computed from the same call, and
    /// the start-code normalisation there is what keeps the driver's slice-header
    /// parse in step with the bytes ([`three_byte_prefix`]).
    ///
    /// # Safety
    ///
    /// Live device (contract); `segments` are in-bounds ranges of `au`; and the
    /// tokens passed to prior [`SlotStates::set_pending`] calls genuinely cover
    /// every GPU read of their slots — recycling rewrites slot bytes as soon as a
    /// token reports done.
    pub(crate) unsafe fn upload<E: From<AllocError>>(
        &mut self,
        dev: &DecodeDevice,
        au: &[u8],
        segments: &[std::ops::Range<usize>],
        poll: &mut dyn FnMut(&Token) -> Result<bool, E>,
        wait: &mut dyn FnMut(&Token) -> Result<(), E>,
    ) -> Result<UploadedAu, E> {
        let len: u64 = segments.iter().map(|s| s.len() as u64).sum();
        if !self.layout.fits(len) {
            // Grow: drain EVERYTHING in flight (their reads target the old buffer),
            // then recreate the backing under the grown layout.
            for token in self.pending.in_flight() {
                wait(token)?;
            }
            self.pending.clear();
            let grown = self.layout.grown_for(len);
            debug!(
                old = self.layout.slot_size,
                new = grown.slot_size,
                au = len,
                "bitstream ring grows for an oversized AU"
            );
            // SAFETY: every in-flight read was drained above; destroy_backing only
            // touches this ring's own objects.
            unsafe { self.destroy_backing() };
            // SAFETY: caller's live-device contract.
            let (buffer, memory, ptr) =
                unsafe { Self::allocate_backing(dev, &grown, self.profile)? };
            self.layout = grown;
            self.buffer = buffer;
            self.memory = memory;
            self.ptr = ptr;
            self.pending = SlotStates::new(grown.slots as usize);
        }

        let slot = match self.pending.acquire(&mut *poll)? {
            Some(slot) => slot,
            None => {
                // The oldest slot is still in flight: wait it out, then retry —
                // guaranteed to succeed now.
                if let Some(token) = self.pending.oldest() {
                    wait(token)?;
                }
                self.pending
                    .acquire(|_| Ok(true))?
                    .expect("the waited slot is free")
            }
        };

        let offset = self.layout.offset_of(slot as u32);
        let range = self.layout.record_range(len);
        // SAFETY: `ptr` is the live persistent mapping of a buffer of
        // `layout.buffer_size()` bytes; `offset + range <= buffer_size` because
        // `range <= slot_size` (fits/grown above) and offset is `slot * slot_size`
        // with `slot < slots`, so the `range` bytes from `offset` are one whole
        // initialized, aliasing-free slot of the mapping. The slot is not
        // concurrently read: its previous use completed (poll/wait above) and its
        // next use is submitted after this copy.
        let slot_bytes = unsafe {
            std::slice::from_raw_parts_mut(self.ptr.add(offset as usize), range as usize)
        };
        // Segments are in-bounds ranges of `au` (fn contract) summing to `len`,
        // and `len <= range` (fits/grown above), so `pack_into` cannot panic.
        pack_into(slot_bytes, au, segments);
        Ok(UploadedAu {
            offset,
            range,
            slot,
        })
    }

    /// Destroy buffer + memory (which implicitly unmaps). Callers must have
    /// drained in-flight reads first.
    ///
    /// # Safety
    ///
    /// Live device; no submitted-and-unfinished GPU work reads the buffer.
    unsafe fn destroy_backing(&mut self) {
        // SAFETY: the fn-level contract — objects are this ring's own, reads drained.
        unsafe {
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
        self.buffer = vk::Buffer::null();
        self.memory = vk::DeviceMemory::null();
        self.ptr = std::ptr::null_mut();
    }
}

impl Drop for BitstreamRing {
    fn drop(&mut self) {
        if self.buffer == vk::Buffer::null() {
            return;
        }
        // SAFETY: the owning decoder drains its queue before dropping state (and the
        // borrowed device is alive by the DeviceHandles liveness contract).
        unsafe { self.destroy_backing() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_offsets_and_ranges_honour_both_alignments() {
        // Deliberately DIFFERENT alignments: offset 256, size 64.
        let layout = RingLayout::new(1000, 4, 256, 64);
        // Slot size rounds up to a multiple of max(256, 64).
        assert_eq!(layout.slot_size, 1024);
        for slot in 0..4 {
            assert_eq!(layout.offset_of(slot) % 256, 0, "offset alignment");
        }
        assert_eq!(layout.buffer_size(), 4096);
        // Ranges round to the SIZE alignment, independent of the offset one.
        assert_eq!(layout.record_range(1), 64);
        assert_eq!(layout.record_range(64), 64);
        assert_eq!(layout.record_range(65), 128);
        assert!(layout.fits(1024));
        assert!(!layout.fits(1025));
    }

    #[test]
    fn growth_doubles_the_slot_size_until_the_au_fits_and_keeps_alignment() {
        let layout = RingLayout::new(1024, 4, 128, 128);
        let grown = layout.grown_for(5000);
        assert_eq!(grown.slot_size, 8192, "1024 → 2048 → 4096 → 8192");
        assert_eq!(grown.slots, 4);
        assert!(grown.fits(5000));
        assert_eq!(grown.offset_of(3) % 128, 0);

        // An AU already fitting changes nothing.
        assert_eq!(layout.grown_for(512), layout);

        // The aligned RANGE drives growth, not the raw length: a 1025-byte AU has
        // a 1152-byte range under a 128 alignment and needs the next size up.
        assert_eq!(layout.grown_for(1025).slot_size, 2048);
    }

    #[test]
    fn one_byte_alignments_degenerate_cleanly() {
        let layout = RingLayout::new(100, 2, 1, 1);
        assert_eq!(layout.slot_size, 100);
        assert_eq!(layout.record_range(37), 37);
        assert!(layout.fits(100));
        assert!(!layout.fits(101));
    }

    #[test]
    fn slots_recycle_in_fifo_order_only_after_their_token_completes() {
        let mut states: SlotStates<u64> = SlotStates::new(2);
        let s0 = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
        states.set_pending(s0, 10);
        let s1 = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
        states.set_pending(s1, 11);
        assert_ne!(s0, s1);

        // Ring full, oldest (slot 0, token 10) not done: acquire yields None and
        // names the token to wait on.
        assert_eq!(states.acquire(|&t| Ok::<_, ()>(t > 10)).unwrap(), None);
        assert_eq!(states.oldest(), Some(&10));

        // Once done, the OLDEST slot is the one handed back (FIFO, not LIFO).
        let s2 = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
        assert_eq!(s2, s0);

        // Errors from the completion probe pass through untouched.
        states.set_pending(s2, 12);
        assert_eq!(states.acquire(|_| Err("gpu gone")).unwrap_err(), "gpu gone");
    }

    #[test]
    fn slice_offsets_rebase_out_of_au_coordinates_into_the_packed_slot() {
        // A realistic AU: AUD at 0, SPS/PPS, then two slice NALUs at 40 and 900,
        // both with three-byte prefixes (so nothing is trimmed here). Only the
        // slices are uploaded, so their PACKED offsets are 0 and (900 - 40) =
        // 860's worth of the first slice's own length — never the AU ones.
        let mut au = vec![0xAAu8; 1500];
        au[40..43].copy_from_slice(&[0, 0, 1]);
        au[900..903].copy_from_slice(&[0, 0, 1]);
        let segments = [40..900, 900..1500];
        let packed = pack_slices(&au, &segments).unwrap();
        assert_eq!(packed.offsets, vec![0, 860]);
        assert_eq!(packed.segments, segments);

        // A single-slice AU always records offset 0, whatever the AU offset was.
        // (Via `from_ref`: a one-element array literal of a range reads to clippy
        // as a mis-typed range-fill, and the lint is right to say so.)
        let single = 400usize..1000;
        assert_eq!(
            pack_slices(&au, std::slice::from_ref(&single))
                .unwrap()
                .offsets,
            vec![0]
        );
        // No slices, no offsets (the callers reject empty plans before this).
        assert_eq!(pack_slices(&au, &[]).unwrap().offsets, Vec::<u32>::new());

        // Three segments accumulate by LENGTH, not by AU position (a gap between
        // slice 1 and 2 — an SEI mid-AU — must not shift the third offset).
        let segments = [0..100, 500..600, 1000..1100];
        assert_eq!(
            pack_slices(&au, &segments).unwrap().offsets,
            vec![0, 100, 200]
        );
    }

    #[test]
    fn a_four_byte_annex_b_prefix_is_trimmed_to_three_and_the_offsets_follow() {
        // Two slices, the first with the four-byte prefix real encoders put on the
        // first NALU of an access unit, the second with a three-byte one. Both must
        // land on `00 00 01`, and — the part a separate `rebased_offsets` call got
        // wrong by construction — the SECOND offset must count the FIRST slice's
        // trimmed length, not its AU length.
        let mut au = vec![0xAAu8; 200];
        au[0..4].copy_from_slice(&[0, 0, 0, 1]);
        au[100..103].copy_from_slice(&[0, 0, 1]);
        let packed = pack_slices(&au, &[0..100, 100..200]).unwrap();
        assert_eq!(packed.segments, vec![1..100, 100..200]);
        assert_eq!(packed.offsets, vec![0, 99], "99, not 100");

        let mut slot = vec![0xFFu8; 256];
        pack_into(&mut slot, &au, &packed.segments);
        for (offset, segment) in packed.offsets.iter().zip(&packed.segments) {
            let at = &slot[*offset as usize..];
            assert_eq!(
                at[..3],
                [0, 0, 1],
                "the packed slice at offset {offset} must open with a THREE-byte \
                 start code, or a driver reaching the slice header by a fixed \
                 `+3 +2` skip lands a byte early"
            );
            assert_eq!(&at[..segment.len()], &au[segment.clone()]);
        }
        // And the alignment tail is zeroed, never a previous AU's bytes.
        let packed_len: usize = packed.segments.iter().map(|s| s.len()).sum();
        assert!(slot[packed_len..].iter().all(|&b| b == 0));

        // Annex-B allows more than one leading zero byte; all of them go.
        let mut au = vec![0xAAu8; 64];
        au[0..6].copy_from_slice(&[0, 0, 0, 0, 0, 1]);
        assert_eq!(
            pack_slices(&au, std::slice::from_ref(&(0usize..64)))
                .unwrap()
                .segments,
            vec![3..64]
        );

        // A range that is not a start code at all (a hand-built plan) is left
        // exactly as it came: the trim only ever drops a zero followed by two more.
        let au = vec![0x42u8; 32];
        assert_eq!(
            pack_slices(&au, std::slice::from_ref(&(0usize..32)))
                .unwrap()
                .segments,
            vec![0..32]
        );
    }

    /// The regression test for the M3 HEVC field failure: what the ring ACTUALLY
    /// receives for every AU of the vendored vectors, checked against the streams'
    /// own facts rather than against a golden the same code produced.
    ///
    /// It exists because nothing CPU-side checked the submitted bytes against the
    /// offsets that describe them, and the consequence was invisible without a GPU:
    /// 249 of this vector's 250 slice segments carry a four-byte Annex-B prefix, so
    /// every offset pointed a `+3 +2`-skipping driver at the second byte of the NAL
    /// unit header instead of the slice header, and NVIDIA answered with
    /// `Invalid PPS/SPS id in slice header (pps_id=115)` on every AU.
    ///
    /// Both codecs run the same assertions on purpose. H.264's vector happens to
    /// carry three-byte prefixes on all 500 of its slices — an encoder convention,
    /// not a structural guarantee — which is exactly why it never tripped this and
    /// why it CANNOT serve as the canary. Its leg here is the guard that the
    /// normalisation stays a no-op where nothing needs normalising; the four-byte
    /// case for both codecs is covered by
    /// [`super::tests::a_four_byte_annex_b_prefix_is_trimmed_to_three_and_the_offsets_follow`],
    /// which is codec-neutral for the same reason.
    #[test]
    fn every_packed_slice_of_both_vendored_vectors_opens_at_its_own_nal_header() {
        // H.265: three-byte prefix, then the TWO-byte NAL unit header
        // (forbidden_zero_bit 0, nal_unit_type < 32 for a VCL NALU, nuh_layer_id 0),
        // then the slice segment header — whose first bit is
        // first_slice_segment_in_pic_flag.
        let mut planner = pf_bitstream::h265::H265Planner::new();
        let mut aus = 0usize;
        for au in split_h265_aus(TEST_25FPS_H265) {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let plan_segments: Vec<std::ops::Range<usize>> =
                plan.slices.iter().map(|s| s.data.clone()).collect();
            let packed = pack_slices(au, &plan_segments).expect("offsets fit u32");
            assert_eq!(packed.offsets.len(), plan.slices.len(), "one per segment");
            let slot = packed_slot(au, &packed);
            for (index, offset) in packed.offsets.iter().enumerate() {
                let at = &slot[*offset as usize..];
                assert_eq!(
                    at[..3],
                    [0, 0, 1],
                    "AU {aus} segment {index}: a three-byte start code"
                );
                assert_eq!(at[3] & 0x80, 0, "AU {aus} segment {index}: forbidden_zero");
                let nal_type = (at[3] >> 1) & 0x3F;
                assert!(
                    nal_type < 32,
                    "AU {aus} segment {index}: VCL type, not {nal_type}"
                );
                let layer_id = ((at[3] & 1) << 5) | (at[4] >> 3);
                assert_eq!(layer_id, 0, "AU {aus} segment {index}: nuh_layer_id");
                assert!(
                    at[4] & 7 > 0,
                    "AU {aus} segment {index}: temporal_id_plus1 > 0"
                );
                // The slice segment header begins at `+5`. Only the FIRST segment of
                // a picture sets first_slice_segment_in_pic_flag.
                assert_eq!(
                    at[5] & 0x80 != 0,
                    index == 0,
                    "AU {aus} segment {index}: first_slice_segment_in_pic_flag"
                );
            }
            aus += 1;
        }
        assert_eq!(aus, 250, "the vector's own golden");

        // H.264: three-byte prefix, then the ONE-byte NAL unit header, then the
        // slice header — whose first element is first_mb_in_slice `ue(v)`, so a
        // leading 1 bit means "0", i.e. the first slice of the picture.
        let mut planner = pf_bitstream::h264::H264Planner::new();
        let mut aus = 0usize;
        for au in split_h264_aus(TEST_25FPS_H264) {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let plan_segments: Vec<std::ops::Range<usize>> =
                plan.slices.iter().map(|s| s.data.clone()).collect();
            let packed = pack_slices(au, &plan_segments).expect("offsets fit u32");
            let slot = packed_slot(au, &packed);
            for (index, offset) in packed.offsets.iter().enumerate() {
                let at = &slot[*offset as usize..];
                assert_eq!(
                    at[..3],
                    [0, 0, 1],
                    "AU {aus} segment {index}: a three-byte start code"
                );
                assert_eq!(at[3] & 0x80, 0, "AU {aus} segment {index}: forbidden_zero");
                let nal_type = at[3] & 0x1F;
                assert!(
                    nal_type == 1 || nal_type == 5,
                    "AU {aus} segment {index}: a coded slice, not type {nal_type}"
                );
                assert_eq!(
                    at[4] & 0x80 != 0,
                    index == 0,
                    "AU {aus} segment {index}: first_mb_in_slice == 0"
                );
            }
            // The vector this crate's H.264 parity leg decodes is MULTI-slice; if it
            // ever stopped being, its leg would stop covering the multi-segment
            // offset arithmetic and this file's H.265 leg would be the only cover.
            assert!(packed.offsets.len() >= 2, "AU {aus}: multi-slice");
            aus += 1;
        }
        assert_eq!(aus, 250, "the vector's own golden");
    }

    /// The vendored H.265 vector, at the path `pic_h265`'s tests use.
    const TEST_25FPS_H265: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );
    /// Its H.264 twin — the codec that shares this module and must not regress.
    const TEST_25FPS_H264: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    /// One AU packed exactly as [`BitstreamRing::upload`] would pack it, into a
    /// buffer as long as the recorded `srcBufferRange` under the widest alignment
    /// any driver in the fleet reports.
    fn packed_slot(au: &[u8], packed: &PackedSlices) -> Vec<u8> {
        let len: u64 = packed.segments.iter().map(|s| s.len() as u64).sum();
        let layout = RingLayout::new(INITIAL_SLOT_SIZE, RING_SLOTS, 256, 256);
        let mut slot = vec![0xFFu8; layout.record_range(len) as usize];
        pack_into(&mut slot, au, &packed.segments);
        slot
    }

    /// Test-only H.265 AU splitter — the same one `pic_h265`'s tests, the GPU legs'
    /// `tests/common` and pf-bitstream's own tests each carry (it is `#[cfg(test)]`
    /// private there): a new AU starts at a non-VCL NALU following slices, or at a
    /// slice segment whose `first_slice_segment_in_pic_flag` (the top bit of the
    /// byte after the TWO-byte NAL header) is set while the current AU already has
    /// slices.
    fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
        use cros_codecs::codec::h265::parser::Nalu;

        let mut aus = Vec::new();
        let mut cursor = std::io::Cursor::new(stream);
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

    /// [`split_h265_aus`]' H.264 twin: a ONE-byte NAL header, so `first_mb_in_slice`
    /// is the top bit of the byte after it, and "is a slice" is the two-type enum.
    fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
        use cros_codecs::codec::h264::parser::Nalu;
        use cros_codecs::codec::h264::parser::NaluType;

        let mut aus = Vec::new();
        let mut cursor = std::io::Cursor::new(stream);
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
    fn clear_forgets_every_token_and_restarts_the_cursor() {
        let mut states: SlotStates<u64> = SlotStates::new(3);
        for token in 0..3 {
            let s = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
            states.set_pending(s, token);
        }
        assert_eq!(states.in_flight().count(), 3);
        states.clear();
        assert_eq!(states.in_flight().count(), 0);
        assert_eq!(states.acquire(|_| Ok::<_, ()>(true)).unwrap(), Some(0));
    }
}
