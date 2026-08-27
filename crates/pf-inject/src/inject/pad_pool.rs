//! Host-wide allocation of the OS-level virtual-pad slots ([`PadSlotPool`]), and the per-session
//! wire-index → slot mapping built on it ([`PadSlotMap`]).
//!
//! # Why this exists
//!
//! Every OS-level name a virtual pad needs is derived from a pad index and nothing else:
//!
//! - the bootstrap mailboxes `Global\pfxusb-boot-<i>` and `Global\pfds-boot-<i>`
//!   (`pf_driver_proto::gamepad::xusb_boot_name` / `pf_driver_proto::gamepad::pad_boot_name`);
//! - the `SwDeviceCreate` instance ids — `pf_xusb_<i>`, `pf_pad_<i>`, `pf_ds4_<i>`, `pf_xbox_<i>`;
//! - on Linux the DualSense pairing MAC, the Steam Deck serial and the Switch Pro MAC — the last
//!   three *explicitly required to be unique per pad*, because `hid-playstation` adopts the MAC as
//!   the HID `uniq` and SDL/Steam dedup controllers by that serial.
//!
//! The host serves several sessions at once (`native::DEFAULT_MAX_CONCURRENT`), each with its own
//! input thread and its own pad router, and **every client numbers its first controller wire pad
//! 0**. Those two facts together mean two clients each holding a controller collide on every name
//! above. On Windows the second session's `Shm::create_named` sees `ERROR_ALREADY_EXISTS` for all
//! five retries and reports [`crate::pad_slots::PadCreateFault::IndexOwnedElsewhere`] — whose
//! remedy tells the operator to restart the service, which would kill both sessions, and no other
//! process is even involved. On Linux there is no error at all: both sessions mint a DualSense
//! with the same pairing MAC, `hid-playstation` writes it into `HID_UNIQ` for both, and SDL/Steam
//! merge the two pads into one controller.
//!
//! # The fix
//!
//! Stop treating the wire index as an OS identity. A session's wire indices are its own business;
//! the **OS slot is host-wide**, claimed on a pad's first frame and released when that pad goes
//! away. One translation, performed once in the session's pad router, makes every name above
//! unique — and because the *format* of those names is unchanged, the drivers (which read the
//! index back out of `pszDeviceLocation`) need no change at all.
//!
//! Slots are claimed lazily rather than handed out as fixed per-session windows, so the common
//! single-session case still reaches all [`MAX_PADS`] pads; two sessions simply share the range
//! between them.
//!
//! # Why a process-global pool
//!
//! The names being protected are process-wide (`Global\…` kernel objects, PnP instance ids), so
//! the thing that arbitrates them is process-wide too — there is no configuration in which two
//! pools within one host would be correct. A global also keeps the fix off every session
//! signature between here and the accept loop. Collisions with a *separate* live process are a
//! different problem and remain [`crate::pad_slots::PadCreateFault::IndexOwnedElsewhere`]'s.

use punktfunk_core::input::MAX_PADS;
use std::sync::Mutex;

/// The set of OS pad slots currently spoken for, host-wide.
#[derive(Debug)]
pub struct PadSlotPool {
    /// Bit `i` set = OS slot `i` is claimed. `MAX_PADS <= 16` is asserted in [`crate::pad_slots`].
    taken: Mutex<u16>,
}

impl Default for PadSlotPool {
    fn default() -> Self {
        Self::new()
    }
}

impl PadSlotPool {
    pub const fn new() -> Self {
        Self {
            taken: Mutex::new(0),
        }
    }

    /// Claim the lowest free OS slot, or `None` when the host already holds [`MAX_PADS`] pads.
    ///
    /// Lowest-free rather than round-robin so a single-session host keeps the slot numbering it
    /// has always had — pad 0 is slot 0 — and so a field log reads the same as it used to.
    pub fn claim(&self) -> Option<u8> {
        let mut taken = self.lock();
        (0..MAX_PADS).find_map(|i| {
            (*taken & (1 << i) == 0).then(|| {
                *taken |= 1 << i;
                i as u8
            })
        })
    }

