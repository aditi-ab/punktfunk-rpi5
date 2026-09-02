//! Host-wide OS-level virtual-pad slots ([`PadSlotPool`]) and the per-session
//! wire-index → slot map ([`PadSlotMap`]).
//!
//! Every OS name a pad needs is derived from a pad index: the
//! `Global\pfxusb-boot-<i>` / `Global\pfds-boot-<i>` mailboxes, `SwDeviceCreate`
//! instance ids, DualSense pairing MAC, Deck serial, and Switch Pro MAC.
//! `hid-playstation` uses the MAC as HID `uniq`; SDL/Steam dedup on that serial.
//! Clients each number their first pad 0, and the host serves several sessions.
//!
//! A session's wire index stays its own; the OS slot is claimed on first frame
//! and released when the pad (or this map) goes away. Name format is unchanged,
//! so drivers that parse the index need no change. Lazy claim, not per-session
//! windows: one session still reaches [`MAX_PADS`]. The pool is process-global
//! because the names are. A separate live process remains
//! [`crate::pad_slots::PadCreateFault::IndexOwnedElsewhere`].
//!
//! Tests in this file pin the contract.

use punktfunk_core::input::MAX_PADS;
use std::sync::Mutex;

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

    /// Lowest free slot. Not round-robin: a single session still maps wire 0 → slot 0.
    pub fn claim(&self) -> Option<u8> {
        let mut taken = self.lock();
        (0..MAX_PADS).find_map(|i| {
            (*taken & (1 << i) == 0).then(|| {
                *taken |= 1 << i;
                i as u8
            })
        })
    }

    /// No-op if `slot` was never claimed, so a double release cannot free another
    /// session's pad.
    pub fn release(&self, slot: u8) {
        if (slot as usize) < MAX_PADS {
            *self.lock() &= !(1u16 << slot);
        }
    }

    /// Recover from poison. A `u16` bitmap has no torn state, and a panic in one
    /// session must not block every future pad on the host.
    fn lock(&self) -> std::sync::MutexGuard<'_, u16> {
        self.taken.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn taken_mask(&self) -> u16 {
        *self.lock()
    }
}

pub fn global() -> &'static PadSlotPool {
    static POOL: PadSlotPool = PadSlotPool::new();
    &POOL
}

/// Per-session wire-index → OS-slot map over a [`PadSlotPool`].
///
/// Drop releases every slot still held, so a panicking input thread cannot
/// strand an OS name for the life of the host.
#[derive(Debug)]
pub struct PadSlotMap<'a> {
    pool: &'a PadSlotPool,
    slot: [Option<u8>; MAX_PADS],
}

impl PadSlotMap<'static> {
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
    /// Caller-supplied pool so tests do not touch the process-wide bitmap.
    pub fn with_pool(pool: &'a PadSlotPool) -> Self {
        Self {
            pool,
            slot: [None; MAX_PADS],
        }
    }

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

    pub fn slot_of(&self, wire: usize) -> Option<u8> {
        self.slot.get(wire).copied().flatten()
    }

    /// Reverse map. Backends tag rumble / HID output with the OS slot they
    /// created; the client only knows its wire index.
    pub fn wire_of(&self, slot: u8) -> Option<usize> {
        self.slot.iter().position(|s| *s == Some(slot))
    }

    pub fn release(&mut self, wire: usize) {
        if let Some(slot) = self.slot.get_mut(wire).and_then(Option::take) {
            self.pool.release(slot);
        }
    }

    /// Wire-space active mask in OS-slot space. The unplug sweep walks created
    /// slots, so this must use the numbering the devices were created under.
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

    #[test]
    fn two_sessions_numbering_their_first_pad_zero_get_different_os_slots() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);

        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(b.claim_for(0), Some(1), "session B must not reuse slot 0");
        assert_eq!(a.claim_for(1), Some(2));
        assert_eq!(b.claim_for(1), Some(3));

        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(pool.taken_mask(), 0b1111);
    }

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
        assert_eq!(b.claim_for(1), Some(0));

        // Double-release must not clear a slot another session now holds.
        a.release(0);
        assert_eq!(pool.taken_mask(), 0b11);
    }

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

    #[test]
    fn the_active_mask_is_translated_into_slot_space() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);
        a.claim_for(0);
        b.claim_for(0);
        b.claim_for(1);

        assert_eq!(b.os_mask(0b11), 0b110);
        assert_eq!(a.os_mask(0b11), 0b1);
        assert_eq!(a.os_mask(0), 0);
    }

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
