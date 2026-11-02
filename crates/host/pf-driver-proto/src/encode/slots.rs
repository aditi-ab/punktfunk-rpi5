//! The encode pool's slot bookkeeping as a value: where the drain worker's pass writes, which
//! frame the encoder takes next, which slot a recovery frame re-encodes, and when a bypass hold
//! ends. The driver's pool (`encode/pool.rs`) keeps the textures, the events and the held
//! surface; each of its verbs asks [`Slots`] first and does only the GPU or event work the
//! answer names. The rules live here so they run on every OS.
//!
//! A slot sits in exactly one of `free`, the queue or `encoding`. The stash names the newest
//! frame the encoder took and is re-encoded only while its slot is idle in `free` with nothing
//! queued. [`DIRECT`] names the acquired surface itself: it is queued only while held, never
//! sits in `free` and never reaches a pass.
//!
//! Each row of the table test pins one fix, named by its commit subject.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Which slot a keyframe request re-encodes when the desktop composed nothing: `stash`, the
/// newest slot the encode thread took, but only while `queued` is 0 — a composed frame already
/// carries the IDR — and the slot sits in `idle`, so no drain pass can be writing the pixels
/// the encoder is about to read. `None` means do nothing.
#[must_use]
pub fn republish_slot(stash: Option<usize>, queued: usize, idle: &[usize]) -> Option<usize> {
    stash.filter(|s| queued == 0 && idle.contains(s))
}

/// Where the drain worker's pass writes. A free slot always wins; with none left the oldest
/// queued frame is overwritten, so under back-pressure the encoder takes the freshest composed
/// picture and the incoming one is kept. `lost` is whether a consumer was there to miss the
/// recycled frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferSlot {
    Free(usize),
    Recycle { slot: usize, lost: bool },
}

/// [`OfferSlot`] for a pool with `free` (any free slot) and `oldest_full` (the front of the
/// queue), `live` while an encode thread is consuming. `None` means no slot at all.
#[must_use]
pub fn offer_slot(
    free: Option<usize>,
    oldest_full: Option<usize>,
    live: bool,
) -> Option<OfferSlot> {
    match (free, oldest_full) {
        (Some(slot), _) => Some(OfferSlot::Free(slot)),
        (None, Some(slot)) => Some(OfferSlot::Recycle { slot, lost: live }),
        (None, None) => None,
    }
}

/// The bypass hold's debt. The encoder names every held surface [`DIRECT`], so once a hold
/// timeout took a surface back early, the release the encoder still owes for it settles here
/// instead of freeing the surface held since; and while one is owed no new hold starts, so a
/// slow encoder costs the head one timeout, not one per frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BypassHold {
    owed: u32,
}

impl BypassHold {
    /// A hold timed out with the encoder still naming the surface.
    pub fn timed_out(&mut self) {
        self.owed += 1;
    }

    /// Whether the next composed frame may be held.
    #[must_use]
    pub fn may_hold(&self) -> bool {
        self.owed == 0
    }

    /// The encoder gave a surface back: `true` when the current hold is its to free.
    #[must_use]
    pub fn released(&mut self) -> bool {
        if self.owed == 0 {
            return true;
        }
        self.owed -= 1;
        false
    }

    /// The consumer is gone (detached or closed): nothing is owed any more.
    pub fn clear(&mut self) {
        self.owed = 0;
    }
}

/// The slot number of a frame that is the acquired surface itself, in no slot.
pub const DIRECT: usize = usize::MAX;

/// `(slot, PresentDisplayQPCTime, source_seq)`: a queued frame, or the stash.
pub type Queued = (usize, u64, u64);

/// Where [`Slots::offer`] puts the drain worker's next surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Pass into `slot`, already out of its list. `recycled_drop` is the new drop total when
    /// the slot was the oldest queued frame and a consumer was there to lose it.
    Slot {
        slot: usize,
        recycled_drop: Option<u64>,
    },
    /// The encoder reads the surface itself: hold it, then queue it as [`DIRECT`].
    Direct,
    /// No slot and no hold. Counted; the new drop total.
    Drop(u64),
}

