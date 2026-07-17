//! Shared virtual-pad creation-retry policy, used by every backend manager (Linux uinput/uhid,
//! Windows XUSB/UMDF). See [`PadGate`].

use std::time::{Duration, Instant};

/// Backoff after the first failed pad creation…
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// …doubling on each consecutive failure, capped here so a persistently-broken host retries at most
/// this often (a negligible cost) while still self-healing within one window of the fix.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Create-retry gate shared by every virtual-pad manager.
///
/// Each backend used to carry a `broken: bool` that latched permanently on the FIRST pad-creation
/// error, so a single transient failure — a startup race on `/dev/uinput`, a momentary `EBUSY`, the
/// Windows companion driver not yet ready — disabled EVERY controller for the rest of the session,
/// even after the underlying cause cleared. `PadGate` replaces that latch with capped exponential
/// backoff:
///
/// * After a failure, creation is blocked only until the backoff elapses — so the manager does not
///   re-attempt (and re-log) on every one of the 60–240 input frames a second — then a single
///   retry is permitted.
/// * A success clears the backoff, so the next failure starts fresh from [`FIRST_BACKOFF`].
/// * Consecutive failures widen the window, doubling up to [`MAX_BACKOFF`].
///
/// Even a genuinely broken setup (bad `/dev/uinput` permissions, missing Windows driver) therefore
/// self-heals within [`MAX_BACKOFF`] of the fix — a udev-rule reload, a driver install, the next
/// client connect — with no host restart, while costing at most one failed syscall plus one log
/// line per backoff window. The gate is manager-wide (not per slot), matching the old `broken`
/// flag: these failures are systemic (device-node permissions, absent driver), not per-controller.
#[derive(Debug, Default)]
pub struct PadGate {
    /// When the current backoff ends. `None` = creation is allowed right now.
    retry_at: Option<Instant>,
    /// Current backoff length: `ZERO` until the first failure, then [`FIRST_BACKOFF`] doubling
    /// toward [`MAX_BACKOFF`].
    backoff: Duration,
}

impl PadGate {
    /// A gate that permits creation immediately (no failures recorded yet).
    pub fn new() -> PadGate {
        PadGate::default()
    }

    /// May a pad be created at `now`? `true` unless a post-failure backoff is still in effect.
    pub fn allow(&self, now: Instant) -> bool {
        match self.retry_at {
            None => true,
            Some(t) => now >= t,
        }
    }

    /// Record a successful pad creation — clear the backoff so the next failure starts fresh.
    pub fn on_success(&mut self) {
        self.retry_at = None;
        self.backoff = Duration::ZERO;
    }

    /// Record a failed pad creation at `now` — arm the next retry a capped-exponential backoff out.
    pub fn on_failure(&mut self, now: Instant) {
        self.backoff = if self.backoff.is_zero() {
            FIRST_BACKOFF
        } else {
            (self.backoff * 2).min(MAX_BACKOFF)
        };
        self.retry_at = Some(now + self.backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_gate_allows_creation() {
        assert!(PadGate::new().allow(Instant::now()));
    }

    #[test]
    fn failure_blocks_until_backoff_elapses_then_allows_one_retry() {
        let t0 = Instant::now();
        let mut g = PadGate::new();
        g.on_failure(t0);
        // Blocked for the whole first-backoff window…
        assert!(!g.allow(t0));
        assert!(!g.allow(t0 + FIRST_BACKOFF - Duration::from_millis(1)));
        // …then a single retry is permitted.
        assert!(g.allow(t0 + FIRST_BACKOFF));
    }

    #[test]
    fn consecutive_failures_double_the_backoff_up_to_the_cap() {
        let t0 = Instant::now();
        let mut g = PadGate::new();
        g.on_failure(t0); // window = 1s
        g.on_failure(t0); // window = 2s
        assert!(!g.allow(t0 + FIRST_BACKOFF)); // still blocked at 1s — the window is now 2s
        assert!(g.allow(t0 + 2 * FIRST_BACKOFF));
        // Drive well past the cap and confirm the window never exceeds MAX_BACKOFF.
        for _ in 0..20 {
            g.on_failure(t0);
        }
        assert!(!g.allow(t0 + MAX_BACKOFF - Duration::from_millis(1)));
        assert!(g.allow(t0 + MAX_BACKOFF));
    }

    #[test]
    fn success_resets_the_backoff() {
        let t0 = Instant::now();
        let mut g = PadGate::new();
        g.on_failure(t0);
        g.on_failure(t0); // window grown to 2s
        g.on_success();
        // Success clears the backoff: creation is immediately allowed again.
        assert!(g.allow(t0));
        // The next failure starts from FIRST_BACKOFF, not the grown value.
        g.on_failure(t0);
        assert!(!g.allow(t0 + FIRST_BACKOFF - Duration::from_millis(1)));
        assert!(g.allow(t0 + FIRST_BACKOFF));
    }
}
