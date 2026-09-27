//! Host half of a Windows pad's `PadShm` DATA section: the stamp order, the input seqlock
//! and the output-ring reader.
//!
//! Only raw pointers and atomics over [`pf_driver_proto::gamepad::PadShm`] offsets, so the
//! reader's tests run on every OS. The section itself, its sealed delivery and the devnode
//! live in `windows/`.

use pf_driver_proto::gamepad::PadShm;
use std::sync::atomic::{fence, AtomicU32, Ordering};

/// Byte size of [`PadShm`]. Offsets and magic come from the same struct so a layout change
/// is a compile error; the driver maps that type too.
pub(crate) const SHM_SIZE: usize = core::mem::size_of::<PadShm>();
pub(crate) const SHM_MAGIC: u32 = pf_driver_proto::gamepad::PAD_MAGIC; // "PFDS"
pub(crate) const OFF_INPUT: usize = core::mem::offset_of!(PadShm, input);
pub(crate) const OFF_OUT_SEQ: usize = core::mem::offset_of!(PadShm, out_seq);
pub(crate) const OFF_OUTPUT: usize = core::mem::offset_of!(PadShm, output);
/// The driver picks HID identity from this byte.
pub(crate) const OFF_DEVTYPE: usize = core::mem::offset_of!(PadShm, device_type);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(PadShm, driver_proto);
const OFF_DRIVER_REV: usize = core::mem::offset_of!(PadShm, driver_rev);
pub(crate) const OFF_PAD_INDEX: usize = core::mem::offset_of!(PadShm, pad_index);
const OFF_OUT_RING_VER: usize = core::mem::offset_of!(PadShm, out_ring_ver);
const OFF_RING_HEAD: usize = core::mem::offset_of!(PadShm, ring_head);
const OFF_OUT_RING_LEN: usize = core::mem::offset_of!(PadShm, out_ring_len);
const OFF_OUT_RING: usize = core::mem::offset_of!(PadShm, out_ring);
const OUT_SLOT_SIZE: usize = core::mem::size_of::<pf_driver_proto::gamepad::OutSlot>();
const OUT_RING_LEN: u32 = pf_driver_proto::gamepad::OUT_RING_LEN;
const OUT_RING_LEN_V22: u32 = pf_driver_proto::gamepad::OUT_RING_LEN_V22;
/// v2.3 input seqlock — see [`publish_input`].
const OFF_INPUT_GEN: usize = core::mem::offset_of!(PadShm, input_gen);
/// The input slot's size; a longer report would overrun into `out_seq`.
pub(crate) const INPUT_SLOT: usize = 64;

/// Stamp a fresh section for the driver: device type, pad index, ring version `2` and the
/// neutral report, then the magic last. The driver trusts nothing before the magic, and a
/// device type it reads late enumerates the pad as a DualSense. `2` means this host drains
/// the v2.2 long ring; a v2.1 driver reads it as a boolean and stays on 8-slot math.
///
/// # Safety
/// `base` points at a live, writable [`PadShm`] mapping; `neutral` fits the input slot.
pub(crate) unsafe fn stamp(base: *mut u8, devtype: u8, index: u8, neutral: &[u8]) {
    debug_assert!(
        neutral.len() <= INPUT_SLOT,
        "neutral report overruns the input slot"
    );
    // SAFETY: the caller's contract; every offset is inside the section.
    unsafe {
        *base.add(OFF_DEVTYPE) = devtype;
        std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
        std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
        std::ptr::copy_nonoverlapping(neutral.as_ptr(), base.add(OFF_INPUT), neutral.len());
        std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
    }
}

/// `(driver_proto, driver_rev)` from a pad section. The driver stamps the revision first and the
/// protocol with Release, so a revision read after a nonzero protocol is the driver's.
///
/// # Safety
/// `base` points at a live, mapped [`PadShm`].
pub(crate) unsafe fn driver_marks(base: *mut u8) -> (u32, u32) {
    // SAFETY: the caller's contract; both offsets are 4-aligned fields inside the section.
    unsafe {
        let proto = (*(base.add(OFF_DRIVER_PROTO) as *const AtomicU32)).load(Ordering::Acquire);
        (
            proto,
            std::ptr::read_volatile(base.add(OFF_DRIVER_REV) as *const u32),
        )
    }
}

