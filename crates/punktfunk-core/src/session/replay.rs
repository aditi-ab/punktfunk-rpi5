//! Sliding-window anti-replay filter over the AEAD-authenticated wire sequence
//! (plan §1). Applied on both encrypted receive paths —
//! [`Session::poll_frame`](super::Session::poll_frame) and
//! [`Session::poll_input`](super::Session::poll_input).

/// Extract the AEAD-authenticated 8-byte big-endian sequence prefix from a sealed wire datagram.
/// Only called on the encrypted receive path, where a preceding successful open has already
/// established `wire.len() >= 8`.
pub(super) fn seq_of(wire: &[u8]) -> u64 {
    u64::from_be_bytes(wire[..8].try_into().unwrap())
}

/// Depth of the anti-replay window, in sequences. The sender advances its sequence once per
/// datagram, so this must cover the reassembler's 120 ms loss window
/// ([`LOSS_WINDOW_NS`](crate::packet)) at line-rate packet rates — otherwise the replay filter
/// silently re-tightens the "late ≠ lost" fix: a Wi-Fi-retry-delayed shard the reassembler would
/// still use gets dropped here as "older than the window" first (4096 was only ~33 ms at the
/// ~125k pkt/s of a 1 Gbps stream; 32768 topped out around ~2 Gbps — which the client now
/// exceeds: the 2026-07-14 zero-copy + hardware-AES work measured ~4.8 Gbps wire ≈ 430k pkt/s
/// delivered). 131072 covers 120 ms up to ~1.09M pkt/s (≈12 Gbps wire) and is effectively
/// unbounded for the sparse input stream, while still bounding how far back a replay could
/// hide; the bitmap costs 16 KiB per session.
const REPLAY_WINDOW: u64 = 131072;
const REPLAY_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

/// Sliding-window anti-replay filter over the AEAD-authenticated wire sequence. The sender counts
/// its datagrams from 0, and the protocol never legitimately re-sends a sequence (FEC recovery
/// shards get fresh ones), so a sequence seen twice is a replay. The AEAD tag already authenticates
/// the sequence — a forged one can't open — so this only has to reject *duplicates* of validly
/// sealed datagrams (and anything older than the window, which we can no longer prove is fresh).
/// Genuine reordering within the window is accepted. Bitmap-per-sequence, indexed `seq % WINDOW`.
pub(super) struct ReplayWindow {
    /// Highest sequence accepted so far; `seen` stays false until the first datagram.
    highest: u64,
    seen: bool,
    /// One bit per in-window sequence in `(highest - WINDOW, highest]`.
    bits: [u64; REPLAY_WORDS],
}

impl ReplayWindow {
    pub(super) fn new() -> ReplayWindow {
        ReplayWindow {
            highest: 0,
            seen: false,
            bits: [0; REPLAY_WORDS],
        }
    }

    #[inline]
    fn word_bit(seq: u64) -> (usize, u64) {
        let idx = (seq % REPLAY_WINDOW) as usize;
        (idx / 64, 1u64 << (idx % 64))
    }
    fn is_set(&self, seq: u64) -> bool {
        let (w, b) = Self::word_bit(seq);
        self.bits[w] & b != 0
    }
    fn set(&mut self, seq: u64) {
        let (w, b) = Self::word_bit(seq);
        self.bits[w] |= b;
    }

    /// Clear the slots for sequences `from..to` (exclusive), word at a time: a masked edit on the
    /// (possibly partial) word at each step, whole-word stores in the interior. The old
    /// bit-at-a-time clear ran up to `REPLAY_WINDOW - 1` iterations for one valid large in-window
    /// forward jump — a bounded but pointless CPU spike an AEAD-valid peer could repeat
    /// (security-review 2026-08-31 L-3). Callers guarantee `to - from < REPLAY_WINDOW`, so the
    /// range spans distinct ring bits and never aliases itself, and the result is bit-for-bit the
    /// per-sequence clear it replaces.
    fn clear_range(&mut self, from: u64, to: u64) {
        debug_assert!(to.saturating_sub(from) < REPLAY_WINDOW);
        let mut s = from;
        while s < to {
            let bit = (s % 64) as u32; // position within this sequence's ring word
            let run = (64 - u64::from(bit)).min(to - s); // bits cleared in this word this step
            let mask = if run == 64 {
                u64::MAX
            } else {
                ((1u64 << run) - 1) << bit
            };
            self.bits[Self::word_bit(s).0] &= !mask;
            s += run;
        }
    }

