//! The rumble policy engine — one place, all platforms (`design/rumble-root-fix.md` §D).
//!
//! Historically every client re-implemented the *when/what-level* decisions (lease deadlines,
//! legacy staleness, actuator keepalives, stop-on-close), each with its own magic numbers
//! (Apple 1.6 s, Android 60 s, SDL 1.5 s, Deck 1 s + 40 ms) — five parallel policy forks, five
//! places for a stuck-rumble bug. The engine consumes seq-gated wire updates and emits
//! **effective actuator commands**: embedders never see a TTL, never own a deadline, never invent
//! a staleness constant. A platform keeps only *how to make this actuator vibrate* plus a small
//! [`ActuatorQuirks`] declaration (decay keepalive, duration floor) that parameterizes the shared
//! engine instead of forking it.
//!
//! Command sources, in priority order: an expiry zero (v2 lease ran out unrenewed — the host-died
//! safety net), a legacy-staleness zero (no-TTL host went quiet past [`LEGACY_STALE_MS`]), the
//! current level on every wire update (renewals re-emit so duration-parameterized platform APIs
//! keep getting re-armed — exactly the pre-engine cadence), and quirk keepalives (an actuator
//! whose hardware decays between renewals, e.g. the Deck's, gets sub-renewal re-kicks with an
//! optional 1-LSB jitter to defeat SDL's identical-value dedupe). On connection close the engine
//! drains one zero per still-buzzing pad before reporting closed, so every platform silences on
//! detach by contract.
//!
//! The engine replaces the bounded `RUMBLE_QUEUE` ring for embedders on the command API: state is
//! a per-pad mailbox and commands are generated on demand, so a stalled embedder wakes to ONE
//! current-level command instead of a backlog — and a stop can never be the update that an
//! overflowing queue drops.

use crate::input::MAX_PADS;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// The uniform no-TTL-host staleness bound: a legacy host refreshes state every 500 ms, so two
/// missed refreshes = quiet host → silence. Replaces the per-platform zoo (1.6 s / 60 s / 1.5 s /
/// 1 s), and matches the ratio the Steam Deck ceiling shipped with.
pub const LEGACY_STALE_MS: u64 = 1000;

/// Backstop duration handed to duration-parameterized platform APIs against a legacy host (the
/// engine's staleness zero lands at 1 s; this is the hardware-level net under an engine stall).
const BACKSTOP_LEGACY_MS: u32 = 2000;

/// One effective actuator command. `(0, 0)` means stop now. `backstop_ms` is a safety-net
/// duration for platform APIs that take one (SDL rumble, Android one-shots): the engine emits
/// explicit zeros at every policy stop, so the backstop only matters if the embedder thread itself
/// stalls; platforms with explicit-stop APIs ignore it. Zero commands carry `backstop_ms == 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RumbleCommand {
    pub pad: u16,
    pub low: u16,
    pub high: u16,
    pub backstop_ms: u32,
}

/// A physical actuator's declared quirks — how a platform parameterizes the shared policy instead
/// of forking it. Defaults (all zero/false) describe a well-behaved actuator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActuatorQuirks {
    /// Re-emit an unchanged non-zero level every this many ms — for actuators whose hardware
    /// output decays between wire renewals (Steam Deck ≈ 40, macOS DualSense-over-HID BT ≈ 900).
    /// `0` = no keepalive (the common case).
    pub keepalive_ms: u16,
    /// Floor for `backstop_ms` on non-zero commands (Android's `createOneShot` throws on 0).
    pub min_pulse_ms: u16,
    /// Alternate the low motor's LSB on keepalive re-emits (imperceptible) so an SDL-class layer
    /// that no-ops identical values still writes the device — the Deck's dedupe-defeat.
    pub dedup_jitter: bool,
}

