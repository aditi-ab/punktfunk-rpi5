//! Cross-crate topology-churn deadline.
//!
//! The exclusive-topology reassert (`pf-vdisplay` manager) opens a window before it
//! evicts or restores displays. The IDD-push descriptor follower (`pf-capture`) skips
//! samples while [`held`] is true: a reassert bounce is a transient mode, and acting
//! on it recreates the capture ring at a mode the recovery is about to undo. The
//! generation-keyed recovery rebuild (`recreate_ring_in_place`) is not gated — only
//! passive following is.
//!
//! [`hold`] is a deadline, not a flag: it self-expires so a holder that dies mid-churn
//! cannot wedge following off. [`release`] expires early when the watchdog sees a
//! stable topology.
//!
//! Contract test: `hold_release_expire`.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    OnceLock,
};
use std::time::{Duration, Instant};

/// `0` = no hold.
static HOLD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// `Instant` cannot live in an atomic.
fn clock() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// `fetch_max`: a reassert racing a slot transition must not shorten the other hold.
pub fn hold(dur: Duration) {
    let until = clock().saturating_add(dur.as_millis() as u64);
    HOLD_UNTIL_MS.fetch_max(until, Ordering::Relaxed);
}

pub fn release() {
    HOLD_UNTIL_MS.store(0, Ordering::Relaxed);
}

#[must_use]
pub fn held() -> bool {
    clock() < HOLD_UNTIL_MS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