/// Publish one HID input report into the section's input slot under the v2.3 seqlock.
///
/// The slot is a single unqueued buffer. The driver's timer can copy 64 bytes out of it mid-write,
/// which for gyro is a spike a game will integrate as aim.
///
/// `generation` goes odd before the body and even after. The driver samples either side of its
/// read and retries on disagreement. The `Release` fence keeps body stores below the odd marker;
/// the `Release` store publishes them ahead of even. Both are no-ops on x86-TSO, load-bearing on ARM64.
///
/// # Safety
/// `base` must point at a live mapped pad section of at least [`SHM_SIZE`] bytes, and `report`
/// must be no longer than [`INPUT_SLOT`].
pub(crate) unsafe fn publish_input(base: *mut u8, generation: &mut u32, report: &[u8]) {
    debug_assert!(report.len() <= INPUT_SLOT, "report overruns the input slot");
    // Odd: a report is in flight.
    *generation = generation.wrapping_add(1);
    // SAFETY: the caller guarantees `base` maps the section; `OFF_INPUT_GEN` is 4-aligned off the
    // page-aligned base and sits in the v2 legacy region every driver generation maps.
    unsafe {
        (*(base.add(OFF_INPUT_GEN) as *const AtomicU32)).store(*generation, Ordering::Relaxed)
    };
    // Ordered, not ordering: keeps the body stores below from being hoisted above the odd marker.
    fence(Ordering::Release);
    // SAFETY: the caller guarantees the mapping and that `report` fits the slot at OFF_INPUT.
    unsafe { std::ptr::copy_nonoverlapping(report.as_ptr(), base.add(OFF_INPUT), report.len()) };
    // Even: the slot holds a whole report again.
    *generation = generation.wrapping_add(1);
    // SAFETY: as the first store.
    unsafe {
        (*(base.add(OFF_INPUT_GEN) as *const AtomicU32)).store(*generation, Ordering::Release)
    };
}

/// Drain of a pad section's output plane: the lossless report ring when the driver publishes one
/// (8 slots on v2.1, [`OUT_RING_LEN_V22`] after both sides negotiate v2.2 — the driver's
/// `out_ring_len` echo decides), else the legacy latest-report slot. The ring is the only path
/// that cannot coalesce a rumble-STOP behind a following LED/trigger report inside one ~4 ms
/// poll (`design/rumble-root-fix.md`).
pub(crate) struct OutputDrain {
    /// Driver `ring_head` value drained up to.
    tail: u32,
    /// Last `out_seq` consumed — single-slot path only.
    last_out_seq: u32,
    /// Latched on first ring activity; the legacy path never re-engages after it (the driver
    /// dual-writes both planes, so consuming both would double-parse every report).
    ring_live: bool,
}

impl OutputDrain {
    pub(crate) fn new() -> OutputDrain {
        OutputDrain {
            tail: 0,
            last_out_seq: 0,
            ring_live: false,
        }
    }