    /// Record `seq`, returning `true` if it's fresh (accept) or `false` if it's a replay / too old.
    pub(super) fn accept(&mut self, seq: u64) -> bool {
        if !self.seen {
            self.seen = true;
            self.highest = seq;
            self.set(seq);
            return true;
        }
        if seq > self.highest {
            // Advance the window. Sequences between the old and new high slide in unseen, so clear
            // their (possibly stale, from a full window ago) slots — unless we jumped an entire
            // window, in which case wipe the bitmap wholesale.
            if seq - self.highest >= REPLAY_WINDOW {
                self.bits = [0; REPLAY_WORDS];
            } else {
                self.clear_range(self.highest + 1, seq);
            }
            self.highest = seq;
            self.set(seq);
            true
        } else if self.highest - seq >= REPLAY_WINDOW || self.is_set(seq) {
            // Older than the window (can't prove it isn't a replay) or already seen (a duplicate) —
            // either way, drop it.
            false
        } else {
            self.set(seq); // in-window and not yet seen — a genuine reorder
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_in_order_and_rejects_duplicates() {
        let mut w = ReplayWindow::new();
        for seq in 0..1000 {
            assert!(w.accept(seq), "fresh in-order seq {seq} must be accepted");
        }
        // Every one of those is now a replay.
        for seq in 0..1000 {
            assert!(!w.accept(seq), "replayed seq {seq} must be rejected");
        }
    }

    #[test]
    fn accepts_reorder_within_window_once() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        // Earlier-but-in-window sequences (a genuine reorder) are accepted exactly once.
        assert!(w.accept(80));
        assert!(!w.accept(80), "second copy of a reordered seq is a replay");
        assert!(w.accept(99));
        assert!(
            !w.accept(100),
            "the high-water seq itself can't be replayed"
        );
    }

    #[test]
    fn rejects_older_than_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(REPLAY_WINDOW * 2));
        // Anything a full window or more behind the high-water mark is dropped (can't prove fresh).
        assert!(!w.accept(REPLAY_WINDOW * 2 - REPLAY_WINDOW));
        assert!(!w.accept(0));
        // But just inside the window is still accepted.
        assert!(w.accept(REPLAY_WINDOW * 2 - (REPLAY_WINDOW - 1)));
    }

    #[test]
    fn large_forward_jump_wipes_stale_bits() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(5));
        // Jump far forward (more than a window). The slot for an old seq that aliases 5 mod WINDOW
        // must read as unseen afterward, i.e. the jump cleared it — so a NEW seq there is accepted.
        let far = 10 * REPLAY_WINDOW + 5;
        assert!(w.accept(far));
        assert!(
            !w.accept(5),
            "the pre-jump seq is now far older than the window"
        );
        // A fresh seq aliasing 5 (mod WINDOW) but inside the new window is accepted, proving the
        // stale bit was cleared rather than mistaken for a replay.
        assert!(w.accept(far - REPLAY_WINDOW + 1));
    }

    /// A large-but-in-window forward jump clears exactly the slid-in slots word-wise: bits below
    /// the jump stay set (still replays), a fresh seq inside the jumped span is accepted, and the
    /// stale slot a full window before the new high reads as clear. Guards `clear_range` against
    /// off-by-one/masking bugs the bit-at-a-time loop couldn't have (security-review 2026-08-31
    /// L-3).
    #[test]
    fn large_in_window_jump_clears_word_wise() {
        let mut w = ReplayWindow::new();
        // Seed a scattering of low sequences (partial + whole ring words touched).
        for seq in [0u64, 1, 63, 64, 100, 4000] {
            assert!(w.accept(seq));
        }
        // Jump most of a window forward in one step — the slow path the fix targets.
        let hi = REPLAY_WINDOW - 1;
        assert!(w.accept(hi));
        // A seq that slid in during the jump is fresh (its slot was cleared).
        assert!(w.accept(50_000));
        assert!(!w.accept(50_000), "…and now a replay");
        // The seeded low seqs are still within the window (< REPLAY_WINDOW behind hi) and remain
        // replays — the clear must not have touched them.
        for seq in [1u64, 63, 64, 100, 4000] {
            assert!(!w.accept(seq), "seeded seq {seq} must still read as seen");
        }
        // The high-water seq itself is never replayable.
        assert!(!w.accept(hi));
    }

    #[test]
    fn first_seq_need_not_be_zero() {
        // Startup loss can mean the first datagram we ever open isn't seq 0.
        let mut w = ReplayWindow::new();
        assert!(w.accept(42));
        assert!(!w.accept(42));
        assert!(w.accept(43));
    }

    #[test]
    fn seq_of_reads_the_big_endian_prefix() {
        let mut wire = 0x0102_0304_0506_0708u64.to_be_bytes().to_vec();
        wire.extend_from_slice(b"ciphertext-and-tag");
        assert_eq!(seq_of(&wire), 0x0102_0304_0506_0708);
    }
}
