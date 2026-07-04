//! Pure per-display **lifecycle state machine** (design: `design/display-management.md` §3).
//!
//! One virtual display's earned refcount + linger + pin state, with **no I/O and no OS-specific
//! types** — the registry ([`super::registry`]) executes the side effects (backend create /
//! teardown / linger timer) that this machine's transitions dictate. Extracted so the lifecycle
//! logic is unit- and property-testable in isolation, and so the Linux registry and (later) the
//! Windows manager share one audited machine instead of each re-deriving refcount+linger by hand.
//!
//! It is the platform-neutral distillation of the model the Windows `VirtualDisplayManager` already
//! runs on glass: `Idle → Active{refs} → Lingering{until} → Idle`, plus a `Pinned` state for
//! keep-alive-forever. The registry pairs one [`State`] with the owned backend resource; the machine
//! only tracks the discriminant + refcount + deadline and reports what to do.

use std::time::Instant;

use super::policy::Linger;

/// The lifecycle state of one virtual-display slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum State {
    /// No display exists.
    #[default]
    Idle,
    /// A display exists with `refs` live sessions holding it.
    Active { refs: u32 },
    /// The last session left; the display is kept until `until`, then torn down.
    Lingering { until: Instant },
    /// The last session left; the display is kept indefinitely (keep-alive forever), until an
    /// explicit release.
    Pinned,
}

/// What acquiring a slot means for the backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquire {
    /// The slot was empty — the backend must CREATE a fresh display.
    Create,
    /// The slot was already Active — another session JOINS the live display (refcount++).
    Join,
    /// The slot was kept alive (Lingering/Pinned) — REUSE the existing display (re-attach capture).
    Reuse,
}

/// What releasing a hold on a slot means for the backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Another session still holds the display — nothing to do.
    Decref,
    /// The last session left; keep the display until its deadline ([`State::Lingering`]), then tear down.
    Linger,
    /// The last session left; keep the display indefinitely ([`State::Pinned`]).
    Pin,
    /// The last session left and keep-alive is off — tear the display down now.
    Teardown,
    /// A release with no live hold (stale/duplicate) — no-op.
    Noop,
}

impl State {
    /// True while a backend display resource exists (Active/Lingering/Pinned) — the registry holds
    /// the keepalive in exactly these states, and `Idle` means it has been dropped.
    pub fn has_display(self) -> bool {
        !matches!(self, State::Idle)
    }

    /// Number of live sessions holding the display (0 unless Active).
    pub fn refs(self) -> u32 {
        match self {
            State::Active { refs } => refs,
            _ => 0,
        }
    }

    /// A session acquires the slot. Transitions the state and reports whether the backend must
    /// create a fresh display, join the live one, or reuse the kept one.
    pub fn acquire(&mut self) -> Acquire {
        match *self {
            State::Idle => {
                *self = State::Active { refs: 1 };
                Acquire::Create
            }
            State::Active { refs } => {
                *self = State::Active { refs: refs + 1 };
                Acquire::Join
            }
            State::Lingering { .. } | State::Pinned => {
                *self = State::Active { refs: 1 };
                Acquire::Reuse
            }
        }
    }

    /// A session releases the slot. When the LAST session leaves, `now` + the resolved `linger`
    /// decide the kept state. Returns what the registry should do.
    pub fn release(&mut self, now: Instant, linger: Linger) -> Release {
        match *self {
            State::Active { refs } if refs > 1 => {
                *self = State::Active { refs: refs - 1 };
                Release::Decref
            }
            State::Active { .. } => match linger {
                Linger::Immediate => {
                    *self = State::Idle;
                    Release::Teardown
                }
                Linger::For(d) => {
                    *self = State::Lingering { until: now + d };
                    Release::Linger
                }
                Linger::Forever => {
                    *self = State::Pinned;
                    Release::Pin
                }
            },
            // Releasing a slot with no live hold is a stale/duplicate release. The registry's
            // gen-stamped leases already make a stale lease's drop a no-op before it reaches here;
            // this is the defensive backstop.
            State::Idle | State::Lingering { .. } | State::Pinned => Release::Noop,
        }
    }

    /// The registry's linger-timer tick: a Lingering slot past its deadline goes Idle and returns
    /// `true` (the registry tears the display down). Pinned and every other state are untouched.
    pub fn poll_expiry(&mut self, now: Instant) -> bool {
        match *self {
            State::Lingering { until } if now >= until => {
                *self = State::Idle;
                true
            }
            _ => false,
        }
    }