    /// Drain every output report published since the last call, oldest → newest.
    ///
    /// `per_report` gets the slot bytes and a `feature` flag: bit 31 of the raw ring length is a
    /// Triton FEATURE set ([`pf_driver_proto::triton::out_is_feature`]).
    /// [`pf_driver_proto::triton::out_len`] masks that bit **before** the 64-byte clamp, so a
    /// tagged slot clamps on payload size, not `raw_len | 0x8000_0000`.
    ///
    /// Returns `true` on overflow (more than the negotiated length landed, or the driver lapped
    /// mid-copy): the pending window is discarded as possibly torn, the untagged latest-report slot
    /// is salvaged into one `per_report` call, and the caller must `PadFeedback::resync` planes that
    /// report did not carry. Overflow salvage and the pre-ring path both read that untagged slot, so
    /// `feature` is always `false` there — a FEATURE that lands on overflow or on an old driver
    /// replays as OUTPUT until the next ring-fed poll.
    ///
    /// # Safety
    /// `base` points at a live, mapped [`PadShm`] of [`SHM_SIZE`] bytes.
    pub(crate) unsafe fn drain_tagged(
        &mut self,
        base: *mut u8,
        mut per_report: impl FnMut(&[u8], bool),
    ) -> bool {
        // SAFETY: base points at SHM_SIZE bytes; `OFF_RING_HEAD` is 4-aligned off the
        // page-aligned base. The driver bumps `ring_head` AFTER writing the slot, so an Acquire
        // load orders the slot copies below.
        let head =
            unsafe { (*(base.add(OFF_RING_HEAD) as *const AtomicU32)).load(Ordering::Acquire) };
        if self.ring_live || head != 0 {
            self.ring_live = true;
            if head == self.tail {
                return false;
            }
            // Driver's slot-math modulo (0 = pre-v2.2, hardcodes 8). Loaded after Acquire on
            // `ring_head`; restamped before every bump. Out-of-range clamps to v2.1 so offsets
            // stay inside the v2.2 ring.
            // SAFETY: `OFF_OUT_RING_LEN` is 4-aligned off the page-aligned base.
            let echo = unsafe {
                (*(base.add(OFF_OUT_RING_LEN) as *const AtomicU32)).load(Ordering::Relaxed)
            };
            let ring_len = if (1..=OUT_RING_LEN_V22).contains(&echo) {
                echo
            } else {
                OUT_RING_LEN
            };
            let pending = head.wrapping_sub(self.tail);
            if pending <= ring_len {
                // Copy slots first, then re-check head: a writer that lapped the window during
                // the copy may have overwritten what we read.
                let n = pending as usize;
                let mut bufs =
                    [([0u8; 64], 0usize, false); pf_driver_proto::gamepad::OUT_RING_LEN_V22_USIZE];
                for (k, buf) in bufs.iter_mut().enumerate().take(n) {
                    let idx = (self.tail.wrapping_add(k as u32) % ring_len) as usize;
                    let slot = OFF_OUT_RING + idx * OUT_SLOT_SIZE;
                    // SAFETY: slot .. slot+OUT_SLOT_SIZE is inside the SHM_SIZE section (idx <
                    // `ring_len` ≤ OUT_RING_LEN_V22, whose last slot ends at 4064 ≤ SHM_SIZE);
                    // the len field is 4-aligned (`OFF_OUT_RING` == 256, `OUT_SLOT_SIZE` == 68).
                    let raw_len = unsafe { std::ptr::read_unaligned(base.add(slot) as *const u32) };
                    buf.2 = pf_driver_proto::triton::out_is_feature(raw_len);
                    buf.1 = (pf_driver_proto::triton::out_len(raw_len) as usize).min(64);
                    // SAFETY: the slot's data region is slot+4 .. slot+4+64, inside the section;
                    // `buf.0` is a live local 64-byte array.
                    unsafe {
                        std::ptr::copy_nonoverlapping(base.add(slot + 4), buf.0.as_mut_ptr(), buf.1)
                    };
                }
                // SAFETY: as the first `ring_head` load above.
                let head2 = unsafe {
                    (*(base.add(OFF_RING_HEAD) as *const AtomicU32)).load(Ordering::Acquire)
                };
                if head2.wrapping_sub(self.tail) <= ring_len {
                    for (data, len, feature) in bufs.iter().take(n) {
                        if *len > 0 {
                            per_report(&data[..*len], *feature);
                        }
                    }
                    self.tail = head;
                    return false;
                }
            }
            // Overflow or lapped mid-copy: skip to the freshest head and salvage the untagged
            // latest-report slot (driver dual-publishes every report there). No seqlock; parser
            // gates drop most tears, caller resync silences planes the salvage does not assert.
            // SAFETY: as the first `ring_head` load above.
            self.tail =
                unsafe { (*(base.add(OFF_RING_HEAD) as *const AtomicU32)).load(Ordering::Acquire) };
            let mut out = [0u8; 64];
            // SAFETY: the legacy output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe { std::ptr::copy_nonoverlapping(base.add(OFF_OUTPUT), out.as_mut_ptr(), 64) };
            per_report(&out, false);
            return true;
        }
        // Pre-ring driver: latest-report slot + seq, coalescing. No feature tag on this slot.
        // SAFETY: `OFF_OUT_SEQ` is 4-aligned off the page-aligned base; Acquire pairs with the
        // driver's publish-then-bump store order.
        let seq = unsafe { (*(base.add(OFF_OUT_SEQ) as *const AtomicU32)).load(Ordering::Acquire) };
        if seq != self.last_out_seq {
            self.last_out_seq = seq;
            let mut out = [0u8; 64];
            // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe { std::ptr::copy_nonoverlapping(base.add(OFF_OUTPUT), out.as_mut_ptr(), 64) };
            per_report(&out, false);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section() -> Vec<u32> {
        vec![0u32; SHM_SIZE / 4]
    }

    fn base(buf: &mut [u32]) -> *mut u8 {
        buf.as_mut_ptr() as *mut u8
    }

    /// v2.1 dual write: legacy slot + seq, then ring slot (8-slot math, no length echo), then head.
    fn publish(buf: &mut [u32], bytes: &[u8]) {
        ring_publish(buf, bytes, OUT_RING_LEN, false);
    }

    /// v2.1 dual write with `OUT_FEATURE_BIT` ORed into the slot length — the tag `drain_tagged`
    /// must strip and surface as `feature`.
    fn publish_tagged(buf: &mut [u32], bytes: &[u8]) {
        legacy_publish(buf, bytes);
        let head = read32(buf, OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % OUT_RING_LEN) as usize * OUT_SLOT_SIZE;
        write32(
            buf,
            slot,
            bytes.len() as u32 | pf_driver_proto::triton::OUT_FEATURE_BIT,
        );
        let b = bytes_mut(buf);
        b[slot + 4..slot + 4 + bytes.len()].copy_from_slice(bytes);
        write32(buf, OFF_RING_HEAD, head.wrapping_add(1));
    }

    /// v2.2 dual write: long-ring slot math, `out_ring_len` echo stamped before the head bump.
    fn v22_publish(buf: &mut [u32], bytes: &[u8]) {
        ring_publish(buf, bytes, OUT_RING_LEN_V22, true);
    }

    fn ring_publish(buf: &mut [u32], bytes: &[u8], len: u32, echo: bool) {
        legacy_publish(buf, bytes);
        let head = read32(buf, OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % len) as usize * OUT_SLOT_SIZE;
        write32(buf, slot, bytes.len() as u32);
        let b = bytes_mut(buf);
        b[slot + 4..slot + 4 + bytes.len()].copy_from_slice(bytes);
        if echo {
            write32(buf, OFF_OUT_RING_LEN, len);
        }
        write32(buf, OFF_RING_HEAD, head.wrapping_add(1));
    }

    /// Pre-ring driver: latest-report slot + seq only.
    fn legacy_publish(buf: &mut [u32], bytes: &[u8]) {
        let b = bytes_mut(buf);
        b[OFF_OUTPUT..OFF_OUTPUT + bytes.len()].copy_from_slice(bytes);
        let seq = read32(buf, OFF_OUT_SEQ).wrapping_add(1);
        write32(buf, OFF_OUT_SEQ, seq);
    }

    /// Byte view of the source slice's allocation, including short test buffers.
    fn bytes_mut(buf: &mut [u32]) -> &mut [u8] {
        let byte_len = buf
            .len()
            .checked_mul(size_of::<u32>())
            .expect("u32 slice byte length overflow");
        // SAFETY: `byte_len` is exactly the source slice's allocation range; u8 needs less alignment.
        unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), byte_len) }
    }

    #[test]
    fn byte_view_stays_within_the_source_slice() {
        let mut buf = [0u32; 2];
        assert_eq!(bytes_mut(&mut buf).len(), 2 * size_of::<u32>());
    }

    fn read32(buf: &mut [u32], off: usize) -> u32 {
        u32::from_ne_bytes(bytes_mut(buf)[off..off + 4].try_into().unwrap())
    }

    fn write32(buf: &mut [u32], off: usize, v: u32) {
        bytes_mut(buf)[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }

    fn collect(d: &mut OutputDrain, buf: &mut [u32]) -> (Vec<Vec<u8>>, bool) {
        let mut got = Vec::new();
        // SAFETY: `buf` is a live SHM_SIZE-byte section.
        let resync = unsafe { d.drain_tagged(base(buf), |b, _| got.push(b.to_vec())) };
        (got, resync)
    }

    /// The stamp lands every field the driver reads, magic included.
    #[test]
    fn stamp_writes_every_field_the_driver_reads() {
        let mut buf = section();
        // SAFETY: `buf` is a live SHM_SIZE-byte section.
        unsafe { stamp(base(&mut buf), 7, 3, &[0x42, 0x01]) };
        let b = bytes_mut(&mut buf);
        assert_eq!(b[OFF_DEVTYPE], 7);
        assert_eq!(&b[OFF_INPUT..OFF_INPUT + 2], &[0x42, 0x01]);
        assert_eq!(read32(&mut buf, OFF_PAD_INDEX), 3);
        assert_eq!(read32(&mut buf, OFF_OUT_RING_VER), 2);
        assert_eq!(read32(&mut buf, 0), SHM_MAGIC);
    }

    /// Bit 31 of ring `len` is FEATURE; the tagged drain must strip it from the length and surface
    /// it as a flag. Untagged slots must come through with `feature == false`.
    #[test]
    fn tagged_drain_separates_feature_frames_from_output_frames() {
        let mut buf = section();
        publish(&mut buf, &[0x80, 0x00, 0xFF]);
        publish_tagged(&mut buf, &[0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        let mut got = Vec::new();
        let mut d = OutputDrain::new();
        // SAFETY: `buf` is a live SHM_SIZE-byte section.
        unsafe {
            d.drain_tagged(base(&mut buf), |bytes, feature| {
                got.push((bytes.to_vec(), feature));
            })
        };
        assert_eq!(got[0], (vec![0x80, 0x00, 0xFF], false));
        assert_eq!(got[1].0, vec![0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        assert!(got[1].1);
    }

    /// A rumble-stop then an LED-only report in one poll must yield both, oldest first
    /// (`design/rumble-root-fix.md`). On the legacy single slot the stop is overwritten.
    #[test]
    fn ring_preserves_a_stop_followed_by_an_led_report() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        publish(&mut buf, &[0x02, 0x03, 0, 0xFF, 0xFF]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 0x03, 0, 0xFF, 0xFF]]);

        publish(&mut buf, &[0x02, 0x03, 0, 0, 0]);
        publish(&mut buf, &[0x02, 0, 0x04, 0, 0]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got,
            vec![vec![0x02, 0x03, 0, 0, 0], vec![0x02, 0, 0x04, 0, 0]],
            "the stop report must survive the burst, oldest first"
        );
        assert_eq!(collect(&mut d, &mut buf).0.len(), 0);
    }

    #[test]
    fn ring_wraps_across_polls() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..6u8 {
            publish(&mut buf, &[0x02, i]);
        }
        assert_eq!(collect(&mut d, &mut buf).0.len(), 6);
        for i in 6..12u8 {
            // 12 wraps past the 8-slot v2.1 ring
            publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            vec![6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn overflow_salvages_the_latest_slot_and_flags_resync_then_recovers() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..12u8 {
            // 12 > OUT_RING_LEN pending — the oldest 4 were overwritten in-ring
            publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(resync, "an overflowed window must be reported");
        assert_eq!(
            got.len(),
            1,
            "the possibly-torn ring window must not be parsed — only the legacy latest slot"
        );
        assert_eq!(
            &got[0][..2],
            &[0x02, 11],
            "the salvage must be the freshest coalesced state, not silence"
        );
        publish(&mut buf, &[0x02, 99]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 99]]);
    }

    /// 40 pending fits in 56 slots and overflows every poll against the 8-slot ring.
    #[test]
    fn v22_ring_absorbs_a_burst_the_v21_ring_could_not() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..40u8 {
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync, "40 pending ≤ 56 slots — no overflow");
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            (0..40).collect::<Vec<_>>()
        );
    }

    #[test]
    fn v22_ring_wraps_across_polls() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..50u8 {
            v22_publish(&mut buf, &[0x02, i]);
        }
        assert_eq!(collect(&mut d, &mut buf).0.len(), 50);
        for i in 50..100u8 {
            // 100 wraps past the 56-slot v2.2 ring
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            (50..100).collect::<Vec<_>>()
        );
    }

    #[test]
    fn v22_overflow_still_salvages_and_recovers() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..60u8 {
            // 60 > OUT_RING_LEN_V22 pending
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(resync);
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0][..2], &[0x02, 59]);
        v22_publish(&mut buf, &[0x02, 99]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 99]]);
    }

    /// Torn or hostile `out_ring_len` must clamp to the v2.1 length, not index past the ring.
    #[test]
    fn garbage_length_echo_clamps_to_the_v21_length() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        publish(&mut buf, &[0x02, 1]); // 8-slot math, matching the clamp fallback
        write32(&mut buf, OFF_OUT_RING_LEN, 9999);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 1]]);
    }

    #[test]
    fn legacy_driver_still_drains_the_latest_slot() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        legacy_publish(&mut buf, &[0x02, 1]);
        legacy_publish(&mut buf, &[0x02, 2]); // coalesced: latest wins
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0][..2], &[0x02, 2]);
        assert_eq!(collect(&mut d, &mut buf).0.len(), 0);
    }
}