/// One monitor's pool bookkeeping. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slots {
    free: Vec<usize>,
    /// In acquire order.
    full: VecDeque<Queued>,
    /// Handed to the encoder, AU still owed.
    encoding: Vec<usize>,
    stash: Option<Queued>,
    /// An encode thread is consuming. A recycle costs a frame only while this is set.
    live: bool,
    /// The acquired surface is the encoder's. One at a time.
    held: bool,
    owed: BypassHold,
    dropped: u64,
}

impl Slots {
    /// `n` free slots, nobody consuming.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            free: (0..n).collect(),
            full: VecDeque::new(),
            encoding: Vec::new(),
            stash: None,
            live: false,
            held: false,
            owed: BypassHold::default(),
            dropped: 0,
        }
    }

    /// Count one dropped frame; the new total, for the header.
    pub fn drop_one(&mut self) -> u64 {
        self.dropped += 1;
        self.dropped
    }

    /// Where the next surface goes. While one is held the new one is dropped, so a hold never
    /// nests. A live pool with no debt holds it when `may_bypass` says the encoder can read it
    /// in place; otherwise a free slot wins, then the oldest queued frame ([`offer_slot`]).
    pub fn offer(&mut self, may_bypass: impl FnOnce() -> bool) -> Place {
        if self.held {
            return Place::Drop(self.drop_one());
        }
        if self.live && self.owed.may_hold() && may_bypass() {
            return Place::Direct;
        }
        let free_top = self.free.last().copied();
        let oldest = self.full.front().map(|f| f.0);
        match offer_slot(free_top, oldest, self.live) {
            Some(OfferSlot::Free(slot)) => {
                self.free.pop();
                Place::Slot {
                    slot,
                    recycled_drop: None,
                }
            }
            Some(OfferSlot::Recycle { slot, lost }) => {
                self.full.pop_front();
                let recycled_drop = lost.then(|| self.drop_one());
                Place::Slot {
                    slot,
                    recycled_drop,
                }
            }
            None => Place::Drop(self.drop_one()),
        }
    }

    /// The pass into `slot` failed: the slot is free again and the frame is a counted drop.
    pub fn pass_failed(&mut self, slot: usize) -> u64 {
        self.free.push(slot);
        self.drop_one()
    }

    /// Queue `slot` as the newest frame, taking it out of `free` if it sat there idle.
    /// [`DIRECT`] starts the hold.
    pub fn queue(&mut self, slot: usize, qpc: u64, seq: u64) {
        if slot == DIRECT {
            self.held = true;
        } else {
            self.free.retain(|&s| s != slot);
        }
        self.full.push_back((slot, qpc, seq));
    }

    /// Whether a composed frame is queued.
    #[must_use]
    pub fn has_full(&self) -> bool {
        !self.full.is_empty()
    }

    /// The newest queued frame's source sequence: what a new session opens on.
    #[must_use]
    pub fn newest_seq(&self) -> Option<u64> {
        self.full.back().map(|f| f.2)
    }

    /// The slot a still picture is passed into: the top free one, and only while nothing is
    /// queued, since a queued frame is newer.
    #[must_use]
    pub fn still_slot(&self) -> Option<usize> {
        if self.full.is_empty() {
            self.free.last().copied()
        } else {
            None
        }
    }

    /// `slot` now holds the still picture at source `seq`: it becomes the stash and stays idle
    /// in `free`. QPC 0: the drive stamps a re-encode with now.
    pub fn retained(&mut self, slot: usize, seq: u64) {
        self.stash = Some((slot, 0, seq));
    }

    /// The encode thread starts (`true`: only the newest queued frame is kept) or stops
    /// (`false`: the pool recycles again, the debt is forgotten and a surface nobody took is
    /// given up). `true` when a hold ended.
    pub fn set_live(&mut self, live: bool) -> bool {
        self.live = live;
        if live {
            while self.full.len() > 1 {
                if let Some((slot, ..)) = self.full.pop_front() {
                    self.free.push(slot);
                }
            }
            return false;
        }
        self.owed.clear();
        self.unhold()
    }

    /// The oldest queued frame within `budget`, now the encoder's and the stash, after the
    /// frames past the budget are shed oldest first. With no budget the slots wait for credit,
    /// but a held surface is shed: the drain worker must not wait on credit. Returns the frame,
    /// the shed count and whether a hold ended.
    pub fn take_within(&mut self, budget: usize) -> (Option<Queued>, u64, bool) {
        let mut shed = 0;
        let mut unhold = false;
        if budget == 0 {
            if self.full.iter().any(|f| f.0 == DIRECT) {
                unhold = self.unhold();
                shed = 1;
            }
            return (None, shed, unhold);
        }
        while self.full.len() > budget {
            let Some((slot, ..)) = self.full.pop_front() else {
                break;
            };
            if slot == DIRECT {
                unhold |= core::mem::take(&mut self.held);
            } else {
                self.free.push(slot);
            }
            shed += 1;
        }
        let f = self.full.pop_front();
        if let Some(f) = f {
            self.encoding.push(f.0);
            self.stash = Some(f);
        }
        (f, shed, unhold)
    }

    /// The stash again, for a recovery frame the desktop composed nothing for: only while its
    /// slot is idle and nothing is queued ([`republish_slot`]). A peek; [`Self::hand_out`]
    /// takes the slot. QPC 0: the drive stamps the re-encode with now, not the stale present.
    #[must_use]
    pub fn republish(&self) -> Option<Queued> {
        let (slot, _, seq) = self.stash?;
        republish_slot(Some(slot), self.full.len(), &self.free)?;
        Some((slot, 0, seq))
    }

    /// The stash slot for a pointer-only re-encode. `spend_mark` takes the pointer's move mark
    /// and runs only once the slot is free to take: spent on a busy slot, a gesture's last step
    /// never reached the client.
    pub fn cursor_republish_slot(&self, spend_mark: impl FnOnce() -> bool) -> Option<usize> {
        let (slot, ..) = self.republish()?;
        spend_mark().then_some(slot)
    }

    /// The encoder takes the idle `slot`: out of `free`, its AU owed.
    pub fn hand_out(&mut self, slot: usize) {
        self.free.retain(|&s| s != slot);
        self.encoding.push(slot);
    }

    /// The encoder hands `slot` back, published or skipped. For [`DIRECT`] a release a timeout
    /// left owed settles the debt and frees nothing, so it never ends the hold taken since.
    /// `true` when a hold ended.
    pub fn release(&mut self, slot: usize) -> bool {
        if slot == DIRECT {
            if let Some(i) = self.encoding.iter().position(|&s| s == DIRECT) {
                self.encoding.swap_remove(i);
            }
            return self.owed.released() && self.unhold();
        }
        self.encoding.retain(|&s| s != slot);
        if !self.free.contains(&slot) {
            self.free.push(slot);
        }
        false
    }

    /// Every slot a detached encoder still held, free again. The debt goes with it, and with no
    /// consumer left the pool recycles instead of holding. `true` when a hold ended.
    pub fn reclaim(&mut self) -> bool {
        for slot in core::mem::take(&mut self.encoding) {
            if slot != DIRECT && !self.free.contains(&slot) {
                self.free.push(slot);
            }
        }
        self.owed.clear();
        self.live = false;
        self.unhold()
    }

    /// The drain worker's hold ran out. A surface still held is given up and its frame counted
    /// (the new drop total); if the encoder already took it, the release it still owes settles
    /// a debt instead of ending a later hold. `None` when the release won the race.
    pub fn hold_timed_out(&mut self) -> Option<u64> {
        if !self.unhold() {
            return None;
        }
        if self.encoding.contains(&DIRECT) {
            self.owed.timed_out();
        }
        Some(self.drop_one())
    }

    /// Give the hold up with the queue entry that names it. `true` when one was held.
    fn unhold(&mut self) -> bool {
        self.full.retain(|f| f.0 != DIRECT);
        core::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOTS: usize = 3;

    /// The drain worker's pass as the shell runs it, every pass succeeding: offer, then queue
    /// what was placed at QPC `seq * 10`.
    fn compose(s: &mut Slots, bypass: bool, seq: u64) -> Place {
        let place = s.offer(|| bypass);
        match place {
            Place::Slot { slot, .. } => s.queue(slot, seq * 10, seq),
            Place::Direct => s.queue(DIRECT, seq * 10, seq),
            Place::Drop(_) => {}
        }
        place
    }

    fn live() -> Slots {
        let mut s = Slots::new(SLOTS);
        s.set_live(true);
        s
    }

    /// Every slot in exactly one list, and [`DIRECT`] in none of the free ones.
    fn conserved(s: &Slots) {
        let mut all: Vec<usize> = s.free.clone();
        all.extend(s.full.iter().map(|f| f.0).filter(|&i| i != DIRECT));
        all.extend(s.encoding.iter().copied().filter(|&i| i != DIRECT));
        all.sort_unstable();
        assert_eq!(all, (0..SLOTS).collect::<Vec<_>>(), "{s:?}");
    }

    // Pins "fix(vdisplay/encode): keep the composed frame the pool would drop".
    #[test]
    fn a_recycle_under_a_live_consumer_counts_a_drop() {
        let mut s = live();
        for seq in 1..=3 {
            compose(&mut s, false, seq);
        }
        let Place::Slot {
            slot,
            recycled_drop,
        } = compose(&mut s, false, 4)
        else {
            panic!("a full pool recycles");
        };
        assert_eq!((slot, recycled_drop), (2, Some(1)), "the oldest frame goes");
        let seqs: Vec<u64> = s.full.iter().map(|f| f.2).collect();
        assert_eq!(seqs, [2, 3, 4], "the encoder reads the newest picture");
        let mut idle = Slots::new(SLOTS);
        for seq in 1..=4 {
            compose(&mut idle, false, seq);
        }
        assert_eq!(idle.dropped, 0, "nobody was there to lose a frame");
    }

    // Pins "fix(windows): close the 0.35.0 pre-release triage findings" (a slot lost per offer
    // between sessions): a free slot wins, and a failed pass puts its slot back.
    #[test]
    fn a_free_slot_always_wins_and_no_slot_is_lost() {
        let mut s = Slots::new(SLOTS);
        for seq in 1..=10 {
            compose(&mut s, false, seq);
            conserved(&s);
        }
        let mut s = live();
        compose(&mut s, false, 1);
        compose(&mut s, false, 2);
        assert_eq!(
            s.offer(|| false),
            Place::Slot {
                slot: 0,
                recycled_drop: None
            },
            "a free slot beats the queue",
        );
        assert_eq!(s.pass_failed(0), 1);
        conserved(&s);
    }

    // Pins "feat(vdisplay/driver): pool, encode thread, AU writer": the encoder holds two in
    // flight while the drain worker fills the third; with all three out a frame is dropped.
    #[test]
    fn a_frame_with_every_slot_out_is_dropped() {
        let mut s = live();
        for seq in 1..=3 {
            compose(&mut s, false, seq);
            assert!(s.take_within(1).0.is_some());
        }
        assert_eq!(s.offer(|| false), Place::Drop(1));
        s.release(1);
        assert!(matches!(s.offer(|| false), Place::Slot { slot: 1, .. }));
    }

    // Pins "fix(vdisplay/driver): open each session on the newest composed frame".
    #[test]
    fn a_session_opens_on_the_newest_queued_frame() {
        let mut s = Slots::new(SLOTS);
        for seq in 1..=4 {
            compose(&mut s, false, seq);
        }
        s.set_live(true);
        assert_eq!(s.newest_seq(), Some(4));
        assert_eq!(s.full.len(), 1, "only the newest frame is kept");
        assert_eq!(
            s.still_slot(),
            None,
            "a queued frame is newer than any still picture"
        );
        conserved(&s);
        // Nothing queued: the seed goes into the top free slot, which leaves `free`.
        let mut fresh = Slots::new(SLOTS);
        assert_eq!(fresh.newest_seq(), None);
        let slot = fresh.still_slot().expect("a free slot");
        fresh.queue(slot, 0, 7);
        assert_eq!(fresh.newest_seq(), Some(7));
        conserved(&fresh);
    }

    // Pins "perf(vdisplay/driver): read the acquired surface on AMF by default": a still desktop
    // takes one slot, which survives the session change as the next session's recovery frame.
    #[test]
    fn the_retained_slot_survives_a_session_change() {
        let mut s = live();
        let slot = s.still_slot().expect("an idle pool");
        s.retained(slot, 5);
        assert!(s.free.contains(&slot), "the stash stays idle");
        s.set_live(false);
        s.set_live(true);
        assert_eq!(s.republish(), Some((slot, 0, 5)));
    }

    // Pins "fix(vdisplay/driver): skip the zero-copy hold with no encoder reading".
    #[test]
    fn no_hold_without_an_encoder_reading() {
        let mut s = Slots::new(SLOTS);
        let mut asked = false;
        let place = s.offer(|| {
            asked = true;
            true
        });
        assert!(matches!(place, Place::Slot { .. }));
        assert!(!asked, "the pointer is not read for a pool nobody drains");
        let mut s = live();
        compose(&mut s, true, 1);
        s.take_within(1);
        assert!(s.reclaim());
        assert!(matches!(compose(&mut s, true, 2), Place::Slot { .. }));
        assert!(!s.held);
    }

    // Pins "fix(vdisplay/driver): hold the next acquire for a zero-copy encode": one surface
    // out at a time, the next dropped until the encoder gives it back.
    #[test]
    fn a_held_surface_drops_the_next_one() {
        let mut s = live();
        assert_eq!(compose(&mut s, true, 1), Place::Direct);
        assert_eq!(s.offer(|| true), Place::Drop(1), "a hold never nests");
        assert_eq!(
            s.offer(|| false),
            Place::Drop(2),
            "not even into a free slot"
        );
        assert_eq!(s.take_within(1).0, Some((DIRECT, 10, 1)));
        assert!(s.release(DIRECT), "the release ends the hold");
        assert_eq!(compose(&mut s, true, 2), Place::Direct);
    }

    // Pins "feat(vdisplay/driver): add the S6 pool-bypass spike behind a feature": DIRECT is
    // the surface itself, so no slot verb ever names it.
    #[test]
    fn a_direct_entry_is_never_handed_to_a_pass() {
        let mut s = live();
        compose(&mut s, true, 1);
        assert_eq!(s.take_within(1).0.map(|f| f.0), Some(DIRECT));
        assert_eq!(s.stash.map(|f| f.0), Some(DIRECT));
        assert_eq!(s.republish(), None, "the stash is the surface, not a slot");
        assert_eq!(s.cursor_republish_slot(|| true), None);
        assert_ne!(s.still_slot(), Some(DIRECT));
        s.release(DIRECT);
        compose(&mut s, true, 2);
        s.take_within(1);
        s.reclaim();
        assert!(!s.free.contains(&DIRECT));
        conserved(&s);
    }

    // Pins "fix(driver/windows): pair a bypass release with its own hold".
    #[test]
    fn a_release_a_timeout_left_owed_frees_nothing() {
        let mut s = live();
        compose(&mut s, true, 1);
        s.take_within(1);
        assert_eq!(s.hold_timed_out(), Some(1), "frame N given up and counted");
        assert!(
            matches!(compose(&mut s, true, 2), Place::Slot { .. }),
            "no hold while a release is owed",
        );
        s.take_within(1);
        assert!(!s.release(DIRECT), "N's late release settles the debt");
        s.release(2);
        assert_eq!(compose(&mut s, true, 3), Place::Direct);
        s.take_within(1);
        assert!(s.release(DIRECT), "the next hold's release ends it");
        // A timeout before the encoder took the surface owes nothing.
        compose(&mut s, true, 4);
        assert_eq!(s.hold_timed_out(), Some(2));
        assert!(s.full.is_empty());
        assert_eq!(compose(&mut s, true, 5), Place::Direct);
        assert_eq!(s.hold_timed_out(), Some(3));
        assert_eq!(s.hold_timed_out(), None, "the release won the race");
    }

    // Pins "fix(vdisplay): run the Windows panel at twice the stream rate": frames past the
    // stream-rate credit are shed oldest first, so the encoder takes the newest.
    #[test]
    fn frames_past_the_budget_are_shed_oldest_first() {
        let mut s = live();
        for seq in 1..=3 {
            compose(&mut s, false, seq);
        }
        let (f, shed, unhold) = s.take_within(1);
        assert_eq!((f.map(|f| f.2), shed, unhold), (Some(3), 2, false));
        assert_eq!(s.stash, f);
        conserved(&s);
    }

    // Pins "fix(vdisplay/driver): hold the next acquire for a zero-copy encode": a held
    // surface never waits on credit; the queued slots do.
    #[test]
    fn no_budget_sheds_a_held_surface_and_nothing_else() {
        let mut s = live();
        compose(&mut s, false, 1);
        assert_eq!(s.take_within(0), (None, 0, false), "slots wait for credit");
        compose(&mut s, true, 2);
        assert_eq!(s.take_within(0), (None, 1, true));
        let seqs: Vec<u64> = s.full.iter().map(|f| f.2).collect();
        assert_eq!(seqs, [1], "the queued slot stays");
        assert!(!s.held);
    }

    // Pins "feat(vdisplay/driver): ENCODE_CTL with detach on reset": a detached encoder's slots
    // all come back, its debt with them, and the pool stops holding.
    #[test]
    fn reclaim_frees_every_slot_and_leaves_the_pool_not_live() {
        let mut s = live();
        compose(&mut s, false, 1);
        s.take_within(1);
        compose(&mut s, true, 2);
        s.take_within(1);
        assert!(s.reclaim(), "the held surface goes back");
        conserved(&s);
        assert!(s.encoding.is_empty() && !s.live && !s.held);
        let mut s = live();
        compose(&mut s, true, 1);
        s.take_within(1);
        s.hold_timed_out();
        assert!(!s.reclaim(), "nothing held");
        assert!(s.owed.may_hold(), "the debt left with the encoder");
        for seq in 2..=5 {
            assert!(matches!(compose(&mut s, true, seq), Place::Slot { .. }));
        }
        assert_eq!(s.dropped, 1, "between sessions a recycle is free");
    }

    // Pins "fix(vdisplay/encode): re-encode the stash on a keyframe request".
    #[test]
    fn a_keyframe_republishes_only_an_idle_stash_with_nothing_queued() {
        let mut s = live();
        compose(&mut s, false, 1);
        let (Some((slot, ..)), ..) = s.take_within(1) else {
            panic!("a queued frame");
        };
        assert_eq!(s.republish(), None, "its AU is still owed");
        s.release(slot);
        assert_eq!(s.republish(), Some((slot, 0, 1)), "QPC 0: stamped with now");
        compose(&mut s, false, 2);
        assert_eq!(s.republish(), None, "a queued frame carries the keyframe");
        s.take_within(1);
        s.release(2);
        let (stash, ..) = s.republish().expect("idle again");
        s.hand_out(stash);
        assert!(
            !s.free.contains(&stash),
            "no pass can write the pixels being read"
        );
        assert_eq!(s.republish(), None);
        conserved(&s);
    }

    // Pins "fix(encode/windows): close the Windows encode sweep findings" and "fix(vdisplay/
    // driver): retry a dropped cursor-only frame": the move mark is spent once, on a slot the
    // re-encode can take.
    #[test]
    fn the_pointer_republish_spends_the_mark_once() {
        let mut s = live();
        compose(&mut s, false, 1);
        let (Some((slot, ..)), ..) = s.take_within(1) else {
            panic!("a queued frame");
        };
        let mut mark = true;
        let mut spend = |s: &Slots| s.cursor_republish_slot(|| core::mem::take(&mut mark));
        assert_eq!(spend(&s), None, "a busy stash keeps the mark");
        s.release(slot);
        assert_eq!(spend(&s), Some(slot));
        s.hand_out(slot);
        assert_eq!(spend(&s), None, "the stash is out again");
        // The cursor-only frame was dropped: its slot comes back and the move is re-marked.
        s.release(slot);
        assert_eq!(spend(&s), None, "no move, no re-encode");
        mark = true;
        assert_eq!(
            s.cursor_republish_slot(|| core::mem::take(&mut mark)),
            Some(slot)
        );
        assert!(!mark);
    }
}