#[derive(Clone, Copy)]
struct PadState {
    level: (u16, u16),
    /// v2 lease expiry — `None` for a zero level or a legacy pad.
    deadline: Option<Instant>,
    /// Last v2 TTL (drives the backstop); 0 ⇔ legacy.
    ttl_ms: u16,
    /// Last wire arrival iff the pad is driven by a legacy (no-TTL) host — the staleness clock.
    legacy_wire: Option<Instant>,
    /// A wire update landed since the last emit (level change OR renewal — renewals re-emit).
    dirty: bool,
    next_keepalive: Option<Instant>,
    /// Current jitter phase (see [`ActuatorQuirks::dedup_jitter`]).
    jitter: bool,
    quirks: ActuatorQuirks,
}

impl PadState {
    const NEUTRAL: PadState = PadState {
        level: (0, 0),
        deadline: None,
        ttl_ms: 0,
        legacy_wire: None,
        dirty: false,
        next_keepalive: None,
        jitter: false,
        quirks: ActuatorQuirks {
            keepalive_ms: 0,
            min_pulse_ms: 0,
            dedup_jitter: false,
        },
    };

    fn backstop(&self) -> u32 {
        let b = if self.ttl_ms > 0 {
            (2 * self.ttl_ms as u32).clamp(500, 5000)
        } else {
            BACKSTOP_LEGACY_MS
        };
        b.max(self.quirks.min_pulse_ms as u32)
    }

    /// Zero the pad's level + timers and produce the stop command.
    fn silence(&mut self, pad: u16) -> RumbleCommand {
        self.level = (0, 0);
        self.deadline = None;
        self.legacy_wire = None;
        self.next_keepalive = None;
        self.dirty = false;
        RumbleCommand {
            pad,
            low: 0,
            high: 0,
            backstop_ms: 0,
        }
    }
}

/// The pure per-connection policy state machine. Time is always passed in (`now`) so the policy
/// is deterministic and table-testable — the [`RumbleShared`] wrapper owns the real clock.
pub(crate) struct RumbleEngine {
    pads: [PadState; MAX_PADS],
}

fn merge_wake(wake: &mut Option<Instant>, t: Instant) {
    *wake = Some(wake.map_or(t, |w| w.min(t)));
}

impl RumbleEngine {
    pub(crate) fn new() -> RumbleEngine {
        RumbleEngine {
            pads: [PadState::NEUTRAL; MAX_PADS],
        }
    }

    /// Fold one seq-gated wire update in. Every update dirties the pad (renewals re-emit so
    /// platform duration timers re-arm); a v2 update replaces the lease deadline, a legacy update
    /// refreshes the staleness clock.
    pub(crate) fn wire_update(
        &mut self,
        now: Instant,
        pad: u16,
        low: u16,
        high: u16,
        ttl_ms: Option<u16>,
    ) {
        let Some(p) = self.pads.get_mut(pad as usize) else {
            return;
        };
        p.level = (low, high);
        p.dirty = true;
        match ttl_ms {
            Some(t) => {
                p.ttl_ms = t;
                p.legacy_wire = None;
                p.deadline = if (low, high) != (0, 0) {
                    Some(now + Duration::from_millis(t as u64))
                } else {
                    None
                };
            }
            None => {
                p.ttl_ms = 0;
                p.deadline = None;
                p.legacy_wire = Some(now);
            }
        }
    }

    pub(crate) fn set_quirks(&mut self, pad: u16, q: ActuatorQuirks) {
        if let Some(p) = self.pads.get_mut(pad as usize) {
            p.quirks = q;
            // A changed cadence re-derives from the next emit/poll; a stale scheduled kick from
            // the old cadence is harmless, a cleared quirk must cancel it.
            if q.keepalive_ms == 0 {
                p.next_keepalive = None;
            }
        }
    }

