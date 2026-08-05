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

use crate::caps::H264ProfileChain;
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
    std_profile_idc: ash::vk::native::StdVideoH264ProfileIdc,
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
        std_profile_idc: ash::vk::native::StdVideoH264ProfileIdc,
    ) -> Result<Self, AllocError> {
        // SAFETY: live device; allocate_backing only creates objects it returns.
        let (buffer, memory, ptr) =
            unsafe { Self::allocate_backing(dev, &layout, std_profile_idc)? };
        Ok(Self {
            device: dev.ash().clone(),
            layout,
            std_profile_idc,
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
        std_profile_idc: ash::vk::native::StdVideoH264ProfileIdc,
    ) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), AllocError> {
        let mut chain = H264ProfileChain::new(std_profile_idc);
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
                unsafe { Self::allocate_backing(dev, &grown, self.std_profile_idc)? };
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
        // with `slot < slots`; each segment is an in-bounds range of `au` (fn
        // contract) and the cursor advances by exactly the bytes written, staying
        // within `len <= range`. The slot is not concurrently read: its previous
        // use completed (poll/wait above) and its next use is submitted after
        // this copy.
        unsafe {
            let base = self.ptr.add(offset as usize);
            let mut cursor = 0usize;
            for segment in segments {
                let bytes = &au[segment.clone()];
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(cursor), bytes.len());
                cursor += bytes.len();
            }
            // Zero the alignment tail so the recorded range never hands the driver
            // stale bytes from a previous AU behind this one's end.
            std::ptr::write_bytes(base.add(cursor), 0, (range - len) as usize);
        }
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
