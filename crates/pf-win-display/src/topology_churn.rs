//! Cross-crate "topology churn in flight" latch (stall-immunity program): the exclusive-topology
//! reassert watchdog (pf-vdisplay's manager) announces that it is evicting/restoring displays, and
//! the IDD-push capturer's descriptor follower (pf-capture) defers acting on descriptor changes
//! while the window is open.
//!
//! Why: the reassert's forced re-commit transiently bounces the virtual display's mode — the
//! descriptor poller can sample the EVICTION state (field log 2026-07-27 10:30:44Z: `hdr=true` →
//! `hdr=false` → recreate → recovery restores `hdr=true` → second recreate). Every descriptor
//! sampled inside the window is potentially that transient, and acting on it recreates the ring at
//! a mode the recovery chain is about to undo. The deliberate recovery rebuild
//! (`recreate_ring_in_place`, keyed off the reassert generation) is NOT affected — only the
//! passive descriptor-following is.
//!
//! Deadline semantics, not a flag: a `hold()` self-expires, so a holder that dies mid-churn (or a
//! release lost to a teardown race) can never wedge descriptor-following off forever. `release()`
//! just expires the deadline early on the watchdog's "stable again" observation.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock,
};
use std::time::{Duration, Instant};

/// Deadline as milliseconds on the process-local [`clock`]; `0` = no hold.
static HOLD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// Process-local monotonic epoch (an `Instant` cannot live in an atomic).
fn clock() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Open (or extend) the churn window for `dur` from now. `fetch_max` so overlapping holders — a
/// reassert round racing a slot transition — never shorten each other's window.
pub fn hold(dur: Duration) {
    let until = clock().saturating_add(dur.as_millis() as u64);
    HOLD_UNTIL_MS.fetch_max(until, Ordering::Relaxed);
}

/// Expire the window now (the watchdog observed a stable topology again).
pub fn release() {
    HOLD_UNTIL_MS.store(0, Ordering::Relaxed);
}

/// Is a churn window open? Cheap enough for a per-frame path (one atomic load + a monotonic read).
#[must_use]
pub fn held() -> bool {
    clock() < HOLD_UNTIL_MS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The latch's contract end-to-end: closed at rest, open after `hold`, extended by the longer
    /// of two overlapping holds, closed by `release`, and self-expiring without one.
    #[test]
    fn hold_release_expire() {
        release();
        assert!(!held());
        hold(Duration::from_secs(60));
        assert!(held());
        // A shorter overlapping hold must not shorten the window.
        hold(Duration::from_millis(1));
        assert!(held());
        release();
        assert!(!held());
        // Self-expiry: a millisecond-scale hold lapses on its own.
        hold(Duration::from_millis(30));
        assert!(held());
        std::thread::sleep(Duration::from_millis(60));
        assert!(!held());
    }
}