    /// Produce the next due command at `now`, if any, plus the earliest future instant something
    /// becomes due (the caller's wake deadline; `None` = nothing scheduled, wait for wire input).
    pub(crate) fn poll(&mut self, now: Instant) -> (Option<RumbleCommand>, Option<Instant>) {
        let mut wake: Option<Instant> = None;
        for i in 0..MAX_PADS {
            let p = &mut self.pads[i];
            let pad = i as u16;
            if p.level != (0, 0) {
                // 1) v2 lease expiry — the host stopped renewing (died / stopped caring). This
                // firing in the wild is the signature of a host-side bug: worth a log line.
                if let Some(d) = p.deadline {
                    if now >= d {
                        tracing::info!(pad, "rumble: envelope expired unrenewed — silencing");
                        return (Some(p.silence(pad)), None);
                    }
                    merge_wake(&mut wake, d);
                }
                // 2) legacy-host staleness — the uniform replacement for every per-platform bound.
                if let Some(lw) = p.legacy_wire {
                    let stale_at = lw + Duration::from_millis(LEGACY_STALE_MS);
                    if now >= stale_at {
                        tracing::debug!(pad, "rumble: legacy host went quiet — silencing");
                        return (Some(p.silence(pad)), None);
                    }
                    merge_wake(&mut wake, stale_at);
                }
            }
            // 3) a wire update to relay (level change or renewal re-arm).
            if p.dirty {
                p.dirty = false;
                if p.level == (0, 0) {
                    return (Some(p.silence(pad)), None);
                }
                if p.quirks.keepalive_ms > 0 {
                    p.next_keepalive =
                        Some(now + Duration::from_millis(p.quirks.keepalive_ms as u64));
                }
                let (low, high) = p.level;
                return (
                    Some(RumbleCommand {
                        pad,
                        low,
                        high,
                        backstop_ms: p.backstop(),
                    }),
                    None,
                );
            }
            // 4) actuator-decay keepalive, bounded by (1)/(2) above by construction: an expired
            // or stale pad was silenced before reaching here, so a keepalive can never sustain a
            // level the policy has ended.
            if p.level != (0, 0) && p.quirks.keepalive_ms > 0 {
                let ka = Duration::from_millis(p.quirks.keepalive_ms as u64);
                let due = *p.next_keepalive.get_or_insert(now + ka);
                if now >= due {
                    p.next_keepalive = Some(now + ka);
                    let (mut low, high) = p.level;
                    if p.quirks.dedup_jitter {
                        p.jitter = !p.jitter;
                        low ^= p.jitter as u16;
                    }
                    return (
                        Some(RumbleCommand {
                            pad,
                            low,
                            high,
                            backstop_ms: p.backstop(),
                        }),
                        None,
                    );
                }
                merge_wake(&mut wake, due);
            }
        }
        (None, wake)
    }

    /// Close drain: one stop per still-buzzing pad (call until `None`), so a session drop mid-buzz
    /// silences every platform by contract instead of by per-client accident.
    pub(crate) fn close_drain(&mut self) -> Option<RumbleCommand> {
        for i in 0..MAX_PADS {
            if self.pads[i].level != (0, 0) {
                return Some(self.pads[i].silence(i as u16));
            }
        }
        None
    }
}

/// The engine behind a lock + condvar — the demux thread feeds it, one embedder thread polls it.
pub(crate) struct RumbleShared {
    inner: Mutex<SharedState>,
    cv: Condvar,
}

struct SharedState {
    engine: RumbleEngine,
    closed: bool,
}

/// Sets `closed` (and wakes the poller) when the owner — the datagram demux task — ends for any
/// reason, so the embedder's command poll always observes connection teardown.
pub(crate) struct RumbleFeed(pub(crate) std::sync::Arc<RumbleShared>);

impl RumbleFeed {
    pub(crate) fn wire_update(&self, pad: u16, low: u16, high: u16, ttl_ms: Option<u16>) {
        let mut g = self.0.inner.lock().unwrap();
        g.engine.wire_update(Instant::now(), pad, low, high, ttl_ms);
        drop(g);
        self.0.cv.notify_all();
    }
}

impl Drop for RumbleFeed {
    fn drop(&mut self) {
        self.0.inner.lock().unwrap().closed = true;
        self.0.cv.notify_all();
    }
}