    /// Hand `slot` back. Releasing a slot that was never claimed is a no-op, so a double release
    /// on a teardown path cannot free somebody else's pad.
    pub fn release(&self, slot: u8) {
        if (slot as usize) < MAX_PADS {
            *self.lock() &= !(1u16 << slot);
        }
    }

    /// A poisoned pool must not wedge every future pad on the host: one session panicking while
    /// holding the lock says nothing about whether the *bitmap* is usable, and it is — a `u16`
    /// has no torn state.
    fn lock(&self) -> std::sync::MutexGuard<'_, u16> {
        self.taken.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn taken_mask(&self) -> u16 {
        *self.lock()
    }
}

/// The process-wide pool — see the module docs for why this is a global.
pub fn global() -> &'static PadSlotPool {
    static POOL: PadSlotPool = PadSlotPool::new();
    &POOL
}

/// One session's wire-index → OS-slot mapping, drawn from a [`PadSlotPool`].
///
/// Dropping releases every slot the session still holds, so a session that ends abruptly — a
/// panicking input thread included — cannot strand an OS name for the life of the host.
#[derive(Debug)]
pub struct PadSlotMap<'a> {
    pool: &'a PadSlotPool,
    slot: [Option<u8>; MAX_PADS],
}

impl PadSlotMap<'static> {
    /// A mapping against the process-wide pool — what a real session uses.
    pub fn new() -> Self {
        Self::with_pool(global())
    }
}

impl Default for PadSlotMap<'static> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> PadSlotMap<'a> {
    /// A mapping against a caller-supplied pool. Exists so the allocation policy is testable
    /// without touching host-wide state.
    pub fn with_pool(pool: &'a PadSlotPool) -> Self {
        Self {
            pool,
            slot: [None; MAX_PADS],
        }
    }

    /// This session's OS slot for `wire`, claiming one on first use.
    ///
    /// `None` means the wire index is out of range, or the host is already holding [`MAX_PADS`]
    /// pads across all its sessions — in which case no device may be created for it.
    pub fn claim_for(&mut self, wire: usize) -> Option<u8> {
        if wire >= MAX_PADS {
            return None;
        }
        if let Some(slot) = self.slot[wire] {
            return Some(slot);
        }
        let slot = self.pool.claim()?;
        self.slot[wire] = Some(slot);
        Some(slot)
    }

    /// This session's OS slot for `wire` **without** claiming one.
    pub fn slot_of(&self, wire: usize) -> Option<u8> {
        self.slot.get(wire).copied().flatten()
    }

    /// The wire index this session has mapped to `slot` — the reverse direction, needed because
    /// every backend reports feedback (rumble, rich HID output) tagged with the OS index it was
    /// created under, while the client only knows its own wire numbering.
    pub fn wire_of(&self, slot: u8) -> Option<usize> {
        self.slot.iter().position(|s| *s == Some(slot))
    }

    /// Release `wire`'s slot back to the pool, if it holds one.
    pub fn release(&mut self, wire: usize) {
        if let Some(slot) = self.slot.get_mut(wire).and_then(Option::take) {
            self.pool.release(slot);
        }
    }

    /// Translate a wire-space active mask into OS-slot space.
    ///
    /// The managers' unplug sweep walks this mask against the slots they actually created, so it
    /// has to speak the same numbering the devices were created under. A wire bit with no slot
    /// contributes nothing — it names a pad this session never got a device for.
    pub fn os_mask(&self, wire_mask: u16) -> u16 {
        (0..MAX_PADS)
            .filter(|w| wire_mask & (1 << w) != 0)
            .filter_map(|w| self.slot[w])
            .fold(0u16, |m, slot| m | (1u16 << slot))
    }
}