    /// Force-release a kept display (the `/display/release` endpoint): a Lingering/Pinned slot goes
    /// Idle and the registry tears it down (`true`). An Active slot is refused (`false`) — releasing
    /// a display that still has live sessions is session management, not display management. Idle → `false`.
    pub fn force_release(&mut self) -> bool {
        match *self {
            State::Lingering { .. } | State::Pinned => {
                *self = State::Idle;
                true
            }
            State::Active { .. } | State::Idle => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn create_join_reuse_and_teardown() {
        let mut s = State::default();
        assert_eq!(s.acquire(), Acquire::Create);
        assert_eq!(s, State::Active { refs: 1 });
        // A concurrent session joins.
        assert_eq!(s.acquire(), Acquire::Join);
        assert_eq!(s.refs(), 2);
        // One leaves — still active.
        let now = Instant::now();
        assert_eq!(s.release(now, Linger::Immediate), Release::Decref);
        assert_eq!(s.refs(), 1);
        // The last leaves with keep-alive off — teardown.
        assert_eq!(s.release(now, Linger::Immediate), Release::Teardown);
        assert_eq!(s, State::Idle);
        assert!(!s.has_display());
    }

    #[test]
    fn linger_then_reuse_within_window() {
        let mut s = State::default();
        let t0 = Instant::now();
        s.acquire();
        assert_eq!(
            s.release(t0, Linger::For(Duration::from_secs(10))),
            Release::Linger
        );
        assert!(s.has_display());
        // A tick before the deadline does nothing.
        assert!(!s.poll_expiry(t0 + Duration::from_secs(5)));
        // A reconnect inside the window reuses the kept display.
        assert_eq!(s.acquire(), Acquire::Reuse);
        assert_eq!(s, State::Active { refs: 1 });
    }

    #[test]
    fn linger_expires_to_teardown() {
        let mut s = State::default();
        let t0 = Instant::now();
        s.acquire();
        s.release(t0, Linger::For(Duration::from_secs(10)));
        // Past the deadline → teardown.
        assert!(s.poll_expiry(t0 + Duration::from_secs(11)));
        assert_eq!(s, State::Idle);
        // A second tick is idempotent (nothing to tear down).
        assert!(!s.poll_expiry(t0 + Duration::from_secs(12)));
    }

    #[test]
    fn pinned_never_expires_but_force_releases() {
        let mut s = State::default();
        let t0 = Instant::now();
        s.acquire();
        assert_eq!(s.release(t0, Linger::Forever), Release::Pin);
        assert_eq!(s, State::Pinned);
        // No amount of ticking tears a pinned display down.
        assert!(!s.poll_expiry(t0 + Duration::from_secs(86_400)));
        assert!(s.has_display());
        // Only an explicit release does.
        assert!(s.force_release());
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn force_release_refuses_active() {
        let mut s = State::default();
        s.acquire();
        assert!(
            !s.force_release(),
            "an active display can't be force-released"
        );
        assert_eq!(s.refs(), 1);
        // Idle also can't.
        let mut idle = State::default();
        assert!(!idle.force_release());
    }

    #[test]
    fn stale_release_is_noop() {
        let mut s = State::default();
        assert_eq!(s.release(Instant::now(), Linger::Immediate), Release::Noop);
        assert_eq!(s, State::Idle);
    }

    /// Property test (deterministic seeded walk): across an arbitrary interleaving of acquire /
    /// release / expiry-tick / force-release, the machine must never (a) leak or double-free the
    /// backend resource — `has_display()` must exactly track a shadow "resource alive" flag, with
    /// every Create preceded by no live resource and every teardown preceded by one — nor (b)
    /// underflow the refcount, nor (c) tear a display down while a session still holds it.
    #[test]
    fn property_no_leaks_no_double_free_no_underflow() {
        // Tiny deterministic LCG (Numerical Recipes) — reproducible, no dependency.
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng >> 33) as u32
        };

        let base = Instant::now();
        let mut logical_ms: u64 = 0;
        let mut s = State::default();
        // Shadow model.
        let mut resource_alive = false;
        let mut live_holds: u32 = 0;

        for _ in 0..200_000 {
            // Advance logical time by 0..2000 ms each step so lingers cross their deadlines.
            logical_ms += (next() % 2000) as u64;
            let now = base + Duration::from_millis(logical_ms);

            match next() % 5 {
                0 => {
                    // acquire
                    let before_alive = resource_alive;
                    let a = s.acquire();
                    match a {
                        Acquire::Create => {
                            assert!(!before_alive, "Create while a resource was alive")
                        }
                        Acquire::Join | Acquire::Reuse => {
                            assert!(before_alive, "Join/Reuse with no live resource")
                        }
                    }
                    resource_alive = true;
                    live_holds += 1;
                }
                1 | 2 => {
                    // release (weighted 2/5 so refs actually drain)
                    let linger = match next() % 3 {
                        0 => Linger::Immediate,
                        1 => Linger::For(Duration::from_millis((next() % 3000) as u64 + 1)),
                        _ => Linger::Forever,
                    };
                    let held_before = live_holds;
                    let r = s.release(now, linger);
                    match r {
                        Release::Noop => assert_eq!(held_before, 0, "Noop only with no live hold"),
                        Release::Decref => {
                            assert!(held_before >= 2, "Decref must leave the display held");
                            live_holds -= 1;
                        }
                        Release::Teardown => {
                            assert_eq!(held_before, 1, "Teardown only on the last hold");
                            live_holds = 0;
                            resource_alive = false;
                        }
                        Release::Linger | Release::Pin => {
                            assert_eq!(held_before, 1, "Linger/Pin only on the last hold");
                            live_holds = 0;
                            // resource stays alive (kept)
                        }
                    }
                }
                3 => {
                    // expiry tick
                    if s.poll_expiry(now) {
                        assert_eq!(live_holds, 0, "expiry tore down a held display");
                        resource_alive = false;
                    }
                }
                _ => {
                    // force release
                    if s.force_release() {
                        assert_eq!(live_holds, 0, "force-release tore down a held display");
                        resource_alive = false;
                    }
                }
            }

            // Invariant after every step: the machine's own view of "a display exists" matches the
            // shadow, and the refcount matches the live-hold count.
            assert_eq!(
                s.has_display(),
                resource_alive,
                "has_display drifted from the shadow model"
            );
            assert_eq!(
                s.refs(),
                live_holds,
                "refs drifted from the live-hold count"
            );
        }
    }
}