impl RumbleShared {
    pub(crate) fn new() -> RumbleShared {
        RumbleShared {
            inner: Mutex::new(SharedState {
                engine: RumbleEngine::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn set_quirks(&self, pad: u16, q: ActuatorQuirks) {
        self.inner.lock().unwrap().engine.set_quirks(pad, q);
        self.cv.notify_all();
    }

    /// Block up to `timeout` for the next effective command. `Ok(Some)` = a command, `Ok(None)` =
    /// timeout, `Err(Closed)` = the connection ended AND the close-drain zeros were all delivered.
    pub(crate) fn next_command(&self, timeout: Duration) -> Result<Option<RumbleCommand>, Closed> {
        let overall = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            let now = Instant::now();
            let (cmd, wake) = g.engine.poll(now);
            if let Some(c) = cmd {
                return Ok(Some(c));
            }
            if g.closed {
                return match g.engine.close_drain() {
                    Some(c) => Ok(Some(c)),
                    None => Err(Closed),
                };
            }
            let until = wake.map_or(overall, |w| w.min(overall));
            if until <= now {
                if now >= overall {
                    return Ok(None);
                }
                continue;
            }
            let (guard, _) = self.cv.wait_timeout(g, until - now).unwrap();
            g = guard;
        }
    }
}

/// The connection ended and every close-drain stop has been delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Closed;

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn cmd(pad: u16, low: u16, high: u16, backstop_ms: u32) -> RumbleCommand {
        RumbleCommand {
            pad,
            low,
            high,
            backstop_ms,
        }
    }

    #[test]
    fn v2_level_emits_and_expires_at_the_lease() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 0x4000, 0x8000, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 0x4000, 0x8000, 800))); // backstop = 2×ttl
                                                                     // No renewal: at the deadline the engine self-silences — the host-died safety net.
        let (c, wake) = e.poll(t0 + ms(200));
        assert_eq!(c, None);
        assert_eq!(wake, Some(t0 + ms(400)));
        assert_eq!(e.poll(t0 + ms(400)).0, Some(cmd(0, 0, 0, 0)));
        assert_eq!(e.poll(t0 + ms(500)), (None, None)); // silenced — nothing scheduled
    }

    #[test]
    fn renewal_re_emits_and_extends_the_deadline() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 0, Some(400));
        assert!(e.poll(t0).0.is_some());
        // A same-level renewal at t+300 re-emits (platform duration timers re-arm) and pushes the
        // deadline to t+700 — so t+500 (past the ORIGINAL deadline) still rumbles.
        e.wire_update(t0 + ms(300), 0, 100, 0, Some(400));
        assert_eq!(e.poll(t0 + ms(300)).0, Some(cmd(0, 100, 0, 800)));
        assert_eq!(e.poll(t0 + ms(500)).0, None);
        assert_eq!(e.poll(t0 + ms(700)).0, Some(cmd(0, 0, 0, 0)));
    }

    #[test]
    fn explicit_stop_is_immediate_and_cancels_the_lease() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 2, 500, 500, Some(400));
        assert!(e.poll(t0).0.is_some());
        e.wire_update(t0 + ms(50), 2, 0, 0, Some(0));
        assert_eq!(e.poll(t0 + ms(50)).0, Some(cmd(2, 0, 0, 0)));
        assert_eq!(e.poll(t0 + ms(600)), (None, None)); // no phantom expiry later
    }

    #[test]
    fn legacy_host_gets_the_uniform_staleness_bound() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 300, 0, None); // legacy: no TTL
        assert_eq!(e.poll(t0).0, Some(cmd(0, 300, 0, 2000)));
        // The legacy 500 ms refresh keeps it alive…
        e.wire_update(t0 + ms(500), 0, 300, 0, None);
        assert_eq!(e.poll(t0 + ms(500)).0, Some(cmd(0, 300, 0, 2000)));
        assert_eq!(e.poll(t0 + ms(1400)).0, None); // 900 ms since last wire — inside the bound
                                                   // …and one second of silence cuts it, on every platform alike.
        assert_eq!(e.poll(t0 + ms(1500)).0, Some(cmd(0, 0, 0, 0)));
    }

    #[test]
    fn keepalive_rekicks_with_jitter_and_never_outlives_the_lease() {
        let mut e = RumbleEngine::new();
        e.set_quirks(
            0,
            ActuatorQuirks {
                keepalive_ms: 40,
                min_pulse_ms: 0,
                dedup_jitter: true,
            },
        );
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 200, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 200, 800)));
        // Keepalives at the quirk cadence, alternating the low LSB to defeat SDL's dedupe.
        assert_eq!(e.poll(t0 + ms(40)).0, Some(cmd(0, 101, 200, 800)));
        assert_eq!(e.poll(t0 + ms(80)).0, Some(cmd(0, 100, 200, 800)));
        // At the lease deadline the EXPIRY wins — a keepalive can never sustain an ended level.
        assert_eq!(e.poll(t0 + ms(400)).0, Some(cmd(0, 0, 0, 0)));
        assert_eq!(e.poll(t0 + ms(440)), (None, None));
    }

    #[test]
    fn quirk_registered_mid_rumble_starts_keepalives() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 0, Some(400));
        assert!(e.poll(t0).0.is_some());
        e.set_quirks(
            0,
            ActuatorQuirks {
                keepalive_ms: 40,
                min_pulse_ms: 0,
                dedup_jitter: false,
            },
        );
        // First poll schedules from `now`; the next cadence tick emits.
        let (c, wake) = e.poll(t0 + ms(10));
        assert_eq!(c, None);
        assert_eq!(wake, Some(t0 + ms(50)));
        assert_eq!(e.poll(t0 + ms(50)).0, Some(cmd(0, 100, 0, 800)));
    }

    #[test]
    fn min_pulse_floors_the_backstop() {
        let mut e = RumbleEngine::new();
        e.set_quirks(
            0,
            ActuatorQuirks {
                keepalive_ms: 0,
                min_pulse_ms: 5000,
                dedup_jitter: false,
            },
        );
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 0, Some(100));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 0, 5000)));
    }

    #[test]
    fn close_drain_silences_every_buzzing_pad_once() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        e.wire_update(t0, 0, 100, 0, Some(400));
        e.wire_update(t0, 3, 0, 900, Some(400));
        let _ = e.poll(t0);
        let _ = e.poll(t0);
        let a = e.close_drain().unwrap();
        let b = e.close_drain().unwrap();
        assert_eq!((a.pad, a.low, a.high), (0, 0, 0));
        assert_eq!((b.pad, b.low, b.high), (3, 0, 0));
        assert_eq!(e.close_drain(), None);
    }

    #[test]
    fn stalled_embedder_wakes_to_one_current_command_not_a_backlog() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        // 20 renewals landed while the embedder was stalled — state, not a queue: exactly one
        // command comes out, carrying the latest level.
        for k in 0..20u64 {
            e.wire_update(t0 + ms(k * 120), 0, 100 + k as u16, 0, Some(400));
        }
        let t = t0 + ms(20 * 120);
        assert_eq!(e.poll(t).0, Some(cmd(0, 119, 0, 800)));
        assert_eq!(e.poll(t).0, None);
    }

    #[test]
    fn shared_close_delivers_drain_zero_then_closed() {
        let shared = std::sync::Arc::new(RumbleShared::new());
        let feed = RumbleFeed(shared.clone());
        feed.wire_update(1, 100, 0, Some(400));
        assert_eq!(
            shared.next_command(ms(100)).unwrap().unwrap(),
            cmd(1, 100, 0, 800)
        );
        drop(feed); // demux ended
        assert_eq!(
            shared.next_command(ms(100)).unwrap().unwrap(),
            cmd(1, 0, 0, 0)
        );
        assert_eq!(shared.next_command(ms(10)), Err(Closed));
    }
}