impl Drop for PadSlotMap<'_> {
    fn drop(&mut self) {
        for wire in 0..MAX_PADS {
            self.release(wire);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module exists for: two sessions, each numbering its first pad 0, must not
    /// land on the same OS slot — every pad name on both platforms is derived from that number.
    #[test]
    fn two_sessions_numbering_their_first_pad_zero_get_different_os_slots() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);

        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(b.claim_for(0), Some(1), "session B must not reuse slot 0");
        assert_eq!(a.claim_for(1), Some(2));
        assert_eq!(b.claim_for(1), Some(3));

        // And the claim is stable: asking again is not a second allocation.
        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(pool.taken_mask(), 0b1111);
    }

    /// A single session still reaches every pad — the fix must not cost the common case.
    #[test]
    fn one_session_still_reaches_every_pad() {
        let pool = PadSlotPool::new();
        let mut only = PadSlotMap::with_pool(&pool);
        for wire in 0..MAX_PADS {
            assert_eq!(only.claim_for(wire), Some(wire as u8), "wire {wire}");
        }
        assert_eq!(pool.taken_mask(), u16::MAX);
    }

    #[test]
    fn a_released_slot_goes_back_to_the_pool() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);

        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(b.claim_for(0), Some(1));
        a.release(0);
        assert_eq!(a.slot_of(0), None);
        // The freed slot is the lowest one now, so the next claim takes it.
        assert_eq!(b.claim_for(1), Some(0));

        // Releasing twice must not free a slot somebody else now holds.
        a.release(0);
        assert_eq!(pool.taken_mask(), 0b11);
    }

    /// A session that ends — abruptly included — strands nothing.
    #[test]
    fn dropping_a_session_returns_every_slot_it_held() {
        let pool = PadSlotPool::new();
        {
            let mut s = PadSlotMap::with_pool(&pool);
            s.claim_for(0);
            s.claim_for(3);
            s.claim_for(7);
            assert_eq!(pool.taken_mask(), 0b111);
        }
        assert_eq!(
            pool.taken_mask(),
            0,
            "a dropped session must free its slots"
        );
    }

    /// An exhausted host refuses honestly instead of handing out a colliding slot.
    #[test]
    fn an_exhausted_pool_refuses_rather_than_colliding() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        for wire in 0..MAX_PADS {
            assert!(a.claim_for(wire).is_some());
        }
        let mut b = PadSlotMap::with_pool(&pool);
        assert_eq!(b.claim_for(0), None, "no slot left, and none may be shared");
    }

    #[test]
    fn an_out_of_range_wire_index_claims_nothing() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        assert_eq!(a.claim_for(MAX_PADS), None);
        assert_eq!(a.claim_for(usize::MAX), None);
        assert_eq!(
            pool.taken_mask(),
            0,
            "a rejected index must not consume a slot"
        );
    }

    /// The sweep mask has to speak the numbering the devices were created under.
    #[test]
    fn the_active_mask_is_translated_into_slot_space() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);
        a.claim_for(0); // slot 0
        b.claim_for(0); // slot 1
        b.claim_for(1); // slot 2

        // B holds wire 0 and 1; in slot space that is bits 1 and 2, never bit 0 (A's pad).
        assert_eq!(b.os_mask(0b11), 0b110);
        // A wire bit with no device contributes nothing.
        assert_eq!(a.os_mask(0b11), 0b1);
        assert_eq!(a.os_mask(0), 0);
    }

    /// Feedback comes back tagged with the OS slot; it has to reach the right wire pad.
    #[test]
    fn feedback_maps_back_to_the_wire_pad_that_owns_it() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);
        a.claim_for(0);
        b.claim_for(0);

        assert_eq!(a.wire_of(0), Some(0));
        assert_eq!(
            a.wire_of(1),
            None,
            "B's pad must not resolve to a wire pad of A's - that is rumble on the wrong client"
        );
        assert_eq!(b.wire_of(1), Some(0));
    }
}
